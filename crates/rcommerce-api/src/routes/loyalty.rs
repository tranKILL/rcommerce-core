//! Loyalty points.
//!
//! - `GET /customers/me/loyalty`: the customer's points, their value, lapse
//!   date, latest entries and loyalty card number (`enabled` false when the
//!   programme is off or does not reach them)
//! - `PUT /carts/:id/loyalty` `{"points": 200}`: points to redeem on the
//!   customer's cart (0 to stop); the cart takes what the balance, the
//!   minimum and the amount left to pay allow
//! - staff: `GET|PUT /admin/loyalty/settings` (settings), the points of a
//!   customer `GET /admin/loyalty/customers/:id`, a change with a note
//!   `POST /admin/loyalty/customers/:id/adjust` `{"points": -50, "note": "…"}`,
//!   and the customer of a card `GET /admin/loyalty/cards/:card` (customers)

use axum::{
    extract::{Path, State},
    routing::{get, post, put},
    Extension, Json, Router,
};
use rcommerce_core::services::loyalty_service::{
    self, LoyaltyAccount, LoyaltyService, LoyaltySettings, SettingsUpdate,
};
use rcommerce_core::Error;
use serde::Deserialize;
use uuid::Uuid;

use crate::middleware::JwtAuth;
use crate::state::AppState;

fn service(state: &AppState) -> LoyaltyService {
    LoyaltyService::new(state.db.pool().clone())
}

async fn my_loyalty(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
) -> Result<Json<LoyaltyAccount>, Error> {
    Ok(Json(service(&state).account(auth.customer_id, 20).await?))
}

#[derive(Debug, Deserialize)]
struct Redeem {
    points: i32,
}

async fn redeem_on_cart(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(cart): Path<Uuid>,
    Json(body): Json<Redeem>,
) -> Result<Json<serde_json::Value>, Error> {
    let done = sqlx::query(
        "UPDATE carts SET loyalty_points = $3 WHERE id = $1 AND customer_id = $2 AND NOT converted_to_order",
    )
    .bind(cart)
    .bind(auth.customer_id)
    .bind(body.points.max(0))
    .execute(state.db.pool())
    .await?;
    if done.rows_affected() == 0 {
        return Err(Error::not_found("Cart not found"));
    }
    state.cart_service.refresh_prices(cart).await?;
    let cart = state.cart_service.get_cart_with_items(cart).await?;
    Ok(Json(serde_json::json!({ "cart": cart.cart, "items": cart.items })))
}

async fn settings(State(state): State<AppState>) -> Result<Json<LoyaltySettings>, Error> {
    Ok(Json(service(&state).settings().await?))
}

async fn update_settings(
    State(state): State<AppState>,
    Json(update): Json<SettingsUpdate>,
) -> Result<Json<LoyaltySettings>, Error> {
    Ok(Json(service(&state).update_settings(update).await?))
}

async fn customer_points(
    State(state): State<AppState>,
    Path(customer): Path<Uuid>,
) -> Result<Json<LoyaltyAccount>, Error> {
    Ok(Json(service(&state).account(customer, 50).await?))
}

#[derive(Debug, Deserialize)]
struct Adjust {
    points: i32,
    note: String,
}

async fn adjust(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(customer): Path<Uuid>,
    Json(body): Json<Adjust>,
) -> Result<Json<LoyaltyAccount>, Error> {
    Ok(Json(service(&state).adjust(customer, body.points, &body.note, auth.customer_id).await?))
}

async fn card_owner(State(state): State<AppState>, Path(card): Path<String>) -> Result<Json<serde_json::Value>, Error> {
    let mut conn = state.db.pool().acquire().await?;
    let (id, name, email) = loyalty_service::find_card(&mut conn, &card)
        .await?
        .ok_or_else(|| Error::not_found("No customer has this card"))?;
    let account = service(&state).account(id, 20).await?;
    Ok(Json(serde_json::json!({ "customer_id": id, "name": name, "email": email, "account": account })))
}

/// Customer routes (signed in)
pub fn protected_router() -> Router<AppState> {
    Router::new()
        .route("/customers/me/loyalty", get(my_loyalty))
        .route("/carts/:id/loyalty", put(redeem_on_cart))
}

/// Staff routes (settings: `settings`, the rest: `customers`)
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin/loyalty/settings", get(settings).put(update_settings))
        .route("/admin/loyalty/customers/:id", get(customer_points))
        .route("/admin/loyalty/customers/:id/adjust", post(adjust))
        .route("/admin/loyalty/cards/:card", get(card_owner))
}
