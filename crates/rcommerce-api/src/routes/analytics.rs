//! Finance and marketing reports for staff with `reports` (see
//! `services::analytics`):
//!
//! - `GET /admin/analytics/finance?location_id=&audience=&from=&to=&horizon=`
//! - `GET /admin/analytics/finance.csv` (same query): one row per month
//! - `GET /admin/analytics/marketing?location_id=&audience=&from=&to=`
//! - `GET /admin/analytics/marketing.csv?section=` (same query): customers,
//!   companies, customer_months, seasonality, restock or promotions
//!
//! `audience` is `all` (default), `retail` or `trade`; `from` and `to` are
//! days (default: the last 12 months to today); `horizon` is how many months
//! after the current one to project (default 6, at most 12).

use axum::{
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use chrono::NaiveDate;
use rcommerce_core::services::analytics::{
    export, finance::FinanceReport, marketing::MarketingReport, Audience, ReportFilter,
};
use rcommerce_core::Error;
use serde::Deserialize;
use uuid::Uuid;

use crate::state::AppState;

/// Query of every report
#[derive(Debug, Default, Deserialize)]
pub struct ReportQuery {
    pub location_id: Option<Uuid>,
    #[serde(default)]
    pub audience: Audience,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    /// Finance: months projected after the current one
    pub horizon: Option<u32>,
    /// Marketing CSV: which part
    pub section: Option<String>,
}

impl ReportQuery {
    fn filter(&self) -> Result<ReportFilter, Error> {
        if let (Some(from), Some(to)) = (self.from, self.to) {
            if from > to {
                return Err(Error::validation("'from' must not be after 'to'"));
            }
        }
        Ok(ReportFilter {
            location_id: self.location_id,
            audience: self.audience,
            from: self.from,
            to: self.to,
        })
    }

    fn horizon(&self) -> u32 {
        self.horizon.unwrap_or(6).min(12)
    }
}

/// A CSV download named `name`
fn csv_file(name: &str, body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}.csv\""),
            ),
        ],
        body,
    )
        .into_response()
}

async fn finance(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
) -> Result<Json<FinanceReport>, Error> {
    let report = state
        .analytics
        .finance(&query.filter()?, query.horizon())
        .await?;
    Ok(Json(report))
}

async fn finance_csv(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
) -> Result<Response, Error> {
    let report = state
        .analytics
        .finance(&query.filter()?, query.horizon())
        .await?;
    let name = format!("finance-{}-{}", report.from, report.to);
    Ok(csv_file(&name, export::finance_csv(&report)?))
}

async fn marketing(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
) -> Result<Json<MarketingReport>, Error> {
    Ok(Json(state.analytics.marketing(&query.filter()?).await?))
}

async fn marketing_csv(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
) -> Result<Response, Error> {
    let section = query.section.clone().unwrap_or_else(|| "customers".into());
    if !export::MARKETING_SECTIONS.contains(&section.as_str()) {
        return Err(Error::validation(format!(
            "Unknown section '{section}'; expected one of {}",
            export::MARKETING_SECTIONS.join(", ")
        )));
    }
    let report = state.analytics.marketing(&query.filter()?).await?;
    let name = format!("{section}-{}-{}", report.from, report.to);
    Ok(csv_file(&name, export::marketing_csv(&report, &section)?))
}

/// Staff routes (the staff middleware checks the `reports` resource)
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin/analytics/finance", get(finance))
        .route("/admin/analytics/finance.csv", get(finance_csv))
        .route("/admin/analytics/marketing", get(marketing))
        .route("/admin/analytics/marketing.csv", get(marketing_csv))
}
