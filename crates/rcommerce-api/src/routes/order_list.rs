//! Order lists: a customer's own orders, or every order for staff with
//! `orders:read`, newest first, paged and filtered.

use std::collections::HashMap;

use axum::{
    extract::{Query, State},
    Extension, Json,
};
use rcommerce_core::list_sort::{order_by, SortColumns};
use rcommerce_core::{Action, Error, Resource};
use serde::Deserialize;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use super::order::{order_summary, ORDER_COLUMNS};
use crate::middleware::{staff_access::staff_can, JwtAuth};
use crate::state::AppState;

const DEFAULT_PER_PAGE: i64 = 50;
const MAX_PER_PAGE: i64 = 100;
const ORDER_STATUSES: [&str; 7] = [
    "pending",
    "confirmed",
    "processing",
    "on_hold",
    "completed",
    "cancelled",
    "refunded",
];

/// `GET /orders` query
#[derive(Debug, Default, Deserialize)]
pub struct OrderListQuery {
    pub page: Option<i64>,
    pub per_page: Option<i64>,
    /// Order status, e.g. `confirmed`
    pub status: Option<String>,
    /// Part of the order number or of the customer's e-mail
    pub q: Option<String>,
    /// Orders whose stock comes from this location (pickup or delivery)
    pub location_id: Option<Uuid>,
    /// Only pickup orders (`true`), or only the others (`false`)
    pub pickup: Option<bool>,
    /// `date` (default `-date`), `number`, `customer`, `items`, `total`,
    /// `mode` or `stage`; `-` in front for descending order
    pub sort: Option<String>,
}

/// Columns the order list can be sorted on. `stage` follows an order's life:
/// awaiting payment, received, preparing, ready, shipped, completed, then
/// cancelled or refunded.
const ORDER_SORTS: SortColumns = &[
    ("date", "created_at"),
    ("number", "order_number"),
    (
        "customer",
        "lower(COALESCE(NULLIF(TRIM(CONCAT(shipping_address->>'first_name', ' ', \
         shipping_address->>'last_name')), ''), customer_email))",
    ),
    (
        "items",
        "(SELECT SUM(i.quantity) FROM order_items i WHERE i.order_id = orders.id)",
    ),
    ("total", "total"),
    ("mode", "(metadata->'pickup') IS NULL"),
    (
        "stage",
        "CASE WHEN status::text = 'pending' THEN 0 \
         WHEN status::text IN ('cancelled', 'refunded') THEN 7 \
         WHEN status::text = 'completed' THEN 6 \
         WHEN metadata->'pickup'->>'ready_marked_at' IS NOT NULL THEN 4 \
         WHEN fulfillment_status::text = 'shipped' THEN 5 \
         WHEN status::text = 'processing' THEN 3 ELSE 2 END",
    ),
];

/// The status filter in its database spelling; unknown statuses are a 400
fn status_filter(status: Option<&str>) -> Result<Option<&'static str>, Error> {
    let Some(status) = status.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let wanted = match status.to_ascii_lowercase().as_str() {
        "canceled" => "cancelled".to_string(),
        "onhold" => "on_hold".to_string(),
        other => other.to_string(),
    };
    ORDER_STATUSES
        .iter()
        .find(|s| **s == wanted)
        .map(|s| Some(*s))
        .ok_or_else(|| Error::validation(format!("Unknown order status: {status}")))
}

/// `%text%` for ILIKE, with the pattern characters of `text` escaped
fn contains_pattern(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// The WHERE clause shared by the count and the page queries
fn push_filters(
    qb: &mut QueryBuilder<'_, Postgres>,
    customer: Option<Uuid>,
    status: Option<&'static str>,
    query: &OrderListQuery,
) {
    qb.push(" WHERE TRUE");
    if let Some(customer) = customer {
        qb.push(" AND customer_id = ").push_bind(customer);
    }
    if let Some(status) = status {
        qb.push(" AND status = ")
            .push_bind(status)
            .push("::order_status");
    }
    if let Some(q) = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        let pattern = contains_pattern(q);
        qb.push(" AND (order_number ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR customer_email ILIKE ")
            .push_bind(pattern)
            .push(")");
    }
    if let Some(location) = query.location_id {
        qb.push(" AND metadata->>'fulfillment_location_id' = ")
            .push_bind(location.to_string());
    }
    match query.pickup {
        Some(true) => qb.push(" AND (metadata->'pickup') IS NOT NULL"),
        Some(false) => qb.push(" AND (metadata->'pickup') IS NULL"),
        None => qb,
    };
}

/// Number of articles of each order, in one query
async fn item_counts(state: &AppState, ids: &[Uuid]) -> Result<HashMap<Uuid, i64>, Error> {
    let rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT order_id, COALESCE(SUM(quantity), 0)::bigint FROM order_items
          WHERE order_id = ANY($1) GROUP BY order_id",
    )
    .bind(ids)
    .fetch_all(state.db.pool())
    .await?;
    Ok(rows.into_iter().collect())
}

/// Order summaries with their number of articles
pub(crate) async fn summaries(
    state: &AppState,
    orders: Vec<rcommerce_core::models::Order>,
) -> Result<Vec<super::order::OrderResponse>, Error> {
    let ids: Vec<Uuid> = orders.iter().map(|o| o.id).collect();
    let counts = item_counts(state, &ids).await?;
    Ok(orders
        .into_iter()
        .map(|o| {
            let count = counts.get(&o.id).copied().unwrap_or(0);
            let mut summary = order_summary(o);
            summary.item_count = Some(count);
            summary
        })
        .collect())
}

/// GET /orders - the customer's orders, or all orders for staff, newest first
/// unless sorted otherwise. Lines are not included (see `GET /orders/:id`);
/// `item_count` gives their size.
pub async fn list_orders(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Query(query): Query<OrderListQuery>,
) -> Result<Json<serde_json::Value>, Error> {
    let all = staff_can(&state, &auth, Resource::Orders, Action::Read).await;
    let customer = (!all).then_some(auth.customer_id);
    let status = status_filter(query.status.as_deref())?;
    let order = order_by(query.sort.as_deref(), ORDER_SORTS, "-date", "id")?;
    let per_page = query
        .per_page
        .unwrap_or(DEFAULT_PER_PAGE)
        .clamp(1, MAX_PER_PAGE);
    let page = query.page.unwrap_or(1).max(1);

    let mut count = QueryBuilder::new("SELECT COUNT(*) FROM orders");
    push_filters(&mut count, customer, status, &query);
    let total: i64 = count
        .build_query_scalar()
        .fetch_one(state.db.pool())
        .await?;

    let mut select = QueryBuilder::new(format!("SELECT {ORDER_COLUMNS} FROM orders"));
    push_filters(&mut select, customer, status, &query);
    select
        .push(format!(" ORDER BY {order} LIMIT "))
        .push_bind(per_page)
        .push(" OFFSET ")
        .push_bind((page - 1) * per_page);
    let orders = summaries(
        &state,
        select.build_query_as().fetch_all(state.db.pool()).await?,
    )
    .await?;
    Ok(Json(serde_json::json!({
        "orders": orders,
        "meta": {
            "total": total,
            "page": page,
            "per_page": per_page,
            "total_pages": (total + per_page - 1) / per_page,
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_checked_and_normalised() {
        assert_eq!(status_filter(None).unwrap(), None);
        assert_eq!(status_filter(Some(" ")).unwrap(), None);
        assert_eq!(status_filter(Some("Confirmed")).unwrap(), Some("confirmed"));
        assert_eq!(status_filter(Some("canceled")).unwrap(), Some("cancelled"));
        assert_eq!(status_filter(Some("onhold")).unwrap(), Some("on_hold"));
        assert!(status_filter(Some("shipped")).is_err());
    }

    #[test]
    fn search_text_cannot_widen_the_pattern() {
        assert_eq!(contains_pattern("ORD-1"), "%ORD-1%");
        assert_eq!(contains_pattern("50%_off\\"), "%50\\%\\_off\\\\%");
    }
}
