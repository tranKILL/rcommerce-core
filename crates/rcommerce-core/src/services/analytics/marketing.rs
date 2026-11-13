//! Marketing: when each category sells (seasonality), what to restock before
//! last year's demand comes back, who buys most, new against returning
//! customers, average baskets, channels, how promotions did, and loyalty
//! points.
//!
//! - **Seasonality** uses the 24 complete months before the current one,
//!   whatever the period: per top-level category, each calendar month's
//!   average sales against the category's average month (100).
//! - **Restocking** compares, per product, what sold in the coming 8 weeks
//!   last year with the 8 weeks before them. Products whose sales rose then
//!   are listed with the quantity expected now (last year's, scaled by how
//!   the last 8 weeks compare with the same weeks last year) against the stock
//!   available.
//! - **Promotions** compare the daily sales of the products a promotion priced
//!   during the promotion with the 28 days before it started.
//! - **Loyalty** covers the whole store (points are not tied to a branch).

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Datelike, Duration, Months, NaiveDate, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Serialize;
use uuid::Uuid;

use super::{
    first_of_month, next_day, placed_between, sales_scope, AnalyticsService, ReportFilter,
    LINE_NET, ORDER_MONTH,
};
use crate::Result;

/// Weeks ahead (and back) the restocking compares
const RESTOCK_WEEKS: i64 = 8;

/// A top-level category's seasonal profile
#[derive(Debug, Clone, Serialize)]
pub struct CategorySeason {
    pub category_id: Uuid,
    pub name: String,
    /// Sales excl. VAT over the 24 months
    pub sales: Decimal,
    /// January to December: 100 is the category's average month
    pub index: [i64; 12],
}

/// Stock of a product at one branch
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct BranchStock {
    pub location_id: Uuid,
    pub name: String,
    pub available: i64,
}

/// A product to restock before its season
#[derive(Debug, Clone, Serialize)]
pub struct RestockSuggestion {
    pub product_id: Uuid,
    pub title: String,
    pub sku: Option<String>,
    /// Sold in the coming 8 weeks, last year
    pub last_year_coming: i64,
    /// Sold in the 8 weeks before those, last year
    pub last_year_before: i64,
    /// Sold in the last 8 weeks
    pub recent: i64,
    /// Expected in the coming 8 weeks
    pub expected: i64,
    /// Available now (at the branch, or everywhere)
    pub available: i64,
    /// What is missing to cover `expected`
    pub suggested: i64,
    pub branches: Vec<BranchStock>,
}

/// A customer and what they bought in the period
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TopCustomer {
    pub customer_id: Uuid,
    pub name: String,
    pub email: String,
    pub company_id: Option<Uuid>,
    pub company_name: Option<String>,
    pub orders: i64,
    pub sales: Decimal,
    pub first_order: DateTime<Utc>,
    pub last_order: DateTime<Utc>,
}

/// A business account and what its buyers bought in the period
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TopCompany {
    pub company_id: Uuid,
    pub name: String,
    pub buyers: i64,
    pub orders: i64,
    pub sales: Decimal,
    pub last_order: DateTime<Utc>,
}

/// Customers in a month: first order ever, or coming back; guest orders
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CustomerMonth {
    pub month: NaiveDate,
    pub new_customers: i64,
    pub new_sales: Decimal,
    pub returning_customers: i64,
    pub returning_sales: Decimal,
    pub guest_orders: i64,
    pub guest_sales: Decimal,
}

/// Orders and average basket of retail or trade customers
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Basket {
    /// `retail` or `trade`
    pub audience: String,
    pub orders: i64,
    pub sales: Decimal,
    pub average: Decimal,
}

/// Orders and sales of one channel (online, or the till later)
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ChannelFigures {
    pub channel: String,
    pub orders: i64,
    pub sales: Decimal,
}

/// A promotion used in the period
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PromotionFigures {
    pub promotion_id: Uuid,
    pub name: Option<String>,
    pub kind: Option<String>,
    /// Orders it applied to
    pub orders: i64,
    /// What it saved customers
    pub discount: Decimal,
    /// Sales excl. VAT of those orders
    pub sales: Decimal,
    /// Daily sales of the products it priced, in the 28 days before it started
    #[sqlx(default)]
    pub daily_before: Option<Decimal>,
    /// ...and while it ran (up to today)
    #[sqlx(default)]
    pub daily_during: Option<Decimal>,
}

/// Loyalty points issued, redeemed and still to honour
#[derive(Debug, Default, Clone, Serialize)]
pub struct LoyaltyFigures {
    pub enabled: bool,
    /// Earned in the period
    pub issued: i64,
    /// Spent in the period
    pub redeemed: i64,
    /// Lapsed in the period
    pub expired: i64,
    /// Points on all cards today
    pub outstanding: i64,
    /// Customers with points today
    pub members: i64,
    /// What a point is worth when spent
    pub point_value: Decimal,
    /// `outstanding` × `point_value`
    pub liability: Decimal,
}

/// The marketing report
#[derive(Debug, Clone, Serialize)]
pub struct MarketingReport {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub seasonality: Vec<CategorySeason>,
    pub restock: Vec<RestockSuggestion>,
    pub top_customers: Vec<TopCustomer>,
    pub top_companies: Vec<TopCompany>,
    pub customer_months: Vec<CustomerMonth>,
    pub baskets: Vec<Basket>,
    pub channels: Vec<ChannelFigures>,
    pub promotions: Vec<PromotionFigures>,
    pub loyalty: LoyaltyFigures,
}

/// Quantities a product sold in the restocking windows
#[derive(sqlx::FromRow)]
struct ProductDemand {
    product_id: Uuid,
    title: String,
    sku: Option<String>,
    last_year_coming: i64,
    last_year_before: i64,
    recent: i64,
}

/// The sections of the marketing report to compute (the others stay empty)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarketingParts {
    pub seasonality: bool,
    pub restock: bool,
    /// Best customers and business accounts
    pub top: bool,
    /// New and returning customers, baskets and channels
    pub customers: bool,
    pub promotions: bool,
    pub loyalty: bool,
}

impl MarketingParts {
    /// Every section
    pub const ALL: MarketingParts = MarketingParts {
        seasonality: true,
        restock: true,
        top: true,
        customers: true,
        promotions: true,
        loyalty: true,
    };

    /// Whether any section is asked for
    pub fn any(&self) -> bool {
        *self != MarketingParts::default()
    }
}

impl AnalyticsService {
    /// The marketing report for the filter's period
    pub async fn marketing(&self, filter: &ReportFilter) -> Result<MarketingReport> {
        self.marketing_parts(filter, MarketingParts::ALL).await
    }

    /// Some sections of the marketing report, the others left empty (for the
    /// dashboard, which shows only the widgets a staff member picked)
    pub async fn marketing_parts(
        &self,
        filter: &ReportFilter,
        parts: MarketingParts,
    ) -> Result<MarketingReport> {
        let (from, to) = self.period(filter);
        let mut report = MarketingReport {
            from,
            to,
            seasonality: Vec::new(),
            restock: Vec::new(),
            top_customers: Vec::new(),
            top_companies: Vec::new(),
            customer_months: Vec::new(),
            baskets: Vec::new(),
            channels: Vec::new(),
            promotions: Vec::new(),
            loyalty: LoyaltyFigures::default(),
        };
        if parts.seasonality {
            report.seasonality = self.seasonality(filter).await?;
        }
        if parts.restock {
            report.restock = self.restock(filter).await?;
        }
        if parts.top {
            report.top_customers = self.top_customers(filter, from, to).await?;
            report.top_companies = self.top_companies(filter, from, to).await?;
        }
        if parts.customers {
            report.customer_months = self.customer_months(filter, from, to).await?;
            report.baskets = self.baskets(filter, from, to).await?;
            report.channels = self.channels(filter, from, to).await?;
        }
        if parts.promotions {
            report.promotions = self.promotions(filter, from, to).await?;
        }
        if parts.loyalty {
            report.loyalty = self.loyalty(from, to).await?;
        }
        Ok(report)
    }

    /// Seasonal profile of each top-level category over the 24 complete
    /// months before this one
    pub async fn seasonality(&self, filter: &ReportFilter) -> Result<Vec<CategorySeason>> {
        let current = first_of_month(self.today());
        let start = current - Months::new(24);
        let sql = format!(
            "WITH RECURSIVE tree AS ( \
               SELECT id, id AS root FROM product_categories WHERE parent_id IS NULL \
               UNION ALL SELECT c.id, t.root FROM product_categories c JOIN tree t ON c.parent_id = t.id), \
             product_roots AS ( \
               SELECT DISTINCT r.product_id, t.root FROM product_category_relations r \
               JOIN tree t ON t.id = r.category_id) \
             SELECT pr.root, MAX(c.name), {ORDER_MONTH} AS month, COALESCE(SUM({LINE_NET}), 0) \
             FROM orders o JOIN order_items oi ON oi.order_id = o.id \
             LEFT JOIN order_line_pricing lp ON lp.order_item_id = oi.id \
             JOIN product_roots pr ON pr.product_id = oi.product_id \
             JOIN product_categories c ON c.id = pr.root \
             WHERE {} AND {} GROUP BY pr.root, month",
            sales_scope(),
            placed_between(4, 5)
        );
        let rows: Vec<(Uuid, String, NaiveDate, Decimal)> = self
            .scoped(&sql, filter)
            .bind(start)
            .bind(current)
            .fetch_all(&self.pool)
            .await?;
        let mut categories: BTreeMap<Uuid, (String, BTreeMap<NaiveDate, Decimal>)> =
            BTreeMap::new();
        for (id, name, month, sales) in rows {
            let entry = categories
                .entry(id)
                .or_insert_with(|| (name, BTreeMap::new()));
            entry.1.insert(month, sales);
        }
        let mut seasons: Vec<CategorySeason> = categories
            .into_iter()
            .map(|(category_id, (name, months))| CategorySeason {
                category_id,
                name,
                sales: months.values().copied().sum(),
                index: seasonal_index(&months, start, current),
            })
            .collect();
        seasons.sort_by_key(|s| Reverse(s.sales));
        Ok(seasons)
    }

    /// Products whose sales rose in the coming weeks last year, with what
    /// to order now
    pub async fn restock(&self, filter: &ReportFilter) -> Result<Vec<RestockSuggestion>> {
        let demand = self.product_demand(filter).await?;
        let ids: Vec<Uuid> = demand.iter().map(|d| d.product_id).collect();
        let stock = self.branch_stock(&ids, filter.location_id).await?;
        let mut suggestions: Vec<RestockSuggestion> = demand
            .into_iter()
            .map(|d| {
                let branches = stock.get(&d.product_id).cloned().unwrap_or_default();
                let available: i64 = branches.iter().map(|b| b.available.max(0)).sum();
                let expected = expected_demand(d.last_year_coming, d.last_year_before, d.recent);
                RestockSuggestion {
                    product_id: d.product_id,
                    title: d.title,
                    sku: d.sku,
                    last_year_coming: d.last_year_coming,
                    last_year_before: d.last_year_before,
                    recent: d.recent,
                    expected,
                    available,
                    suggested: (expected - available).max(0),
                    branches,
                }
            })
            .collect();
        suggestions.sort_by_key(|s| Reverse((s.suggested, s.expected)));
        suggestions.truncate(25);
        Ok(suggestions)
    }

    /// Quantities per product last year (coming weeks and the weeks before)
    /// and in the last weeks, for products that sold more in the coming weeks
    async fn product_demand(&self, filter: &ReportFilter) -> Result<Vec<ProductDemand>> {
        let today = self.today();
        let weeks = Duration::days(7 * RESTOCK_WEEKS);
        let year_ago = today - Months::new(12);
        let sql = format!(
            "SELECT oi.product_id, MAX(oi.title) AS title, MAX(oi.sku) AS sku, \
                    COALESCE(SUM(oi.quantity) FILTER (WHERE {coming}), 0)::bigint AS last_year_coming, \
                    COALESCE(SUM(oi.quantity) FILTER (WHERE {before}), 0)::bigint AS last_year_before, \
                    COALESCE(SUM(oi.quantity) FILTER (WHERE {recent}), 0)::bigint AS recent \
             FROM orders o JOIN order_items oi ON oi.order_id = o.id \
             WHERE {scope} AND oi.product_id IS NOT NULL AND {window} \
             GROUP BY oi.product_id \
             HAVING COALESCE(SUM(oi.quantity) FILTER (WHERE {coming}), 0) \
                  > COALESCE(SUM(oi.quantity) FILTER (WHERE {before}), 0)",
            coming = placed_between(5, 6),
            before = placed_between(4, 5),
            recent = placed_between(7, 8),
            scope = sales_scope(),
            window = placed_between(4, 8),
        );
        let rows = self
            .scoped(&sql, filter)
            .bind(year_ago - weeks)
            .bind(year_ago)
            .bind(year_ago + weeks)
            .bind(today - weeks)
            .bind(next_day(today))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Available stock of the products per branch (one branch if given)
    async fn branch_stock(
        &self,
        products: &[Uuid],
        location: Option<Uuid>,
    ) -> Result<HashMap<Uuid, Vec<BranchStock>>> {
        let rows: Vec<(Uuid, Uuid, String, i64)> = sqlx::query_as(
            "SELECT lv.product_id, l.id, l.name, SUM(lv.available_quantity)::bigint \
             FROM inventory_levels lv JOIN inventory_locations l ON l.id = lv.location_id \
             WHERE lv.product_id = ANY($1) AND ($2::uuid IS NULL OR lv.location_id = $2) \
             GROUP BY lv.product_id, l.id, l.name ORDER BY l.name",
        )
        .bind(products)
        .bind(location)
        .fetch_all(&self.pool)
        .await?;
        let mut stock: HashMap<Uuid, Vec<BranchStock>> = HashMap::new();
        for (product, location_id, name, available) in rows {
            stock.entry(product).or_default().push(BranchStock {
                location_id,
                name,
                available,
            });
        }
        Ok(stock)
    }

    /// The 20 customers who bought most in the period
    async fn top_customers(
        &self,
        filter: &ReportFilter,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<TopCustomer>> {
        let sql = format!(
            "SELECT o.customer_id, TRIM(MAX(COALESCE(c.first_name, '') || ' ' || COALESCE(c.last_name, ''))) AS name, \
                    MAX(c.email) AS email, co.id AS company_id, MAX(co.name) AS company_name, \
                    COUNT(*)::bigint AS orders, COALESCE(SUM(o.total - o.tax_total), 0) AS sales, \
                    MIN(o.created_at) AS first_order, MAX(o.created_at) AS last_order \
             FROM orders o JOIN customers c ON c.id = o.customer_id \
             LEFT JOIN companies co ON co.id = o.company_id \
             WHERE {} AND {} GROUP BY o.customer_id, co.id ORDER BY sales DESC LIMIT 20",
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

    /// The 10 business accounts that bought most in the period
    async fn top_companies(
        &self,
        filter: &ReportFilter,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<TopCompany>> {
        let sql = format!(
            "SELECT co.id AS company_id, MAX(co.name) AS name, \
                    COUNT(DISTINCT o.customer_id)::bigint AS buyers, COUNT(*)::bigint AS orders, \
                    COALESCE(SUM(o.total - o.tax_total), 0) AS sales, MAX(o.created_at) AS last_order \
             FROM orders o JOIN companies co ON co.id = o.company_id \
             WHERE {} AND {} GROUP BY co.id ORDER BY sales DESC LIMIT 10",
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

    /// Per month: customers placing their first order ever, customers coming
    /// back, and guest orders
    async fn customer_months(
        &self,
        filter: &ReportFilter,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<CustomerMonth>> {
        let sql = format!(
            "WITH firsts AS ( \
               SELECT o.customer_id, date_trunc('month', MIN(o.created_at) AT TIME ZONE $3)::date AS first \
               FROM orders o WHERE NOT o.test AND o.customer_id IS NOT NULL \
                 AND o.status::text NOT IN ('pending', 'cancelled', 'refunded') GROUP BY 1) \
             SELECT {ORDER_MONTH} AS month, \
                    COUNT(DISTINCT o.customer_id) FILTER (WHERE f.first = {ORDER_MONTH}) AS new_customers, \
                    COALESCE(SUM(o.total - o.tax_total) FILTER (WHERE f.first = {ORDER_MONTH}), 0) AS new_sales, \
                    COUNT(DISTINCT o.customer_id) FILTER (WHERE f.first < {ORDER_MONTH}) AS returning_customers, \
                    COALESCE(SUM(o.total - o.tax_total) FILTER (WHERE f.first < {ORDER_MONTH}), 0) AS returning_sales, \
                    COUNT(*) FILTER (WHERE o.customer_id IS NULL) AS guest_orders, \
                    COALESCE(SUM(o.total - o.tax_total) FILTER (WHERE o.customer_id IS NULL), 0) AS guest_sales \
             FROM orders o LEFT JOIN firsts f ON f.customer_id = o.customer_id \
             WHERE {} AND {} GROUP BY 1 ORDER BY 1",
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

    /// Orders and average basket of retail and trade customers
    async fn baskets(
        &self,
        filter: &ReportFilter,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<Basket>> {
        let sql = format!(
            "SELECT CASE WHEN o.company_id IS NULL THEN 'retail' ELSE 'trade' END AS audience, \
                    COUNT(*)::bigint AS orders, COALESCE(SUM(o.total - o.tax_total), 0) AS sales, \
                    ROUND(COALESCE(AVG(o.total - o.tax_total), 0), 2) AS average \
             FROM orders o WHERE {} AND {} GROUP BY 1 ORDER BY 1",
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

    /// Orders and sales per channel
    async fn channels(
        &self,
        filter: &ReportFilter,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<ChannelFigures>> {
        let sql = format!(
            "SELECT COALESCE(o.metadata->>'source', 'online') AS channel, COUNT(*)::bigint AS orders, \
                    COALESCE(SUM(o.total - o.tax_total), 0) AS sales \
             FROM orders o WHERE {} AND {} GROUP BY 1 ORDER BY sales DESC",
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

    /// The 20 promotions that saved customers most in the period, with the
    /// daily sales of their products before and during
    async fn promotions(
        &self,
        filter: &ReportFilter,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<PromotionFigures>> {
        let sql = format!(
            "SELECT u.promotion_id, MAX(u.name) AS name, MAX(u.kind) AS kind, COUNT(*)::bigint AS orders, \
                    COALESCE(SUM(u.discount), 0) AS discount, COALESCE(SUM(o.total - o.tax_total), 0) AS sales \
             FROM promotion_uses u JOIN orders o ON o.id = u.order_id \
             WHERE {} AND {} GROUP BY u.promotion_id ORDER BY discount DESC LIMIT 20",
            sales_scope(),
            placed_between(4, 5)
        );
        let mut rows: Vec<PromotionFigures> = self
            .scoped(&sql, filter)
            .bind(from)
            .bind(next_day(to))
            .fetch_all(&self.pool)
            .await?;
        let ids: Vec<Uuid> = rows.iter().map(|p| p.promotion_id).collect();
        let lift = self.promotion_lift(filter, &ids).await?;
        for row in &mut rows {
            if let Some((before, during)) = lift.get(&row.promotion_id) {
                row.daily_before = Some(*before);
                row.daily_during = Some(*during);
            }
        }
        Ok(rows)
    }

    /// Daily sales of the products each promotion priced: the 28 days before
    /// it started, and while it ran (up to now). Promotions without a start
    /// date, or that no order line carries, are left out.
    async fn promotion_lift(
        &self,
        filter: &ReportFilter,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, (Decimal, Decimal)>> {
        let sql = format!(
            "WITH promo AS ( \
               SELECT id, starts_at AS s, LEAST(COALESCE(ends_at, NOW()), NOW()) AS e FROM promotions \
               WHERE id = ANY($3) AND starts_at IS NOT NULL AND starts_at < NOW()), \
             items AS ( \
               SELECT DISTINCT promo.id, lp.product_id FROM promo JOIN order_line_pricing lp \
                 ON lp.promotions @> jsonb_build_array(jsonb_build_object('id', promo.id::text))) \
             SELECT promo.id, \
                    ROUND(COALESCE(SUM({LINE_NET}) FILTER (WHERE o.created_at < promo.s), 0) / 28, 2), \
                    ROUND(COALESCE(SUM({LINE_NET}) FILTER (WHERE o.created_at >= promo.s), 0) \
                      / GREATEST(EXTRACT(EPOCH FROM promo.e - promo.s) / 86400, 1), 2) \
             FROM promo JOIN items ON items.id = promo.id \
             JOIN order_items oi ON oi.product_id = items.product_id \
             JOIN orders o ON o.id = oi.order_id \
             LEFT JOIN order_line_pricing lp ON lp.order_item_id = oi.id \
             WHERE {} \
               AND o.created_at >= promo.s - INTERVAL '28 days' AND o.created_at < promo.e \
             GROUP BY promo.id, promo.s, promo.e",
            sales_scope()
        );
        let rows: Vec<(Uuid, Decimal, Decimal)> = self
            .filtered(&sql, filter)
            .bind(ids)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(id, before, during)| (id, (before, during)))
            .collect())
    }

    /// Points earned, spent and lapsed in the period; points on cards today
    async fn loyalty(&self, from: NaiveDate, to: NaiveDate) -> Result<LoyaltyFigures> {
        let (enabled, point_value): (bool, Decimal) =
            sqlx::query_as("SELECT enabled, point_value FROM loyalty_settings LIMIT 1")
                .fetch_optional(&self.pool)
                .await?
                .unwrap_or((false, Decimal::ZERO));
        let (issued, redeemed, expired, outstanding, members): (i64, i64, i64, i64, i64) =
            sqlx::query_as(
                "SELECT COALESCE(SUM(points) FILTER (WHERE kind = 'earn' AND period), 0)::bigint, \
                        COALESCE(-SUM(points) FILTER (WHERE kind = 'redeem' AND period), 0)::bigint, \
                        COALESCE(-SUM(points) FILTER (WHERE kind = 'expire' AND period), 0)::bigint, \
                        COALESCE(SUM(points), 0)::bigint, \
                        (SELECT COUNT(*) FROM (SELECT customer_id FROM loyalty_entries \
                           GROUP BY 1 HAVING SUM(points) > 0) m) \
                 FROM (SELECT points, kind, created_at >= ($1::date::timestamp AT TIME ZONE $3) \
                         AND created_at < ($2::date::timestamp AT TIME ZONE $3) AS period \
                       FROM loyalty_entries) e",
            )
            .bind(from)
            .bind(next_day(to))
            .bind(self.timezone.name())
            .fetch_one(&self.pool)
            .await?;
        Ok(LoyaltyFigures {
            enabled,
            issued,
            redeemed,
            expired,
            outstanding,
            members,
            point_value,
            liability: (Decimal::from(outstanding.max(0)) * point_value).round_dp(2),
        })
    }
}

/// January to December: each calendar month's average sales within the
/// months from `start` (included) to `end` (excluded), against the average
/// of the twelve (100); months without sales count as zero
fn seasonal_index(
    sales: &BTreeMap<NaiveDate, Decimal>,
    start: NaiveDate,
    end: NaiveDate,
) -> [i64; 12] {
    let mut sums = [Decimal::ZERO; 12];
    let mut counts = [0u32; 12];
    let mut month = start;
    while month < end {
        let i = month.month0() as usize;
        sums[i] += sales.get(&month).copied().unwrap_or_default();
        counts[i] += 1;
        month = month + Months::new(1);
    }
    let averages: Vec<Decimal> = sums
        .iter()
        .zip(counts)
        .map(|(sum, n)| {
            if n == 0 {
                Decimal::ZERO
            } else {
                *sum / Decimal::from(n)
            }
        })
        .collect();
    let mean: Decimal = averages.iter().copied().sum::<Decimal>() / Decimal::from(12);
    let mut index = [0i64; 12];
    if mean > Decimal::ZERO {
        for (slot, average) in index.iter_mut().zip(&averages) {
            *slot = (*average / mean * Decimal::from(100))
                .round()
                .to_i64()
                .unwrap_or(0);
        }
    }
    index
}

/// Units expected in the coming weeks: last year's, times how the last weeks
/// compare with the same weeks last year (between ×0.5 and ×2; ×1 when
/// those weeks sold nothing last year)
fn expected_demand(last_year_coming: i64, last_year_before: i64, recent: i64) -> i64 {
    let coming = Decimal::from(last_year_coming);
    let trend = if last_year_before > 0 {
        (Decimal::from(recent) / Decimal::from(last_year_before))
            .max(Decimal::new(5, 1))
            .min(Decimal::from(2))
    } else {
        Decimal::ONE
    };
    (coming * trend).ceil().to_i64().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn month(s: &str) -> NaiveDate {
        format!("{s}-01").parse().unwrap()
    }

    #[test]
    fn the_seasonal_index_compares_each_month_with_the_average_month() {
        // Two years: every month sells 100 except June (400) and nothing in
        // January of the first year
        let start = month("2024-11");
        let end = month("2026-11");
        let mut sales = BTreeMap::new();
        let mut m = start;
        while m < end {
            let amount = match m.month() {
                6 => 400,
                1 if m.year() == 2025 => 0,
                _ => 100,
            };
            sales.insert(m, Decimal::from(amount));
            m = m + Months::new(1);
        }
        let index = seasonal_index(&sales, start, end);
        // Average month: (10 × 100 + 50 + 400) / 12 = 120.83
        assert_eq!(index[5], 331);
        assert_eq!(index[0], 41);
        assert_eq!(index[2], 83);
    }

    #[test]
    fn no_sales_gives_a_flat_zero_index() {
        let index = seasonal_index(&BTreeMap::new(), month("2025-01"), month("2026-01"));
        assert_eq!(index, [0; 12]);
    }

    #[test]
    fn expected_demand_follows_this_years_trend_within_limits() {
        // Last year 100 coming after 50; this year's last weeks 60 against 50
        assert_eq!(expected_demand(100, 50, 60), 120);
        // A collapse is cushioned at half, a boom capped at double
        assert_eq!(expected_demand(100, 50, 5), 50);
        assert_eq!(expected_demand(100, 50, 500), 200);
        // Nothing sold last year before the season: last year's quantity
        assert_eq!(expected_demand(7, 0, 3), 7);
    }
}
