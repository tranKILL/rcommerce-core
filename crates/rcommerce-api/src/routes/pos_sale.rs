//! Sales at the till (`pos:write`):
//!
//! - `POST /admin/pos/baskets` create or replace the session's basket: the
//!   customer (optional), lines with manual discounts within the cashier's
//!   cap, a coupon, loyalty points. The answer is priced like the web shop
//!   (price lists, promotions, coupon, points) with VAT.
//! - `POST /admin/pos/sales` sell a basket, paid with cash and approved card
//!   terminal transactions: the order goes through checkout (stock taken at
//!   the till's branch), is handed over at once, earns loyalty points and gets
//!   its invoice for business customers or when asked.

use axum::{extract::State, http::StatusCode, routing::post, Extension, Json, Router};
use rcommerce_core::common::Address;
use rcommerce_core::payment::PaymentMethod;
use rcommerce_core::services::order_events::{self, Actor};
use rcommerce_core::services::pos::basket::{
    check_lines, settle, till_token, write_basket, BasketLine, BasketWrite, Settlement,
};
use rcommerce_core::services::pos::gateway::TillGateway;
use rcommerce_core::services::pos::session::{record, Entry};
use rcommerce_core::services::pos::{TillSession, TillSessions};
use rcommerce_core::services::promotion_service::CartPromotion;
use rcommerce_core::services::{
    CheckoutConfig, CheckoutService, CheckoutSummary, CompleteCheckoutRequest,
    InitiateCheckoutRequest, PickupLocations,
};
use rcommerce_core::{models::ApplyCouponInput, Error, StaffService};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};
use std::sync::Arc;
use uuid::Uuid;

use super::order::{build_order_response, fetch_order, OrderResponse};
use crate::middleware::JwtAuth;
use crate::state::AppState;

/// `POST /admin/pos/baskets` body
#[derive(Debug, Deserialize)]
pub struct BasketBody {
    pub session_id: Uuid,
    /// The basket to replace; none: a new one
    #[serde(default)]
    pub basket_id: Option<Uuid>,
    #[serde(default)]
    pub customer_id: Option<Uuid>,
    pub lines: Vec<BasketLine>,
    #[serde(default)]
    pub coupon: Option<String>,
    /// Loyalty points to redeem
    #[serde(default)]
    pub points: Option<i32>,
}

/// A basket line as the till shows it
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct BasketLineView {
    pub id: Uuid,
    pub product_id: Uuid,
    pub variant_id: Option<Uuid>,
    pub title: String,
    pub variant_title: Option<String>,
    pub sku: Option<String>,
    pub quantity: i32,
    /// Excl. VAT, after price lists, promotions and the manual discount
    pub unit_price: Decimal,
    /// The catalogue price
    pub original_price: Decimal,
    pub manual_discount_percent: Option<Decimal>,
    pub manual_discount_reason: Option<String>,
    /// What the manual discount takes off each unit
    pub manual_discount: Decimal,
    /// The line's share of the coupon, points and cart promotions
    pub discount_amount: Decimal,
    pub subtotal: Decimal,
    /// Free stock at the till's branch
    pub available: i64,
}

/// The buyer of a basket
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct BasketCustomer {
    pub id: Uuid,
    pub email: String,
    pub name: String,
    pub loyalty_card: Option<String>,
    pub company_name: Option<String>,
}

/// A priced basket
#[derive(Debug, Serialize)]
pub struct BasketView {
    pub basket_id: Uuid,
    pub customer: Option<BasketCustomer>,
    pub lines: Vec<BasketLineView>,
    /// Excl. VAT, before discounts
    pub subtotal: Decimal,
    pub discount_total: Decimal,
    pub tax_total: Decimal,
    /// To pay, VAT included
    pub total: Decimal,
    /// Cart promotions, the coupon and the points line
    pub promotions: Vec<CartPromotion>,
    pub coupon: Option<String>,
}

/// `POST /admin/pos/sales` body
#[derive(Debug, Deserialize)]
pub struct SaleBody {
    pub session_id: Uuid,
    pub basket_id: Uuid,
    /// Cash handed over by the customer
    #[serde(default, deserialize_with = "super::pos::number_or_text")]
    pub cash_given: Decimal,
    /// Approved terminal transactions paying the sale
    #[serde(default)]
    pub card_payments: Vec<Uuid>,
    /// Issue an invoice even for a retail customer
    #[serde(default)]
    pub invoice: bool,
    /// Who takes the goods (default: the customer's name, or "Comptoir")
    #[serde(default)]
    pub collected_by: Option<String>,
}

/// A completed sale
#[derive(Debug, Serialize)]
pub struct SaleResult {
    pub order: OrderResponse,
    pub settlement: Settlement,
    pub invoice_number: Option<String>,
}

/// A checkout that confirms the till's payment at once (one per sale)
fn till_checkout(state: &AppState) -> CheckoutService {
    CheckoutService::with_coupon_service(
        state.cart_service.clone(),
        state.tax_service.clone(),
        state.order_service.clone(),
        Arc::new(TillGateway::default()),
        state.shipping_factory.clone(),
        CheckoutConfig::default(),
        Some(Arc::new(state.coupon_service.clone())),
    )
    .with_pickup_locations(PickupLocations::new(state.db.pool().clone()))
}

/// The branch's address: where a till sale is "shipped" (picked up)
async fn branch_address(state: &AppState, session: &TillSession) -> Result<Address, Error> {
    let row: (Option<String>, Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT address_line1, postal_code, city, country_code FROM inventory_locations WHERE id = $1",
    )
    .bind(session.location_id)
    .fetch_one(state.db.pool())
    .await?;
    let now = chrono::Utc::now();
    Ok(Address {
        id: Uuid::nil(),
        customer_id: Uuid::nil(),
        first_name: session.location_name.clone(),
        last_name: String::new(),
        company: None,
        phone: None,
        address1: row.0.unwrap_or_else(|| session.location_name.clone()),
        address2: None,
        city: row.2.unwrap_or_default(),
        state: None,
        country: row.3.unwrap_or_else(|| "BE".into()),
        zip: row.1.unwrap_or_default(),
        is_default_shipping: false,
        is_default_billing: false,
        created_at: now,
        updated_at: now,
    })
}

/// Totals with VAT and the branch's pickup rate
async fn quote(
    state: &AppState,
    session: &TillSession,
    basket: Uuid,
    customer: Option<Uuid>,
) -> Result<CheckoutSummary, Error> {
    let request = InitiateCheckoutRequest {
        cart_id: basket,
        shipping_address: branch_address(state, session).await?,
        billing_address: None,
        vat_id: None,
        customer_id: customer,
        currency: None,
        subscription: None,
    };
    till_checkout(state).initiate_checkout(request).await
}

/// The customer's e-mail, when there is one
async fn customer_email(state: &AppState, customer: Option<Uuid>) -> Result<Option<String>, Error> {
    let Some(id) = customer else {
        return Ok(None);
    };
    let email: Option<String> = sqlx::query_scalar("SELECT email FROM customers WHERE id = $1")
        .bind(id)
        .fetch_optional(state.db.pool())
        .await?;
    email
        .map(Some)
        .ok_or_else(|| Error::not_found("Customer not found"))
}

/// POST /admin/pos/baskets
async fn put_basket(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(body): Json<BasketBody>,
) -> Result<Json<BasketView>, Error> {
    let session = TillSessions::new(state.db.pool().clone())
        .open_of(body.session_id, auth.customer_id)
        .await?;
    let cap = StaffService::new(state.db.clone())
        .view(auth.customer_id, auth.is_admin())
        .await?
        .max_discount_percent;
    check_lines(&body.lines, cap)?;
    let email = customer_email(&state, body.customer_id).await?;
    let basket = write_basket(
        state.db.pool(),
        BasketWrite {
            session: session.id,
            basket: body.basket_id,
            customer: body.customer_id,
            email: email.as_deref(),
            currency: &state.default_currency,
            lines: &body.lines,
            points: body.points.unwrap_or(0),
        },
    )
    .await?;
    state.cart_service.remove_coupon(basket).await?;
    if let Some(code) = body
        .coupon
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        let input = ApplyCouponInput {
            coupon_code: code.to_string(),
        };
        state.cart_service.apply_coupon(basket, input).await?;
    }
    let summary = quote(&state, &session, basket, body.customer_id).await?;
    Ok(Json(
        basket_view(&state, &session, basket, body.customer_id, &summary).await?,
    ))
}

/// The basket's lines, buyer and totals
async fn basket_view(
    state: &AppState,
    session: &TillSession,
    basket: Uuid,
    customer: Option<Uuid>,
    summary: &CheckoutSummary,
) -> Result<BasketView, Error> {
    let pool = state.db.pool();
    let lines: Vec<BasketLineView> = sqlx::query_as(
        "SELECT ci.id, ci.product_id, ci.variant_id, ci.title, ci.variant_title, ci.sku, ci.quantity, \
         ci.unit_price, ci.original_price, ci.manual_discount_percent, ci.manual_discount_reason, \
         ci.manual_discount, ci.discount_amount, ci.subtotal, \
         COALESCE((SELECT SUM(l.available_quantity) FROM inventory_levels l \
           WHERE l.product_id = ci.product_id AND l.location_id = $2 \
             AND l.variant_id IS NOT DISTINCT FROM ci.variant_id), 0)::bigint AS available \
         FROM cart_items ci WHERE ci.cart_id = $1 ORDER BY ci.created_at, ci.id",
    )
    .bind(basket)
    .bind(session.location_id)
    .fetch_all(pool)
    .await?;
    let buyer = match customer {
        Some(id) => sqlx::query_as(
            "SELECT c.id, c.email, TRIM(COALESCE(c.first_name, '') || ' ' || COALESCE(c.last_name, '')) AS name, \
             c.loyalty_card, co.name AS company_name FROM customers c \
             LEFT JOIN company_members m ON m.customer_id = c.id \
             LEFT JOIN companies co ON co.id = m.company_id WHERE c.id = $1 LIMIT 1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await?,
        None => None,
    };
    let cart: (sqlx::types::Json<Vec<CartPromotion>>, Option<String>) =
        sqlx::query_as("SELECT promotions, coupon_code FROM carts WHERE id = $1")
            .bind(basket)
            .fetch_one(pool)
            .await?;
    Ok(BasketView {
        basket_id: basket,
        customer: buyer,
        lines,
        subtotal: summary.subtotal,
        discount_total: summary.discount_total,
        tax_total: summary.tax_total,
        total: summary.total,
        promotions: cart.0 .0,
        coupon: cart.1,
    })
}

/// The approved, unused card payments of the session, with their amounts
async fn card_amounts(
    state: &AppState,
    session: Uuid,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, Decimal)>, Error> {
    let rows: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT id, amount FROM terminal_transactions WHERE id = ANY($1) AND session_id = $2 \
         AND kind = 'payment' AND status = 'approved' AND order_id IS NULL",
    )
    .bind(ids)
    .bind(session)
    .fetch_all(state.db.pool())
    .await?;
    if rows.len() != ids.len() {
        return Err(Error::validation(
            "A card payment is not approved, already used, or from another session",
        ));
    }
    Ok(rows)
}

/// POST /admin/pos/sales
async fn sell(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(body): Json<SaleBody>,
) -> Result<(StatusCode, Json<SaleResult>), Error> {
    let session = TillSessions::new(state.db.pool().clone())
        .open_of(body.session_id, auth.customer_id)
        .await?;
    let (customer, email): (Option<Uuid>, Option<String>) =
        sqlx::query_as("SELECT customer_id, email FROM carts WHERE id = $1 AND session_token = $2")
            .bind(body.basket_id)
            .bind(till_token(session.id))
            .fetch_optional(state.db.pool())
            .await?
            .ok_or_else(|| Error::not_found("Basket not found in this till session"))?;
    let cards = card_amounts(&state, session.id, &body.card_payments).await?;
    let summary = quote(&state, &session, body.basket_id, customer).await?;
    let card_total: Vec<Decimal> = cards.iter().map(|c| c.1).collect();
    let plan = settle(summary.total, body.cash_given, &card_total)?;
    let order = place(&state, &session, body.basket_id, customer, email, &summary).await?;
    let collector = collector_name(&state, customer, body.collected_by.as_deref()).await?;
    let sale = Sale {
        order,
        session: &session,
        cashier: auth.customer_id,
        plan: &plan,
        cards: &cards,
        collector: &collector,
    };
    let invoice_number = hand_over(&state, &sale, body.invoice).await?;
    let response = build_order_response(&state, fetch_order(&state, order).await?).await?;
    Ok((
        StatusCode::CREATED,
        Json(SaleResult {
            order: response,
            settlement: plan,
            invoice_number,
        }),
    ))
}

/// The order, through checkout at the branch
async fn place(
    state: &AppState,
    session: &TillSession,
    basket: Uuid,
    customer: Option<Uuid>,
    email: Option<String>,
    summary: &CheckoutSummary,
) -> Result<Uuid, Error> {
    let rate = summary
        .available_shipping_rates
        .iter()
        .find(|r| r.carrier == "pickup" && r.service_code == session.location_id.to_string())
        .cloned()
        .ok_or_else(|| {
            Error::validation("This branch does not hand goods over (not a pickup point)")
        })?;
    let request = CompleteCheckoutRequest {
        cart_id: basket,
        shipping_address: branch_address(state, session).await?,
        billing_address: None,
        payment_method: PaymentMethod::CashOnDelivery,
        customer_email: email
            .unwrap_or_else(|| format!("till+{}@pos.invalid", session.id.simple())),
        customer_id: customer,
        vat_id: None,
        notes: None,
        selected_shipping_rate: rate,
        test: None,
        subscription: None,
        payment_consent_id: None,
        fulfillment_location_id: Some(session.location_id),
        locale: None,
    };
    let result = till_checkout(state).complete_checkout(request).await?;
    Ok(result.order.order.id)
}

/// Who takes the goods: the name typed, else the customer's, else "Comptoir"
async fn collector_name(
    state: &AppState,
    customer: Option<Uuid>,
    typed: Option<&str>,
) -> Result<String, Error> {
    if let Some(name) = typed.map(str::trim).filter(|n| !n.is_empty()) {
        return Ok(super::order_pickup::checked_name(name, "collected_by")?.to_string());
    }
    let name: Option<String> = match customer {
        Some(id) => sqlx::query_scalar(
            "SELECT NULLIF(TRIM(COALESCE(first_name, '') || ' ' || COALESCE(last_name, '')), '') \
             FROM customers WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(state.db.pool())
        .await?
        .flatten(),
        None => None,
    };
    Ok(name.unwrap_or_else(|| "Comptoir".to_string()))
}

/// A sale placed through checkout, to hand over
struct Sale<'a> {
    order: Uuid,
    session: &'a TillSession,
    cashier: Uuid,
    plan: &'a Settlement,
    cards: &'a [(Uuid, Decimal)],
    collector: &'a str,
}

/// Hand the goods over now: the order completed at the branch, the till's
/// movements and payments recorded, then points and the invoice
async fn hand_over(
    state: &AppState,
    sale: &Sale<'_>,
    invoice: bool,
) -> Result<Option<String>, Error> {
    let mut tx = state.db.pool().begin().await?;
    let source: Option<String> =
        sqlx::query_scalar("SELECT metadata->>'source' FROM orders WHERE id = $1 FOR UPDATE")
            .bind(sale.order)
            .fetch_one(&mut *tx)
            .await?;
    if source.as_deref() != Some("pos") {
        mark_sold(&mut tx, sale).await?;
        record_payments(&mut tx, sale).await?;
        tx.commit().await?;
        super::order_history::after_handover_points(state, sale.order).await;
        let company: bool =
            sqlx::query_scalar("SELECT company_id IS NOT NULL FROM orders WHERE id = $1")
                .bind(sale.order)
                .fetch_one(state.db.pool())
                .await?;
        if invoice || company {
            super::invoices::issue_after_handover(state, sale.order).await;
        }
    }
    let number: Option<String> = sqlx::query_scalar(
        "SELECT number FROM invoices WHERE order_id = $1 AND kind = 'invoice' ORDER BY created_at DESC LIMIT 1",
    )
    .bind(sale.order)
    .fetch_optional(state.db.pool())
    .await?;
    Ok(number)
}

/// The order sold at the till: ready and collected at once, its stock out of
/// the branch, the cashier in its history
async fn mark_sold(tx: &mut Transaction<'_, Postgres>, sale: &Sale<'_>) -> Result<(), Error> {
    sqlx::query(
        "UPDATE orders SET status = 'processing', metadata = (metadata || jsonb_build_object( \
           'source', 'pos', 'till_session_id', $2::text, 'cashier_id', $3::text)) \
           || jsonb_build_object('pickup', COALESCE(metadata->'pickup', '{}'::jsonb) \
              || jsonb_build_object('ready_marked_at', NOW())), updated_at = NOW() WHERE id = $1",
    )
    .bind(sale.order)
    .bind(sale.session.id)
    .bind(sale.cashier)
    .execute(&mut **tx)
    .await?;
    super::order_pickup::record_collection(
        tx,
        sale.order,
        sale.collector,
        Some(&sale.session.location_name),
    )
    .await?;
    super::order_pickup::release_committed_stock(tx, sale.order).await?;
    order_events::record(
        tx,
        sale.order,
        "sold_at_till",
        Actor::Staff(sale.cashier),
        serde_json::json!({
            "location": sale.session.location_name,
            "cash": sale.plan.cash,
            "card": sale.plan.card,
            "change": sale.plan.change,
            "collected_by": sale.collector,
        }),
    )
    .await?;
    Ok(())
}

/// The till's movements (cash, each card payment), the card payments tied to
/// the order, and its payment rows by method
async fn record_payments(tx: &mut Transaction<'_, Postgres>, sale: &Sale<'_>) -> Result<(), Error> {
    let ids: Vec<Uuid> = sale.cards.iter().map(|c| c.0).collect();
    let claimed = sqlx::query(
        "UPDATE terminal_transactions SET order_id = $1, updated_at = NOW() \
         WHERE id = ANY($2) AND order_id IS NULL AND status = 'approved'",
    )
    .bind(sale.order)
    .bind(&ids)
    .execute(&mut **tx)
    .await?;
    if claimed.rows_affected() as usize != ids.len() {
        return Err(Error::validation("A card payment was used by another sale"));
    }
    let mut entries: Vec<(&str, Decimal)> = sale.cards.iter().map(|c| ("card", c.1)).collect();
    if sale.plan.cash > Decimal::ZERO {
        entries.push(("cash", sale.plan.cash));
    }
    for (method, amount) in &entries {
        let entry = Entry {
            session: sale.session.id,
            kind: "sale",
            method,
            amount: *amount,
            order: Some(sale.order),
            reason: None,
            staff: sale.cashier,
        };
        record(tx, entry).await?;
    }
    split_payment_rows(tx, sale.order, sale.plan).await
}

/// The checkout's single till payment, split into its cash and card parts
async fn split_payment_rows(
    tx: &mut Transaction<'_, Postgres>,
    order: Uuid,
    plan: &Settlement,
) -> Result<(), Error> {
    let parts = [("cash", plan.cash), ("debit_card", plan.card)];
    for (method, amount) in parts.iter().filter(|p| p.1 > Decimal::ZERO) {
        sqlx::query(
            "INSERT INTO payments (order_id, amount, currency, status, gateway, gateway_payment_id, \
               payment_method, processed_at, test) \
             SELECT order_id, $2, currency, status, gateway, gateway_payment_id || '-' || $3, \
               $3::payment_method_type, processed_at, test \
             FROM payments WHERE order_id = $1 AND gateway = 'till' AND payment_method = 'credit_card'",
        )
        .bind(order)
        .bind(amount)
        .bind(method)
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query("DELETE FROM payments WHERE order_id = $1 AND gateway = 'till' AND payment_method = 'credit_card'")
        .bind(order)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Router for till sales (behind the admin middleware: `pos:write`)
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin/pos/baskets", post(put_basket))
        .route("/admin/pos/sales", post(sell))
}
