//! Product documents and the link checker.
//!
//! - `GET /products/:id/links?lang=` (public): the product's documents and
//!   pages, in the language asked for first; broken links left out
//! - `GET|PUT /admin/products/:id/links` (`products`): every link, or replace
//!   them all (`{"links": [{kind, url, locale, label}]}`)
//! - `GET /admin/links?state=broken|moved|uncertain` (`products`): links to
//!   look at, with where each is shown
//! - `POST /admin/links/recheck` `{"url": "…"}`: check one address now
//! - `POST /admin/links/replace` `{"old": "…", "new": "https://…"}`: replace
//!   an address in product links, and in article bodies (with `content`
//!   write) and footers (with `settings` write)

use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Extension, Json, Router,
};
use rcommerce_core::common::locale::normalize_locale;
use rcommerce_core::middleware::rate_limit::RateLimitError;
use rcommerce_core::services::link_check::{self, LinkChecks, Parts, Policy};
use rcommerce_core::services::product_links::{LinkInput, ProductLinks};
use rcommerce_core::{Action, Error, Resource};
use serde::Deserialize;
use tracing::info;
use uuid::Uuid;

use crate::middleware::{staff_access::staff_can, JwtAuth};
use crate::state::AppState;

fn links(state: &AppState) -> ProductLinks {
    ProductLinks::new(state.db.pool().clone())
}

fn checks(state: &AppState) -> LinkChecks {
    LinkChecks::new(state.db.pool().clone())
}

#[derive(Debug, Default, Deserialize)]
pub struct LangParams {
    pub lang: Option<String>,
}

/// `GET /products/:id/links`
async fn public_links(
    State(state): State<AppState>,
    Path(product): Path<Uuid>,
    Query(params): Query<LangParams>,
) -> Result<Json<serde_json::Value>, Error> {
    let locale = params
        .lang
        .as_deref()
        .and_then(normalize_locale)
        .unwrap_or_else(|| state.email_locale.clone());
    let data = links(&state).public(product, &locale).await?;
    Ok(Json(serde_json::json!({ "data": data })))
}

/// `GET /admin/products/:id/links`
async fn admin_links(
    State(state): State<AppState>,
    Path(product): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Error> {
    let data = links(&state).list(product).await?;
    Ok(Json(serde_json::json!({ "links": data })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinksBody {
    pub links: Vec<LinkInput>,
}

/// `PUT /admin/products/:id/links`
async fn save_links(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(product): Path<Uuid>,
    Json(body): Json<LinksBody>,
) -> Result<Json<serde_json::Value>, Error> {
    let saved = links(&state)
        .replace(product, &body.links)
        .await?
        .ok_or_else(|| Error::not_found("Product not found"))?;
    info!(target: "audit", actor = %auth.customer_id, %product, links = saved.len(), "product links replaced");
    Ok(Json(serde_json::json!({ "links": saved })))
}

#[derive(Debug, Default, Deserialize)]
pub struct StateParams {
    pub state: Option<String>,
}

/// `GET /admin/links`
async fn report(
    State(state): State<AppState>,
    Query(params): Query<StateParams>,
) -> Result<Json<serde_json::Value>, Error> {
    let wanted = params.state.as_deref().unwrap_or("broken");
    let data = checks(&state).report(wanted).await?;
    Ok(Json(serde_json::json!({ "data": data })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecheckBody {
    pub url: String,
}

/// Seconds between two rechecks by the same staff member
const RECHECK_GAP: u64 = 10;

/// When each staff member last asked for a recheck
static RECHECKS: std::sync::Mutex<Option<std::collections::HashMap<Uuid, std::time::Instant>>> =
    std::sync::Mutex::new(None);

/// One recheck per staff member every `RECHECK_GAP` seconds
fn may_recheck(staff: Uuid) -> bool {
    let Ok(mut last) = RECHECKS.lock() else {
        return false;
    };
    let last = last.get_or_insert_with(std::collections::HashMap::new);
    let now = std::time::Instant::now();
    last.retain(|_, at| now.duration_since(*at).as_secs() < RECHECK_GAP);
    if last.contains_key(&staff) {
        return false;
    }
    last.insert(staff, now);
    true
}

/// `POST /admin/links/recheck`: only addresses the store shows, one a staff
/// member every 10 seconds (429 otherwise)
async fn recheck(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(body): Json<RecheckBody>,
) -> Result<Json<serde_json::Value>, Error> {
    let known: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM link_checks WHERE url = $1)")
            .bind(&body.url)
            .fetch_one(state.db.pool())
            .await?;
    if !known {
        return Err(Error::not_found("The store does not show this address"));
    }
    if !may_recheck(auth.customer_id) {
        return Err(Error::RateLimit(RateLimitError::RateLimited {
            retry_after: RECHECK_GAP,
        }));
    }
    let outcome = link_check::check(&body.url, Policy::default()).await;
    let store = checks(&state);
    store.record(&body.url, &outcome).await?;
    let row: Option<link_check::CheckedLink> = sqlx::query_as(
        "SELECT url, state, status, error, final_url, consecutive_failures, failing_since, \
           last_checked_at FROM link_checks WHERE url = $1",
    )
    .bind(&body.url)
    .fetch_optional(state.db.pool())
    .await?;
    let mut row = row.ok_or_else(|| Error::not_found("The store does not show this address"))?;
    row.used_in = store.used_in(&body.url).await?;
    Ok(Json(
        serde_json::to_value(row).map_err(|e| Error::internal(e.to_string()))?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceBody {
    pub old: String,
    pub new: String,
}

/// `POST /admin/links/replace`
async fn replace(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(body): Json<ReplaceBody>,
) -> Result<Json<serde_json::Value>, Error> {
    // article bodies and footers belong to other staff rights
    let parts = Parts {
        articles: staff_can(&state, &auth, Resource::Content, Action::Write).await,
        footers: staff_can(&state, &auth, Resource::Settings, Action::Write).await,
    };
    let changed = checks(&state).replace(&body.old, &body.new, parts).await?;
    info!(target: "audit", actor = %auth.customer_id, old = %body.old, new = %body.new, changed,
          "outside link replaced");
    Ok(Json(serde_json::json!({ "changed": changed })))
}

/// Public routes
pub fn router() -> Router<AppState> {
    Router::new().route("/products/:id/links", get(public_links))
}

/// Staff routes (`products`)
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route(
            "/admin/products/:id/links",
            get(admin_links).put(save_links),
        )
        .route("/admin/links", get(report))
        .route("/admin/links/recheck", post(recheck))
        .route("/admin/links/replace", post(replace))
}
