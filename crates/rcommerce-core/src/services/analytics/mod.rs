//! Finance and marketing reports for the back-office (resource `reports`).
//!
//! - [`finance`]: sales, VAT, discounts and margin by month, invoices,
//!   takings by payment method, open orders, and a projection of the coming
//!   months ([`projection`]).
//! - [`marketing`]: seasonality per category, restocking ahead of last year's
//!   demand, best customers and companies, new against returning customers,
//!   average baskets, channels, promotions and loyalty points.
//!
//! Every report can be narrowed to one branch (the order's fulfilment
//! location) and to retail or trade customers (orders placed for a business
//! account). Sales are orders that are neither pending, cancelled, refunded
//! nor test orders; amounts are excl. VAT unless a field says otherwise.
//! Days and months follow the store's time zone.

pub mod export;
pub mod finance;
pub mod marketing;
pub mod projection;

use chrono::{Datelike, Months, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgArguments, PgRow};
use sqlx::{FromRow, PgPool, Postgres};
use uuid::Uuid;

/// Whose orders a report covers
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    #[default]
    All,
    /// Orders placed without a business account
    Retail,
    /// Orders placed for a business account
    Trade,
}

impl Audience {
    fn as_str(self) -> &'static str {
        match self {
            Audience::All => "all",
            Audience::Retail => "retail",
            Audience::Trade => "trade",
        }
    }
}

/// Branch, audience and period of a report
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReportFilter {
    /// Orders fulfilled by this branch only
    pub location_id: Option<Uuid>,
    #[serde(default)]
    pub audience: Audience,
    /// First day included (default: the first day of the month 11 months ago)
    pub from: Option<NaiveDate>,
    /// Last day included (default: today)
    pub to: Option<NaiveDate>,
}

/// Branch (`$1`, NULL for all) and audience (`$2`) of the orders `o`
const ORDER_SCOPE: &str = "NOT o.test \
    AND ($1::text IS NULL OR o.metadata->>'fulfillment_location_id' = $1) \
    AND ($2::text = 'all' OR ($2::text = 'trade') = (o.company_id IS NOT NULL))";

/// Orders `o` that count as sales, within the report's branch and audience
fn sales_scope() -> String {
    format!("{ORDER_SCOPE} AND o.status::text NOT IN ('pending', 'cancelled', 'refunded')")
}

/// Orders `o` placed from local day `$from` up to, not including, `$until`
fn placed_between(from: u8, until: u8) -> String {
    format!(
        "o.created_at >= (${from}::date::timestamp AT TIME ZONE $3) \
         AND o.created_at < (${until}::date::timestamp AT TIME ZONE $3)"
    )
}

/// Month (first day, local) an order `o` was placed in
const ORDER_MONTH: &str = "date_trunc('month', o.created_at AT TIME ZONE $3)::date";

/// What a line `oi` brought in excl. VAT: its pricing snapshot `lp` net of its
/// share of cart discounts, or the order line itself for older orders
const LINE_NET: &str =
    "COALESCE(lp.unit_price * lp.quantity - lp.discount, oi.price * oi.quantity)";

/// Computes the reports
#[derive(Clone)]
pub struct AnalyticsService {
    pool: PgPool,
    timezone: chrono_tz::Tz,
}

impl AnalyticsService {
    /// A service for `timezone` (an IANA name; Europe/Brussels if unknown)
    pub fn new(pool: PgPool, timezone: &str) -> Self {
        let timezone = timezone.parse().unwrap_or(chrono_tz::Europe::Brussels);
        Self { pool, timezone }
    }

    /// Today in the store's time zone
    pub fn today(&self) -> NaiveDate {
        Utc::now().with_timezone(&self.timezone).date_naive()
    }

    /// The filter's period, first and last day included
    fn period(&self, filter: &ReportFilter) -> (NaiveDate, NaiveDate) {
        let to = filter.to.unwrap_or_else(|| self.today());
        let from = filter
            .from
            .unwrap_or_else(|| first_of_month(to) - Months::new(11));
        (from.min(to), to)
    }

    /// A query whose `$1`, `$2` and `$3` are the filter's branch, audience and
    /// the store's time zone
    fn scoped<'q, O>(
        &self,
        sql: &'q str,
        filter: &ReportFilter,
    ) -> sqlx::query::QueryAs<'q, Postgres, O, PgArguments>
    where
        O: for<'r> FromRow<'r, PgRow>,
    {
        self.filtered(sql, filter).bind(self.timezone.name())
    }

    /// A query whose `$1` and `$2` are the filter's branch and audience
    fn filtered<'q, O>(
        &self,
        sql: &'q str,
        filter: &ReportFilter,
    ) -> sqlx::query::QueryAs<'q, Postgres, O, PgArguments>
    where
        O: for<'r> FromRow<'r, PgRow>,
    {
        sqlx::query_as(sql)
            .bind(filter.location_id.map(|l| l.to_string()))
            .bind(filter.audience.as_str())
    }
}

/// The first day of `day`'s month
fn first_of_month(day: NaiveDate) -> NaiveDate {
    day.with_day(1).unwrap_or(day)
}

/// The day after `day` (the exclusive end of a period ending on `day`)
fn next_day(day: NaiveDate) -> NaiveDate {
    day.succ_opt().unwrap_or(day)
}

/// The first days of the months from `from`'s to `to`'s, both included
fn months_between(from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
    let mut months = Vec::new();
    let mut month = first_of_month(from);
    while month <= to {
        months.push(month);
        month = month + Months::new(1);
    }
    months
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_between_lists_every_month_once() {
        let months = months_between("2025-11-20".parse().unwrap(), "2026-02-03".parse().unwrap());
        let shown: Vec<String> = months.iter().map(|m| m.to_string()).collect();
        assert_eq!(
            shown,
            ["2025-11-01", "2025-12-01", "2026-01-01", "2026-02-01"]
        );
    }

    #[test]
    fn audience_query_values_match_the_sql() {
        assert_eq!(Audience::All.as_str(), "all");
        assert_eq!(Audience::Trade.as_str(), "trade");
        let parsed: ReportFilter = serde_json::from_str(r#"{"audience":"retail"}"#).unwrap();
        assert_eq!(parsed.audience, Audience::Retail);
    }
}
