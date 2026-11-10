//! Pickup orders at the counter: staff mark an order ready for collection, then
//! collected with the collector's name.
//!
//! The steps are recorded under `pickup` in the order metadata, next to the
//! location and the ready time set at checkout. Collecting completes the order
//! with a delivered fulfillment and takes the order's committed stock out of the
//! location's reserved quantity: the goods have left the counter.

use axum::{
    extract::{Path, State},
    Extension, Json,
};
use chrono::{DateTime, Utc};
use rcommerce_core::{Action, Error, Resource};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::order::{build_order_response, fetch_order, OrderResponse};
use crate::middleware::{staff_access::staff_can, JwtAuth};
use crate::state::AppState;

/// Longest collector name accepted
const MAX_COLLECTOR_NAME: usize = 100;

/// A pickup order's collection details, kept under `pickup` in the order metadata
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OrderPickup {
    pub location_id: Option<Uuid>,
    pub location_name: Option<String>,
    /// When the order was expected to be ready, as quoted at checkout
    pub ready_at: Option<DateTime<Utc>>,
    /// When staff marked the order ready for collection
    #[serde(default)]
    pub ready_marked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub collected_at: Option<DateTime<Utc>>,
    /// Who collected the order, as written down at the counter
    #[serde(default)]
    pub collected_by: Option<String>,
}

/// The pickup details of an order's metadata (`None` for other orders)
pub fn pickup_of(metadata: &Option<serde_json::Value>) -> Option<OrderPickup> {
    let pickup = metadata.as_ref()?.get("pickup")?;
    serde_json::from_value(pickup.clone()).ok()
}

/// The location an order's stock was reserved at, when checkout chose one
pub fn fulfillment_location_of(metadata: &Option<serde_json::Value>) -> Option<Uuid> {
    metadata
        .as_ref()?
        .get("fulfillment_location_id")?
        .as_str()?
        .parse()
        .ok()
}

/// `POST /orders/:id/pickup/collected` body
#[derive(Debug, Deserialize)]
pub struct CollectedRequest {
    pub collected_by: String,
}

/// The order's status and pickup details, locked for the update
async fn lock_pickup_order(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<(String, OrderPickup), Error> {
    let row: Option<(String, Option<serde_json::Value>)> =
        sqlx::query_as("SELECT status::text, metadata FROM orders WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    let (status, metadata) = row.ok_or_else(|| Error::not_found("Order not found"))?;
    let pickup = pickup_of(&metadata).ok_or_else(|| Error::validation("Not a pickup order"))?;
    Ok((status, pickup))
}

/// A person's name typed at the counter or on delivery: required, at most 100 characters
pub(crate) fn checked_name<'a>(value: &'a str, field: &str) -> Result<&'a str, Error> {
    let name = value.trim();
    if name.is_empty() || name.chars().count() > MAX_COLLECTOR_NAME {
        return Err(Error::validation(format!(
            "{field} must name a person (at most {MAX_COLLECTOR_NAME} characters)"
        )));
    }
    Ok(name)
}

/// Staff with `orders:write`
pub(crate) async fn require_order_writer(state: &AppState, auth: &JwtAuth) -> Result<(), Error> {
    if staff_can(state, auth, Resource::Orders, Action::Write).await {
        Ok(())
    } else {
        Err(Error::forbidden("Admin access required"))
    }
}

/// POST /orders/:id/pickup/ready - the order is prepared and waits at the counter
pub async fn mark_ready(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(id): Path<Uuid>,
) -> Result<Json<OrderResponse>, Error> {
    require_order_writer(&state, &auth).await?;
    let mut tx = state.db.pool().begin().await?;
    let (status, pickup) = lock_pickup_order(&mut tx, id).await?;
    if !matches!(status.as_str(), "confirmed" | "processing") {
        return Err(Error::validation(
            "Only confirmed orders can be made ready for collection",
        ));
    }
    if pickup.ready_marked_at.is_some() {
        return Err(Error::validation(
            "The order is already ready for collection",
        ));
    }
    sqlx::query(
        "UPDATE orders SET status = 'processing', fulfillment_status = 'processing',
             metadata = jsonb_set(metadata, '{pickup,ready_marked_at}', to_jsonb(NOW())),
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    super::email_helpers::send_order_ready_email(&state, id).await;
    let order = fetch_order(&state, id).await?;
    Ok(Json(build_order_response(&state, order).await?))
}

/// POST /orders/:id/pickup/collected - handed over at the counter: the order is
/// completed with a delivered fulfillment
pub async fn mark_collected(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(id): Path<Uuid>,
    Json(request): Json<CollectedRequest>,
) -> Result<Json<OrderResponse>, Error> {
    require_order_writer(&state, &auth).await?;
    let collector = checked_name(&request.collected_by, "collected_by")?;
    let mut tx = state.db.pool().begin().await?;
    let (status, pickup) = lock_pickup_order(&mut tx, id).await?;
    if status != "processing" || pickup.ready_marked_at.is_none() {
        return Err(Error::validation(
            "Only orders ready for collection can be collected",
        ));
    }
    record_collection(&mut tx, id, collector, pickup.location_name.as_deref()).await?;
    release_committed_stock(&mut tx, id).await?;
    tx.commit().await?;
    super::invoices::issue_after_handover(&state, id).await;
    let order = fetch_order(&state, id).await?;
    Ok(Json(build_order_response(&state, order).await?))
}

/// A delivered fulfillment for every line, the order completed, the collector
/// in the metadata and the order notes
async fn record_collection(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    collector: &str,
    location: Option<&str>,
) -> Result<(), Error> {
    let fulfillment = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO fulfillments (id, order_id, status, tracking_company, shipped_at, delivered_at)
         VALUES ($1, $2, 'delivered', 'pickup', NOW(), NOW())",
    )
    .bind(fulfillment)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO fulfillment_items (id, fulfillment_id, order_item_id, quantity)
         SELECT gen_random_uuid(), $1, id, quantity FROM order_items WHERE order_id = $2",
    )
    .bind(fulfillment)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    let note = format!(
        "[{}] Collected at {} by {}",
        Utc::now().to_rfc3339(),
        location.unwrap_or("the store"),
        collector
    );
    sqlx::query(
        "UPDATE orders SET status = 'completed', fulfillment_status = 'delivered',
             completed_at = NOW(), updated_at = NOW(),
             metadata = jsonb_set(metadata, '{pickup}', (metadata->'pickup')
                 || jsonb_build_object('collected_at', NOW(), 'collected_by', $2::text)),
             notes = CASE WHEN notes IS NULL OR btrim(notes) = '' THEN $3
                          ELSE notes || E'\\n' || $3 END
         WHERE id = $1",
    )
    .bind(id)
    .bind(collector)
    .bind(note)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The order's committed stock leaves the reserved quantity of its location
/// (handed over or delivered)
pub(crate) async fn release_committed_stock(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE inventory_levels l
            SET reserved_quantity = GREATEST(l.reserved_quantity - r.quantity, 0),
                updated_at = NOW()
           FROM stock_reservations r
          WHERE r.order_id = $1 AND r.status = 'committed'
            AND l.product_id = r.product_id
            AND l.variant_id IS NOT DISTINCT FROM r.variant_id
            AND l.location_id = r.location_id",
    )
    .bind(id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE stock_reservations SET status = 'released', updated_at = NOW()
          WHERE order_id = $1 AND status = 'committed'",
    )
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pickup_details_come_from_the_metadata() {
        let location = Uuid::new_v4();
        let metadata = Some(serde_json::json!({
            "fulfillment_location_id": location,
            "pickup": {"location_id": location, "location_name": "North", "ready_at": "2026-11-10T07:00:00Z"}
        }));
        let pickup = pickup_of(&metadata).unwrap();
        assert_eq!(pickup.location_id, Some(location));
        assert_eq!(pickup.location_name.as_deref(), Some("North"));
        assert!(pickup.ready_marked_at.is_none() && pickup.collected_at.is_none());
        assert_eq!(fulfillment_location_of(&metadata), Some(location));
    }

    #[test]
    fn other_orders_have_no_pickup_details() {
        let metadata = Some(serde_json::json!({"cart_id": "x", "fulfillment_location_id": null}));
        assert_eq!(pickup_of(&metadata), None);
        assert_eq!(fulfillment_location_of(&metadata), None);
        assert_eq!(pickup_of(&None), None);
    }
}
