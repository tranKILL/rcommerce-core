//! Promotions for staff (`/admin/promotions`, a coupons area): list with
//! filters, sort and paging, create, replace, switch on or off, delete. See
//! `promotion_service` for the kinds, conditions and how they add up.
//! `POST /admin/promotions/preview?exclude=<id>` shows what a draft would do
//! (`promotion_preview`); it saves nothing, so reading promotions is enough.

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

/// `?exclude=` the promotion being edited (left out of the overlaps)
#[derive(Debug, Default, Deserialize)]
pub struct PreviewQuery {
    pub exclude: Option<Uuid>,
}

/// `POST /admin/promotions/preview`: a draft's reach, prices, example carts,
/// overlaps and warnings
async fn preview(
    State(state): State<AppState>,
    Query(query): Query<PreviewQuery>,
    Json(draft): Json<NewPromotion>,
) -> Result<Json<rcommerce_core::services::promotion_preview::Preview>, Error> {
    let preview = rcommerce_core::services::promotion_preview::preview(
        state.db.pool(),
        &draft,
        query.exclude,
    )
    .await?;
    Ok(Json(preview))
}

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
        .route("/admin/promotions/preview", post(preview))
        .route(
            "/admin/promotions/:id",
            get(get_one).put(replace).delete(remove),
        )
        .route("/admin/promotions/:id/active", post(set_active))
}
