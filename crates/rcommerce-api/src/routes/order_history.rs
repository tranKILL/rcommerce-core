//! An order's history (`GET /orders/:id/events`) and what follows a step:
//! points and the invoice after a hand-over, points and promotion uses given
//! back after a cancellation or refund.

use axum::{
    extract::{Path, State},
    Extension, Json,
};
use rcommerce_core::services::loyalty_service::LoyaltyService;
use rcommerce_core::services::order_events::{self, Actor, OrderEvent};
use rcommerce_core::services::promotion_service;
use rcommerce_core::{Action, Error, Resource};
use tracing::warn;
use uuid::Uuid;

use crate::middleware::JwtAuth;
use crate::middleware::staff_access::staff_can;
use crate::state::AppState;

/// GET /orders/:id/events - the order's history, oldest first: for its
/// customer (staff names left out) and for staff who read orders
pub async fn order_events(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Error> {
    let owner: Option<Uuid> = sqlx::query_scalar("SELECT customer_id FROM orders WHERE id = $1")
        .bind(id)
        .fetch_optional(state.db.pool())
        .await?
        .ok_or_else(|| Error::not_found("Order not found"))?;
    let staff = staff_can(&state, &auth, Resource::Orders, Action::Read).await;
    if !staff && owner != Some(auth.customer_id) {
        return Err(Error::forbidden("Access denied"));
    }
    let events: Vec<OrderEvent> = order_events::history(state.db.pool(), id, staff).await?;
    Ok(Json(serde_json::json!({ "events": events })))
}

/// Who acts on an order: its customer, else a staff member
pub(crate) fn actor(auth: &JwtAuth, owner: Option<Uuid>) -> Actor {
    if owner == Some(auth.customer_id) {
        Actor::Customer(Some(auth.customer_id))
    } else {
        Actor::Staff(auth.customer_id)
    }
}

/// After a collection or delivery: loyalty points, then the invoice
pub(crate) async fn after_handover(state: &AppState, id: Uuid) {
    if let Err(e) = LoyaltyService::new(state.db.pool().clone()).award_for_order(id).await {
        warn!("Failed to award the loyalty points of order {id}: {e}");
    }
    super::invoices::issue_after_handover(state, id).await;
}

/// After a cancellation or refund: points earned taken back, points used
/// given back, promotion uses released
pub(crate) async fn after_cancel(state: &AppState, id: Uuid) {
    if let Err(e) = LoyaltyService::new(state.db.pool().clone()).undo_for_order(id).await {
        warn!("Failed to undo the loyalty points of order {id}: {e}");
    }
    let released = match state.db.pool().acquire().await {
        Ok(mut conn) => promotion_service::release_uses(&mut conn, id).await,
        Err(e) => Err(e.into()),
    };
    if let Err(e) = released {
        warn!("Failed to release the promotion uses of order {id}: {e}");
    }
}
