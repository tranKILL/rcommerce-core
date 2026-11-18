//! Reminders of carts left without an order: the loop that sends them (repriced
//! first, with the customer's prices and VAT shown as at checkout) and the staff
//! figures (`GET /admin/analytics/cart-reminders`)

use std::collections::{HashMap, HashSet};

use axum::{
    extract::{Query, State},
    routing::get,
    Json, Router,
};
use chrono::{Duration, Utc};
use rcommerce_core::models::{CartItem, CartWithItems, Customer};
use rcommerce_core::notification::email_templates::CartReminderMail;
use rcommerce_core::notification::{i18n, EmailNotificationFactory, Notification};
use rcommerce_core::push::{self, PushMessage};
use rcommerce_core::services::cart_reminders::{self, DueReminder, ReminderLine};
use rcommerce_core::services::marketing_consent as consent;
use rcommerce_core::tax::{CustomerTaxInfo, TaxAddress, TaxContext, TaxService, TaxableItem};
use rcommerce_core::Error;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::state::AppState;

/// Longest wait for one e-mail
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// One reminder ready to go
struct Reminder {
    mail: Notification,
    push: PushMessage,
}

/// Send the reminders due, a batch per run, at most one per customer. A cart
/// whose reminder cannot be prepared counts a failed try and the run goes on; a
/// failed send also counts one and ends the run (the mail service may be down).
/// Returns how many were sent.
pub async fn send_due(state: &AppState) -> rcommerce_core::Result<usize> {
    let Some(service) = state.notification_service.clone() else {
        return Ok(0);
    };
    let carts = &state.auth_service.config().carts;
    let schedule = cart_reminders::schedule(&carts.reminder_after_hours);
    let min = carts.reminder_min_subtotal.unwrap_or(Decimal::ZERO);
    let pool = state.db.pool();
    let mut reminded = HashSet::new();
    let mut sent = 0;
    for due in cart_reminders::due(pool, &schedule, min, 100).await? {
        // One reminder per customer (several carts may be due at once)
        if !reminded.insert(due.customer_id) {
            continue;
        }
        let reminder = match prepare(state, &due).await {
            Ok(Some(reminder)) => reminder,
            // No longer wanted (a "no" since the batch was read): nothing to record
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!("Cart reminder for cart {} not prepared: {}", due.cart_id, e);
                record(state, &due, false).await;
                continue;
            }
        };
        // The provider's error text can carry the address: only the cart is logged
        let delivered = tokio::time::timeout(SEND_TIMEOUT, service.send(&reminder.mail)).await;
        if !matches!(delivered, Ok(Ok(_))) {
            tracing::warn!("Cart reminder for cart {} not sent", due.cart_id);
            record(state, &due, false).await;
            break;
        }
        sent += 1;
        record(state, &due, true).await;
        if let Some(sender) = &state.push {
            let pushed = push::notify_customer(pool, sender, due.customer_id, &reminder.push);
            if let Err(e) = pushed.await {
                tracing::warn!("Cart reminder for cart {} not pushed: {}", due.cart_id, e);
            }
        }
    }
    Ok(sent)
}

/// Record a sent or failed reminder with the cart's `updated_at` as it is now
/// (after repricing); a failure to record is logged, the run goes on
async fn record(state: &AppState, due: &DueReminder, sent: bool) {
    let pool = state.db.pool();
    let recorded = match cart_reminders::seen_at(pool, due.cart_id).await {
        Ok(Some(seen)) if sent => cart_reminders::mark_sent(pool, due, seen).await,
        Ok(Some(seen)) => cart_reminders::failed(pool, due, seen).await,
        Ok(None) => Ok(()),
        Err(e) => Err(e),
    };
    if let Err(e) = recorded {
        tracing::warn!("Cart reminder for cart {} not recorded: {}", due.cart_id, e);
    }
}

/// Reprice the cart, then build its reminder in the customer's language; none
/// when the customer no longer accepts commercial e-mails
async fn prepare(state: &AppState, due: &DueReminder) -> rcommerce_core::Result<Option<Reminder>> {
    let Some(customer) = state.customer_service.find_by_id(due.customer_id).await? else {
        return Ok(None);
    };
    if !customer.accepts_marketing || !customer.email_verified {
        return Ok(None);
    }
    // Today's prices (price lists, promotions) before the reminder
    state.cart_service.refresh_prices(due.cart_id).await?;
    let locale = i18n::pick_locale([
        customer.locale.as_deref(),
        Some(state.email_locale.as_str()),
    ]);
    let cart = state.cart_service.get_cart_with_items(due.cart_id).await?;
    let own = own_lines(&cart.items);
    let mut lines = cart_reminders::lines(state.db.pool(), due.cart_id, &locale).await?;
    if lines.is_empty() {
        return Err(Error::validation("the cart has no lines to show"));
    }
    let (total, vat_included) = prices(state, &cart, &own, &mut lines, due.customer_id).await?;
    build(
        state,
        &customer,
        &locale,
        &cart,
        &lines,
        own.len(),
        total,
        vat_included,
    )
    .map(Some)
}

/// The lines a customer sees (bundle parts are priced with their bundle)
fn own_lines(items: &[CartItem]) -> Vec<&CartItem> {
    items
        .iter()
        .filter(|i| {
            !i.custom_attributes
                .as_ref()
                .and_then(|a| a.get("is_bundle_component"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .collect()
}

/// The total to show and whether it includes VAT. Trade customers see prices
/// excluding VAT, as on the storefront; others with VAT, when a rate applies.
async fn prices(
    state: &AppState,
    cart: &CartWithItems,
    own: &[&CartItem],
    lines: &mut [ReminderLine],
    customer_id: Uuid,
) -> rcommerce_core::Result<(Decimal, bool)> {
    let trade: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM company_members WHERE customer_id = $1)")
            .bind(customer_id)
            .fetch_one(state.db.pool())
            .await?;
    let vat = if trade {
        None
    } else {
        vat_by_line(state, cart, own, customer_id).await
    };
    Ok(match vat {
        Some(vat) => (with_vat(lines, &cart.cart, own, &vat), true),
        None => (cart.cart.total, false),
    })
}

/// The e-mail and the push notification of one reminder
#[allow(clippy::too_many_arguments)]
fn build(
    state: &AppState,
    customer: &Customer,
    locale: &str,
    cart: &CartWithItems,
    lines: &[ReminderLine],
    count: usize,
    total: Decimal,
    vat_included: bool,
) -> rcommerce_core::Result<Reminder> {
    let base = state.storefront_url.trim_end_matches('/');
    let key = crate::routes::marketing::unsubscribe_key(state);
    let token = consent::unsubscribe_token(&key, customer.id)?;
    let unsubscribe_url = format!("{base}/account/unsubscribe?token={token}");
    let cart_url = format!("{base}/cart?lang={locale}");
    let name = format!("{} {}", customer.first_name, customer.last_name);
    let currency = cart.cart.currency.to_string();
    let mail = CartReminderMail {
        recipient: &customer.email,
        customer_name: name.trim(),
        locale,
        currency: &currency,
        lines,
        more: count.saturating_sub(lines.len()) as i64,
        total,
        vat_included,
        cart_url: &cart_url,
        unsubscribe_url: &unsubscribe_url,
    };
    let push = PushMessage {
        title: i18n::text(locale, "cart_reminder", "push_title"),
        body: i18n::text(locale, "cart_reminder", "push_body")
            .replace("{{ count }}", &count.to_string())
            .replace("{{ total }}", &i18n::money(locale, total, &currency)),
        url: cart_url.clone(),
    };
    Ok(Reminder {
        mail: EmailNotificationFactory::cart_reminder(&mail, &state.branding)?,
        push,
    })
}

/// Where VAT is worked out, as checkout does: the customer's default delivery
/// address, else the first public branch
async fn tax_address(state: &AppState, customer_id: Uuid) -> Option<TaxAddress> {
    let pool = state.db.pool();
    let mine: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT country, zip, city FROM addresses WHERE customer_id = $1 \
         ORDER BY is_default_shipping DESC, created_at LIMIT 1",
    )
    .bind(customer_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let place = match mine {
        Some(place) => place,
        None => sqlx::query_as(
            "SELECT country_code, postal_code, city FROM inventory_locations \
             WHERE is_public AND is_active AND country_code IS NOT NULL \
             ORDER BY sort_order, name LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()?,
    };
    Some(TaxAddress {
        country_code: place.0.trim().to_string(),
        region_code: None,
        postal_code: place.1,
        city: place.2,
    })
}

/// The VAT of each line (by cart line); none when no rate applies (the e-mail
/// then shows prices excluding VAT)
async fn vat_by_line(
    state: &AppState,
    cart: &CartWithItems,
    own: &[&CartItem],
    customer_id: Uuid,
) -> Option<HashMap<Uuid, Decimal>> {
    let address = tax_address(state, customer_id).await?;
    let items: Vec<TaxableItem> = own
        .iter()
        .map(|item| TaxableItem {
            id: item.id,
            product_id: item.product_id,
            quantity: item.quantity,
            unit_price: item.unit_price,
            total_price: item.total,
            tax_category_id: None,
            is_digital: false,
            title: item.title.clone(),
            sku: item.sku.clone(),
        })
        .collect();
    let context = TaxContext {
        customer: CustomerTaxInfo {
            customer_id: Some(customer_id),
            is_tax_exempt: false,
            vat_id: None,
            exemptions: vec![],
        },
        shipping_address: address.clone(),
        billing_address: address,
        currency: cart.cart.currency,
        transaction_type: Default::default(),
    };
    let tax = state
        .tax_service
        .calculate_tax(&items, &context)
        .await
        .ok()?;
    let vat: HashMap<Uuid, Decimal> = tax
        .line_items
        .iter()
        .map(|l| (l.item_id, l.tax_amount))
        .collect();
    (!vat.values().all(|v| v.is_zero())).then_some(vat)
}

/// Lines with their VAT, and the total with VAT: the goods (after cart discounts)
/// at the lines' average rate, delivery as it is (not taxed)
fn with_vat(
    lines: &mut [ReminderLine],
    cart: &rcommerce_core::models::Cart,
    own: &[&CartItem],
    vat: &HashMap<Uuid, Decimal>,
) -> Decimal {
    for line in lines.iter_mut() {
        line.total += vat.get(&line.id).copied().unwrap_or_default();
    }
    gross_total(
        cart.total,
        cart.shipping_total,
        own.iter().map(|i| i.total).sum(),
        vat.values().copied().sum(),
    )
}

/// `total` with VAT: its goods part (`total - shipping`) taxed at the lines'
/// average rate (`vat / lines`)
fn gross_total(total: Decimal, shipping: Decimal, lines: Decimal, vat: Decimal) -> Decimal {
    if lines <= Decimal::ZERO {
        return total;
    }
    let goods = total - shipping;
    (goods + goods * vat / lines + shipping).round_dp(2)
}

/// The period of the figures: the last `days` days (30 by default, at most 366)
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatsQuery {
    days: Option<i64>,
}

/// `GET /admin/analytics/cart-reminders?days=`: reminders sent in the period, the
/// customers reminded, and the orders they placed within 7 days of one
async fn stats(
    State(state): State<AppState>,
    Query(query): Query<StatsQuery>,
) -> Result<Json<Value>, Error> {
    let days = query.days.unwrap_or(30);
    if !(1..=366).contains(&days) {
        return Err(Error::validation("days must be between 1 and 366"));
    }
    let to = Utc::now();
    let figures = cart_reminders::stats(state.db.pool(), to - Duration::days(days), to).await?;
    let carts = &state.auth_service.config().carts;
    Ok(Json(json!({
        "days": days,
        "schedule_hours": cart_reminders::schedule(&carts.reminder_after_hours),
        "stats": figures,
    })))
}

/// Staff routes (`reports` read)
pub fn admin_router() -> Router<AppState> {
    Router::new().route("/admin/analytics/cart-reminders", get(stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_total_with_vat_taxes_the_discounted_goods_not_the_delivery() {
        let d = |s: &str| s.parse::<Decimal>().unwrap();
        // 100 of lines, a 50 % coupon (total 50), 21 VAT on the lines, no delivery
        assert_eq!(gross_total(d("50"), d("0"), d("100"), d("21")), d("60.50"));
        // with 10 of delivery
        assert_eq!(gross_total(d("60"), d("10"), d("100"), d("21")), d("70.50"));
        // no discount
        assert_eq!(
            gross_total(d("219.72"), d("0"), d("219.72"), d("46.14")),
            d("265.86")
        );
        assert_eq!(gross_total(d("5"), d("0"), d("0"), d("0")), d("5"));
    }
}
