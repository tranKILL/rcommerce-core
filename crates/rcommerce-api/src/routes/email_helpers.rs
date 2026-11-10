//! Shared fire-and-forget email helpers for order lifecycle events.
//!
//! All sends are best-effort: failures are logged with `warn!` and never
//! propagate back to the API request, mirroring the order-confirmation flow.

use axum::http::{header::ACCEPT_LANGUAGE, HeaderMap};
use rcommerce_core::models::Order;
use rcommerce_core::notification::i18n;
use rcommerce_core::notification::{Notification, NotificationChannel};
use rcommerce_core::order::service::OrderDetail;
use rust_decimal::Decimal;
use tracing::warn;
use uuid::Uuid;

use crate::state::AppState;

/// The store's branding and default e-mail language, for e-mails built in
/// background tasks
#[derive(Debug, Clone)]
pub struct MailBrand {
    pub branding: rcommerce_core::BrandingConfig,
    /// Language when neither the order nor the customer names one
    pub default_locale: String,
}

impl MailBrand {
    /// The branding and default language of the store serving `state`
    pub fn of(state: &AppState) -> Self {
        Self {
            branding: state.branding.clone(),
            default_locale: state.email_locale.clone(),
        }
    }
}

/// A language tag a client may send (`fr`, `nl-BE`): starts with a letter,
/// then letters, digits, `-` or `_`; 2 to 35 characters
pub fn is_locale_tag(tag: &str) -> bool {
    (2..=35).contains(&tag.len())
        && tag.starts_with(|c: char| c.is_ascii_alphabetic())
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The language a request asks for: a tag the client sent (kept even when no
/// catalogue has it yet), else the first `Accept-Language` language e-mails
/// can be written in
pub fn requested_locale(explicit: Option<&str>, headers: &HeaderMap) -> Option<String> {
    explicit
        .map(str::trim)
        .filter(|tag| is_locale_tag(tag))
        .map(|tag| tag.replace('_', "-").to_ascii_lowercase())
        .or_else(|| {
            headers
                .get(ACCEPT_LANGUAGE)
                .and_then(|v| v.to_str().ok())
                .and_then(i18n::from_accept_language)
        })
}

/// The language of an order's e-mails: the one it was placed in, else the
/// customer's preferred one, else `default`
pub async fn order_email_locale(pool: &sqlx::PgPool, order_id: Uuid, default: &str) -> String {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT o.metadata->>'locale', c.locale FROM orders o \
         LEFT JOIN customers c ON c.id = o.customer_id WHERE o.id = $1",
    )
    .bind(order_id)
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|e| {
        warn!("Failed to read the language of order {}: {}", order_id, e);
        None
    });
    let (placed, preferred) = row.unwrap_or_default();
    i18n::pick_locale([placed.as_deref(), preferred.as_deref(), Some(default)])
}

/// Shop operator address (configured notification sender), falling back to
/// the store's branding support email
pub fn shop_email(state: &AppState) -> String {
    state
        .notification_from_email
        .as_deref()
        .filter(|email| !email.is_empty())
        .unwrap_or(&state.branding.support_email)
        .to_string()
}

/// Shop operator address for admin notifications (e.g. new-order alerts):
/// the configured `notifications.admin_email`, falling back to the sender.
pub fn admin_email(state: &AppState) -> String {
    state
        .notification_admin_email
        .as_deref()
        .filter(|email| !email.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| shop_email(state))
}

/// Fire-and-forget delivery of a pre-built notification
pub fn spawn_send(state: &AppState, notification: Notification, context: &str) {
    let Some(ref svc) = state.notification_service else {
        return;
    };
    let svc = svc.clone();
    let context = context.to_string();
    tokio::spawn(async move {
        if let Err(e) = svc.send(&notification).await {
            warn!("Failed to send {}: {}", context, e);
        }
    });
}

/// Notify the shop operator about a paid, confirmed order (fire-and-forget).
///
/// Only fires once the order actually left `pending` (payment confirmed), and
/// claims the notification atomically so a racing webhook/client-confirm pair
/// cannot email the operator twice.
pub async fn send_admin_order_email(state: &AppState, order_id: Uuid) {
    if state.notification_service.is_none() {
        return;
    }
    let detail = match state.order_service.get_order(order_id).await {
        Ok(Some(detail)) => detail,
        Ok(None) => return,
        Err(e) => {
            warn!(
                "Failed to load order {} for admin notification: {}",
                order_id, e
            );
            return;
        }
    };
    use rcommerce_core::order::OrderStatus;
    if !matches!(
        detail.order.status,
        OrderStatus::Confirmed
            | OrderStatus::Processing
            | OrderStatus::Shipped
            | OrderStatus::Delivered
            | OrderStatus::Completed
    ) {
        warn!(
            "Skipping admin notification for order {} in status {:?} (payment not confirmed)",
            order_id, detail.order.status
        );
        return;
    }
    // Atomic claim: the first caller to flip metadata.admin_notified wins;
    // concurrent duplicates see no row returned and bail out.
    let claimed: Option<Uuid> = sqlx::query_scalar(
        r#"
        UPDATE orders
        SET metadata = CASE
            WHEN jsonb_typeof(COALESCE(metadata, '{}'::jsonb)) = 'object'
            THEN jsonb_set(COALESCE(metadata, '{}'::jsonb), '{admin_notified}', 'true'::jsonb)
            ELSE '{"admin_notified": true}'::jsonb
        END
        WHERE id = $1 AND COALESCE(metadata->>'admin_notified', 'false') <> 'true'
        RETURNING id
        "#,
    )
    .bind(order_id)
    .fetch_optional(state.db.pool())
    .await
    .ok()
    .flatten();
    if claimed.is_none() {
        return; // another path already notified (or order vanished)
    }
    // The order detail's core Order shape has no address JSON; read the stored
    // address snapshots directly (tolerating a missing row/column)
    let row: Option<(Option<serde_json::Value>, Option<serde_json::Value>)> =
        sqlx::query_as("SELECT shipping_address, billing_address FROM orders WHERE id = $1")
            .bind(order_id)
            .fetch_optional(state.db.pool())
            .await
            .unwrap_or(None);
    let (shipping_address, billing_address) = row.unwrap_or((None, None));
    let mut notification = match build_admin_order_notification(
        &admin_email(state),
        &detail,
        shipping_address.as_ref(),
        billing_address.as_ref(),
        &state.branding,
        &state.email_locale,
    ) {
        Ok(n) => n,
        Err(e) => {
            warn!(
                "Failed to build admin new-order email for {}: {}",
                order_id, e
            );
            return;
        }
    };

    // Attach the invoice PDF, same as the customer confirmation email
    match super::payment::fetch_invoice_order(&state.db, order_id).await {
        Ok((order, items)) => {
            let invoice_data =
                super::invoice::invoice_data_from_order(&order, &items, &state.branding);
            match rcommerce_core::invoice::generate_invoice_pdf(&invoice_data) {
                Ok(pdf) => {
                    notification
                        .attachments
                        .push(rcommerce_core::notification::EmailAttachment {
                            filename: format!("invoice-{}.pdf", order.order_number),
                            content_type: "application/pdf".to_string(),
                            content: pdf,
                        });
                }
                Err(e) => warn!(
                    "Failed to generate invoice PDF for admin email (order {}): {}",
                    order_id, e
                ),
            }
        }
        Err(e) => warn!(
            "Failed to load order {} for admin email invoice PDF: {}",
            order_id, e
        ),
    }

    spawn_send(state, notification, "admin new-order email");
}

/// Notify the customer that their order has shipped (fire-and-forget).
///
/// Uses the branded `order_shipped` template. Triggered when a fulfillment
/// tool (e.g. Dianxiaomi via the WooCommerce-compatible tracking API) adds a
/// shipment tracking record to an order.
pub async fn send_order_shipped_email(
    state: &AppState,
    order_id: Uuid,
    tracking_number: &str,
    tracking_provider: Option<&str>,
    tracking_link: Option<&str>,
) {
    use rcommerce_core::notification::templates::{NotificationTemplate, TemplateVariables};

    if state.notification_service.is_none() {
        return;
    }

    let row: Option<(String, String, chrono::DateTime<chrono::Utc>, Option<serde_json::Value>)> =
        match sqlx::query_as(
            "SELECT customer_email, order_number, created_at, shipping_address FROM orders WHERE id = $1",
        )
        .bind(order_id)
        .fetch_optional(state.db.pool())
        .await
        {
            Ok(row) => row,
            Err(e) => {
                warn!("Failed to load order {} for shipped email: {}", order_id, e);
                return;
            }
        };
    let Some((customer_email, order_number, created_at, shipping_address)) = row else {
        return;
    };
    if customer_email.is_empty() {
        return;
    }

    let customer_name = shipping_address
        .as_ref()
        .and_then(|addr| {
            let first = addr
                .get("first_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let last = addr.get("last_name").and_then(|v| v.as_str()).unwrap_or("");
            let name = format!("{} {}", first, last).trim().to_string();
            if name.is_empty() {
                None
            } else {
                Some(name)
            }
        })
        .unwrap_or_default();
    let locale = order_email_locale(state.db.pool(), order_id, &state.email_locale).await;
    let customer_name = if customer_name.is_empty() {
        i18n::text(&locale, "common", "customer_fallback")
    } else {
        customer_name
    };

    let template = match NotificationTemplate::load_localized("order_shipped_html", &locale) {
        Ok(t) => t,
        Err(e) => {
            warn!("Failed to load order_shipped template: {}", e);
            return;
        }
    };

    let mut vars = TemplateVariables::new();
    vars.insert("customer_name", &customer_name);
    vars.insert("order_number", &order_number);
    vars.insert("order_date", i18n::date(&locale, created_at.date_naive()));
    vars.insert("tracking_number", tracking_number);
    vars.insert(
        "tracking_url",
        tracking_link.unwrap_or("https://example.com/orders"),
    );
    let carrier = tracking_provider
        .map(str::to_string)
        .unwrap_or_else(|| i18n::text(&locale, "order_shipped", "carrier"));
    vars.insert("shipping_carrier", carrier);
    vars.insert("company_name", &state.branding.name);
    vars.insert("support_email", &shop_email(state));

    let notification = match build_from_template(&customer_email, &template, vars, &state.branding)
    {
        Ok(n) => n,
        Err(e) => {
            warn!("Failed to render order_shipped notification: {}", e);
            return;
        }
    };
    spawn_send(state, notification, "order shipped email");
}

/// The ready e-mail's recipient, order number, customer name and pickup location
async fn ready_email_details(
    state: &AppState,
    order_id: Uuid,
) -> Option<(String, String, String, String)> {
    type Row = (
        String,
        String,
        Option<serde_json::Value>,
        Option<serde_json::Value>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT customer_email, order_number, shipping_address, metadata FROM orders WHERE id = $1",
    )
    .bind(order_id)
    .fetch_optional(state.db.pool())
    .await
    .map_err(|e| warn!("Failed to load order {} for ready email: {}", order_id, e))
    .ok()?;
    let (email, number, address, metadata) = row?;
    if email.is_empty() {
        return None;
    }
    let text = |value: &Option<serde_json::Value>, path: &[&str]| {
        let mut node = value.as_ref()?;
        for key in path {
            node = node.get(*key)?;
        }
        node.as_str().map(str::to_string)
    };
    let name = format!(
        "{} {}",
        text(&address, &["first_name"]).unwrap_or_default(),
        text(&address, &["last_name"]).unwrap_or_default()
    );
    let location = text(&metadata, &["pickup", "location_name"]).unwrap_or_default();
    Some((email, number, name.trim().to_string(), location))
}

/// Tell the customer a pickup order waits at the counter. Fire-and-forget: a
/// failure is logged and never fails the request.
pub async fn send_order_ready_email(state: &AppState, order_id: Uuid) {
    use rcommerce_core::notification::templates::{NotificationTemplate, TemplateVariables};

    if state.notification_service.is_none() {
        return;
    }
    let Some((email, number, name, location)) = ready_email_details(state, order_id).await else {
        return;
    };
    let locale = order_email_locale(state.db.pool(), order_id, &state.email_locale).await;
    let section = "order_ready_for_pickup";
    let or_text = |value: String, key: &str| {
        if value.is_empty() {
            i18n::text(&locale, section, key)
        } else {
            value
        }
    };
    let name = or_text(name, "customer_fallback");
    let location = or_text(location, "location_fallback");
    let template =
        match NotificationTemplate::load_localized("order_ready_for_pickup_html", &locale) {
            Ok(t) => t,
            Err(e) => {
                warn!("Failed to load order_ready_for_pickup template: {}", e);
                return;
            }
        };
    let mut vars = TemplateVariables::new();
    vars.insert("customer_name", name);
    vars.insert("order_number", number);
    vars.insert("location_name", location);
    vars.insert("company_name", &state.branding.name);
    vars.insert("support_email", shop_email(state));
    match build_from_template(&email, &template, vars, &state.branding) {
        Ok(notification) => spawn_send(state, notification, "order ready email"),
        Err(e) => warn!(
            "Failed to render order_ready_for_pickup notification: {}",
            e
        ),
    }
}

/// Render a template into a Notification (subject/text/html)
fn build_from_template(
    recipient: &str,
    template: &rcommerce_core::notification::templates::NotificationTemplate,
    variables: rcommerce_core::notification::templates::TemplateVariables,
    branding: &rcommerce_core::BrandingConfig,
) -> rcommerce_core::Result<Notification> {
    use rcommerce_core::notification::types::{DeliveryStatus, NotificationPriority};

    Ok(Notification {
        id: Uuid::new_v4(),
        channel: NotificationChannel::Email,
        recipient: recipient.to_string(),
        subject: template.render_subject(&variables)?,
        body: template.render(&variables)?,
        html_body: template.render_html_with_brand(&variables, branding)?,
        priority: NotificationPriority::Normal,
        status: DeliveryStatus::Pending,
        attempt_count: 0,
        max_attempts: 3,
        error_message: None,
        metadata: serde_json::json!({"template_id": template.id}),
        scheduled_at: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        attachments: Vec::new(),
    })
}

/// Build the "New order" notification sent to the shop operator.
///
/// Uses the branded `admin_new_order_html` template (same shell and layout as
/// the customer order confirmation) with the customer identity and both
/// address columns; the invoice PDF is attached by the caller.
pub fn build_admin_order_notification(
    admin_email: &str,
    detail: &OrderDetail,
    shipping_address: Option<&serde_json::Value>,
    billing_address: Option<&serde_json::Value>,
    branding: &rcommerce_core::BrandingConfig,
    locale: &str,
) -> rcommerce_core::Result<Notification> {
    use rcommerce_core::notification::templates::{NotificationTemplate, TemplateVariables};
    use rcommerce_core::notification::{Address, EmailNotificationFactory, OrderItem};

    let order = &detail.order;
    let template = NotificationTemplate::load_localized("admin_new_order_html", locale)?;
    let locale = template.locale.clone();
    let currency = order.currency.to_string();
    let money = |amount: Decimal| i18n::money(&locale, amount, &currency);

    // Customer identity from the shipping address snapshot, fallback to email
    let customer_name = shipping_address
        .and_then(|addr| {
            let first = addr
                .get("first_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let last = addr.get("last_name").and_then(|v| v.as_str()).unwrap_or("");
            let name = format!("{} {}", first, last).trim().to_string();
            if name.is_empty() {
                None
            } else {
                Some(name)
            }
        })
        .unwrap_or_else(|| {
            order
                .customer_email
                .split('@')
                .next()
                .unwrap_or("Customer")
                .to_string()
        });

    let mut vars = TemplateVariables::new();
    vars.insert("order_number", &order.order_number);
    vars.insert(
        "order_date",
        i18n::date(&locale, order.created_at.date_naive()),
    );
    vars.insert("order_total", money(order.total));
    vars.insert("customer_name", &customer_name);
    vars.insert("customer_email", &order.customer_email);
    vars.insert("subtotal", money(order.subtotal));
    vars.insert("shipping_cost", money(order.shipping_total));

    // Tax row is omitted entirely when no tax was charged
    if !order.tax_total.round_dp(2).is_zero() {
        let tax_row = format!(
            "<tr><td style=\"padding:4px 0;font-size:13px;color:#666666;\">{}</td><td align=\"right\" style=\"padding:4px 0;font-size:13px;color:#1a1a1a;\">{}</td></tr>",
            i18n::text(&locale, "common", "tax"),
            money(order.tax_total)
        );
        vars.insert("tax_row", tax_row);
    } else {
        vars.insert("tax_row", "");
    }

    // Discount row follows the same conditional pattern: negative amount in
    // green, labelled with the coupon code when known.
    if order.discount_total > rust_decimal::Decimal::ZERO {
        let label = order
            .coupon_code
            .as_deref()
            .filter(|c| !c.is_empty())
            .map(|c| i18n::text(&locale, "common", "discount_code").replace("{code}", c))
            .unwrap_or_else(|| i18n::text(&locale, "common", "discount"));
        let discount_row = format!(
            "<tr><td style=\"padding:4px 0;font-size:13px;color:#666666;\">{label}</td><td align=\"right\" style=\"padding:4px 0;font-size:13px;color:#059669;\">-{}</td></tr>",
            money(order.discount_total)
        );
        vars.insert("discount_row", discount_row);
    } else {
        vars.insert("discount_row", "");
    }

    // Item rows with SKU sub-lines, same formatter as the customer email
    let email_items: Vec<OrderItem> = detail
        .items
        .iter()
        .map(|item| OrderItem {
            name: item.name.clone(),
            variant_title: item.variant_name.clone(),
            sku: item.sku.clone().unwrap_or_default(),
            quantity: item.quantity,
            price: format!(
                "{:.2}",
                item.price * rust_decimal::Decimal::from(item.quantity)
            ),
            is_bundle_component: item.is_bundle_component == Some(true),
        })
        .collect();
    let items_html =
        EmailNotificationFactory::format_order_items_in(&email_items, &locale, &currency);
    vars.insert("items", items_html);

    // Address columns via the shared formatter (JSON snapshot -> Address)
    let empty_addr = |name: &str| Address {
        name: name.to_string(),
        street: String::new(),
        city: String::new(),
        state: String::new(),
        zip: String::new(),
        country: String::new(),
    };
    let shipping = shipping_address
        .and_then(super::payment::email_address_from_json)
        .unwrap_or_else(|| empty_addr(&customer_name));
    let billing_json = billing_address.or(shipping_address);
    let billing = billing_json
        .and_then(super::payment::email_address_from_json)
        .unwrap_or_else(|| shipping.clone());
    let shipping_text = format!(
        "{}\n{}\n{}, {} {}\n{}",
        shipping.name,
        shipping.street,
        shipping.city,
        shipping.state,
        shipping.zip,
        shipping.country
    );
    vars.insert(
        "shipping_address",
        EmailNotificationFactory::format_address(&shipping),
    );
    vars.insert(
        "billing_address",
        EmailNotificationFactory::format_address(&billing),
    );
    vars.insert("shipping_address_text", shipping_text);

    vars.insert("company_name", &branding.name);
    vars.insert("support_email", &branding.support_email);

    build_from_template(admin_email, &template, vars, branding)
}

/// Build the customer-facing order-cancelled notification, in `locale`
pub fn build_order_cancelled_notification(
    order: &Order,
    payment_captured: bool,
    locale: &str,
) -> Notification {
    let says = |key: &str, number: &str| {
        i18n::text(locale, "order_cancelled", key).replace("{{ order_number }}", number)
    };
    let number = &order.order_number;
    let strong_number = format!("<strong>{number}</strong>");
    let (greeting, sorry) = (says("greeting", number), says("sorry", number));
    let refunded = says("refunded", number);
    let refund_note = if payment_captured {
        format!(" {refunded}")
    } else {
        String::new()
    };
    let refund_note_html = if payment_captured {
        format!("<p>{refunded}</p>")
    } else {
        String::new()
    };

    let body = format!(
        "{greeting}\n\n{}\n\n{sorry}{refund_note}",
        says("cancelled", number)
    );
    let html_body = format!(
        "<p>{greeting}</p><p>{}</p><p>{sorry}</p>{refund_note_html}",
        says("cancelled", &strong_number)
    );

    Notification::new(
        NotificationChannel::Email,
        order.customer_email.clone(),
        says("subject", number),
        body,
    )
    .with_html_body(html_body)
    .with_metadata(serde_json::json!({"template_id": "order_cancelled"}))
}

/// Notify the customer that their order was refunded (fire-and-forget).
///
/// Uses the branded `refund_processed_html` template (same shell and layout as
/// the order confirmation and shipped emails).
pub async fn send_order_refunded_email(state: &AppState, order_id: Uuid, refunded_amount: Decimal) {
    if state.notification_service.is_none() {
        return;
    }

    let row: Option<(String, String, String, Option<serde_json::Value>)> = match sqlx::query_as(
        "SELECT customer_email, order_number, currency, shipping_address FROM orders WHERE id = $1",
    )
    .bind(order_id)
    .fetch_optional(state.db.pool())
    .await
    {
        Ok(row) => row,
        Err(e) => {
            warn!("Failed to load order {} for refund email: {}", order_id, e);
            return;
        }
    };
    let Some((customer_email, order_number, currency, shipping_address)) = row else {
        return;
    };
    if customer_email.is_empty() {
        return;
    }

    let customer_name = shipping_address
        .as_ref()
        .and_then(|addr| {
            let first = addr
                .get("first_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let last = addr.get("last_name").and_then(|v| v.as_str()).unwrap_or("");
            let name = format!("{} {}", first, last).trim().to_string();
            if name.is_empty() {
                None
            } else {
                Some(name)
            }
        })
        .unwrap_or_default();
    let locale = order_email_locale(state.db.pool(), order_id, &state.email_locale).await;

    let notification = match build_order_refunded_notification(
        &customer_email,
        &order_number,
        &currency,
        refunded_amount,
        &customer_name,
        &shop_email(state),
        &MailBrand {
            branding: state.branding.clone(),
            default_locale: locale,
        },
    ) {
        Ok(n) => n,
        Err(e) => {
            warn!("Failed to render order-refunded notification: {}", e);
            return;
        }
    };
    spawn_send(state, notification, "order-refunded email");
}

/// Build the customer-facing order-refunded notification from the branded
/// `refund_processed_html` template.
pub fn build_order_refunded_notification(
    recipient: &str,
    order_number: &str,
    currency: &str,
    refunded_amount: Decimal,
    customer_name: &str,
    support_email: &str,
    mail: &MailBrand,
) -> rcommerce_core::Result<Notification> {
    use rcommerce_core::notification::templates::{NotificationTemplate, TemplateVariables};

    let branding = &mail.branding;
    let template =
        NotificationTemplate::load_localized("refund_processed_html", &mail.default_locale)?;
    let locale = template.locale.clone();
    let section = "refund_processed";

    let mut vars = TemplateVariables::new();
    let customer_name = if customer_name.is_empty() {
        i18n::text(&locale, section, "customer_fallback")
    } else {
        customer_name.to_string()
    };
    vars.insert("customer_name", customer_name);
    vars.insert("order_number", order_number);
    vars.insert(
        "refund_amount",
        i18n::money(&locale, refunded_amount, currency),
    );
    vars.insert(
        "refund_method",
        i18n::text(&locale, section, "original_method"),
    );
    vars.insert(
        "processing_time",
        i18n::text(&locale, section, "business_days"),
    );
    vars.insert("company_name", &branding.name);
    vars.insert("support_email", support_email);

    build_from_template(recipient, &template, vars, branding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn sample_order() -> Order {
        Order {
            id: Uuid::new_v4(),
            order_number: "ORD-1001".to_string(),
            customer_id: None,
            customer_email: "customer@example.com".to_string(),
            currency: "USD".to_string(),
            order_type: rcommerce_core::models::OrderType::OneTime,
            subtotal: Decimal::new(1000, 2),
            tax_total: Decimal::new(80, 2),
            shipping_total: Decimal::new(500, 2),
            discount_total: Decimal::ZERO,
            total: Decimal::new(1580, 2),
            status: rcommerce_core::models::OrderStatus::Cancelled,
            fulfillment_status: rcommerce_core::models::FulfillmentStatus::Cancelled,
            payment_status: rcommerce_core::models::PaymentStatus::Refunded,
            shipping_address_id: None,
            billing_address_id: None,
            payment_method: None,
            shipping_method: None,
            notes: None,
            tags: Vec::new(),
            draft: false,
            invoice_number: Some("88-1001".to_string()),
            test: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: None,
            subscription_id: None,
            billing_cycle: None,
            shipping_address: Some(serde_json::json!({
                "first_name": "Jane",
                "last_name": "Doe",
                "address1": "123 Main St",
                "address2": "",
                "city": "Los Angeles",
                "state": "CA",
                "zip": "90001",
                "country": "US"
            })),
            billing_address: None,
            metadata: None,
            coupon_code: None,
            coupon_id: None,
            ip_address: None,
            user_agent: None,
            cancelled_at: Some(Utc::now()),
        }
    }

    /// A store-branded identity for tests that assert branded output
    /// (code defaults are deliberately neutral)
    pub fn brand_fixture() -> rcommerce_core::BrandingConfig {
        rcommerce_core::BrandingConfig {
            name: "Example Store".to_string(),
            support_email: "support@example.com".to_string(),
            site_url: "https://example.com".to_string(),
            logo_url: "https://example.com/images/logo.png".to_string(),
            accent_color: "#ff4d00".to_string(),
        }
    }

    pub fn sample_order_detail(tax: &str) -> OrderDetail {
        use rust_decimal::Decimal;
        let now = Utc::now();
        let order = rcommerce_core::order::Order {
            id: Uuid::new_v4(),
            order_number: "ORD-1001".to_string(),
            customer_id: None,
            customer_email: "customer@example.com".to_string(),
            billing_address_id: None,
            shipping_address_id: None,
            status: rcommerce_core::order::OrderStatus::Confirmed,
            fulfillment_status: rcommerce_core::order::FulfillmentStatus::Pending,
            payment_status: rcommerce_core::order::PaymentStatus::Paid,
            currency: "USD".to_string(),
            subtotal: Decimal::new(1000, 2),
            tax_total: tax.parse().unwrap(),
            shipping_total: Decimal::new(500, 2),
            discount_total: Decimal::ZERO,
            total: Decimal::new(1500, 2) + tax.parse::<Decimal>().unwrap(),
            notes: None,
            tags: Some(Vec::new()),
            invoice_number: Some("88-1001".to_string()),
            test: false,
            coupon_code: None,
            coupon_id: None,
            metadata: serde_json::json!({}),
            created_at: now,
            updated_at: now,
            completed_at: None,
        };
        let item = rcommerce_core::order::OrderItem {
            id: Uuid::new_v4(),
            order_id: order.id,
            product_id: Uuid::new_v4(),
            variant_id: None,
            quantity: 2,
            price: Decimal::new(500, 2),
            subtotal: Decimal::new(1000, 2),
            tax_amount: Decimal::ZERO,
            total: Decimal::new(1000, 2),
            sku: Some("WHEEL-70".to_string()),
            name: "70mm Wheels".to_string(),
            variant_name: None,
            weight: None,
            weight_unit: None,
            is_bundle_component: None,
            metadata: serde_json::json!({}),
            created_at: now,
        };
        OrderDetail {
            order,
            items: vec![item],
        }
    }

    pub fn sample_address_json() -> serde_json::Value {
        serde_json::json!({
            "first_name": "Jane",
            "last_name": "Doe",
            "address1": "123 Main St",
            "address2": "",
            "city": "Los Angeles",
            "state": "CA",
            "zip": "90001",
            "country": "US"
        })
    }

    #[test]
    fn test_admin_order_email_uses_branded_shell_and_layout() {
        let detail = sample_order_detail("0.80");
        let addr = sample_address_json();
        let notification = build_admin_order_notification(
            "admin@example.com",
            &detail,
            Some(&addr),
            Some(&addr),
            &brand_fixture(),
            "en",
        )
        .unwrap();

        assert_eq!(notification.recipient, "admin@example.com");
        assert_eq!(notification.subject, "New order: ORD-1001");
        let html = notification.html_body.as_deref().unwrap();
        // Shared brand shell
        assert!(html.contains("logo.png"), "logo header missing");
        assert!(
            html.contains("support@example.com"),
            "footer contact missing"
        );
        // Admin-specific identity block
        assert!(html.contains("New order"), "pretext missing");
        assert!(html.contains("ORD-1001"));
        assert!(html.contains("Jane Doe"));
        assert!(html.contains("customer@example.com"));
        // Same layout blocks as the customer confirmation
        assert!(html.contains("Order Number"));
        assert!(html.contains("Order Date"));
        assert!(html.contains(">Total<"));
        assert!(html.contains("SKU: WHEEL-70"), "SKU sub-line missing");
        assert!(html.contains("Shipping Address"));
        assert!(html.contains("Billing Address"));
        assert!(html.contains("$10.00")); // subtotal
        assert!(html.contains("$5.00")); // shipping
        assert!(html.contains("$0.80")); // tax
        assert!(html.contains("$15.80")); // total, written like every amount
    }

    #[test]
    fn test_admin_order_email_hides_zero_tax_row() {
        let detail = sample_order_detail("0.00");
        let addr = sample_address_json();
        let notification = build_admin_order_notification(
            "admin@example.com",
            &detail,
            Some(&addr),
            None,
            &brand_fixture(),
            "en",
        )
        .unwrap();
        let html = notification.html_body.as_deref().unwrap();
        assert!(!html.contains(">Tax<"), "zero tax row should be hidden");
        // Billing falls back to shipping
        assert!(html.contains("123 Main St"));
    }

    #[test]
    fn test_build_order_cancelled_notification_with_refund_note() {
        let order = sample_order();
        let notification = build_order_cancelled_notification(&order, true, "en");

        assert_eq!(notification.recipient, "customer@example.com");
        assert_eq!(
            notification.subject,
            "Your order ORD-1001 has been cancelled"
        );
        assert!(notification.body.contains("ORD-1001"));
        assert!(notification.body.contains("refunded"));
        assert!(notification
            .html_body
            .as_deref()
            .unwrap()
            .contains("ORD-1001"));
    }

    #[test]
    fn test_build_order_cancelled_notification_without_refund_note() {
        let order = sample_order();
        let notification = build_order_cancelled_notification(&order, false, "en");

        assert!(!notification.body.contains("refunded"));
    }

    #[test]
    fn test_build_order_refunded_notification_includes_amount() {
        let notification = build_order_refunded_notification(
            "customer@example.com",
            "ORD-1001",
            "USD",
            Decimal::new(1580, 2),
            "Jane Doe",
            "support@example.com",
            &MailBrand {
                branding: brand_fixture(),
                default_locale: "en".to_string(),
            },
        )
        .expect("refund notification renders");

        assert_eq!(notification.recipient, "customer@example.com");
        assert_eq!(notification.subject, "Refund Processed: ORD-1001");
        assert!(notification.body.contains("$15.80"));
        let html = notification.html_body.as_deref().unwrap();
        assert!(html.contains("ORD-1001"));
        assert!(html.contains("$15.80"));
        // Branded shell: logo, support email, site link
        assert!(html.contains("https://example.com/images/logo.png"));
        assert!(html.contains("support@example.com"));
    }
}
