//! Price lists: staff manage negotiated prices for a customer group or a
//! company; a signed-in customer reads their own prices for products on screen.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post},
    Extension, Json, Router,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::middleware::JwtAuth;
use crate::state::AppState;
use rcommerce_core::services::price_list_service::{
    NewPriceList, NewPriceRule, PriceItem, PriceListUpdate,
};
use rcommerce_core::Error;

/// At most this many products and variants per price request
const MAX_ITEMS: usize = 100;

/// Products and variants to price, as comma-separated ids
#[derive(Debug, Deserialize)]
pub struct PricesQuery {
    #[serde(default)]
    pub product_ids: Option<String>,
    #[serde(default)]
    pub variant_ids: Option<String>,
}

/// Filter of the staff list
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub customer_group_id: Option<Uuid>,
    pub company_id: Option<Uuid>,
}

fn ids(list: Option<&str>) -> Result<Vec<Uuid>, Error> {
    list.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Uuid::parse_str(s).map_err(|_| Error::validation(format!("{s} is not an id"))))
        .collect()
}

/// The signed-in customer's prices for products (their own price) and
/// variants, with quantity tiers; catalogue prices when no list applies
pub async fn my_prices(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Query(query): Query<PricesQuery>,
) -> Result<Json<serde_json::Value>, Error> {
    let products = ids(query.product_ids.as_deref())?;
    let variants = ids(query.variant_ids.as_deref())?;
    if products.len() + variants.len() > MAX_ITEMS {
        return Err(Error::validation(
            "at most 100 products and variants at once",
        ));
    }
    let mut conn = state.db.pool().acquire().await?;
    let (product_prices, variant_prices) =
        rcommerce_core::services::price_list_service::catalogue_prices(
            &mut conn, &products, &variants,
        )
        .await?;
    drop(conn);
    let item = |product_id, variant_id, base_price| PriceItem {
        product_id,
        variant_id,
        base_price,
        quantity: 1,
    };
    let mut items: Vec<PriceItem> = products
        .iter()
        .filter_map(|id| product_prices.get(id).map(|price| item(*id, None, *price)))
        .collect();
    items.extend(variants.iter().filter_map(|id| {
        variant_prices
            .get(id)
            .map(|(product, price)| item(*product, Some(*id), *price))
    }));
    let prices = state
        .price_lists
        .prices_for(Some(auth.customer_id), &items)
        .await?;
    Ok(Json(serde_json::json!({ "prices": prices })))
}

/// Price lists, newest first, optionally of one group or company
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Error> {
    let lists = state
        .price_lists
        .list(query.customer_group_id, query.company_id)
        .await?;
    Ok(Json(serde_json::json!({ "price_lists": lists })))
}

/// One price list with its rules
pub async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Error> {
    let (list, rules) = state.price_lists.get(id).await?;
    Ok(Json(
        serde_json::json!({ "price_list": list, "rules": rules }),
    ))
}

/// Create a price list
pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<NewPriceList>,
) -> Result<(StatusCode, Json<serde_json::Value>), Error> {
    let list = state.price_lists.create(body).await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "price_list": list })),
    ))
}

/// Rename, switch on or off, or change the dates of a price list
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<PriceListUpdate>,
) -> Result<Json<serde_json::Value>, Error> {
    let list = state.price_lists.update(id, body).await?;
    Ok(Json(serde_json::json!({ "price_list": list })))
}

/// Delete a price list with its rules
pub async fn remove(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Error> {
    state.price_lists.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Add a rule to a price list
pub async fn add_rule(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<NewPriceRule>,
) -> Result<(StatusCode, Json<serde_json::Value>), Error> {
    let rule = state.price_lists.add_rule(id, body).await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "rule": rule })),
    ))
}

/// Remove a rule from a price list
pub async fn remove_rule(
    State(state): State<AppState>,
    Path((id, rule)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, Error> {
    state.price_lists.remove_rule(id, rule).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Customer route (JWT required)
pub fn protected_router() -> Router<AppState> {
    Router::new().route("/customers/me/prices", get(my_prices))
}

/// Staff routes, guarded as product data (`products:read` / `products:write`)
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin/price-lists", get(list).post(create))
        .route(
            "/admin/price-lists/:id",
            get(get_one).put(update).delete(remove),
        )
        .route("/admin/price-lists/:id/rules", post(add_rule))
        .route("/admin/price-lists/:id/rules/:rule_id", delete(remove_rule))
}
