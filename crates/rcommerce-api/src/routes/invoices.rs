//! Invoices: issued when an order's goods are handed over, e-mailed with
//! their PDF, sent through Peppol to companies that receive e-invoices.
//! Customers read their own (and their company's); staff list them all and
//! can send one through Peppol again.

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use serde::Deserialize;
use tracing::warn;
use uuid::Uuid;

use crate::middleware::JwtAuth;
use crate::state::AppState;
use rcommerce_core::invoicing::service::InvoiceQuery;
use rcommerce_core::invoicing::Invoice;
use rcommerce_core::notification::i18n;
use rcommerce_core::notification::{EmailAttachment, Notification, NotificationChannel};
use rcommerce_core::services::{Action, Resource};
use rcommerce_core::Error;

/// Issue the invoice of an order whose goods were just handed over, then
/// e-mail it and queue it for Peppol. Best effort: a failure is logged and the
/// hand-over stands (staff can issue it again from the order).
pub async fn issue_after_handover(state: &AppState, order_id: Uuid) {
    if !state.invoices.enabled() {
        return;
    }
    match state.invoices.issue_for_order(order_id).await {
        Ok(invoice) => {
            send_invoice_email(state, &invoice);
            if let Some(peppol) = state.peppol.clone() {
                let invoices = state.invoices.clone();
                let logo = state.branding.pdf_logo.clone();
                tokio::spawn(async move {
                    let pdf =
                        rcommerce_core::invoice::legal::invoice_pdf(&invoice, logo.as_ref()).ok();
                    if invoice.peppol_status.as_deref() == Some("pending") {
                        if let Err(e) = peppol.deliver(&invoices, &invoice, pdf.as_deref()).await {
                            warn!("Peppol: invoice {} queued for retry: {}", invoice.number, e);
                        }
                    }
                });
            }
        }
        Err(e) => warn!("Failed to issue the invoice of order {}: {}", order_id, e),
    }
}

/// A text of the invoice e-mail in the invoice's language, its placeholders filled
fn invoice_text(invoice: &Invoice, company: &str, key: &str) -> String {
    let locale = invoice.locale.as_str();
    let money = |amount| i18n::money(locale, amount, &invoice.currency);
    i18n::text(locale, "invoice_issued", key)
        .replace("{{ customer_name }}", &invoice.buyer.name)
        .replace("{{ invoice_number }}", &invoice.number)
        .replace("{{ order_number }}", &invoice.order_number)
        .replace("{{ due_date }}", &i18n::date(locale, invoice.due_date))
        .replace("{{ payment_reference }}", &invoice.payment_reference)
        .replace("{{ invoice_total }}", &money(invoice.total))
        .replace("{{ company_name }}", company)
}

/// The invoice e-mail, in the invoice's language, with its PDF attached
pub fn invoice_notification(
    invoice: &Invoice,
    branding: &rcommerce_core::BrandingConfig,
    pdf: Vec<u8>,
) -> Notification {
    let says = |key: &str| invoice_text(invoice, &branding.name, key);
    let payment = says(if invoice.amount_due().is_zero() {
        "paid"
    } else {
        "due"
    });
    let peppol = match invoice.peppol_status {
        Some(_) => format!("<p>{}</p>", says("peppol")),
        None => String::new(),
    };
    let total = i18n::money(&invoice.locale, invoice.total, &invoice.currency);
    let body = format!(
        "<h1 style=\"margin:0 0 12px 0;font-size:22px;color:#1a1a1a;\">{}</h1><p>{}</p>         <p><strong>{} : {total}</strong></p><p>{payment}</p>{peppol}<p>{}</p>",
        says("heading"),
        says("intro"),
        says("amount"),
        says("attached"),
    );
    let html = rcommerce_core::notification::templates::wrap_localized_email_html(
        &invoice.locale,
        &says("subject"),
        &body,
        branding,
    );
    let text = says("text").replace("{{ payment_line }}", &payment);
    let recipient = invoice.buyer.email.clone().unwrap_or_default();
    let mut notification =
        Notification::new(NotificationChannel::Email, recipient, says("subject"), text)
            .with_html_body(html)
            .with_metadata(
                serde_json::json!({"template_id": "invoice_issued", "invoice": invoice.number}),
            );
    notification.attachments.push(EmailAttachment {
        filename: format!("{}.pdf", invoice.number),
        content_type: "application/pdf".into(),
        content: pdf,
    });
    notification
}

/// E-mail an invoice to its buyer (fire-and-forget)
fn send_invoice_email(state: &AppState, invoice: &Invoice) {
    if state.notification_service.is_none() || invoice.buyer.email.is_none() {
        return;
    }
    match rcommerce_core::invoice::legal::invoice_pdf(invoice, state.branding.pdf_logo.as_ref()) {
        Ok(pdf) => {
            let notification = invoice_notification(invoice, &state.branding, pdf);
            super::email_helpers::spawn_send(state, notification, "invoice email");
        }
        Err(e) => warn!(
            "Failed to render the PDF of invoice {}: {}",
            invoice.number, e
        ),
    }
}

/// `?order_id=`: the invoice of one order
#[derive(Debug, Deserialize)]
pub struct MineQuery {
    pub order_id: Option<Uuid>,
}

/// The invoice `id` if the caller may see it: theirs, their company's, or staff
async fn readable(state: &AppState, auth: &JwtAuth, id: Uuid) -> Result<Invoice, Error> {
    let invoice = state
        .invoices
        .get(id)
        .await?
        .ok_or_else(|| Error::not_found("Invoice not found"))?;
    if state
        .invoices
        .visible_to(&invoice, auth.customer_id)
        .await?
        || crate::middleware::staff_access::staff_can(state, auth, Resource::Orders, Action::Read)
            .await
    {
        Ok(invoice)
    } else {
        Err(Error::not_found("Invoice not found"))
    }
}

/// The caller's invoices and their company's, newest first
pub async fn my_invoices(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Query(query): Query<MineQuery>,
) -> Result<Json<serde_json::Value>, Error> {
    let invoices = state
        .invoices
        .list_for_customer(auth.customer_id, query.order_id)
        .await?;
    Ok(Json(serde_json::json!({ "invoices": invoices })))
}

/// One invoice
pub async fn get_invoice(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Error> {
    let invoice = readable(&state, &auth, id).await?;
    Ok(Json(serde_json::json!({ "invoice": invoice })))
}

/// The invoice as a PDF
pub async fn invoice_pdf(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(id): Path<Uuid>,
) -> Result<Response, Error> {
    let invoice = readable(&state, &auth, id).await?;
    let pdf =
        rcommerce_core::invoice::legal::invoice_pdf(&invoice, state.branding.pdf_logo.as_ref())?;
    let disposition = format!("inline; filename=\"{}.pdf\"", invoice.number);
    Ok((
        [
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        pdf,
    )
        .into_response())
}

/// The invoice as a Peppol BIS Billing 3.0 UBL document
pub async fn invoice_ubl(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(id): Path<Uuid>,
) -> Result<Response, Error> {
    let invoice = readable(&state, &auth, id).await?;
    let seller = rcommerce_core::peppol::participant_of(&invoice.seller).unwrap_or_default();
    let buyer = invoice
        .peppol_recipient
        .clone()
        .or_else(|| rcommerce_core::peppol::participant_of(&invoice.buyer))
        .unwrap_or_default();
    let xml = rcommerce_core::peppol::ubl::invoice_xml(&invoice, &seller, &buyer, None);
    let disposition = format!("attachment; filename=\"{}.xml\"", invoice.number);
    Ok((
        [
            (header::CONTENT_TYPE, "application/xml".to_string()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        xml,
    )
        .into_response())
}

/// Invoices for staff, newest first, with paging
pub async fn list_invoices(
    State(state): State<AppState>,
    Query(query): Query<InvoiceQuery>,
) -> Result<Json<serde_json::Value>, Error> {
    let (invoices, total) = state.invoices.list(&query).await?;
    let per_page = query.per_page.unwrap_or(50).clamp(1, 100);
    Ok(Json(serde_json::json!({
        "invoices": invoices,
        "meta": {
            "total": total,
            "page": query.page.unwrap_or(1).max(1),
            "per_page": per_page,
            "total_pages": (total + per_page - 1) / per_page,
        },
    })))
}

/// Issue the invoice of a completed order now (if the automatic issue failed)
pub async fn issue_invoice(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
) -> Result<(StatusCode, Json<serde_json::Value>), Error> {
    if !state.invoices.enabled() {
        return Err(Error::validation("Invoicing is not enabled for this store"));
    }
    let status: Option<String> =
        sqlx::query_scalar("SELECT status::text FROM orders WHERE id = $1")
            .bind(order_id)
            .fetch_optional(state.db.pool())
            .await?;
    match status.as_deref() {
        None => return Err(Error::not_found("Order not found")),
        Some("completed") => {}
        Some(_) => return Err(Error::validation("Only completed orders are invoiced")),
    }
    let invoice = state.invoices.issue_for_order(order_id).await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "invoice": invoice })),
    ))
}

/// Send an invoice through Peppol now (again, after a failure)
pub async fn send_peppol(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Error> {
    let peppol = state
        .peppol
        .clone()
        .ok_or_else(|| Error::validation("Peppol is not enabled for this store"))?;
    let invoice = state
        .invoices
        .get(id)
        .await?
        .ok_or_else(|| Error::not_found("Invoice not found"))?;
    if invoice.buyer.vat_number.is_none() {
        return Err(Error::validation(
            "Only invoices to companies go through Peppol",
        ));
    }
    let pdf =
        rcommerce_core::invoice::legal::invoice_pdf(&invoice, state.branding.pdf_logo.as_ref())
            .ok();
    peppol
        .deliver(&state.invoices, &invoice, pdf.as_deref())
        .await?;
    let invoice = state.invoices.get(id).await?;
    Ok(Json(serde_json::json!({ "invoice": invoice })))
}

/// Customer routes (JWT required)
pub fn protected_router() -> Router<AppState> {
    Router::new()
        .route("/invoices", get(my_invoices))
        .route("/invoices/:id", get(get_invoice))
        .route("/invoices/:id/pdf", get(invoice_pdf))
        .route("/invoices/:id/ubl", get(invoice_ubl))
}

/// Staff routes, guarded as order data (`orders:read` / `orders:write`)
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin/invoices", get(list_invoices))
        .route("/admin/invoices/:id/peppol", post(send_peppol))
        .route("/admin/orders/:id/invoice", post(issue_invoice))
}
