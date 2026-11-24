//! Content blocks: short storefront texts staff edit per language.
//!
//! - `GET /content/:key?lang=` (public): the block in the language asked for,
//!   else in the store's default language, else the one shared by every
//!   language (`und`); 404 when none has one (the storefront then shows its
//!   built-in texts)
//! - `GET /content?keys=footer,site_meta&lang=` (public): up to 10 blocks in
//!   one call, each found the same way; keys without a block are left out
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
use rcommerce_core::services::content_rules::ALL_LANGUAGES;
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
    service(&state)
        .get(&key, &locales(&state, query.lang.as_deref()))
        .await?
        .map(Json)
        .ok_or_else(|| Error::not_found("No content for this key"))
}

/// Where a block is looked for: the language asked for, the store's default
/// language, then the blocks shared by every language
fn locales<'a>(state: &'a AppState, lang: Option<&'a str>) -> [&'a str; 3] {
    [
        lang.unwrap_or(&state.email_locale),
        state.email_locale.as_str(),
        ALL_LANGUAGES,
    ]
}

/// Most blocks one `GET /content` asks for
const MAX_KEYS: usize = 10;

/// `?keys=footer,site_meta&lang=fr`
#[derive(Debug, Default, Deserialize)]
pub struct BlocksQuery {
    pub keys: String,
    pub lang: Option<String>,
}

/// Several blocks, by key
#[derive(Debug, Serialize)]
pub struct Blocks {
    pub blocks: BTreeMap<String, ContentBlock>,
}

async fn public_blocks(
    State(state): State<AppState>,
    Query(query): Query<BlocksQuery>,
) -> Result<Json<Blocks>, Error> {
    let mut keys: Vec<String> = query
        .keys
        .split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .collect();
    keys.sort();
    keys.dedup();
    if keys.is_empty() || keys.len() > MAX_KEYS {
        return Err(Error::validation(format!(
            "keys lists 1 to {MAX_KEYS} block names"
        )));
    }
    let blocks = service(&state)
        .get_many(&keys, &locales(&state, query.lang.as_deref()))
        .await?;
    Ok(Json(Blocks { blocks }))
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
    Router::new()
        .route("/content", get(public_blocks))
        .route("/content/:key", get(public_block))
}

/// Staff editing (`/admin/content` is a settings area)
pub fn admin_router() -> Router<AppState> {
    Router::new().route("/admin/content/:key", get(admin_block).put(save_block))
}
