//! Orders the store delivers with its own vehicles (site deliveries): staff
//! mark the order out for delivery with the driver's name, then delivered with
//! the name of who received it.
//!
//! The steps are recorded under `delivery` in the order metadata and as a
//! fulfillment of the store (`tracking_company` = `store`). Delivering completes
//! the order and takes its committed stock out of the location's reserved
//! quantity. Pickup orders use the counter steps instead.

use axum::{
    extract::{Path, State},
    Extension, Json,
};
use chrono::{DateTime, Utc};
use rcommerce_core::Error;
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::order::{build_order_response, fetch_order, OrderResponse};
use super::order_pickup::{checked_name, pickup_of, release_committed_stock, require_order_writer};
use crate::middleware::JwtAuth;
use crate::state::AppState;

/// A store delivery's progress, kept under `delivery` in the order metadata
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OrderDelivery {
    #[serde(default)]
    pub dispatched_at: Option<DateTime<Utc>>,
    /// Who drives the delivery
    #[serde(default)]
    pub driver: Option<String>,
    #[serde(default)]
    pub delivered_at: Option<DateTime<Utc>>,
    /// Who received the goods, as written down on delivery
    #[serde(default)]
    pub received_by: Option<String>,
}

/// The store delivery details of an order's metadata (`None` before dispatch)
pub fn delivery_of(metadata: &Option<serde_json::Value>) -> Option<OrderDelivery> {
    let delivery = metadata.as_ref()?.get("delivery")?;
    serde_json::from_value(delivery.clone()).ok()
}

/// `POST /orders/:id/delivery/dispatched` body
#[derive(Debug, Deserialize)]
pub struct DispatchedRequest {
    pub driver: String,
}

/// `POST /orders/:id/delivery/delivered` body
#[derive(Debug, Deserialize)]
pub struct DeliveredRequest {
    pub received_by: String,
}

/// The order's status, fulfillment status and delivery progress, locked for the
/// update; pickup orders are refused
async fn lock_delivery_order(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<(String, String, Option<OrderDelivery>), Error> {
    let row: Option<(String, String, Option<serde_json::Value>)> = sqlx::query_as(
        "SELECT status::text, fulfillment_status::text, metadata FROM orders WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    let (status, fulfillment, metadata) = row.ok_or_else(|| Error::not_found("Order not found"))?;
    if pickup_of(&metadata).is_some() {
        return Err(Error::validation(
            "Pickup orders are handed over at the counter, not delivered",
        ));
    }
    Ok((status, fulfillment, delivery_of(&metadata)))
}

/// POST /orders/:id/delivery/dispatched - the goods left with the store's driver
pub async fn mark_dispatched(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(id): Path<Uuid>,
    Json(request): Json<DispatchedRequest>,
) -> Result<Json<OrderResponse>, Error> {
    require_order_writer(&state, &auth).await?;
    let driver = checked_name(&request.driver, "driver")?;
    let mut tx = state.db.pool().begin().await?;
    let (status, fulfillment, delivery) = lock_delivery_order(&mut tx, id).await?;
    let ready = matches!(status.as_str(), "confirmed" | "processing")
        && matches!(fulfillment.as_str(), "pending" | "processing");
    if !ready || delivery.is_some() {
        return Err(Error::validation(
            "Only paid orders not yet dispatched can go out for delivery",
        ));
    }
    let fulfillment_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO fulfillments (id, order_id, status, tracking_company, shipped_at)
         VALUES ($1, $2, 'shipped', 'store', NOW())",
    )
    .bind(fulfillment_id)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO fulfillment_items (id, fulfillment_id, order_item_id, quantity)
         SELECT gen_random_uuid(), $1, id, quantity FROM order_items WHERE order_id = $2",
    )
    .bind(fulfillment_id)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let note = format!(
        "[{}] Out for delivery with {}",
        Utc::now().to_rfc3339(),
        driver
    );
    record_step(
        &mut tx,
        id,
        "processing",
        "shipped",
        "dispatched_at",
        ("driver", driver),
        &note,
    )
    .await?;
    tx.commit().await?;
    let order = fetch_order(&state, id).await?;
    Ok(Json(build_order_response(&state, order).await?))
}

/// POST /orders/:id/delivery/delivered - received on site: the order is completed
pub async fn mark_delivered(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(id): Path<Uuid>,
    Json(request): Json<DeliveredRequest>,
) -> Result<Json<OrderResponse>, Error> {
    require_order_writer(&state, &auth).await?;
    let receiver = checked_name(&request.received_by, "received_by")?;
    let mut tx = state.db.pool().begin().await?;
    let (status, _, delivery) = lock_delivery_order(&mut tx, id).await?;
    let dispatched = delivery.as_ref().is_some_and(|d| d.delivered_at.is_none());
    if status != "processing" || !dispatched {
        return Err(Error::validation(
            "Only orders out for delivery can be delivered",
        ));
    }
    sqlx::query(
        "UPDATE fulfillments SET status = 'delivered', delivered_at = NOW(), updated_at = NOW()
          WHERE order_id = $1 AND tracking_company = 'store' AND status = 'shipped'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let note = format!(
        "[{}] Delivered, received by {}",
        Utc::now().to_rfc3339(),
        receiver
    );
    record_step(
        &mut tx,
        id,
        "completed",
        "delivered",
        "delivered_at",
        ("received_by", receiver),
        &note,
    )
    .await?;
    sqlx::query("UPDATE orders SET completed_at = NOW() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    release_committed_stock(&mut tx, id).await?;
    tx.commit().await?;
    let order = fetch_order(&state, id).await?;
    Ok(Json(build_order_response(&state, order).await?))
}

/// Set the order's statuses, add `{at_key: now, name.0: name.1}` to its
/// `delivery` metadata and append `note` to its notes
async fn record_step(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    status: &str,
    fulfillment: &str,
    at_key: &str,
    name: (&str, &str),
    note: &str,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE orders SET status = $2::order_status, fulfillment_status = $3::fulfillment_status,
             updated_at = NOW(),
             metadata = jsonb_set(COALESCE(metadata, '{}'::jsonb), '{delivery}',
                 COALESCE(metadata->'delivery', '{}'::jsonb)
                 || jsonb_build_object($4::text, NOW(), $5::text, $6::text)),
             notes = CASE WHEN notes IS NULL OR btrim(notes) = '' THEN $7
                          ELSE notes || E'\\n' || $7 END
         WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(fulfillment)
    .bind(at_key)
    .bind(name.0)
    .bind(name.1)
    .bind(note)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_progress_comes_from_the_metadata() {
        let metadata = Some(serde_json::json!({
            "delivery": {"dispatched_at": "2026-11-10T07:00:00+00:00", "driver": "Sam"}
        }));
        let delivery = delivery_of(&metadata).unwrap();
        assert_eq!(delivery.driver.as_deref(), Some("Sam"));
        assert!(delivery.dispatched_at.is_some() && delivery.delivered_at.is_none());
        assert_eq!(delivery_of(&Some(serde_json::json!({}))), None);
        assert_eq!(delivery_of(&None), None);
    }
}
