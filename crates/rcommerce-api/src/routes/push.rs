//! The mobile apps' devices for push notifications (`/customers/me/push-devices`)

use axum::{extract::State, http::StatusCode, routing::post, Extension, Json, Router};
use rcommerce_core::push::{self, NewDevice, PushDevice};
use rcommerce_core::Error;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::middleware::JwtAuth;
use crate::state::AppState;

/// `POST /customers/me/push-devices {platform, token}`: this app installation
/// receives the customer's notifications (201)
async fn register(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(device): Json<NewDevice>,
) -> Result<(StatusCode, Json<PushDevice>), Error> {
    let saved = push::register(state.db.pool(), auth.customer_id, &device).await?;
    Ok((StatusCode::CREATED, Json(saved)))
}

/// A device's token
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenBody {
    token: String,
}

/// `DELETE /customers/me/push-devices {token}`: no more notifications on this app
/// installation (signing out); 204, 404 when it is not the customer's
async fn unregister(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(body): Json<TokenBody>,
) -> Result<StatusCode, Error> {
    if push::unregister(state.db.pool(), auth.customer_id, &body.token).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::not_found("Device not found"))
    }
}

/// `GET /customers/me/push-devices`: the customer's devices (no tokens), and whether
/// the store sends push notifications at all
async fn list(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
) -> Result<Json<Value>, Error> {
    let devices = push::devices(state.db.pool(), auth.customer_id).await?;
    Ok(Json(json!({
        "enabled": state.push.is_some(),
        "devices": devices,
    })))
}

/// Customer routes (JWT)
pub fn protected_router() -> Router<AppState> {
    Router::new().route(
        "/customers/me/push-devices",
        post(register).delete(unregister).get(list),
    )
}
