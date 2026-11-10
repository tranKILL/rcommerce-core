//! Figures for the back-office home: the orders waiting for the team, sales
//! by period, stock running low, unpaid invoices and Peppol failures, new
//! customers and the state of the catalogue. Each part is computed on its own,
//! so the caller asks only for what a staff member may see. Orders and stock
//! can be narrowed to one branch; test orders count as work (they are on the
//! counter boards too) but not as sales.

use chrono::{Datelike, Duration, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::inventory::stock_admin::{self, LevelFilter, StockLevelRow};
use crate::Result;

/// Open orders waiting for the team, by what has to happen next
#[derive(Debug, Default, Serialize, sqlx::FromRow)]
pub struct OrderWork {
    /// Placed in the last 7 days and not paid yet
    pub awaiting_payment: i64,
    /// Pickup orders not ready yet
    pub pickups_to_prepare: i64,
    /// ...of which the promised pickup time has passed
    pub pickups_late: i64,
    /// Ready at the counter, not collected yet
    pub pickups_waiting: i64,
    /// ...of which ready for more than two days
    pub pickups_waiting_long: i64,
    /// Delivery orders not dispatched yet
    pub deliveries_to_dispatch: i64,
    /// Out with a driver
    pub deliveries_out: i64,
}

/// Orders and turnover over a period
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct Takings {
    pub orders: i64,
    /// Incl. VAT
    pub revenue: Decimal,
    /// Excl. VAT
    pub revenue_excl_vat: Decimal,
}

/// One day of sales
#[derive(Debug, Clone, Serialize)]
pub struct DayTakings {
    pub date: NaiveDate,
    #[serde(flatten)]
    pub takings: Takings,
}

/// Sales today, this week (from Monday), this month, the same days of last
/// month for comparison, and the last 14 days one by one
#[derive(Debug, Default, Serialize)]
pub struct Sales {
    pub today: Takings,
    pub week: Takings,
    pub month: Takings,
    pub previous_month_to_date: Takings,
    pub days: Vec<DayTakings>,
}

/// Stock at or below its reorder point
#[derive(Debug, Default, Serialize)]
pub struct StockAlerts {
    /// Nothing left to sell
    pub out_of_stock: i64,
    /// Some left, at or below the reorder point
    pub low: i64,
    /// The most urgent lines, emptiest first
    pub levels: Vec<StockLevelRow>,
}

/// Invoices to collect and e-invoices that did not go through
#[derive(Debug, Default, Serialize, sqlx::FromRow)]
pub struct InvoiceAlerts {
    pub unpaid_count: i64,
    pub unpaid_amount: Decimal,
    pub overdue_count: i64,
    pub overdue_amount: Decimal,
    pub peppol_failed: i64,
    /// Issued since the first of the month
    pub month_count: i64,
    pub month_amount: Decimal,
}

/// Customers and companies
#[derive(Debug, Default, Serialize, sqlx::FromRow)]
pub struct CustomerFigures {
    /// Accounts opened in the last 7 days
    pub new_customers: i64,
    /// Business accounts opened in the last 7 days
    pub new_companies: i64,
    /// Active business accounts
    pub companies: i64,
}

/// The catalogue online and offline
#[derive(Debug, Default, Serialize, sqlx::FromRow)]
pub struct CatalogueFigures {
    pub online: i64,
    pub offline: i64,
    /// Online, with no free stock at any branch
    pub online_without_stock: i64,
}

/// Computes the figures of the back-office home
#[derive(Clone)]
pub struct DashboardService {
    pool: PgPool,
    /// The store's time zone, for "today" and "this month"
    timezone: chrono_tz::Tz,
}

/// Orders of one branch (all when `$1` is NULL)
const ORDER_SCOPE: &str = "($1::text IS NULL OR metadata->>'fulfillment_location_id' = $1)";

impl DashboardService {
    /// A service for `timezone` (an IANA name; Europe/Brussels if unknown)
    pub fn new(pool: PgPool, timezone: &str) -> Self {
        let timezone = timezone.parse().unwrap_or(chrono_tz::Europe::Brussels);
        Self { pool, timezone }
    }

    /// Today in the store's time zone
    pub fn today(&self) -> NaiveDate {
        Utc::now().with_timezone(&self.timezone).date_naive()
    }

    /// Open orders by what has to happen next
    pub async fn order_work(&self, location: Option<Uuid>) -> Result<OrderWork> {
        let sql = format!(
            "SELECT \
               COUNT(*) FILTER (WHERE status::text = 'pending' AND created_at > NOW() - INTERVAL '7 days') AS awaiting_payment, \
               COUNT(*) FILTER (WHERE open AND pickup AND NOT ready) AS pickups_to_prepare, \
               COUNT(*) FILTER (WHERE open AND pickup AND NOT ready AND promised < NOW()) AS pickups_late, \
               COUNT(*) FILTER (WHERE open AND pickup AND ready) AS pickups_waiting, \
               COUNT(*) FILTER (WHERE open AND pickup AND ready AND ready_since < NOW() - INTERVAL '2 days') AS pickups_waiting_long, \
               COUNT(*) FILTER (WHERE open AND NOT pickup AND NOT shipped) AS deliveries_to_dispatch, \
               COUNT(*) FILTER (WHERE open AND NOT pickup AND shipped) AS deliveries_out \
             FROM (SELECT status, created_at, \
                     status::text IN ('confirmed', 'processing', 'on_hold') AS open, \
                     (metadata->'pickup') IS NOT NULL AS pickup, \
                     metadata->'pickup'->>'ready_marked_at' IS NOT NULL AS ready, \
                     (metadata->'pickup'->>'ready_marked_at')::timestamptz AS ready_since, \
                     (metadata->'pickup'->>'ready_at')::timestamptz AS promised, \
                     fulfillment_status::text = 'shipped' AS shipped \
                   FROM orders WHERE {ORDER_SCOPE}) o"
        );
        let work = sqlx::query_as::<_, OrderWork>(&sql)
            .bind(location.map(|l| l.to_string()))
            .fetch_one(&self.pool)
            .await?;
        Ok(work)
    }

    /// Sales by period; paid or confirmed orders only (not pending,
    /// cancelled, refunded or test orders)
    pub async fn sales(&self, location: Option<Uuid>) -> Result<Sales> {
        let sql = format!(
            "SELECT (created_at AT TIME ZONE $2)::date AS day, COUNT(*)::bigint, \
                    COALESCE(SUM(total), 0), COALESCE(SUM(total - tax_total), 0) \
             FROM orders WHERE {ORDER_SCOPE} AND NOT test \
               AND status::text NOT IN ('pending', 'cancelled', 'refunded') \
               AND created_at > NOW() - INTERVAL '70 days' \
             GROUP BY 1 ORDER BY 1"
        );
        let rows: Vec<(NaiveDate, i64, Decimal, Decimal)> = sqlx::query_as(&sql)
            .bind(location.map(|l| l.to_string()))
            .bind(self.timezone.name())
            .fetch_all(&self.pool)
            .await?;
        let days = rows
            .into_iter()
            .map(|(date, orders, revenue, revenue_excl_vat)| DayTakings {
                date,
                takings: Takings {
                    orders,
                    revenue,
                    revenue_excl_vat,
                },
            })
            .collect::<Vec<_>>();
        Ok(sales_by_period(&days, self.today()))
    }

    /// Stock lines at or below their reorder point, for online products
    pub async fn stock_alerts(&self, location: Option<Uuid>) -> Result<StockAlerts> {
        let (out_of_stock, low): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*) FILTER (WHERE lv.available_quantity <= 0), \
                    COUNT(*) FILTER (WHERE lv.available_quantity > 0 \
                                     AND lv.available_quantity <= lv.reorder_point) \
             FROM inventory_levels lv JOIN products p ON p.id = lv.product_id \
             WHERE p.is_active AND ($1::uuid IS NULL OR lv.location_id = $1)",
        )
        .bind(location)
        .fetch_one(&self.pool)
        .await?;
        let filter = LevelFilter {
            location_id: location,
            low_only: true,
            sort: Some("available".into()),
            per_page: Some(8),
            ..Default::default()
        };
        let (levels, _) = stock_admin::list_levels(&self.pool, &filter).await?;
        Ok(StockAlerts {
            out_of_stock,
            low,
            levels,
        })
    }

    /// Unpaid and overdue invoices, Peppol failures, this month's invoicing
    pub async fn invoice_alerts(&self) -> Result<InvoiceAlerts> {
        let today = self.today();
        let alerts = sqlx::query_as::<_, InvoiceAlerts>(
            "SELECT COUNT(*) FILTER (WHERE total > paid_amount) AS unpaid_count, \
                    COALESCE(SUM(total - paid_amount) FILTER (WHERE total > paid_amount), 0) AS unpaid_amount, \
                    COUNT(*) FILTER (WHERE total > paid_amount AND due_date < $1) AS overdue_count, \
                    COALESCE(SUM(total - paid_amount) FILTER (WHERE total > paid_amount AND due_date < $1), 0) \
                      AS overdue_amount, \
                    COUNT(*) FILTER (WHERE peppol_status = 'failed') AS peppol_failed, \
                    COUNT(*) FILTER (WHERE issue_date >= $2) AS month_count, \
                    COALESCE(SUM(total) FILTER (WHERE issue_date >= $2), 0) AS month_amount \
             FROM invoices WHERE kind = 'invoice'",
        )
        .bind(today)
        .bind(today.with_day(1).unwrap_or(today))
        .fetch_one(&self.pool)
        .await?;
        Ok(alerts)
    }

    /// New accounts this week and active business accounts
    pub async fn customer_figures(&self) -> Result<CustomerFigures> {
        let figures = sqlx::query_as::<_, CustomerFigures>(
            "SELECT (SELECT COUNT(*) FROM customers \
                      WHERE created_at > NOW() - INTERVAL '7 days') AS new_customers, \
                    (SELECT COUNT(*) FROM companies \
                      WHERE created_at > NOW() - INTERVAL '7 days') AS new_companies, \
                    (SELECT COUNT(*) FROM companies WHERE status = 'active') AS companies",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(figures)
    }

    /// Products online, offline, and online with nothing to sell
    pub async fn catalogue_figures(&self) -> Result<CatalogueFigures> {
        let figures = sqlx::query_as::<_, CatalogueFigures>(
            "SELECT COUNT(*) FILTER (WHERE p.is_active) AS online, \
                    COUNT(*) FILTER (WHERE NOT p.is_active) AS offline, \
                    COUNT(*) FILTER (WHERE p.is_active AND NOT EXISTS ( \
                      SELECT 1 FROM inventory_levels l \
                       WHERE l.product_id = p.id AND l.available_quantity > 0)) AS online_without_stock \
             FROM products p",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(figures)
    }
}

/// Add up the takings of the days from `from` to `to` (both included)
fn takings_between(days: &[DayTakings], from: NaiveDate, to: NaiveDate) -> Takings {
    days.iter()
        .filter(|d| d.date >= from && d.date <= to)
        .fold(Takings::default(), |sum, d| Takings {
            orders: sum.orders + d.takings.orders,
            revenue: sum.revenue + d.takings.revenue,
            revenue_excl_vat: sum.revenue_excl_vat + d.takings.revenue_excl_vat,
        })
}

/// Today, this week, this month, the same days of last month, and the last
/// 14 days (missing days as zero), from daily takings
fn sales_by_period(days: &[DayTakings], today: NaiveDate) -> Sales {
    let monday = today - Duration::days(i64::from(today.weekday().num_days_from_monday()));
    let first = today.with_day(1).unwrap_or(today);
    let previous_first = (first - Duration::days(1)).with_day(1).unwrap_or(first);
    let previous_last =
        (previous_first + Duration::days(i64::from(today.day0()))).min(first - Duration::days(1));
    let last_14 = (0..14)
        .rev()
        .map(|back| {
            let date = today - Duration::days(back);
            DayTakings {
                date,
                takings: takings_between(days, date, date),
            }
        })
        .collect();
    Sales {
        today: takings_between(days, today, today),
        week: takings_between(days, monday, today),
        month: takings_between(days, first, today),
        previous_month_to_date: takings_between(days, previous_first, previous_last),
        days: last_14,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(date: &str, orders: i64, revenue: i64) -> DayTakings {
        DayTakings {
            date: date.parse().unwrap(),
            takings: Takings {
                orders,
                revenue: Decimal::from(revenue),
                revenue_excl_vat: Decimal::from(revenue) / Decimal::from(2),
            },
        }
    }

    #[test]
    fn periods_add_up_the_right_days() {
        // Tuesday 31 March: the week starts on Monday 30, the month on the 1st,
        // and "the same days of last month" stop at the end of February
        let days = [
            day("2026-02-10", 1, 100),
            day("2026-02-28", 1, 50),
            day("2026-03-01", 2, 200),
            day("2026-03-30", 1, 10),
            day("2026-03-31", 3, 30),
        ];
        let sales = sales_by_period(&days, "2026-03-31".parse().unwrap());
        assert_eq!(sales.today.orders, 3);
        assert_eq!(sales.week.revenue, Decimal::from(40));
        assert_eq!(sales.month.orders, 6);
        assert_eq!(sales.previous_month_to_date.revenue, Decimal::from(150));
        assert_eq!(sales.month.revenue_excl_vat, Decimal::from(120));
    }

    #[test]
    fn the_last_14_days_are_all_listed_oldest_first() {
        let days = [day("2026-11-03", 2, 20)];
        let sales = sales_by_period(&days, "2026-11-10".parse().unwrap());
        assert_eq!(sales.days.len(), 14);
        assert_eq!(
            sales.days[0].date,
            "2026-10-28".parse::<NaiveDate>().unwrap()
        );
        assert_eq!(
            sales.days[13].date,
            "2026-11-10".parse::<NaiveDate>().unwrap()
        );
        assert_eq!(sales.days[6].takings.orders, 2);
        assert_eq!(sales.days[7].takings.orders, 0);
    }
}
