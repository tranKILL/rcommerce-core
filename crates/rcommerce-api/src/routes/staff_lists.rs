//! Lists for staff screens: every product (active or not) with its stock, and
//! customers found by name or e-mail. Both are paged.

use axum::{
    extract::{Query, State},
    routing::get,
    Json, Router,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::state::AppState;
use rcommerce_core::Error;

const DEFAULT_PER_PAGE: i64 = 50;
const MAX_PER_PAGE: i64 = 100;

/// Page and size of a list request: page from 1, size within 1..=100
fn paging(page: Option<i64>, per_page: Option<i64>) -> (i64, i64) {
    (
        page.unwrap_or(1).max(1),
        per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE),
    )
}

/// `%text%` for ILIKE, with the pattern characters of `text` escaped
fn contains_pattern(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// The paging envelope of a list answer
fn meta(total: i64, page: i64, per_page: i64) -> serde_json::Value {
    serde_json::json!({
        "total": total,
        "page": page,
        "per_page": per_page,
        "total_pages": (total + per_page - 1) / per_page,
    })
}

/// `GET /admin/products` query
#[derive(Debug, Default, Deserialize)]
pub struct StaffProductQuery {
    /// Part of the title or SKU
    pub q: Option<String>,
    /// Only active (`true`) or inactive (`false`) products
    pub active: Option<bool>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

/// A product row of the staff list
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct StaffProduct {
    pub id: Uuid,
    pub title: String,
    pub slug: String,
    pub sku: Option<String>,
    pub price: Decimal,
    pub compare_at_price: Option<Decimal>,
    pub is_active: bool,
    pub is_featured: bool,
    /// Free stock summed over every location
    pub available: i64,
    pub updated_at: DateTime<Utc>,
}

fn product_filters(qb: &mut QueryBuilder<'_, Postgres>, query: &StaffProductQuery) {
    qb.push(" WHERE TRUE");
    if let Some(active) = query.active {
        qb.push(" AND p.is_active = ").push_bind(active);
    }
    if let Some(q) = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        let pattern = contains_pattern(q);
        qb.push(" AND (p.title ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR p.sku ILIKE ")
            .push_bind(pattern)
            .push(")");
    }
}

/// GET /admin/products - every product, active or not, by title, with its free
/// stock summed over locations (staff with `products:read`)
pub async fn list_products(
    State(state): State<AppState>,
    Query(query): Query<StaffProductQuery>,
) -> Result<Json<serde_json::Value>, Error> {
    let (page, per_page) = paging(query.page, query.per_page);
    let mut count = QueryBuilder::new("SELECT COUNT(*) FROM products p");
    product_filters(&mut count, &query);
    let total: i64 = count
        .build_query_scalar()
        .fetch_one(state.db.pool())
        .await?;

    let mut select = QueryBuilder::new(
        "SELECT p.id, p.title, p.slug, p.sku, p.price, p.compare_at_price, p.is_active,
                p.is_featured, p.updated_at,
                COALESCE((SELECT SUM(l.available_quantity) FROM inventory_levels l
                          WHERE l.product_id = p.id), 0)::bigint AS available
         FROM products p",
    );
    product_filters(&mut select, &query);
    select
        .push(" ORDER BY p.title, p.id LIMIT ")
        .push_bind(per_page)
        .push(" OFFSET ")
        .push_bind((page - 1) * per_page);
    let products: Vec<StaffProduct> = select.build_query_as().fetch_all(state.db.pool()).await?;
    Ok(Json(serde_json::json!({
        "products": products,
        "meta": meta(total, page, per_page),
    })))
}

/// `GET /customers` query (staff)
#[derive(Debug, Default, Deserialize)]
pub struct CustomerQuery {
    /// Part of the e-mail, first name or last name
    pub q: Option<String>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

/// A customer row of the staff list
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct StaffCustomer {
    pub id: Uuid,
    pub email: String,
    pub first_name: String,
    pub last_name: String,
    pub phone: Option<String>,
    pub accepts_marketing: bool,
    pub tax_exempt: bool,
    pub currency: String,
    pub email_verified: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub confirmed_at: Option<DateTime<Utc>>,
}

fn customer_filters(qb: &mut QueryBuilder<'_, Postgres>, query: &CustomerQuery) {
    qb.push(" WHERE TRUE");
    if let Some(q) = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        let pattern = contains_pattern(q);
        qb.push(" AND (email ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR first_name ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR last_name ILIKE ")
            .push_bind(pattern)
            .push(")");
    }
}

/// Customers, newest first, found by part of their e-mail or name. The caller
/// checks the staff permission.
pub async fn customer_page(
    state: &AppState,
    query: &CustomerQuery,
) -> Result<serde_json::Value, Error> {
    let (page, per_page) = paging(query.page, query.per_page);
    let mut count = QueryBuilder::new("SELECT COUNT(*) FROM customers");
    customer_filters(&mut count, query);
    let total: i64 = count
        .build_query_scalar()
        .fetch_one(state.db.pool())
        .await?;
    let mut select = QueryBuilder::new(
        "SELECT id, email, first_name, last_name, phone, accepts_marketing, tax_exempt,
                currency::text AS currency, email_verified, created_at, updated_at, confirmed_at
         FROM customers",
    );
    customer_filters(&mut select, query);
    select
        .push(" ORDER BY created_at DESC, id LIMIT ")
        .push_bind(per_page)
        .push(" OFFSET ")
        .push_bind((page - 1) * per_page);
    let customers: Vec<StaffCustomer> = select.build_query_as().fetch_all(state.db.pool()).await?;
    Ok(serde_json::json!({
        "customers": customers,
        "meta": meta(total, page, per_page),
    }))
}

/// Router for the staff product list (guarded by the admin middleware)
pub fn router() -> Router<AppState> {
    Router::new().route("/admin/products", get(list_products))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging_is_bounded() {
        assert_eq!(paging(None, None), (1, 50));
        assert_eq!(paging(Some(0), Some(500)), (1, 100));
        assert_eq!(paging(Some(3), Some(0)), (3, 1));
    }

    #[test]
    fn search_text_cannot_widen_the_pattern() {
        assert_eq!(contains_pattern("ST-05"), "%ST-05%");
        assert_eq!(contains_pattern("a_b%"), "%a\\_b\\%%");
    }

    #[test]
    fn meta_counts_pages() {
        assert_eq!(meta(0, 1, 50)["total_pages"], 0);
        assert_eq!(meta(101, 2, 50)["total_pages"], 3);
    }
}
