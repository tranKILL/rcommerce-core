//! Finance: sales, VAT, discounts and margin per month; this month so far
//! against the same days last year; invoices issued, paid and still due;
//! takings by payment method; open orders; and the coming months projected.
//!
//! The margin is computed from the cost price snapshot taken when the order
//! was placed, on the lines that have one: `costed` tells how much of the
//! goods sold it covers.

use std::collections::BTreeMap;

use chrono::{Months, NaiveDate};
use rust_decimal::Decimal;
use serde::Serialize;

use super::projection::{self, ProjectedMonth};
use super::{
    first_of_month, months_between, next_day, placed_between, sales_scope, AnalyticsService,
    ReportFilter, LINE_NET, ORDER_MONTH, ORDER_SCOPE,
};
use crate::Result;

/// Orders and amounts over a period
#[derive(Debug, Default, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Totals {
    pub orders: i64,
    /// Excl. VAT
    pub sales: Decimal,
    pub sales_incl_vat: Decimal,
    pub vat: Decimal,
    /// Discounts given on the orders (coupons, cart promotions, points)
    pub discounts: Decimal,
}

impl Totals {
    fn add(&mut self, other: &Totals) {
        self.orders += other.orders;
        self.sales += other.sales;
        self.sales_incl_vat += other.sales_incl_vat;
        self.vat += other.vat;
        self.discounts += other.discounts;
    }
}

/// One month of sales
#[derive(Debug, Clone, Serialize)]
pub struct MonthFigures {
    /// First day of the month
    pub month: NaiveDate,
    #[serde(flatten)]
    pub totals: Totals,
    /// Goods sold excl. VAT, net of discounts (delivery not included)
    pub goods: Decimal,
    /// The part of `goods` whose cost price is known
    pub costed: Decimal,
    /// Cost of the goods in `costed`
    pub cost: Decimal,
    /// `costed` minus `cost`
    pub margin: Decimal,
}

/// This month up to today against the same days a year earlier
#[derive(Debug, Clone, Serialize)]
pub struct MonthToDate {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub current: Totals,
    pub last_year: Totals,
}

/// Invoices issued in the period, and those still due today
#[derive(Debug, Default, Clone, Serialize, sqlx::FromRow)]
pub struct InvoiceFigures {
    pub issued_count: i64,
    pub issued_amount: Decimal,
    /// Issued in the period and paid in full
    pub paid_count: i64,
    /// Received on the invoices issued in the period
    pub paid_amount: Decimal,
    /// Every invoice with something left to pay, whenever issued
    pub outstanding_count: i64,
    pub outstanding_amount: Decimal,
    /// ...of which past their due date
    pub overdue_count: i64,
    pub overdue_amount: Decimal,
}

/// Payments received with one method through one channel
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MethodTakings {
    /// `online`, or the channel an order records in `metadata.source`
    pub channel: String,
    /// Payment provider
    pub gateway: String,
    /// Card, bank transfer... when the provider says
    pub method: Option<String>,
    pub payments: i64,
    /// Incl. VAT
    pub amount: Decimal,
}

/// Orders confirmed and not completed yet: sales still to deliver
#[derive(Debug, Default, Clone, Serialize, sqlx::FromRow)]
pub struct OpenOrders {
    pub orders: i64,
    pub sales: Decimal,
}

/// The finance report
#[derive(Debug, Clone, Serialize)]
pub struct FinanceReport {
    pub from: NaiveDate,
    pub to: NaiveDate,
    /// Every month of the period, oldest first
    pub months: Vec<MonthFigures>,
    /// The months' totals added up
    pub total: Totals,
    pub month_to_date: MonthToDate,
    pub invoices: InvoiceFigures,
    pub takings: Vec<MethodTakings>,
    pub open_orders: OpenOrders,
    /// This month and the next ones, from the last 24 complete months
    pub projection: Vec<ProjectedMonth>,
}

/// Goods, costed goods and their cost in a month
#[derive(sqlx::FromRow)]
struct MonthLines {
    month: NaiveDate,
    goods: Decimal,
    costed: Decimal,
    cost: Decimal,
}

impl AnalyticsService {
    /// The finance report for the filter's period, projected `horizon` months
    /// past the current one
    pub async fn finance(&self, filter: &ReportFilter, horizon: u32) -> Result<FinanceReport> {
        let (from, to) = self.period(filter);
        let today = self.today();
        let current = first_of_month(today);
        let totals = self.monthly_totals(filter, from, to).await?;
        let lines = self.monthly_lines(filter, from, to).await?;
        let months = month_figures(&totals, &lines, from, to);
        let mut total = Totals::default();
        months.iter().for_each(|m| total.add(&m.totals));
        let history: BTreeMap<NaiveDate, Decimal> = self
            .monthly_totals(filter, current - Months::new(24), today)
            .await?
            .into_iter()
            .map(|(month, t)| (month, t.sales))
            .collect();
        Ok(FinanceReport {
            from,
            to,
            months,
            total,
            month_to_date: self.month_to_date(filter, today).await?,
            invoices: self.invoice_figures(filter, from, to).await?,
            takings: self.takings(filter, from, to).await?,
            open_orders: self.open_orders(filter).await?,
            projection: projection::project(&history, current, horizon),
        })
    }

    /// Sales totals per month, for orders placed from `from` to `to` included
    async fn monthly_totals(
        &self,
        filter: &ReportFilter,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<BTreeMap<NaiveDate, Totals>> {
        let sql = format!(
            "SELECT {ORDER_MONTH} AS month, COUNT(*)::bigint, COALESCE(SUM(o.total - o.tax_total), 0), \
                    COALESCE(SUM(o.total), 0), COALESCE(SUM(o.tax_total), 0), COALESCE(SUM(o.discount_total), 0) \
             FROM orders o WHERE {} AND {} GROUP BY 1",
            sales_scope(),
            placed_between(4, 5)
        );
        let rows: Vec<(NaiveDate, i64, Decimal, Decimal, Decimal, Decimal)> = self
            .scoped(&sql, filter)
            .bind(from)
            .bind(next_day(to))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(month, orders, sales, sales_incl_vat, vat, discounts)| {
                let totals = Totals {
                    orders,
                    sales,
                    sales_incl_vat,
                    vat,
                    discounts,
                };
                (month, totals)
            })
            .collect())
    }

    /// Goods sold, the part with a known cost, and that cost, per month
    async fn monthly_lines(
        &self,
        filter: &ReportFilter,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<MonthLines>> {
        let sql = format!(
            "SELECT {ORDER_MONTH} AS month, COALESCE(SUM({LINE_NET}), 0) AS goods, \
                    COALESCE(SUM({LINE_NET}) FILTER (WHERE lp.cost_price IS NOT NULL), 0) AS costed, \
                    COALESCE(SUM(lp.cost_price * lp.quantity), 0) AS cost \
             FROM orders o JOIN order_items oi ON oi.order_id = o.id \
             LEFT JOIN order_line_pricing lp ON lp.order_item_id = oi.id \
             WHERE {} AND {} AND NOT COALESCE(oi.is_bundle_component, false) GROUP BY 1",
            sales_scope(),
            placed_between(4, 5)
        );
        let rows = self
            .scoped(&sql, filter)
            .bind(from)
            .bind(next_day(to))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// This month up to today, and the same days of the same month last year
    async fn month_to_date(&self, filter: &ReportFilter, today: NaiveDate) -> Result<MonthToDate> {
        let from = first_of_month(today);
        let year_ago = |day: NaiveDate| day - Months::new(12);
        let sum = |months: BTreeMap<NaiveDate, Totals>| {
            let mut total = Totals::default();
            months.values().for_each(|t| total.add(t));
            total
        };
        let current = sum(self.monthly_totals(filter, from, today).await?);
        let last_year = sum(self
            .monthly_totals(filter, year_ago(from), year_ago(today))
            .await?);
        Ok(MonthToDate {
            from,
            to: today,
            current,
            last_year,
        })
    }

    /// Invoices issued in the period (paid in full or not), and all those
    /// still due today
    async fn invoice_figures(
        &self,
        filter: &ReportFilter,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<InvoiceFigures> {
        let sql = format!(
            "SELECT COUNT(*) FILTER (WHERE period) AS issued_count, \
                    COALESCE(SUM(i.total) FILTER (WHERE period), 0) AS issued_amount, \
                    COUNT(*) FILTER (WHERE period AND i.paid_amount >= i.total) AS paid_count, \
                    COALESCE(SUM(i.paid_amount) FILTER (WHERE period), 0) AS paid_amount, \
                    COUNT(*) FILTER (WHERE due) AS outstanding_count, \
                    COALESCE(SUM(i.total - i.paid_amount) FILTER (WHERE due), 0) AS outstanding_amount, \
                    COUNT(*) FILTER (WHERE due AND i.due_date < $5) AS overdue_count, \
                    COALESCE(SUM(i.total - i.paid_amount) FILTER (WHERE due AND i.due_date < $5), 0) \
                      AS overdue_amount \
             FROM (SELECT i.total, i.paid_amount, i.due_date, \
                          i.issue_date >= $3 AND i.issue_date <= $4 AS period, \
                          i.total > i.paid_amount AS due \
                   FROM invoices i JOIN orders o ON o.id = i.order_id \
                   WHERE i.kind = 'invoice' AND {ORDER_SCOPE}) i"
        );
        let figures = self
            .filtered(&sql, filter)
            .bind(from)
            .bind(to)
            .bind(self.today())
            .fetch_one(&self.pool)
            .await?;
        Ok(figures)
    }

    /// Payments received in the period by channel, provider and method
    async fn takings(
        &self,
        filter: &ReportFilter,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<MethodTakings>> {
        let sql = format!(
            "SELECT COALESCE(o.metadata->>'source', 'online') AS channel, p.gateway, \
                    p.payment_method::text AS method, COUNT(*)::bigint AS payments, \
                    COALESCE(SUM(p.amount), 0) AS amount \
             FROM payments p JOIN orders o ON o.id = p.order_id \
             WHERE p.status::text = 'paid' AND {ORDER_SCOPE} \
               AND COALESCE(p.processed_at, p.created_at) >= ($4::date::timestamp AT TIME ZONE $3) \
               AND COALESCE(p.processed_at, p.created_at) < ($5::date::timestamp AT TIME ZONE $3) \
             GROUP BY 1, 2, 3 ORDER BY amount DESC"
        );
        let rows = self
            .scoped(&sql, filter)
            .bind(from)
            .bind(next_day(to))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Orders confirmed, being prepared or on hold
    async fn open_orders(&self, filter: &ReportFilter) -> Result<OpenOrders> {
        let sql = format!(
            "SELECT COUNT(*)::bigint AS orders, COALESCE(SUM(o.total - o.tax_total), 0) AS sales \
             FROM orders o WHERE {ORDER_SCOPE} \
               AND o.status::text IN ('confirmed', 'processing', 'on_hold')"
        );
        let open = self.filtered(&sql, filter).fetch_one(&self.pool).await?;
        Ok(open)
    }
}

/// The months from `from` to `to`, with their totals and lines (zero when
/// nothing was sold)
fn month_figures(
    totals: &BTreeMap<NaiveDate, Totals>,
    lines: &[MonthLines],
    from: NaiveDate,
    to: NaiveDate,
) -> Vec<MonthFigures> {
    months_between(from, to)
        .into_iter()
        .map(|month| {
            let line = lines.iter().find(|l| l.month == month);
            let (goods, costed, cost) = line
                .map_or((Decimal::ZERO, Decimal::ZERO, Decimal::ZERO), |l| {
                    (l.goods, l.costed, l.cost)
                });
            MonthFigures {
                month,
                totals: totals.get(&month).cloned().unwrap_or_default(),
                goods,
                costed,
                cost,
                margin: costed - cost,
            }
        })
        .collect()
}
