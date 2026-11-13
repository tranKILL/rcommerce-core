//! The back-office home, the caller's own rights and their dashboard layout.
//! Any staff member may call these. The dashboard computes only the widgets
//! offered to the caller: those their roles' permissions read (global admins
//! get every widget), narrowed by the roles' widget lists. A counter employee
//! therefore sees the branch's orders and stock but not the turnover.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::get,
    Extension, Json, Router,
};
use rcommerce_core::services::analytics::{
    finance::FinanceReport,
    marketing::{MarketingParts, MarketingReport},
    ReportFilter,
};
use rcommerce_core::services::dashboard_service::{
    CatalogueFigures, CustomerFigures, InvoiceAlerts, OrderWork, Sales, StockAlerts,
};
use rcommerce_core::services::dashboard_widgets::{self, LayoutEntry, LayoutSlot};
use rcommerce_core::services::StaffView;
use rcommerce_core::{Error, StaffService};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::order::{OrderResponse, ORDER_COLUMNS};
use crate::middleware::JwtAuth;
use crate::state::AppState;

/// `GET /admin/dashboard` query
#[derive(Debug, Default, Deserialize)]
pub struct DashboardQuery {
    /// Orders, stock and reports of this branch only
    pub location_id: Option<Uuid>,
    /// Widgets to compute, comma-separated (default: the usual home parts:
    /// orders, stock, invoices, sales, customers, catalogue). Widgets not
    /// offered to the caller, or unknown, are left out.
    pub widgets: Option<String>,
}

/// What the caller may do (global admin, or the `<resource>:<action>`
/// permissions of their staff roles) and what they are shown
#[derive(Debug, Serialize)]
pub struct StaffMe {
    pub is_admin: bool,
    pub permissions: Vec<String>,
    /// Back-office menu entries to show; null: every entry the permissions
    /// allow
    pub menu: Option<Vec<String>>,
    /// Dashboard widgets offered to the caller, in their default order
    pub widgets: Vec<String>,
    /// The largest manual discount they may give at the till, in percent
    pub max_discount_percent: Decimal,
}

/// Open orders by next step, and the first ones to handle
#[derive(Debug, Serialize)]
pub struct OrderSection {
    #[serde(flatten)]
    pub work: OrderWork,
    /// Up to 8 orders to prepare or dispatch, earliest promise first
    pub next: Vec<OrderResponse>,
}

/// The widgets computed for the caller; the others are null
#[derive(Debug, Default, Serialize)]
pub struct Dashboard {
    pub orders: Option<OrderSection>,
    pub sales: Option<Sales>,
    pub stock: Option<StockAlerts>,
    pub invoices: Option<InvoiceAlerts>,
    pub customers: Option<CustomerFigures>,
    pub catalogue: Option<CatalogueFigures>,
    /// The finance report of the last 12 months with 6 projected ones (the
    /// `finance` and `projection` widgets)
    pub finance: Option<FinanceReport>,
    /// The marketing sections the widgets need (`top_customers`,
    /// `seasonality`, `restock`, `promotion_results`, `loyalty`); the others
    /// are empty
    pub marketing: Option<MarketingReport>,
}

/// A staff member's dashboard layout
#[derive(Debug, Serialize)]
pub struct LayoutResponse {
    /// Every widget offered to them, in order, with its size, whether it is
    /// hidden and the sizes it can take
    pub layout: Vec<LayoutEntry>,
    /// Whether they arranged it themselves (false: their roles' default)
    pub customised: bool,
}

/// A layout to save
#[derive(Debug, Deserialize)]
pub struct LayoutBody {
    pub layout: Vec<LayoutSlot>,
}

/// The usual home parts, computed when no widget list is given
const USUAL: [&str; 6] = [
    "orders",
    "stock",
    "invoices",
    "sales",
    "customers",
    "catalogue",
];

/// What the caller is shown (menu, widgets, discount cap)
async fn view(state: &AppState, auth: &JwtAuth) -> Result<StaffView, Error> {
    StaffService::new(state.db.clone())
        .view(auth.customer_id, auth.is_admin())
        .await
}

/// GET /admin/staff/me - the caller's rights and display, for apps to build
/// their menus
async fn staff_me(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
) -> Result<Json<StaffMe>, Error> {
    let shown = view(&state, &auth).await?;
    let is_admin = auth.is_admin();
    Ok(Json(StaffMe {
        is_admin,
        permissions: if is_admin {
            Vec::new()
        } else {
            auth.permissions
        },
        menu: shown.menu,
        widgets: shown.widgets.into_iter().map(|w| w.widget).collect(),
        max_discount_percent: shown.max_discount_percent,
    }))
}

/// GET /admin/staff/me/dashboard - the caller's layout (their roles' default
/// until they arrange it)
async fn my_dashboard(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
) -> Result<Json<LayoutResponse>, Error> {
    let offered = view(&state, &auth).await?.widgets;
    let saved = StaffService::new(state.db.clone())
        .saved_layout(auth.customer_id)
        .await?;
    Ok(Json(LayoutResponse {
        layout: dashboard_widgets::with_sizes(dashboard_widgets::resolve(
            saved.as_deref(),
            &offered,
        )),
        customised: saved.is_some(),
    }))
}

/// PUT /admin/staff/me/dashboard - save the caller's layout: widgets offered
/// to them, each once, at a size it can take (400 otherwise). Offered widgets
/// left out are added at the end as their default puts them.
async fn save_my_dashboard(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Json(body): Json<LayoutBody>,
) -> Result<Json<LayoutResponse>, Error> {
    let offered = view(&state, &auth).await?.widgets;
    let layout = StaffService::new(state.db.clone())
        .save_layout(auth.customer_id, &body.layout, &offered)
        .await?;
    Ok(Json(LayoutResponse {
        layout: dashboard_widgets::with_sizes(layout),
        customised: true,
    }))
}

/// DELETE /admin/staff/me/dashboard - back to the roles' default layout
async fn reset_my_dashboard(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
) -> Result<StatusCode, Error> {
    StaffService::new(state.db.clone())
        .reset_layout(auth.customer_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
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

/// The widgets to compute: those asked for (default: the usual home parts)
/// that are offered to the caller
fn wanted_widgets(asked: Option<&str>, offered: &[LayoutSlot]) -> Vec<String> {
    let asked: Vec<&str> = match asked {
        Some(list) => list
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .collect(),
        None => USUAL.to_vec(),
    };
    asked
        .into_iter()
        .filter(|id| offered.iter().any(|o| o.widget == *id))
        .map(String::from)
        .collect()
}

/// GET /admin/dashboard?location_id=&widgets= - what needs doing and how the
/// store is going, limited to the widgets offered to the caller
async fn dashboard(
    State(state): State<AppState>,
    Extension(auth): Extension<JwtAuth>,
    Query(query): Query<DashboardQuery>,
) -> Result<Json<Dashboard>, Error> {
    let offered = view(&state, &auth).await?.widgets;
    let wanted = wanted_widgets(query.widgets.as_deref(), &offered);
    let wants = |id: &str| wanted.iter().any(|w| w == id);
    let figures = &state.dashboard;
    let location = query.location_id;
    let mut board = Dashboard::default();
    if wants("orders") {
        board.orders = Some(OrderSection {
            work: figures.order_work(location).await?,
            next: next_orders(&state, location).await?,
        });
    }
    if wants("invoices") {
        board.invoices = Some(figures.invoice_alerts().await?);
    }
    if wants("sales") {
        board.sales = Some(figures.sales(location).await?);
    }
    if wants("stock") {
        board.stock = Some(figures.stock_alerts(location).await?);
    }
    if wants("customers") {
        board.customers = Some(figures.customer_figures().await?);
    }
    if wants("catalogue") {
        board.catalogue = Some(figures.catalogue_figures().await?);
    }
    add_analytics(&state, location, &wanted, &mut board).await?;
    Ok(Json(board))
}

/// The reports the analytics widgets need: the finance report once for
/// `finance` and `projection`, and only the marketing sections asked for
async fn add_analytics(
    state: &AppState,
    location: Option<Uuid>,
    wanted: &[String],
    board: &mut Dashboard,
) -> Result<(), Error> {
    let wants = |id: &str| wanted.iter().any(|w| w == id);
    let filter = ReportFilter {
        location_id: location,
        ..Default::default()
    };
    if wants("finance") || wants("projection") {
        board.finance = Some(state.analytics.finance(&filter, 6).await?);
    }
    let parts = MarketingParts {
        seasonality: wants("seasonality"),
        restock: wants("restock"),
        top: wants("top_customers"),
        customers: false,
        promotions: wants("promotion_results"),
        loyalty: wants("loyalty"),
    };
    if parts.any() {
        board.marketing = Some(state.analytics.marketing_parts(&filter, parts).await?);
    }
    Ok(())
}

/// Router for the dashboard, the caller's rights and their layout (behind
/// the admin middleware, which lets any staff member through for these paths)
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/staff/me", get(staff_me))
        .route(
            "/admin/staff/me/dashboard",
            get(my_dashboard)
                .put(save_my_dashboard)
                .delete(reset_my_dashboard),
        )
        .route("/admin/dashboard", get(dashboard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashboard_widgets::WidgetSize;

    fn offered(ids: &[&str]) -> Vec<LayoutSlot> {
        ids.iter()
            .map(|id| LayoutSlot {
                widget: id.to_string(),
                size: WidgetSize::Medium,
                hidden: false,
            })
            .collect()
    }

    #[test]
    fn only_offered_widgets_are_computed() {
        let counter = offered(&["orders", "stock", "invoices"]);
        assert_eq!(
            wanted_widgets(None, &counter),
            ["orders", "stock", "invoices"]
        );
        assert_eq!(
            wanted_widgets(Some("finance, stock,,nope"), &counter),
            ["stock"]
        );
        let manager = offered(&["finance", "sales"]);
        assert_eq!(wanted_widgets(Some("finance"), &manager), ["finance"]);
        assert_eq!(wanted_widgets(None, &manager), ["sales"]);
    }
}
