//! The till, for staff with the `pos` permission (`pos:read` for GET,
//! `pos:write` otherwise, see `middleware::staff_access`):
//!
//! - `POST /admin/pos/sessions` open a session (branch, opening float)
//! - `GET /admin/pos/sessions/current` the caller's open session, or null
//! - `GET /admin/pos/sessions?location_id=&limit=` sessions, newest first
//! - `GET /admin/pos/sessions/:id` its Z report (totals and movements)
//! - `POST /admin/pos/sessions/:id/cash` put cash in or take it out
//! - `POST /admin/pos/sessions/:id/close` count the drawer and close
//! - `GET /admin/pos/lookup?code=&session_id=` what a scanned code names
//! - `POST /admin/pos/terminal` start a card payment or refund
//! - `GET /admin/pos/terminal/:id`, `POST /admin/pos/terminal/:id/cancel`

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Extension, Json, Router,
};
use rcommerce_core::services::pos::{
    lookup, session::CashMovement, Lookup, TerminalTransaction, TillSession, TillSessions, ZReport,
};
use rcommerce_core::Error;
use rust_decimal::Decimal;
use serde::Deserialize;
use uuid::Uuid;

use crate::middleware::JwtAuth;
use crate::state::AppState;

/// `POST /admin/pos/sessions` body
#[derive(Debug, Deserialize)]
pub struct OpenBody {
    pub location_id: Uuid,
    #[serde(deserialize_with = "number_or_text")]
    pub opening_float: Decimal,
}

/// `POST /admin/pos/sessions/:id/close` body
#[derive(Debug, Deserialize)]
pub struct CloseBody {
    #[serde(deserialize_with = "number_or_text")]
    pub counted_cash: Decimal,
    #[serde(default)]
    pub notes: Option<String>,
}

/// `POST /admin/pos/sessions/:id/cash` body
#[derive(Debug, Deserialize)]
pub struct CashBody {
    pub kind: String,
    #[serde(deserialize_with = "number_or_text")]
    pub amount: Decimal,
    #[serde(default)]
    pub reason: Option<String>,
}

/// `GET /admin/pos/sessions` query
#[derive(Debug, Default, Deserialize)]
pub struct SessionsQuery {
    pub location_id: Option<Uuid>,
    pub limit: Option<i64>,
}

/// `GET /admin/pos/lookup` query
#[derive(Debug, Deserialize)]
pub struct LookupQuery {
    pub code: String,
    /// The till's session: stock is counted at its branch
    pub session_id: Uuid,
}

/// `POST /admin/pos/terminal` body
#[derive(Debug, Deserialize)]
pub struct TerminalBody {
    pub session_id: Uuid,
    #[serde(deserialize_with = "number_or_text")]
    pub amount: Decimal,
    /// A refund instead of a payment
    #[serde(default)]
    pub refund: bool,
}

/// A decimal sent as a JSON number or as text (a decimal comma accepted)
pub(crate) fn number_or_text<'de, D>(deserializer: D) -> Result<Decimal, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Text(String),
        Number(serde_json::Number),
    }
    let text = match Raw::deserialize(deserializer)? {
        Raw::Text(text) => text,
        Raw::Number(number) => number.to_string(),
    };
    text.trim()
        .replace(',', ".")
        .parse::<Decimal>()
        .map_err(serde::de::Error::custom)
}

fn sessions(state: &AppState) -> TillSessions {
    TillSessions::new(state.db.pool().clone())
}

/// POST /admin/pos/sessions
async fn open_session(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(body): Json<OpenBody>,
) -> Result<(StatusCode, Json<TillSession>), Error> {
    let session = sessions(&state)
        .open(auth.customer_id, body.location_id, body.opening_float)
        .await?;
    Ok((StatusCode::CREATED, Json(session)))
}

/// GET /admin/pos/sessions/current
async fn current_session(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
) -> Result<Json<Option<ZReport>>, Error> {
    let till = sessions(&state);
    let report = match till.current(auth.customer_id).await? {
        Some(session) => Some(till.report(session.id).await?),
        None => None,
    };
    Ok(Json(report))
}

/// GET /admin/pos/sessions
async fn list_sessions(
    State(state): State<AppState>,
    Query(query): Query<SessionsQuery>,
) -> Result<Json<serde_json::Value>, Error> {
    let list = sessions(&state)
        .list(query.location_id, query.limit.unwrap_or(50))
        .await?;
    Ok(Json(serde_json::json!({ "sessions": list })))
}

/// GET /admin/pos/sessions/:id
async fn session_report(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ZReport>, Error> {
    Ok(Json(sessions(&state).report(id).await?))
}

/// POST /admin/pos/sessions/:id/cash - only in the caller's own open session
async fn cash(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(id): Path<Uuid>,
    Json(body): Json<CashBody>,
) -> Result<Json<ZReport>, Error> {
    let till = sessions(&state);
    till.open_of(id, auth.customer_id).await?;
    let movement = CashMovement {
        kind: body.kind,
        amount: body.amount,
        reason: body.reason,
    };
    Ok(Json(till.cash(id, auth.customer_id, movement).await?))
}

/// POST /admin/pos/sessions/:id/close - the caller's own open session
async fn close_session(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(id): Path<Uuid>,
    Json(body): Json<CloseBody>,
) -> Result<Json<ZReport>, Error> {
    let report = sessions(&state)
        .close(id, auth.customer_id, body.counted_cash, body.notes)
        .await?;
    Ok(Json(report))
}

/// GET /admin/pos/lookup
async fn lookup_code(
    State(state): State<AppState>,
    Query(query): Query<LookupQuery>,
) -> Result<Json<Lookup>, Error> {
    let session = sessions(&state).get(query.session_id).await?;
    let found = lookup(state.db.pool(), &query.code, session.location_id).await?;
    Ok(Json(found))
}

/// POST /admin/pos/terminal - in the caller's own open session
async fn start_terminal(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(body): Json<TerminalBody>,
) -> Result<(StatusCode, Json<TerminalTransaction>), Error> {
    sessions(&state)
        .open_of(body.session_id, auth.customer_id)
        .await?;
    let transaction = state
        .pos_terminals()
        .start(body.session_id, body.refund, body.amount)
        .await?;
    Ok((StatusCode::CREATED, Json(transaction)))
}

/// GET /admin/pos/terminal/:id
async fn terminal_status(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<TerminalTransaction>, Error> {
    Ok(Json(state.pos_terminals().get(id).await?))
}

/// POST /admin/pos/terminal/:id/cancel
async fn cancel_terminal(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<TerminalTransaction>, Error> {
    Ok(Json(state.pos_terminals().cancel(id).await?))
}

/// Router for the till (behind the admin middleware: `pos` permission)
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin/pos/sessions", get(list_sessions).post(open_session))
        .route("/admin/pos/sessions/current", get(current_session))
        .route("/admin/pos/sessions/:id", get(session_report))
        .route("/admin/pos/sessions/:id/cash", post(cash))
        .route("/admin/pos/sessions/:id/close", post(close_session))
        .route("/admin/pos/lookup", get(lookup_code))
        .route("/admin/pos/terminal", post(start_terminal))
        .route("/admin/pos/terminal/:id", get(terminal_status))
        .route("/admin/pos/terminal/:id/cancel", post(cancel_terminal))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_read_as_numbers_or_text_with_a_comma() {
        let body: CloseBody = serde_json::from_str(r#"{"counted_cash":"152,40"}"#).unwrap();
        assert_eq!(body.counted_cash, Decimal::new(15_240, 2));
        let body: OpenBody = serde_json::from_str(
            r#"{"location_id":"00000000-0000-0000-0000-000000000000","opening_float":100}"#,
        )
        .unwrap();
        assert_eq!(body.opening_float, Decimal::from(100));
    }
}
