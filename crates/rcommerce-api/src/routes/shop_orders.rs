//! Web orders sent to the shop that prepares them: the addresses each shop
//! lists, the order slip as a PDF, and the job that mails each paid order to
//! its shop (see `rcommerce_core::services::shop_orders`).

use axum::{
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use rcommerce_core::invoice::slip::{order_slip_pdf, OrderSlip, SlipFormat};
use rcommerce_core::middleware::rate_limit::RateLimitError;
use rcommerce_core::notification::{i18n, EmailAttachment, Notification, NotificationChannel};
use rcommerce_core::services::shop_orders::{self, NewRecipient, PendingOrder, Recipient};
use rcommerce_core::{Action, Error, Resource};
use serde::Deserialize;
use tracing::{info, warn};
use uuid::Uuid;

use crate::middleware::{staff_access::staff_can, JwtAuth};
use crate::state::AppState;

/// `{{ name }}` placeholders of a shop e-mail text replaced
fn fill(text: String, order_number: &str, location: &str) -> String {
    text.replace("{{ order_number }}", order_number)
        .replace("{{ location }}", location)
}

/// The slip PDF as an e-mail attachment
fn slip_attachment(
    slip: &OrderSlip,
    format: SlipFormat,
    locale: &str,
) -> rcommerce_core::Result<EmailAttachment> {
    Ok(EmailAttachment {
        filename: format!("{}.pdf", slip.order_number),
        content_type: "application/pdf".into(),
        content: order_slip_pdf(slip, format, locale)?,
    })
}

/// What a shop's new-order e-mails are made from, loaded once per order
struct OrderMail {
    detail: rcommerce_core::order::service::OrderDetail,
    shipping: Option<serde_json::Value>,
    billing: Option<serde_json::Value>,
    slip: OrderSlip,
    shop: shop_orders::ShopInfo,
}

impl OrderMail {
    /// `order` with its addresses and slip, for `shop`; `None`: no such order
    async fn load(
        state: &AppState,
        order: Uuid,
        shop: shop_orders::ShopInfo,
    ) -> rcommerce_core::Result<Option<Self>> {
        let pool = state.db.pool();
        let Some((slip, _)) = shop_orders::slip(pool, order, &state.branding.name).await? else {
            return Ok(None);
        };
        let Some(detail) = state.order_service.get_order(order).await? else {
            return Ok(None);
        };
        let (shipping, billing): (Option<serde_json::Value>, Option<serde_json::Value>) =
            sqlx::query_as("SELECT shipping_address, billing_address FROM orders WHERE id = $1")
                .bind(order)
                .fetch_one(pool)
                .await?;
        Ok(Some(OrderMail {
            detail,
            shipping,
            billing,
            slip,
            shop,
        }))
    }

    /// The e-mail for one shop address, in its language, with the slip attached
    fn for_recipient(
        &self,
        state: &AppState,
        recipient: &Recipient,
    ) -> rcommerce_core::Result<Notification> {
        let locale = recipient
            .locale
            .clone()
            .unwrap_or_else(|| state.email_locale.clone());
        let mut mail = super::email_helpers::build_admin_order_notification(
            &recipient.email,
            &self.detail,
            self.shipping.as_ref(),
            self.billing.as_ref(),
            &state.branding,
            &locale,
        )?;
        let subject = i18n::text(&locale, "shop_order", "subject");
        mail.subject = fill(subject, &self.slip.order_number, &self.shop.name);
        mail.attachments
            .push(slip_attachment(&self.slip, self.shop.format, &locale)?);
        Ok(mail)
    }
}

/// Send `order` to its shop's addresses, or to the operator when the shop
/// lists none; returns how many shop addresses got it. Fails when no address
/// could be reached, so the order is tried again on the next run.
async fn send_one(state: &AppState, pending: &PendingOrder) -> rcommerce_core::Result<usize> {
    let pool = state.db.pool();
    let shop = match pending.location_id {
        Some(id) => shop_orders::shop(pool, id).await?,
        None => None,
    };
    let recipients: Vec<Recipient> = match (&shop, pending.location_id) {
        (Some(_), Some(id)) => shop_orders::recipients(pool, id)
            .await?
            .into_iter()
            .filter(|r| r.active)
            .collect(),
        _ => Vec::new(),
    };
    let (Some(shop), false) = (shop, recipients.is_empty()) else {
        if !pending.admin_notified {
            super::email_helpers::send_admin_order_email(state, pending.id).await;
        }
        return Ok(0);
    };
    let Some(order) = OrderMail::load(state, pending.id, shop).await? else {
        return Ok(0);
    };
    let service = state
        .notification_service
        .as_ref()
        .ok_or_else(|| Error::internal("No notification service"))?;
    let mut sent = 0;
    for recipient in &recipients {
        let result = match order.for_recipient(state, recipient) {
            Ok(mail) => service.send(&mail).await.map(|_| ()),
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => sent += 1,
            Err(e) => warn!(
                "Shop order e-mail for {} to {} failed: {}",
                order.slip.order_number, recipient.email, e
            ),
        }
    }
    if sent == 0 {
        return Err(Error::internal("No shop address could be reached"));
    }
    Ok(sent)
}

/// One run of the job: send the paid web orders not sent yet; returns how
/// many orders went out. An order no address could take is skipped and tried
/// again next run.
pub async fn send_pending(state: &AppState) -> rcommerce_core::Result<usize> {
    if state.notification_service.is_none() {
        return Ok(0);
    }
    let pool = state.db.pool();
    let mut done = 0;
    for pending in shop_orders::pending(pool, shop_orders::BATCH).await? {
        // One shop's failing address must not hold up the other shops' orders:
        // this order is tried again next run
        let sent = match send_one(state, &pending).await {
            Ok(sent) => sent,
            Err(e) => {
                warn!("Shop order {} not sent: {}", pending.id, e);
                continue;
            }
        };
        shop_orders::mark_sent(pool, pending.id, sent).await?;
        done += 1;
    }
    Ok(done)
}

/// `GET /admin/locations/:id/order-recipients`
async fn list_recipients(
    State(state): State<AppState>,
    Path(location): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Error> {
    let shop = shop_orders::shop(state.db.pool(), location)
        .await?
        .ok_or_else(|| Error::not_found("Location not found"))?;
    let recipients = shop_orders::recipients(state.db.pool(), location).await?;
    Ok(Json(serde_json::json!({
        "recipients": recipients,
        "slip_format": shop.format.code(),
    })))
}

/// Body of `PUT /admin/locations/:id/order-recipients`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecipientsInput {
    recipients: Vec<NewRecipient>,
}

/// Who receives a shop's web orders gets its customers' details: changing
/// the list, or mailing it, needs the orders right on top of the settings one
/// the admin middleware checked
async fn may_route_orders(state: &AppState, auth: &JwtAuth) -> Result<(), Error> {
    if staff_can(state, auth, Resource::Orders, Action::Read).await {
        Ok(())
    } else {
        Err(Error::forbidden(
            "Choosing who receives web orders needs the orders permission",
        ))
    }
}

/// `PUT /admin/locations/:id/order-recipients`: replace the list
async fn put_recipients(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(location): Path<Uuid>,
    Json(input): Json<RecipientsInput>,
) -> Result<Json<serde_json::Value>, Error> {
    may_route_orders(&state, &auth).await?;
    let list = shop_orders::check(&input.recipients)?;
    let recipients = shop_orders::replace(state.db.pool(), location, &list, auth.customer_id)
        .await?
        .ok_or_else(|| Error::not_found("Location not found"))?;
    info!(
        target: "audit",
        actor = %auth.customer_id,
        %location,
        recipients = list.len(),
        "shop order recipients replaced"
    );
    Ok(Json(serde_json::json!({ "recipients": recipients })))
}

/// The test e-mail for one address, in its language, with a sample slip
fn test_mail(
    state: &AppState,
    recipient: &Recipient,
    slip: &OrderSlip,
    shop: &shop_orders::ShopInfo,
) -> rcommerce_core::Result<Notification> {
    let locale = recipient
        .locale
        .clone()
        .unwrap_or_else(|| state.email_locale.clone());
    let says = |key: &str| {
        let text = i18n::text(&locale, "shop_order", key);
        fill(text, &slip.order_number, &shop.name)
    };
    let mut mail = Notification::new(
        NotificationChannel::Email,
        recipient.email.clone(),
        says("test_subject"),
        says("intro"),
    );
    mail.attachments
        .push(slip_attachment(slip, shop.format, &locale)?);
    Ok(mail)
}

/// `POST /admin/locations/:id/order-recipients/test`: a sample slip to every
/// active address, so the shop sees the e-mail and its printer prints it; at
/// most one a minute per shop (429 otherwise)
async fn send_test(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(location): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Error> {
    may_route_orders(&state, &auth).await?;
    let pool = state.db.pool();
    let shop = shop_orders::shop(pool, location)
        .await?
        .ok_or_else(|| Error::not_found("Location not found"))?;
    let service = state
        .notification_service
        .as_ref()
        .ok_or_else(|| Error::validation("E-mail is not configured"))?;
    if !shop_orders::claim_test(pool, location).await? {
        return Err(Error::RateLimit(RateLimitError::RateLimited {
            retry_after: 60,
        }));
    }
    let slip = shop_orders::sample(&shop, &state.branding.name);
    let mut sent = 0;
    let recipients = shop_orders::recipients(pool, location).await?;
    for recipient in recipients.iter().filter(|r| r.active) {
        let result = match test_mail(&state, recipient, &slip, &shop) {
            Ok(mail) => service.send(&mail).await.map(|_| ()),
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => sent += 1,
            Err(e) => warn!("Test shop e-mail to {} failed: {}", recipient.email, e),
        }
    }
    info!(target: "audit", actor = %auth.customer_id, %location, sent, "shop order test e-mail");
    Ok(Json(serde_json::json!({ "sent": sent })))
}

/// `?format=` of the slip route (default: the shop's paper)
#[derive(Debug, Deserialize)]
struct SlipQuery {
    format: Option<String>,
    lang: Option<String>,
}

/// `GET /admin/orders/:id/slip.pdf`: the order slip to print
async fn slip_pdf(
    State(state): State<AppState>,
    Path(order): Path<Uuid>,
    Query(query): Query<SlipQuery>,
) -> Result<Response, Error> {
    let pool = state.db.pool();
    let (slip, location) = shop_orders::slip(pool, order, &state.branding.name)
        .await?
        .ok_or_else(|| Error::not_found("Order not found"))?;
    let format = match query.format.as_deref() {
        Some(code) => SlipFormat::parse(code)
            .ok_or_else(|| Error::validation("format must be a4 or ticket80"))?,
        None => match location {
            Some(id) => shop_orders::shop(pool, id)
                .await?
                .map(|s| s.format)
                .unwrap_or_default(),
            None => SlipFormat::A4,
        },
    };
    let locale = query
        .lang
        .as_deref()
        .and_then(rcommerce_core::common::locale::normalize_locale)
        .unwrap_or_else(|| state.email_locale.clone());
    let pdf = order_slip_pdf(&slip, format, &locale)?;
    let disposition = format!("inline; filename=\"{}.pdf\"", slip.order_number);
    Ok((
        [
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        pdf,
    )
        .into_response())
}

/// Shop order routes (behind the admin middleware: `settings` for the
/// addresses, `orders` for the slip)
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route(
            "/admin/locations/:id/order-recipients",
            get(list_recipients).put(put_recipients),
        )
        .route(
            "/admin/locations/:id/order-recipients/test",
            post(send_test),
        )
        .route("/admin/orders/:id/slip.pdf", get(slip_pdf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_filled() {
        let text = "Web order {{ order_number }} for {{ location }}".to_string();
        assert_eq!(fill(text, "ORD-1", "North"), "Web order ORD-1 for North");
    }
}
