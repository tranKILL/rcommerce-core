//! News and building guides, their categories, and uploaded pictures.
//!
//! Public:
//! - `GET /articles?kind=news|wiki&category=<slug>&lang=&page=&per_page=`:
//!   published articles shoppers see now, newest first (`data`, `meta`)
//! - `GET /articles/:slug?lang=`: one article with its body and products
//! - `GET /article-categories?kind=&lang=`: categories with their names
//! - `GET /products/:id/articles?lang=`: the articles linked to a product
//! - `GET /media/:file`: an uploaded picture (`<id>.jpg` or `<id>.png`)
//!
//! Staff (`content`): `GET|POST /admin/articles`, `GET|PUT|DELETE
//! /admin/articles/:id`, `GET|POST /admin/article-categories`, `PUT|DELETE
//! /admin/article-categories/:id`, `GET|POST /admin/media` (the body is the
//! picture; `?name=` labels it), `DELETE /admin/media/:id`.

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Extension, Json, Router,
};
use rcommerce_core::common::locale::normalize_locale;
use rcommerce_core::services::articles::{ArticleInput, Articles, Category, ListQuery};
use rcommerce_core::services::media::{self, MediaLibrary};
use rcommerce_core::Error;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tracing::info;
use uuid::Uuid;

use crate::middleware::JwtAuth;
use crate::state::AppState;

mod categories;

/// Most articles on one page
const MAX_PER_PAGE: i64 = 50;
/// Highest page number a list accepts
const MAX_PAGE: i64 = 10_000;
/// Uploads decoded at the same time (a decode can take a few hundred MB)
static DECODES: Semaphore = Semaphore::const_new(2);

/// A page number between 1 and `MAX_PAGE`
fn page_of(page: Option<i64>) -> i64 {
    page.unwrap_or(1).clamp(1, MAX_PAGE)
}

fn articles(state: &AppState) -> Articles {
    Articles::new(state.db.pool().clone())
}

fn library(state: &AppState) -> MediaLibrary {
    MediaLibrary::new(state.db.pool().clone())
}

/// The language asked for (normalised), else the store's
fn locales(state: &AppState, lang: Option<&str>) -> (String, String) {
    let fallback = state.email_locale.clone();
    let locale = lang
        .and_then(normalize_locale)
        .unwrap_or_else(|| fallback.clone());
    (locale, fallback)
}

#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    pub kind: Option<String>,
    pub category: Option<String>,
    pub lang: Option<String>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

fn page_meta(total: i64, page: i64, per_page: i64) -> serde_json::Value {
    serde_json::json!({
        "total": total,
        "page": page,
        "per_page": per_page,
        "total_pages": (total + per_page - 1) / per_page,
    })
}

/// `GET /articles`
async fn list_articles(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> Result<Json<serde_json::Value>, Error> {
    if let Some(kind) = params.kind.as_deref() {
        if !matches!(kind, "news" | "wiki") {
            return Err(Error::validation("kind must be news or wiki"));
        }
    }
    let query = ListQuery {
        kind: params.kind,
        category: params.category,
        page: page_of(params.page),
        per_page: params.per_page.unwrap_or(12).clamp(1, MAX_PER_PAGE),
    };
    let (locale, fallback) = locales(&state, params.lang.as_deref());
    let (rows, total) = articles(&state).list(&query, &locale, &fallback).await?;
    Ok(Json(serde_json::json!({
        "data": rows,
        "meta": page_meta(total, query.page, query.per_page),
    })))
}

#[derive(Debug, Default, Deserialize)]
pub struct LangParams {
    pub lang: Option<String>,
}

/// `GET /articles/:slug`
async fn get_article(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(params): Query<LangParams>,
) -> Result<Json<serde_json::Value>, Error> {
    let (locale, fallback) = locales(&state, params.lang.as_deref());
    let article = articles(&state)
        .get(&slug, &locale, &fallback)
        .await?
        .ok_or_else(|| Error::not_found("Article not found"))?;
    Ok(Json(
        serde_json::to_value(article).map_err(|e| Error::internal(e.to_string()))?,
    ))
}

/// `GET /products/:id/articles`
async fn product_articles(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<LangParams>,
) -> Result<Json<serde_json::Value>, Error> {
    let (locale, fallback) = locales(&state, params.lang.as_deref());
    let rows = articles(&state).for_product(id, &locale, &fallback).await?;
    Ok(Json(serde_json::json!({ "data": rows })))
}

/// `GET /media/:file`: pictures never change (a new upload is a new id);
/// browsers keep them 30 days, so a deleted one goes from caches in that time
async fn media_file(
    State(state): State<AppState>,
    Path(file): Path<String>,
) -> Result<Response, Error> {
    let (id, content_type) =
        media::parse_file_name(&file).ok_or_else(|| Error::not_found("No such picture"))?;
    let bytes = library(&state)
        .bytes(id, content_type)
        .await?
        .ok_or_else(|| Error::not_found("No such picture"))?;
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=2592000"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CONTENT_SECURITY_POLICY, "default-src 'none'"),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Debug, Default, Deserialize)]
pub struct AdminListParams {
    pub kind: Option<String>,
    pub q: Option<String>,
    pub lang: Option<String>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

/// `GET /admin/articles`: drafts included
async fn admin_list(
    State(state): State<AppState>,
    Query(params): Query<AdminListParams>,
) -> Result<Json<serde_json::Value>, Error> {
    let page = page_of(params.page);
    let per_page = params.per_page.unwrap_or(25).clamp(1, MAX_PER_PAGE);
    let (locale, _) = locales(&state, params.lang.as_deref());
    let (rows, total) = articles(&state)
        .admin_list(
            params.kind.as_deref(),
            params.q.as_deref(),
            &locale,
            per_page,
            (page - 1) * per_page,
        )
        .await?;
    Ok(Json(serde_json::json!({
        "data": rows,
        "meta": page_meta(total, page, per_page),
    })))
}

async fn admin_view(state: &AppState, id: Uuid) -> Result<Json<serde_json::Value>, Error> {
    let article = articles(state)
        .admin_get(id)
        .await?
        .ok_or_else(|| Error::not_found("Article not found"))?;
    Ok(Json(
        serde_json::to_value(article).map_err(|e| Error::internal(e.to_string()))?,
    ))
}

/// `POST /admin/articles`
async fn create_article(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(input): Json<ArticleInput>,
) -> Result<(StatusCode, Json<serde_json::Value>), Error> {
    let id = articles(&state).create(&input, auth.customer_id).await?;
    info!(target: "audit", actor = %auth.customer_id, article = %id, "article created");
    Ok((StatusCode::CREATED, admin_view(&state, id).await?))
}

/// `GET /admin/articles/:id`
async fn admin_get(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Error> {
    admin_view(&state, id).await
}

/// `PUT /admin/articles/:id`
async fn update_article(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(id): Path<Uuid>,
    Json(input): Json<ArticleInput>,
) -> Result<Json<serde_json::Value>, Error> {
    if !articles(&state).update(id, &input).await? {
        return Err(Error::not_found("Article not found"));
    }
    info!(target: "audit", actor = %auth.customer_id, article = %id, "article changed");
    admin_view(&state, id).await
}

/// `DELETE /admin/articles/:id`
async fn delete_article(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Error> {
    if articles(&state).remove(id).await? {
        info!(target: "audit", actor = %auth.customer_id, article = %id, "article deleted");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::not_found("Article not found"))
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct UploadParams {
    pub name: Option<String>,
}

/// `POST /admin/media`: the body is the picture
async fn upload(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Query(params): Query<UploadParams>,
    body: Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), Error> {
    // Decoding is CPU and memory work: off the request threads, two at a time
    let _permit = DECODES
        .acquire()
        .await
        .map_err(|e| Error::internal(e.to_string()))?;
    let picture = tokio::task::spawn_blocking(move || media::process(&body))
        .await
        .map_err(|e| Error::internal(e.to_string()))??;
    let info = library(&state)
        .store(&picture, params.name.as_deref(), auth.customer_id)
        .await?;
    info!(target: "audit", actor = %auth.customer_id, media = %info.id, "picture uploaded");
    let mut value = serde_json::to_value(&info).map_err(|e| Error::internal(e.to_string()))?;
    value["url"] = serde_json::json!(info.url());
    Ok((StatusCode::CREATED, Json(value)))
}

#[derive(Debug, Default, Deserialize)]
pub struct PageParams {
    pub page: Option<i64>,
}

/// `GET /admin/media`: the newest pictures, 48 a page
async fn media_list(
    State(state): State<AppState>,
    Query(params): Query<PageParams>,
) -> Result<Json<serde_json::Value>, Error> {
    let page = page_of(params.page);
    let rows = library(&state).list(48, (page - 1) * 48).await?;
    let data: Vec<serde_json::Value> = rows
        .iter()
        .map(|info| {
            let mut v = serde_json::to_value(info).unwrap_or_default();
            v["url"] = serde_json::json!(info.url());
            v
        })
        .collect();
    Ok(Json(serde_json::json!({ "data": data })))
}

/// `DELETE /admin/media/:id`
async fn media_remove(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Error> {
    if library(&state).remove(id).await? {
        info!(target: "audit", actor = %auth.customer_id, media = %id, "picture deleted");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::not_found("No such picture"))
    }
}

/// Public routes
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/articles", get(list_articles))
        .route("/articles/:slug", get(get_article))
        .route("/article-categories", get(categories::public_list))
        .route("/products/:id/articles", get(product_articles))
        .route("/media/:file", get(media_file))
}

/// Staff routes (`content`)
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin/articles", get(admin_list).post(create_article))
        .route(
            "/admin/articles/:id",
            get(admin_get).put(update_article).delete(delete_article),
        )
        .route(
            "/admin/article-categories",
            get(categories::admin_list).post(categories::create),
        )
        .route(
            "/admin/article-categories/:id",
            axum::routing::put(categories::update).delete(categories::remove),
        )
        .route(
            "/admin/media",
            get(media_list)
                .post(upload)
                .layer(DefaultBodyLimit::max(media::MAX_UPLOAD_BYTES + 64 * 1024)),
        )
        .route("/admin/media/:id", axum::routing::delete(media_remove))
}

/// Checked category input (kind, slug, names)
fn check_category(category: &Category) -> Result<(), Error> {
    if !matches!(category.kind.as_str(), "news" | "wiki") {
        return Err(Error::validation("kind must be news or wiki"));
    }
    let slug_ok = (1..=100).contains(&category.slug.len())
        && category.slug.split('-').all(|w| {
            !w.is_empty()
                && w.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        });
    if !slug_ok {
        return Err(Error::validation(
            "slug must be lower-case letters and digits joined by hyphens",
        ));
    }
    let named = category.names.iter().any(|(_, n)| !n.trim().is_empty());
    if !named || category.names.values().any(|n| n.chars().count() > 100) {
        return Err(Error::validation(
            "a category needs a name (at most 100 characters) in one language",
        ));
    }
    Ok(())
}
