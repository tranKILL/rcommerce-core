//! Commercial e-mails: the customer's choice and its history, the unsubscribe link
//! of each e-mail, and the staff export of the e-mail list

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Extension, Json, Router,
};
use rcommerce_core::services::marketing_consent::{self as consent, Audience, Choice, Source};
use rcommerce_core::services::two_factor::events::{self, Event};
use rcommerce_core::services::two_factor::FRESH_SECS;
use rcommerce_core::{Action, Error, Resource};
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::IpAddr;
use uuid::Uuid;

use crate::middleware::{staff_access::staff_can, ClientIp, JwtAuth};
use crate::state::AppState;

/// The address a request came from (none behind no proxy information)
pub(crate) fn ip_of(client_ip: Option<Extension<ClientIp>>) -> Option<IpAddr> {
    client_ip.map(|ext| ext.0.ip())
}

/// The browser that sent a request
pub(crate) fn user_agent(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
}

/// Record a customer's choice made from `source` (sign-up, account)
pub(crate) async fn record_choice(
    state: &AppState,
    customer_id: Uuid,
    granted: bool,
    source: Source,
    wording: Option<&str>,
    ip: Option<IpAddr>,
    headers: &HeaderMap,
) -> Result<bool, Error> {
    let choice = Choice {
        granted,
        source,
        wording,
        ip,
        user_agent: user_agent(headers),
    };
    consent::choose(state.db.pool(), customer_id, &choice).await
}

/// The key of the unsubscribe links
pub(crate) fn unsubscribe_key(state: &AppState) -> Vec<u8> {
    let security = &state.auth_service.config().security;
    match &security.unsubscribe_key {
        Some(key) => key.0.as_bytes().to_vec(),
        None => security.jwt.secret.as_bytes().to_vec(),
    }
}

/// `GET /customers/me/marketing`: the choice and its history
async fn mine(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
) -> Result<Json<Value>, Error> {
    let customer = state
        .customer_service
        .find_by_id(auth.customer_id)
        .await?
        .ok_or_else(|| Error::not_found("Customer not found"))?;
    let history = consent::history(state.db.pool(), auth.customer_id).await?;
    Ok(Json(json!({
        "accepts_marketing": customer.accepts_marketing,
        "history": history,
    })))
}

/// An unsubscribe link's token
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenBody {
    token: String,
}

/// `a***@example.com`: enough for the owner to recognise, not the address
fn masked(email: &str) -> String {
    match email.split_once('@') {
        Some((name, domain)) => {
            let first: String = name.chars().take(1).collect();
            // `family.be` would name the person: only its first letter and ending
            let (host, ending) = domain.rsplit_once('.').unwrap_or((domain, ""));
            let host: String = host.chars().take(1).collect();
            format!("{first}***@{host}***.{ending}")
        }
        None => "***".to_string(),
    }
}

/// The customer of an unsubscribe token (404 for a wrong or unknown one)
async fn token_customer(
    state: &AppState,
    token: &str,
) -> Result<rcommerce_core::models::Customer, Error> {
    let id = consent::unsubscribe_customer(&unsubscribe_key(state), token)
        .ok_or_else(|| Error::not_found("Unknown unsubscribe link"))?;
    state
        .customer_service
        .find_by_id(id)
        .await?
        .ok_or_else(|| Error::not_found("Unknown unsubscribe link"))
}

/// `GET /marketing/unsubscribe?token=`: whose link it is (masked) and their choice
async fn preview(
    State(state): State<AppState>,
    Query(query): Query<TokenBody>,
) -> Result<Json<Value>, Error> {
    let customer = token_customer(&state, &query.token).await?;
    Ok(Json(json!({
        "email": masked(&customer.email),
        "accepts_marketing": customer.accepts_marketing,
    })))
}

/// `POST /marketing/unsubscribe {token}`: no more commercial e-mails
async fn unsubscribe(
    State(state): State<AppState>,
    client_ip: Option<Extension<ClientIp>>,
    headers: HeaderMap,
    Json(body): Json<TokenBody>,
) -> Result<Json<Value>, Error> {
    let customer = token_customer(&state, &body.token).await?;
    let ip = ip_of(client_ip);
    record_choice(
        &state,
        customer.id,
        false,
        Source::Unsubscribe,
        None,
        ip,
        &headers,
    )
    .await?;
    Ok(Json(json!({ "accepts_marketing": false })))
}

/// `GET /admin/customers/:id/marketing`: a customer's choice and its history
async fn of_customer(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, Error> {
    let customer = state
        .customer_service
        .find_by_id(id)
        .await?
        .ok_or_else(|| Error::not_found("Customer not found"))?;
    let history = consent::history(state.db.pool(), id).await?;
    Ok(Json(json!({
        "accepts_marketing": customer.accepts_marketing,
        "history": history,
    })))
}

/// Who an export is for: `audience` (retail, trade, all) and `lang` (a language
/// prefix such as `fr` or `nl-be`)
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportQuery {
    #[serde(default)]
    audience: Option<String>,
    #[serde(default)]
    lang: Option<String>,
}

impl ExportQuery {
    fn parse(&self) -> Result<(Audience, Option<String>), Error> {
        let audience = Audience::parse(self.audience.as_deref())?;
        let lang = self
            .lang
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_ascii_lowercase);
        if let Some(l) = &lang {
            if l.len() > 10 || !l.chars().all(|c| c.is_ascii_alphabetic() || c == '-') {
                return Err(Error::validation("lang: a language such as fr or nl-be"));
            }
        }
        Ok((audience, lang))
    }
}

/// `GET /admin/exports/marketing/count`: how many customers the export holds
async fn export_count(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Query(query): Query<ExportQuery>,
) -> Result<Json<Value>, Error> {
    // The same rights as the list itself
    if !staff_can(&state, &auth, Resource::Customers, Action::Read).await {
        return Err(Error::forbidden("The e-mail list needs customers reading"));
    }
    let (audience, lang) = query.parse()?;
    let pool = state.db.pool();
    let n = consent::count(pool, audience, lang.as_deref(), &state.email_locale).await?;
    Ok(Json(json!({ "count": n })))
}

/// `GET /admin/exports/marketing.csv`: the e-mail list (customers who accept
/// commercial e-mails and confirmed their address), each row with its unsubscribe
/// link. Needs `exports` and `customers` reading, and a proof of identity in the
/// last 5 minutes; every export is logged.
async fn export_csv(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    client_ip: Option<Extension<ClientIp>>,
    Query(query): Query<ExportQuery>,
) -> Result<Response, Error> {
    if !auth.is_fresh(FRESH_SECS) {
        let error = json!({ "code": "reauth_required", "message": "Confirm it is you to export" });
        return Ok((StatusCode::FORBIDDEN, Json(json!({ "error": error }))).into_response());
    }
    if !staff_can(&state, &auth, Resource::Customers, Action::Read).await {
        return Err(Error::forbidden("The e-mail list needs customers reading"));
    }
    let (audience, lang) = query.parse()?;
    let pool = state.db.pool();
    let list = consent::recipients(pool, audience, lang.as_deref(), &state.email_locale).await?;
    // The key and the base computed once for the whole list
    let key = unsubscribe_key(&state);
    let base = state.storefront_url.trim_end_matches('/').to_string();
    let csv = consent::recipients_csv(&list, &state.email_locale, |id| {
        let token = consent::unsubscribe_token(&key, id)?;
        Ok(format!("{base}/account/unsubscribe?token={token}"))
    })?;
    let detail = json!({
        "audience": query.audience.as_deref().unwrap_or("all"),
        "lang": lang,
        "rows": list.len(),
    });
    let event = Event::own(
        auth.customer_id,
        events::MARKETING_EXPORT,
        ip_of(client_ip),
        detail,
    );
    // No record, no list
    events::record(pool, &event).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"marketing-list.csv\"".to_string(),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        csv,
    )
        .into_response())
}

/// Public routes: the unsubscribe link
pub fn router() -> Router<AppState> {
    Router::new().route("/marketing/unsubscribe", get(preview).post(unsubscribe))
}

/// Customer routes (JWT)
pub fn protected_router() -> Router<AppState> {
    Router::new().route("/customers/me/marketing", get(mine))
}

/// Staff routes: a customer's history (`customers`), the export (`exports`)
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin/customers/:id/marketing", get(of_customer))
        .route("/admin/exports/marketing/count", get(export_count))
        .route("/admin/exports/marketing.csv", get(export_csv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_the_address() {
        assert_eq!(masked("anne@example.com"), "a***@e***.com");
        assert_eq!(masked("nope"), "***");
    }
}
