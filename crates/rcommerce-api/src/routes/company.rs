//! Company accounts: a customer opens one with a VAT number and adds buyers;
//! staff list companies, suspend them, or move them to another customer group.

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
use rcommerce_core::services::company_service::{Company, CompanyQuery, CompanyUpdate};
use rcommerce_core::Error;

/// Body of a company sign-up
#[derive(Debug, Deserialize)]
pub struct SignUpRequest {
    pub vat_number: String,
    /// Legal name; default: the name in the VAT register
    #[serde(default)]
    pub name: Option<String>,
}

/// Body of a member addition
#[derive(Debug, Deserialize)]
pub struct AddMemberRequest {
    pub email: String,
}

/// Body of a customer group creation
#[derive(Debug, Deserialize)]
pub struct GroupRequest {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub prices_exclude_tax: bool,
}

/// A company with the caller's role and, for its owner, its members
async fn company_json(
    state: &AppState,
    company: &Company,
    role: &str,
) -> Result<serde_json::Value, Error> {
    let members = if role == "owner" {
        serde_json::to_value(state.companies.members(company.id).await?)?
    } else {
        serde_json::Value::Null
    };
    Ok(serde_json::json!({ "company": company, "role": role, "members": members }))
}

/// Open a company account; the caller becomes its owner
pub async fn sign_up(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(body): Json<SignUpRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), Error> {
    let company = state
        .companies
        .sign_up(auth.customer_id, &body.vat_number, body.name.as_deref())
        .await?;
    let json = company_json(&state, &company, "owner").await?;
    Ok((StatusCode::CREATED, Json(json)))
}

/// The caller's company, their role, and (for the owner) its members
pub async fn my_company(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
) -> Result<Json<serde_json::Value>, Error> {
    let (company, role) = state
        .companies
        .company_of(auth.customer_id)
        .await?
        .ok_or_else(|| Error::not_found("You do not belong to a company account"))?;
    Ok(Json(company_json(&state, &company, &role).await?))
}

/// Add an existing customer to the caller's company as a buyer (owner only)
pub async fn add_member(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(body): Json<AddMemberRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), Error> {
    let member = state
        .companies
        .add_member(auth.customer_id, &body.email)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "member": member })),
    ))
}

/// Remove a buyer from the caller's company (owner only)
pub async fn remove_member(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Path(customer_id): Path<Uuid>,
) -> Result<StatusCode, Error> {
    state
        .companies
        .remove_member(auth.customer_id, customer_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Companies for staff, newest first, with paging
pub async fn list_companies(
    State(state): State<AppState>,
    Query(query): Query<CompanyQuery>,
) -> Result<Json<serde_json::Value>, Error> {
    let (companies, total) = state.companies.list(&query).await?;
    let per_page = query.per_page.unwrap_or(50).clamp(1, 100);
    Ok(Json(serde_json::json!({
        "companies": companies,
        "meta": {
            "total": total,
            "page": query.page.unwrap_or(1).max(1),
            "per_page": per_page,
            "total_pages": (total + per_page - 1) / per_page,
        },
    })))
}

/// One company with its members, for staff
pub async fn get_company(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Error> {
    let company = state
        .companies
        .get(id)
        .await?
        .ok_or_else(|| Error::not_found("Company not found"))?;
    let members = state.companies.members(id).await?;
    Ok(Json(
        serde_json::json!({ "company": company, "members": members }),
    ))
}

/// Change a company's name, status or customer group
pub async fn update_company(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(update): Json<CompanyUpdate>,
) -> Result<Json<serde_json::Value>, Error> {
    let company = state.companies.update(id, update).await?;
    Ok(Json(serde_json::json!({ "company": company })))
}

/// Every customer group
pub async fn list_groups(State(state): State<AppState>) -> Result<Json<serde_json::Value>, Error> {
    let groups = state.companies.groups().await?;
    Ok(Json(serde_json::json!({ "customer_groups": groups })))
}

/// Create a customer group
pub async fn create_group(
    State(state): State<AppState>,
    Json(body): Json<GroupRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), Error> {
    let group = state
        .companies
        .create_group(&body.code, &body.name, body.prices_exclude_tax)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "customer_group": group })),
    ))
}

/// Customer routes (JWT required)
pub fn protected_router() -> Router<AppState> {
    Router::new()
        .route("/companies", post(sign_up))
        .route("/companies/me", get(my_company))
        .route("/companies/me/members", post(add_member))
        .route("/companies/me/members/:customer_id", delete(remove_member))
}

/// Staff routes, guarded as customer data (`customers:read` / `customers:write`)
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin/companies", get(list_companies))
        .route("/admin/companies/:id", get(get_company).put(update_company))
        .route(
            "/admin/customer-groups",
            get(list_groups).post(create_group),
        )
}
