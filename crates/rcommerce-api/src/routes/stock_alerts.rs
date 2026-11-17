//! Stock alerts of the signed-in customer (`/customers/me/alerts`), and the loop that
//! e-mails them once their stock has arrived at the branch

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get},
    Extension, Json, Router,
};
use rcommerce_core::notification::{i18n, EmailNotificationFactory};
use rcommerce_core::services::stock_alerts::{self, DueAlert, NewAlert, StockAlert};
use rcommerce_core::Error;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::middleware::JwtAuth;
use crate::state::AppState;

/// `POST /customers/me/alerts`: tell me when this product is in stock (201)
async fn create(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(new): Json<NewAlert>,
) -> Result<(StatusCode, Json<StockAlert>), Error> {
    let alert = stock_alerts::create(state.db.pool(), auth.customer_id, &new).await?;
    Ok((StatusCode::CREATED, Json(alert)))
}

/// `GET /customers/me/alerts`: open alerts, then those sent in the last 30 days
async fn list(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
) -> Result<Json<Value>, Error> {
    let alerts = stock_alerts::list(state.db.pool(), auth.customer_id).await?;
    Ok(Json(json!({ "alerts": alerts })))
}

/// `DELETE /customers/me/alerts/:id`: close an open alert (204; 404 when none)
async fn cancel(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Error> {
    if stock_alerts::cancel(state.db.pool(), auth.customer_id, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::not_found("Alert not found"))
    }
}

/// Customer routes (JWT)
pub fn protected_router() -> Router<AppState> {
    Router::new()
        .route("/customers/me/alerts", get(list).post(create))
        .route("/customers/me/alerts/:id", delete(cancel))
}

/// Longest wait for one e-mail
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// E-mail the alerts whose stock has arrived, a batch per run; an alert closes once
/// its e-mail went. A failed send counts against that alert and ends the run (the
/// mail service may be down: the others wait, uncounted). Returns how many were sent.
pub async fn send_due(state: &AppState) -> rcommerce_core::Result<usize> {
    let Some(service) = state.notification_service.clone() else {
        return Ok(0);
    };
    let pool = state.db.pool();
    stock_alerts::close_unreachable(pool).await?;
    stock_alerts::purge_closed(pool).await?;
    let mut sent = 0;
    for alert in stock_alerts::due(pool, 200).await? {
        let notification = match alert_mail(state, &alert).await {
            Ok(Some(notification)) => Some(notification),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("Stock alert {} not built: {}", alert.id, e);
                None
            }
        };
        // The provider's error text can carry the address: only the alert is logged
        let delivered = match notification {
            Some(notification) => matches!(
                tokio::time::timeout(SEND_TIMEOUT, service.send(&notification)).await,
                Ok(Ok(_))
            ),
            None => false,
        };
        let recorded = if delivered {
            sent += 1;
            stock_alerts::mark_sent(pool, alert.id).await.map(|_| ())
        } else {
            tracing::warn!("Stock alert {} not sent; tried again later", alert.id);
            stock_alerts::failed(pool, alert.id).await
        };
        if let Err(e) = recorded {
            tracing::warn!("Stock alert {} not updated: {}", alert.id, e);
        }
        if !delivered {
            break;
        }
    }
    Ok(sent)
}

/// The e-mail of one due alert, in the customer's language (none when the
/// customer or product is gone)
async fn alert_mail(
    state: &AppState,
    alert: &DueAlert,
) -> rcommerce_core::Result<Option<rcommerce_core::notification::Notification>> {
    let pool = state.db.pool();
    let Some(customer) = state.customer_service.find_by_id(alert.customer_id).await? else {
        return Ok(None);
    };
    let locale = i18n::pick_locale([
        customer.locale.as_deref(),
        Some(state.email_locale.as_str()),
    ]);
    let product: Option<(String, String)> = sqlx::query_as(
        "SELECT COALESCE(t.title, p.title), p.slug FROM products p \
         LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = $2 \
         WHERE p.id = $1",
    )
    .bind(alert.product_id)
    .bind(&locale)
    .fetch_optional(pool)
    .await?;
    let Some((title, slug)) = product else {
        return Ok(None);
    };
    let base = state.storefront_url.trim_end_matches('/');
    let url = format!("{base}/products/{slug}?lang={locale}");
    let name = format!("{} {}", customer.first_name, customer.last_name);
    EmailNotificationFactory::stock_alert(
        &customer.email,
        name.trim(),
        &title,
        &alert.location_name,
        alert.quantity,
        &url,
        &state.branding,
        &locale,
    )
    .map(Some)
}
