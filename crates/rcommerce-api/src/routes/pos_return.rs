//! Returns at the till (`pos`):
//!
//! - `GET /admin/pos/orders/:id/returnable` the order's lines with what can
//!   still come back, and what each payment method can still refund.
//! - `POST /admin/pos/returns/preview` price a return: the credited lines
//!   (the discount's share included), VAT, total and the refund by method.
//! - `POST /admin/pos/returns` take the goods back: back in stock at the
//!   till's branch (unless damaged), money back by the original methods (the
//!   online payment through its gateway, the card through approved terminal
//!   refunds, then cash), the points taken back, and a credit note when the
//!   order was invoiced.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Extension, Json, Router,
};
use rcommerce_core::invoicing::{CreditNoteDraft, Invoice};
use rcommerce_core::notification::i18n;
use rcommerce_core::services::loyalty_service;
use rcommerce_core::services::order_events::{self, Actor};
use rcommerce_core::services::pos::returns::{
    card_refunds, close_when_all_back, plan_refund, price_return, record_return,
    refund_till_payments, restock, returnable, PricedReturn, RefundPlan, ReturnLineInput,
    ReturnRecord, Returnable,
};
use rcommerce_core::services::pos::session::{record, Entry};
use rcommerce_core::services::pos::{TillSession, TillSessions};
use rcommerce_core::Error;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::middleware::JwtAuth;
use crate::state::AppState;

/// Longest reason kept with a return
const MAX_REASON: usize = 500;

/// `POST /admin/pos/returns/preview` body
#[derive(Debug, Deserialize)]
pub struct PreviewBody {
    pub session_id: Uuid,
    pub order_id: Uuid,
    pub lines: Vec<ReturnLineInput>,
}

/// `POST /admin/pos/returns` body
#[derive(Debug, Deserialize)]
pub struct ReturnBody {
    pub session_id: Uuid,
    pub order_id: Uuid,
    pub lines: Vec<ReturnLineInput>,
    /// Why the goods came back (kept with the return and on the stock movements)
    pub reason: String,
    /// Approved terminal refunds paying the card's part back
    #[serde(default)]
    pub card_refunds: Vec<Uuid>,
}

/// A priced return and how it would be paid back
#[derive(Debug, Serialize)]
pub struct ReturnPreview {
    #[serde(flatten)]
    pub priced: PricedReturn,
    /// The card's part is a suggestion until the terminal refunds it
    pub refund: RefundPlan,
    pub currency: String,
}

/// A return taken back
#[derive(Debug, Serialize)]
pub struct ReturnResult {
    pub return_id: Uuid,
    pub order_id: Uuid,
    pub order_number: String,
    #[serde(flatten)]
    pub priced: PricedReturn,
    pub refund: RefundPlan,
    pub currency: String,
    pub points_taken_back: i32,
    pub points_given_back: i32,
    pub credit_note_number: Option<String>,
}

/// GET /admin/pos/orders/:id/returnable
async fn get_returnable(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Returnable>, Error> {
    let mut conn = state.db.pool().acquire().await?;
    Ok(Json(returnable(&mut conn, id).await?))
}

/// The language of the credited lines: the invoice's, else the store's
fn label_for(state: &AppState, invoice: Option<&Invoice>) -> String {
    let locale = invoice.map_or(state.invoices.default_locale(), |i| i.locale.as_str());
    i18n::text(locale, "invoice_document", "discount")
}

/// Only goods handed over come back
fn check_returnable(order: &Returnable) -> Result<(), Error> {
    if order.status != "completed" {
        return Err(Error::validation(
            "Only orders whose goods were handed over can be returned",
        ));
    }
    Ok(())
}

/// POST /admin/pos/returns/preview
async fn preview(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(body): Json<PreviewBody>,
) -> Result<Json<ReturnPreview>, Error> {
    TillSessions::new(state.db.pool().clone())
        .open_of(body.session_id, auth.customer_id)
        .await?;
    let mut conn = state.db.pool().acquire().await?;
    let order = returnable(&mut conn, body.order_id).await?;
    check_returnable(&order)?;
    let invoice = state.invoices.for_order(order.order_id).await?;
    let priced = price_return(&order, &body.lines, &label_for(&state, invoice.as_ref()))?;
    let refund = plan_refund(priced.total, &order.refundable, None)?;
    Ok(Json(ReturnPreview {
        priced,
        refund,
        currency: order.currency,
    }))
}

/// A reason, trimmed and checked
fn checked_reason(reason: &str) -> Result<&str, Error> {
    let reason = reason.trim();
    if reason.is_empty() || reason.chars().count() > MAX_REASON {
        return Err(Error::validation(format!(
            "reason is required (at most {MAX_REASON} characters)"
        )));
    }
    Ok(reason)
}

/// POST /admin/pos/returns
async fn take_back(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(body): Json<ReturnBody>,
) -> Result<(StatusCode, Json<ReturnResult>), Error> {
    let session = TillSessions::new(state.db.pool().clone())
        .open_of(body.session_id, auth.customer_id)
        .await?;
    let reason = checked_reason(&body.reason)?;
    let mut tx = state.db.pool().begin().await?;
    // One return at a time per order; NO KEY UPDATE lets the gateway's refund
    // row (a foreign key to the order) be written meanwhile
    sqlx::query("SELECT id FROM orders WHERE id = $1 FOR NO KEY UPDATE")
        .bind(body.order_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::not_found("Order not found"))?;
    let order = returnable(&mut tx, body.order_id).await?;
    check_returnable(&order)?;
    let invoice = state.invoices.for_order(order.order_id).await?;
    let priced = price_return(&order, &body.lines, &label_for(&state, invoice.as_ref()))?;
    let cards = card_refunds(&mut tx, session.id, &body.card_refunds).await?;
    let card_total: Decimal = cards.iter().map(|c| c.1).sum();
    let plan = plan_refund(priced.total, &order.refundable, Some(card_total))?;
    let taken = TakenBack {
        session: &session,
        staff: auth.customer_id,
        reason,
        lines: &body.lines,
        order: &order,
        priced: &priced,
        plan: &plan,
        cards: &cards,
    };
    let (return_id, points) = write_return(&mut tx, &taken).await?;
    let credit_note = match &invoice {
        Some(invoice) => Some(credit(&state, &mut tx, invoice, return_id, &priced, &plan).await?),
        None => None,
    };
    let number = credit_note.as_ref().map(|n| n.number.clone());
    record_event(&mut tx, &taken, return_id, number.as_deref()).await?;
    if plan.online > Decimal::ZERO {
        // Last, so a refused online refund leaves nothing behind
        state
            .order_service
            .process_refund(order.order_id, Some(plan.online), reason.to_string())
            .await?;
    }
    tx.commit().await?;
    if let Some(note) = credit_note {
        super::invoices::send_issued(&state, note);
    }
    Ok((
        StatusCode::CREATED,
        Json(ReturnResult {
            return_id,
            order_id: order.order_id,
            order_number: order.order_number.clone(),
            priced,
            refund: plan,
            currency: order.currency.clone(),
            points_taken_back: points.0,
            points_given_back: points.1,
            credit_note_number: number,
        }),
    ))
}

/// A return being written
struct TakenBack<'a> {
    session: &'a TillSession,
    staff: Uuid,
    reason: &'a str,
    lines: &'a [ReturnLineInput],
    order: &'a Returnable,
    priced: &'a PricedReturn,
    plan: &'a RefundPlan,
    cards: &'a [(Uuid, Decimal)],
}

/// The return, its stock, the till's money and payment rows, the points;
/// its id and the points taken back and given back
async fn write_return(
    tx: &mut Transaction<'_, Postgres>,
    taken: &TakenBack<'_>,
) -> Result<(Uuid, (i32, i32)), Error> {
    let record_of = ReturnRecord {
        order: taken.order,
        session: taken.session.id,
        location: taken.session.location_id,
        staff: taken.staff,
        reason: taken.reason,
        lines: taken.lines,
        priced: taken.priced,
        plan: taken.plan,
    };
    let return_id = record_return(tx, &record_of).await?;
    restock(tx, &record_of).await?;
    pay_back_at_till(tx, taken, return_id).await?;
    let order = taken.order.order_id;
    refund_till_payments(tx, order, return_id, taken.plan, taken.reason).await?;
    let points = loyalty_service::undo_for_return(
        tx,
        order,
        return_id,
        taken.priced.ratio,
        taken.priced.last,
    )
    .await?;
    sqlx::query(
        "UPDATE order_returns SET points_taken_back = $2, points_given_back = $3 WHERE id = $1",
    )
    .bind(return_id)
    .bind(points.0)
    .bind(points.1)
    .execute(&mut **tx)
    .await?;
    close_when_all_back(tx, taken.order, taken.priced, taken.plan).await?;
    Ok((return_id, points))
}

/// The card refunds tied to the return, and the session's money out: each
/// card refund, then the cash
async fn pay_back_at_till(
    tx: &mut Transaction<'_, Postgres>,
    taken: &TakenBack<'_>,
    return_id: Uuid,
) -> Result<(), Error> {
    let order = taken.order.order_id;
    let ids: Vec<Uuid> = taken.cards.iter().map(|c| c.0).collect();
    let claimed = sqlx::query(
        "UPDATE terminal_transactions SET order_id = $1, return_id = $2, updated_at = NOW() \
         WHERE id = ANY($3) AND order_id IS NULL AND return_id IS NULL AND status = 'approved'",
    )
    .bind(order)
    .bind(return_id)
    .bind(&ids)
    .execute(&mut **tx)
    .await?;
    if claimed.rows_affected() as usize != ids.len() {
        return Err(Error::validation(
            "A card refund was used by another return",
        ));
    }
    let mut out: Vec<(&str, Decimal)> = taken.cards.iter().map(|c| ("card", c.1)).collect();
    if taken.plan.cash > Decimal::ZERO {
        out.push(("cash", taken.plan.cash));
    }
    for (method, amount) in out {
        let entry = Entry {
            session: taken.session.id,
            kind: "refund",
            method,
            amount: -amount,
            order: Some(order),
            reason: Some(taken.reason),
            staff: taken.staff,
        };
        record(tx, entry).await?;
    }
    Ok(())
}

/// The credit note of an invoiced order's return
async fn credit(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    invoice: &Invoice,
    return_id: Uuid,
    priced: &PricedReturn,
    plan: &RefundPlan,
) -> Result<Invoice, Error> {
    let draft = CreditNoteDraft {
        invoice,
        return_id,
        lines: priced.lines.clone(),
        refunded: plan.refunded(),
    };
    state.invoices.issue_credit_note(tx, draft).await
}

/// The order's history: goods back at the till, with the money and the
/// credit note
async fn record_event(
    tx: &mut Transaction<'_, Postgres>,
    taken: &TakenBack<'_>,
    return_id: Uuid,
    credit_note: Option<&str>,
) -> Result<(), Error> {
    let units: i32 = taken.lines.iter().map(|l| l.quantity).sum();
    order_events::record(
        tx,
        taken.order.order_id,
        "returned",
        Actor::Staff(taken.staff),
        serde_json::json!({
            "return_id": return_id,
            "location": taken.session.location_name,
            "units": units,
            "total": taken.priced.total,
            "online": taken.plan.online,
            "card": taken.plan.card,
            "cash": taken.plan.cash,
            "not_refunded": taken.plan.not_refunded,
            "credit_note": credit_note,
            "reason": taken.reason,
        }),
    )
    .await?;
    Ok(())
}

/// Router for till returns (behind the admin middleware: `pos:read` for
/// `GET`, `pos:write` otherwise)
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin/pos/orders/:id/returnable", get(get_returnable))
        .route("/admin/pos/returns/preview", post(preview))
        .route("/admin/pos/returns", post(take_back))
}
