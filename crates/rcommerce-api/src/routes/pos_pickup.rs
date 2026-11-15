//! Pickup orders handed over at the till (`pos:write`).
//!
//! `POST /admin/pos/pickups` hands over a pickup order collected at the
//! session's branch, in one step: the counter's "ready" and "collected". An
//! order not paid yet (payment at pickup, a phone order) is paid there first,
//! by card (approved terminal transactions) and cash, with the change given
//! back; the money is a sale of the till session. Loyalty points and the
//! invoice follow as at the counter.

use axum::{extract::State, routing::post, Extension, Json, Router};
use rcommerce_core::services::order_events::{self, Actor};
use rcommerce_core::services::pos::basket::{settle, Settlement};
use rcommerce_core::services::pos::session::{record, Entry};
use rcommerce_core::services::pos::{TillSession, TillSessions};
use rcommerce_core::Error;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::order::{build_order_response, fetch_order, OrderResponse};
use super::order_pickup::{checked_name, pickup_of, record_collection, release_committed_stock};
use crate::middleware::JwtAuth;
use crate::state::AppState;

/// `POST /admin/pos/pickups` body
#[derive(Debug, Deserialize)]
pub struct PickupBody {
    pub session_id: Uuid,
    pub order_id: Uuid,
    /// Who takes the goods (default: the customer's name on the order)
    #[serde(default)]
    pub collected_by: Option<String>,
    /// Cash handed over, when the order is not paid yet
    #[serde(default, deserialize_with = "super::pos::number_or_text")]
    pub cash_given: Decimal,
    /// Approved terminal transactions paying it
    #[serde(default)]
    pub card_payments: Vec<Uuid>,
}

/// A pickup order handed over
#[derive(Debug, Serialize)]
pub struct PickupResult {
    pub order: OrderResponse,
    /// What was paid at the till (all zero for an order paid online)
    pub settlement: Settlement,
    pub invoice_number: Option<String>,
}

/// The order as the hand-over needs it, locked
#[derive(Debug, sqlx::FromRow)]
struct PickupOrder {
    status: String,
    payment_status: String,
    total: Decimal,
    currency: String,
    metadata: Option<serde_json::Value>,
    customer_name: Option<String>,
}

/// Lock the order and check it can be handed over at this branch
async fn lock(
    tx: &mut Transaction<'_, Postgres>,
    order: Uuid,
    session: &TillSession,
) -> Result<PickupOrder, Error> {
    let row: PickupOrder = sqlx::query_as(
        "SELECT o.status::text AS status, o.payment_status::text AS payment_status, o.total, \
         o.currency::text AS currency, o.metadata, \
         NULLIF(TRIM(COALESCE(o.billing_address->>'first_name', o.shipping_address->>'first_name', '') || ' ' || \
           COALESCE(o.billing_address->>'last_name', o.shipping_address->>'last_name', '')), '') AS customer_name \
         FROM orders o WHERE o.id = $1 FOR UPDATE",
    )
    .bind(order)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::not_found("Order not found"))?;
    let pickup = pickup_of(&row.metadata).ok_or_else(|| Error::validation("Not a pickup order"))?;
    if pickup.location_id != Some(session.location_id) {
        return Err(Error::validation(format!(
            "This order is collected at {}",
            pickup.location_name.as_deref().unwrap_or("another branch")
        )));
    }
    let paid = row.payment_status == "paid";
    let open = match row.status.as_str() {
        "confirmed" | "processing" => true,
        // not paid yet: paid at the till
        "pending" => !paid,
        _ => false,
    };
    if !open {
        return Err(Error::validation(format!(
            "This order cannot be handed over (status {})",
            row.status
        )));
    }
    Ok(row)
}

/// POST /admin/pos/pickups
async fn hand_over(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(body): Json<PickupBody>,
) -> Result<Json<PickupResult>, Error> {
    let session = TillSessions::new(state.db.pool().clone())
        .open_of(body.session_id, auth.customer_id)
        .await?;
    let cards = super::pos_sale::card_amounts(&state, session.id, &body.card_payments).await?;
    let mut tx = state.db.pool().begin().await?;
    let order = lock(&mut tx, body.order_id, &session).await?;
    let due = if order.payment_status == "paid" {
        Decimal::ZERO
    } else {
        order.total
    };
    let card_amounts: Vec<Decimal> = cards.iter().map(|c| c.1).collect();
    if due.is_zero() && (!cards.is_empty() || body.cash_given > Decimal::ZERO) {
        return Err(Error::validation("This order is already paid"));
    }
    let plan = settle(due, body.cash_given, &card_amounts)?;
    let collector = match body.collected_by.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => checked_name(name, "collected_by")?.to_string(),
        _ => order
            .customer_name
            .clone()
            .unwrap_or_else(|| "Comptoir".to_string()),
    };
    let handed = HandOver {
        order: body.order_id,
        session: &session,
        cashier: auth.customer_id,
        plan: &plan,
        cards: &cards,
        currency: &order.currency,
    };
    if !due.is_zero() {
        take_payment(&mut tx, &handed).await?;
    }
    collect(&mut tx, &handed, &collector).await?;
    tx.commit().await?;
    super::order_history::after_handover(&state, body.order_id).await;
    let invoice_number: Option<String> = sqlx::query_scalar(
        "SELECT number FROM invoices WHERE order_id = $1 AND kind = 'invoice' LIMIT 1",
    )
    .bind(body.order_id)
    .fetch_optional(state.db.pool())
    .await?;
    let response = build_order_response(&state, fetch_order(&state, body.order_id).await?).await?;
    Ok(Json(PickupResult {
        order: response,
        settlement: plan,
        invoice_number,
    }))
}

/// A pickup order being handed over at the till
struct HandOver<'a> {
    order: Uuid,
    session: &'a TillSession,
    cashier: Uuid,
    plan: &'a Settlement,
    cards: &'a [(Uuid, Decimal)],
    currency: &'a str,
}

/// The money taken at the till: the card payments tied to the order, the
/// session's movements, a payment row per method, the order paid
async fn take_payment(
    tx: &mut Transaction<'_, Postgres>,
    handed: &HandOver<'_>,
) -> Result<(), Error> {
    let ids: Vec<Uuid> = handed.cards.iter().map(|c| c.0).collect();
    let claimed = sqlx::query(
        "UPDATE terminal_transactions SET order_id = $1, updated_at = NOW() \
         WHERE id = ANY($2) AND order_id IS NULL AND status = 'approved'",
    )
    .bind(handed.order)
    .bind(&ids)
    .execute(&mut **tx)
    .await?;
    if claimed.rows_affected() as usize != ids.len() {
        return Err(Error::validation("A card payment was used by another sale"));
    }
    let mut entries: Vec<(&str, Decimal)> = handed.cards.iter().map(|c| ("card", c.1)).collect();
    if handed.plan.cash > Decimal::ZERO {
        entries.push(("cash", handed.plan.cash));
    }
    for (method, amount) in entries {
        let entry = Entry {
            session: handed.session.id,
            kind: "sale",
            method,
            amount,
            order: Some(handed.order),
            reason: None,
            staff: handed.cashier,
        };
        record(tx, entry).await?;
    }
    for (method, amount) in [("cash", handed.plan.cash), ("debit_card", handed.plan.card)] {
        if amount <= Decimal::ZERO {
            continue;
        }
        sqlx::query(
            "INSERT INTO payments (order_id, amount, currency, status, gateway, gateway_payment_id, \
               payment_method, processed_at) \
             VALUES ($1, $2, UPPER($3)::currency, 'paid', 'till', $4, $5::payment_method_type, NOW())",
        )
        .bind(handed.order)
        .bind(amount)
        .bind(handed.currency)
        .bind(format!("till-{}-{method}", handed.order.simple()))
        .bind(method)
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query("UPDATE orders SET payment_status = 'paid', updated_at = NOW() WHERE id = $1")
        .bind(handed.order)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Ready and collected at once: the order completed, its stock out of the
/// branch, the hand-over (and any payment) in its history
async fn collect(
    tx: &mut Transaction<'_, Postgres>,
    handed: &HandOver<'_>,
    collector: &str,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE orders SET status = 'processing', fulfillment_status = 'processing', \
           metadata = jsonb_set(metadata, '{pickup,ready_marked_at}', \
             COALESCE(metadata->'pickup'->'ready_marked_at', to_jsonb(NOW()))), \
           updated_at = NOW() WHERE id = $1",
    )
    .bind(handed.order)
    .execute(&mut **tx)
    .await?;
    let location = handed.session.location_name.as_str();
    record_collection(tx, handed.order, collector, Some(location)).await?;
    release_committed_stock(tx, handed.order).await?;
    order_events::record(
        tx,
        handed.order,
        "collected",
        Actor::Staff(handed.cashier),
        serde_json::json!({
            "collected_by": collector,
            "location": location,
            "at_till": true,
            "cash": handed.plan.cash,
            "card": handed.plan.card,
            "change": handed.plan.change,
        }),
    )
    .await?;
    Ok(())
}

/// Router for pickups at the till (behind the admin middleware: `pos:write`)
pub fn admin_router() -> Router<AppState> {
    Router::new().route("/admin/pos/pickups", post(hand_over))
}
