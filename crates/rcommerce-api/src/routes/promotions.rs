//! Promotions for staff (`/admin/promotions`, a coupons area): list with
//! filters, sort and paging, create, replace, switch on or off, delete. See
//! `promotion_service` for the kinds, conditions and how they add up.

use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use rcommerce_core::services::promotion_service::{
    NewPromotion, Promotion, PromotionQuery, PromotionService,
};
use rcommerce_core::Error;
use serde::Deserialize;
use uuid::Uuid;

use crate::state::AppState;

fn service(state: &AppState) -> PromotionService {
    PromotionService::new(state.db.pool().clone())
}

async fn list(
    State(state): State<AppState>,
    Query(query): Query<PromotionQuery>,
) -> Result<Json<serde_json::Value>, Error> {
    let (promotions, total) = service(&state).list(&query).await?;
    let per_page = query.per_page.unwrap_or(50).clamp(1, 100);
    Ok(Json(serde_json::json!({
        "promotions": promotions,
        "meta": {
            "page": query.page.unwrap_or(1).max(1),
            "per_page": per_page,
            "total": total,
            "total_pages": (total + per_page - 1) / per_page,
        }
    })))
}

async fn create(
    State(state): State<AppState>,
    Json(new): Json<NewPromotion>,
) -> Result<(axum::http::StatusCode, Json<Promotion>), Error> {
    let promotion = service(&state).save(None, new).await?;
    Ok((axum::http::StatusCode::CREATED, Json(promotion)))
}

async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Promotion>, Error> {
    Ok(Json(service(&state).get(id).await?))
}

async fn replace(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(new): Json<NewPromotion>,
) -> Result<Json<Promotion>, Error> {
    Ok(Json(service(&state).save(Some(id), new).await?))
}

async fn remove(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<axum::http::StatusCode, Error> {
    service(&state).delete(id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct Active {
    active: bool,
}

async fn set_active(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<Active>,
) -> Result<Json<Promotion>, Error> {
    Ok(Json(service(&state).set_active(id, body.active).await?))
}

/// Staff routes (the staff middleware checks the `coupons` resource)
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin/promotions", get(list).post(create))
        .route(
            "/admin/promotions/:id",
            get(get_one).put(replace).delete(remove),
        )
        .route("/admin/promotions/:id/active", post(set_active))
}
