//! Content blocks: short storefront texts staff edit per language.
//!
//! - `GET /content/:key?lang=` (public): the block in the language asked for,
//!   else in the store's default language; 404 when neither has one (the
//!   storefront then shows its built-in texts)
//! - `GET /admin/content/:key`: the block in every language
//! - `PUT /admin/content/:key`: `{"languages": {"fr": {"title": "…", "text": "…"}}}`
//!   replaces the languages given; a language with only empty fields loses
//!   its block (staff with `settings:write`)

use std::collections::BTreeMap;

use axum::{
    extract::{Path, Query, State},
    routing::get,
    Extension, Json, Router,
};
use rcommerce_core::services::content_service::{ContentBlock, ContentService, Fields};
use rcommerce_core::Error;
use serde::{Deserialize, Serialize};

use crate::middleware::JwtAuth;
use crate::state::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct LangQuery {
    pub lang: Option<String>,
}

/// A block in every language
#[derive(Debug, Serialize, Deserialize)]
pub struct Languages {
    pub languages: BTreeMap<String, Fields>,
}

fn service(state: &AppState) -> ContentService {
    ContentService::new(state.db.pool().clone())
}

async fn public_block(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(query): Query<LangQuery>,
) -> Result<Json<ContentBlock>, Error> {
    let locales = [
        query.lang.as_deref().unwrap_or(&state.email_locale),
        state.email_locale.as_str(),
    ];
    service(&state)
        .get(&key, &locales)
        .await?
        .map(Json)
        .ok_or_else(|| Error::not_found("No content for this key"))
}

async fn admin_block(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<Languages>, Error> {
    let languages = service(&state).all_languages(&key).await?;
    Ok(Json(Languages { languages }))
}

async fn save_block(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(key): Path<String>,
    Json(body): Json<Languages>,
) -> Result<Json<Languages>, Error> {
    let languages = service(&state)
        .save(&key, &body.languages, auth.customer_id)
        .await?;
    Ok(Json(Languages { languages }))
}

/// Public content
pub fn router() -> Router<AppState> {
    Router::new().route("/content/:key", get(public_block))
}

/// Staff editing (`/admin/content` is a settings area)
pub fn admin_router() -> Router<AppState> {
    Router::new().route("/admin/content/:key", get(admin_block).put(save_block))
}
