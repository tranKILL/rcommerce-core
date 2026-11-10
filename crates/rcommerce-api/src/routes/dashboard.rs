//! The back-office home and the caller's own rights. Any staff member may ask
//! for both. Each part of the dashboard is filled only when one of the
//! caller's roles covers it (global admins see everything), so a counter
//! employee sees the branch's orders and stock but not the turnover.

use axum::{
    extract::{Query, State},
    routing::get,
    Extension, Json, Router,
};
use rcommerce_core::services::dashboard_service::{
    CatalogueFigures, CustomerFigures, InvoiceAlerts, OrderWork, Sales, StockAlerts,
};
use rcommerce_core::{Error, Resource};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::order::{OrderResponse, ORDER_COLUMNS};
use crate::middleware::JwtAuth;
use crate::state::AppState;

/// `GET /admin/dashboard` query
#[derive(Debug, Default, Deserialize)]
pub struct DashboardQuery {
    /// Orders and stock of this branch only
    pub location_id: Option<Uuid>,
}

/// What the caller may do: global admin, or the `<resource>:<action>`
/// permissions of their staff roles
#[derive(Debug, Serialize)]
pub struct StaffMe {
    pub is_admin: bool,
    pub permissions: Vec<String>,
}

/// Open orders by next step, and the first ones to handle
#[derive(Debug, Serialize)]
pub struct OrderSection {
    #[serde(flatten)]
    pub work: OrderWork,
    /// Up to 8 orders to prepare or dispatch, earliest promise first
    pub next: Vec<OrderResponse>,
}

/// The parts of the dashboard the caller may see; the others are null
#[derive(Debug, Default, Serialize)]
pub struct Dashboard {
    pub orders: Option<OrderSection>,
    pub sales: Option<Sales>,
    pub stock: Option<StockAlerts>,
    pub invoices: Option<InvoiceAlerts>,
    pub customers: Option<CustomerFigures>,
    pub catalogue: Option<CatalogueFigures>,
}

/// Whether the caller may read `resource` (write includes read, global
/// admins read everything)
fn sees(auth: &JwtAuth, resource: Resource) -> bool {
    auth.can_read(resource)
}

/// GET /admin/staff/me - the caller's rights, for apps to build their menus
async fn staff_me(Extension(auth): Extension<JwtAuth>) -> Json<StaffMe> {
    let is_admin = auth.is_admin();
    Json(StaffMe {
        is_admin,
        permissions: if is_admin {
            Vec::new()
        } else {
            auth.permissions
        },
    })
}

/// Open orders still to prepare or dispatch, earliest promise first
async fn next_orders(
    state: &AppState,
    location: Option<Uuid>,
) -> Result<Vec<OrderResponse>, Error> {
    let orders: Vec<rcommerce_core::models::Order> = sqlx::query_as(&format!(
        "SELECT {ORDER_COLUMNS} FROM orders \
         WHERE status::text IN ('confirmed', 'processing', 'on_hold') \
           AND metadata->'pickup'->>'ready_marked_at' IS NULL \
           AND fulfillment_status::text <> 'shipped' \
           AND ($1::text IS NULL OR metadata->>'fulfillment_location_id' = $1) \
         ORDER BY COALESCE((metadata->'pickup'->>'ready_at')::timestamptz, created_at), id \
         LIMIT 8"
    ))
    .bind(location.map(|l| l.to_string()))
    .fetch_all(state.db.pool())
    .await?;
    super::order_list::summaries(state, orders).await
}

/// GET /admin/dashboard?location_id= - what needs doing and how the store is
/// going, limited to what the caller's roles cover
async fn dashboard(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Query(query): Query<DashboardQuery>,
) -> Result<Json<Dashboard>, Error> {
    let figures = &state.dashboard;
    let location = query.location_id;
    let mut board = Dashboard::default();
    if sees(&auth, Resource::Orders) {
        board.orders = Some(OrderSection {
            work: figures.order_work(location).await?,
            next: next_orders(&state, location).await?,
        });
        board.invoices = Some(figures.invoice_alerts().await?);
    }
    if sees(&auth, Resource::Reports) {
        board.sales = Some(figures.sales(location).await?);
    }
    if sees(&auth, Resource::Inventory) {
        board.stock = Some(figures.stock_alerts(location).await?);
    }
    if sees(&auth, Resource::Customers) {
        board.customers = Some(figures.customer_figures().await?);
    }
    if sees(&auth, Resource::Products) {
        board.catalogue = Some(figures.catalogue_figures().await?);
    }
    Ok(Json(board))
}

/// Router for the dashboard and the caller's rights (behind the admin
/// middleware, which lets any staff member through for these two paths)
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/staff/me", get(staff_me))
        .route("/admin/dashboard", get(dashboard))
}
