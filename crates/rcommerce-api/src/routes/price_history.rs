//! Price and promotion history for staff (see `pricing_history`):
//!
//! - `GET /admin/products/:id/price-history?customer=&from=&to=&limit=`
//!   (products area): catalogue prices, promotions that target the product,
//!   negotiated rules over time, prices charged;
//! - `GET /admin/customers/:id/price-history?product=&from=&to=&limit=`
//!   (customers area): prices charged to the customer and the negotiated rules
//!   in force for them;
//! - `GET /admin/promotions/:id/uses?page=&per_page=` (coupons area): the
//!   orders a promotion was used on, with what it saved.

use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use rcommerce_core::services::pricing_history::{
    self, CustomerPriceHistory, HistoryQuery, ProductPriceHistory, PromotionUses,
};
use rcommerce_core::Error;
use serde::Deserialize;
use uuid::Uuid;

use crate::state::AppState;

async fn product(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<ProductPriceHistory>, Error> {
    let mut conn = state.db.pool().acquire().await?;
    Ok(Json(
        pricing_history::product_history(&mut conn, id, &query).await?,
    ))
}

async fn customer(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<CustomerPriceHistory>, Error> {
    let mut conn = state.db.pool().acquire().await?;
    Ok(Json(
        pricing_history::customer_history(&mut conn, id, &query).await?,
    ))
}

#[derive(Debug, Deserialize)]
struct Page {
    page: Option<i64>,
    per_page: Option<i64>,
}

async fn promotion(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(page): Query<Page>,
) -> Result<Json<PromotionUses>, Error> {
    let mut conn = state.db.pool().acquire().await?;
    let per_page = page.per_page.unwrap_or(50).clamp(1, 200);
    let uses =
        pricing_history::promotion_uses(&mut conn, id, page.page.unwrap_or(1), per_page).await?;
    Ok(Json(uses))
}

/// Staff routes (the staff middleware checks each area's resource)
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin/products/:id/price-history", get(product))
        .route("/admin/customers/:id/price-history", get(customer))
        .route("/admin/promotions/:id/uses", get(promotion))
}
