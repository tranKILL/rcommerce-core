//! Price and promotion history
//!
//! - `order_line_pricing`: what each order line was charged and why (catalogue
//!   price, negotiated price and its list, catalogue promotions with what each
//!   saved, the line's share of cart discounts, the cost price), written when
//!   the order is placed.
//! - `promotion_uses`: what each promotion saved each order, under its name,
//!   so a deleted promotion keeps its history.
//! - `price_list_rule_history`: every rule price lists had, from when to when,
//!   kept by database triggers.
//!
//! Staff read it per product, per customer and per promotion.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::types::Json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::models::{Cart, CartItem};
use crate::services::price_list_service::{category_paths, pricing_context};
use crate::services::promotion_service::{promo_price, Promotion};
use crate::{Error, Result};

/// What one promotion saved on one order line
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineSaving {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub saving: Decimal,
}

impl LineSaving {
    fn of(promotion: &Promotion, saving: Decimal) -> Self {
        Self {
            id: promotion.id,
            name: promotion.name.clone(),
            kind: promotion.kind.clone(),
            saving,
        }
    }
}

/// Each catalogue promotion's share of what a line saves against its
/// negotiated price: a combinable chain step by step, in the order it is
/// applied (fixed price, percentages, amounts off); a single promotion all of
/// it. The shares add up to `(negotiated - unit) × quantity`.
pub fn chain_savings(
    promotions: &[&Promotion],
    negotiated: Decimal,
    unit: Decimal,
    quantity: i32,
) -> Vec<(Uuid, Decimal)> {
    let quantity = Decimal::from(quantity);
    let total = ((negotiated - unit) * quantity).max(Decimal::ZERO);
    let mut ordered = promotions.to_vec();
    ordered.sort_by_key(|p| {
        ["fixed_price", "percent_off", "amount_off"]
            .iter()
            .position(|k| *k == p.kind)
    });
    let (mut price, mut given) = (negotiated, Decimal::ZERO);
    let last = ordered.len().saturating_sub(1);
    ordered
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let saving = if i == last {
                total - given
            } else {
                let next = promo_price(p, price);
                let step = ((price - next) * quantity).min(total - given);
                price = next;
                step
            };
            given += saving;
            (p.id, saving)
        })
        .collect()
}

/// An order just placed from a cart
pub struct PlacedOrder<'a> {
    pub order_id: Uuid,
    pub customer_id: Option<Uuid>,
    pub email: &'a str,
    pub cart: &'a Cart,
    pub items: &'a [CartItem],
    /// (cart line, order line)
    pub lines: &'a [(Uuid, Uuid)],
}

/// What the cart model doesn't carry about a line's price
#[derive(Debug, sqlx::FromRow)]
struct LineFacts {
    id: Uuid,
    price_list_id: Option<Uuid>,
    price_list_name: Option<String>,
    negotiated_price: Option<Decimal>,
    cost_price: Option<Decimal>,
}

fn gift_promotion(item: &CartItem) -> Option<Uuid> {
    let attributes = item.custom_attributes.as_ref()?;
    attributes.get("gift_promotion_id")?.as_str()?.parse().ok()
}

fn is_bundle_component(item: &CartItem) -> bool {
    let flag = |a: &serde_json::Value| a.get("is_bundle_component").and_then(|v| v.as_bool());
    item.custom_attributes
        .as_ref()
        .and_then(flag)
        .unwrap_or(false)
}

/// What each promotion saved on a line: the gift's catalogue value, or the
/// catalogue promotions' shares
fn line_savings(
    item: &CartItem,
    facts: Option<&LineFacts>,
    promotions: &HashMap<Uuid, Promotion>,
) -> Vec<LineSaving> {
    if let Some(gift) = gift_promotion(item).and_then(|id| promotions.get(&id)) {
        return vec![LineSaving::of(
            gift,
            item.original_price * Decimal::from(item.quantity),
        )];
    }
    let negotiated = facts
        .and_then(|f| f.negotiated_price)
        .unwrap_or(item.original_price);
    let applied: Vec<&Promotion> = item
        .promotion_ids
        .iter()
        .filter_map(|id| promotions.get(id))
        .collect();
    chain_savings(&applied, negotiated, item.unit_price, item.quantity)
        .into_iter()
        .filter_map(|(id, saving)| promotions.get(&id).map(|p| LineSaving::of(p, saving)))
        .collect()
}

async fn used_promotions(
    conn: &mut PgConnection,
    placed: &PlacedOrder<'_>,
) -> Result<HashMap<Uuid, Promotion>> {
    let mut ids: Vec<Uuid> = placed
        .items
        .iter()
        .flat_map(|i| i.promotion_ids.iter().copied())
        .collect();
    ids.extend(placed.items.iter().filter_map(gift_promotion));
    ids.extend(
        placed
            .cart
            .promotions
            .0
            .iter()
            .map(|p| p.id)
            .filter(|id| !id.is_nil()),
    );
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<Promotion> = sqlx::query_as("SELECT * FROM promotions WHERE id = ANY($1)")
        .bind(&ids)
        .fetch_all(conn)
        .await?;
    Ok(rows.into_iter().map(|p| (p.id, p)).collect())
}

async fn line_facts(conn: &mut PgConnection, cart_id: Uuid) -> Result<HashMap<Uuid, LineFacts>> {
    let rows: Vec<LineFacts> = sqlx::query_as(
        "SELECT ci.id, ci.price_list_id, pl.name AS price_list_name, ci.negotiated_price, \
                COALESCE(v.cost_price, p.cost_price)::numeric AS cost_price \
         FROM cart_items ci \
         LEFT JOIN price_lists pl ON pl.id = ci.price_list_id \
         LEFT JOIN products p ON p.id = ci.product_id \
         LEFT JOIN product_variants v ON v.id = ci.variant_id \
         WHERE ci.cart_id = $1",
    )
    .bind(cart_id)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(|f| (f.id, f)).collect())
}

/// Record an order's line prices and what each promotion saved it, when it
/// is placed
pub async fn record_order(conn: &mut PgConnection, placed: &PlacedOrder<'_>) -> Result<()> {
    let promotions = used_promotions(&mut *conn, placed).await?;
    let facts = line_facts(&mut *conn, placed.cart.id).await?;
    let company = match placed.customer_id {
        Some(customer) => pricing_context(&mut *conn, customer).await?.company_id,
        None => None,
    };
    if company.is_some() {
        sqlx::query("UPDATE orders SET company_id = $2 WHERE id = $1")
            .bind(placed.order_id)
            .bind(company)
            .execute(&mut *conn)
            .await?;
    }
    let mut savings: HashMap<Uuid, Decimal> = HashMap::new();
    for (cart_line, order_line) in placed.lines {
        let Some(item) = placed
            .items
            .iter()
            .find(|i| i.id == *cart_line && !is_bundle_component(i))
        else {
            continue;
        };
        let line = line_savings(item, facts.get(cart_line), &promotions);
        for saving in &line {
            *savings.entry(saving.id).or_default() += saving.saving;
        }
        let row = LineRow {
            order_line: *order_line,
            company,
            item,
            facts: facts.get(cart_line),
            savings: line,
        };
        insert_line(&mut *conn, placed, &row).await?;
    }
    for promotion in placed.cart.promotions.0.iter().filter(|p| !p.id.is_nil()) {
        *savings.entry(promotion.id).or_default() += promotion.discount;
    }
    record_uses(conn, placed, &promotions, &savings).await
}

struct LineRow<'a> {
    order_line: Uuid,
    company: Option<Uuid>,
    item: &'a CartItem,
    facts: Option<&'a LineFacts>,
    savings: Vec<LineSaving>,
}

async fn insert_line(
    conn: &mut PgConnection,
    placed: &PlacedOrder<'_>,
    row: &LineRow<'_>,
) -> Result<()> {
    let item = row.item;
    let negotiated = row
        .facts
        .and_then(|f| f.negotiated_price)
        .unwrap_or(item.original_price);
    sqlx::query(
        "INSERT INTO order_line_pricing (order_item_id, order_id, product_id, variant_id, customer_id, company_id, \
             quantity, base_price, negotiated_price, unit_price, discount, cost_price, price_list_id, \
             price_list_name, promotions) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) ON CONFLICT DO NOTHING",
    )
    .bind(row.order_line)
    .bind(placed.order_id)
    .bind(item.product_id)
    .bind(item.variant_id)
    .bind(placed.customer_id)
    .bind(row.company)
    .bind(item.quantity)
    .bind(item.original_price.max(item.unit_price))
    .bind(negotiated.max(item.unit_price))
    .bind(item.unit_price)
    .bind(item.discount_amount)
    .bind(row.facts.and_then(|f| f.cost_price))
    .bind(row.facts.and_then(|f| f.price_list_id))
    .bind(row.facts.and_then(|f| f.price_list_name.clone()))
    .bind(Json(&row.savings))
    .execute(conn)
    .await?;
    Ok(())
}

/// The order's promotion uses (usage limits) with what each saved
async fn record_uses(
    conn: &mut PgConnection,
    placed: &PlacedOrder<'_>,
    promotions: &HashMap<Uuid, Promotion>,
    savings: &HashMap<Uuid, Decimal>,
) -> Result<()> {
    let used: Vec<&Promotion> = promotions.values().collect();
    if used.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = used.iter().map(|p| p.id).collect();
    let names: Vec<String> = used.iter().map(|p| p.name.clone()).collect();
    let kinds: Vec<String> = used.iter().map(|p| p.kind.clone()).collect();
    let amounts: Vec<Decimal> = used
        .iter()
        .map(|p| savings.get(&p.id).copied().unwrap_or_default())
        .collect();
    sqlx::query(
        "INSERT INTO promotion_uses (promotion_id, order_id, customer_id, email, name, kind, discount) \
         SELECT u.id, $5, $6, $7, u.name, u.kind, u.discount \
         FROM UNNEST($1::uuid[], $2::text[], $3::text[], $4::numeric[]) AS u(id, name, kind, discount) \
         ON CONFLICT DO NOTHING",
    )
    .bind(&ids)
    .bind(&names)
    .bind(&kinds)
    .bind(&amounts)
    .bind(placed.order_id)
    .bind(placed.customer_id)
    .bind(placed.email)
    .execute(conn)
    .await?;
    Ok(())
}

/// An order line as charged
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ChargedLine {
    pub order_id: Uuid,
    pub order_number: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub customer_id: Option<Uuid>,
    pub customer_name: Option<String>,
    pub company_name: Option<String>,
    pub product_id: Option<Uuid>,
    pub variant_id: Option<Uuid>,
    pub title: String,
    pub sku: Option<String>,
    pub quantity: i32,
    pub base_price: Decimal,
    pub negotiated_price: Decimal,
    pub unit_price: Decimal,
    /// The line's share of cart promotions, coupon and points
    pub discount: Decimal,
    /// What one unit finally cost, cart discounts included
    pub net_unit_price: Decimal,
    pub price_list_name: Option<String>,
    pub promotions: Json<Vec<LineSaving>>,
}

/// Which lines to read
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HistoryQuery {
    pub customer: Option<Uuid>,
    pub product: Option<Uuid>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
}

impl HistoryQuery {
    fn limit(&self) -> i64 {
        self.limit.unwrap_or(200).clamp(1, 1000)
    }
}

async fn charged_lines(conn: &mut PgConnection, q: &HistoryQuery) -> Result<Vec<ChargedLine>> {
    Ok(sqlx::query_as(
        "SELECT l.order_id, o.order_number, o.status::text AS status, l.created_at, l.customer_id, \
                NULLIF(TRIM(CONCAT_WS(' ', c.first_name, c.last_name)), '') AS customer_name, \
                co.name AS company_name, l.product_id, l.variant_id, oi.title, oi.sku, l.quantity, \
                l.base_price, l.negotiated_price, l.unit_price, l.discount, \
                ROUND((l.unit_price * l.quantity - l.discount) / GREATEST(l.quantity, 1), 4) AS net_unit_price, \
                l.price_list_name, l.promotions \
         FROM order_line_pricing l \
         JOIN orders o ON o.id = l.order_id \
         JOIN order_items oi ON oi.id = l.order_item_id \
         LEFT JOIN customers c ON c.id = l.customer_id \
         LEFT JOIN companies co ON co.id = l.company_id \
         WHERE ($1::uuid IS NULL OR l.product_id = $1) AND ($2::uuid IS NULL OR l.customer_id = $2) \
           AND ($3::timestamptz IS NULL OR l.created_at >= $3) AND ($4::timestamptz IS NULL OR l.created_at < $4) \
         ORDER BY l.created_at DESC, oi.title LIMIT $5",
    )
    .bind(q.product)
    .bind(q.customer)
    .bind(q.from)
    .bind(q.to)
    .bind(q.limit())
    .fetch_all(conn)
    .await?)
}

/// A catalogue price, from when
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CataloguePrice {
    pub variant_id: Option<Uuid>,
    pub variant_title: Option<String>,
    pub price: Decimal,
    pub valid_from: DateTime<Utc>,
}

/// A promotion that targets a product (live or not)
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PromotionPeriod {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub audience: String,
    pub value: Option<Decimal>,
    pub combinable: bool,
    pub is_active: bool,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
}

/// A price list rule as it was over a period (`valid_to` empty: in force)
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RulePeriod {
    pub price_list_id: Uuid,
    pub price_list_name: String,
    pub company_id: Option<Uuid>,
    pub company_name: Option<String>,
    pub customer_group_id: Option<Uuid>,
    pub group_name: Option<String>,
    pub list_active: bool,
    pub list_starts_at: Option<DateTime<Utc>>,
    pub list_ends_at: Option<DateTime<Utc>>,
    pub scope: String,
    pub target_id: Option<Uuid>,
    pub target_name: Option<String>,
    pub adjustment: String,
    pub value: Decimal,
    pub min_quantity: i32,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
    /// The price it gave the product, at the catalogue price when the
    /// period started (product history only)
    pub price: Option<Decimal>,
}

/// A product's price history
#[derive(Debug, Clone, Serialize)]
pub struct ProductPriceHistory {
    pub title: String,
    pub sku: Option<String>,
    pub catalogue: Vec<CataloguePrice>,
    pub promotions: Vec<PromotionPeriod>,
    pub negotiated: Vec<RulePeriod>,
    pub charged: Vec<ChargedLine>,
}

/// A customer's prices: lines they were charged, and the negotiated rules
/// in force for them
#[derive(Debug, Clone, Serialize)]
pub struct CustomerPriceHistory {
    pub name: String,
    pub email: String,
    pub company_name: Option<String>,
    pub lines: Vec<ChargedLine>,
    pub negotiated: Vec<RulePeriod>,
}

/// What a product and its variants can be targeted through
struct Targets {
    product: Uuid,
    variants: Vec<Uuid>,
    categories: Vec<Uuid>,
}

async fn targets_of(conn: &mut PgConnection, product: Uuid) -> Result<Targets> {
    let variants: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM product_variants WHERE product_id = $1")
            .bind(product)
            .fetch_all(&mut *conn)
            .await?;
    let paths = category_paths(&mut *conn, &[product]).await?;
    let categories: HashSet<Uuid> = paths.into_values().flatten().collect();
    Ok(Targets {
        product,
        variants,
        categories: categories.into_iter().collect(),
    })
}

const TARGETS: &str = "(scope = 'all' OR (scope = 'product' AND target_id = $1) \
     OR (scope = 'variant' AND target_id = ANY($2)) OR (scope = 'category' AND target_id = ANY($3)))";

async fn catalogue_history(conn: &mut PgConnection, product: Uuid) -> Result<Vec<CataloguePrice>> {
    Ok(sqlx::query_as(
        "SELECT h.variant_id, v.title AS variant_title, h.price, h.valid_from FROM price_history h \
         LEFT JOIN product_variants v ON v.id = h.variant_id \
         WHERE h.product_id = $1 ORDER BY h.valid_from DESC, h.id DESC LIMIT 500",
    )
    .bind(product)
    .fetch_all(conn)
    .await?)
}

async fn product_promotions(conn: &mut PgConnection, t: &Targets) -> Result<Vec<PromotionPeriod>> {
    let sql = format!(
        "SELECT id, name, kind, audience, value, combinable, is_active, starts_at, ends_at FROM promotions \
         WHERE {TARGETS} AND kind NOT IN ('spend_tiers', 'free_delivery') \
         ORDER BY COALESCE(starts_at, created_at) DESC LIMIT 200"
    );
    Ok(sqlx::query_as(&sql)
        .bind(t.product)
        .bind(&t.variants)
        .bind(&t.categories)
        .fetch_all(conn)
        .await?)
}

/// The rule periods that target a product, those of `customer`'s company
/// and group only when given; each with the price it gave
async fn product_rules(
    conn: &mut PgConnection,
    t: &Targets,
    q: &HistoryQuery,
) -> Result<Vec<RulePeriod>> {
    let sql = format!(
        "SELECT h.price_list_id, h.price_list_name, h.company_id, co.name AS company_name, h.customer_group_id, \
                g.name AS group_name, h.list_active, h.list_starts_at, h.list_ends_at, h.scope, h.target_id, \
                NULL::text AS target_name, h.adjustment, h.value, h.min_quantity, h.valid_from, h.valid_to, \
                GREATEST(ROUND(CASE h.adjustment WHEN 'fixed' THEN h.value \
                     WHEN 'percent_off' THEN b.price * (100 - h.value) / 100 ELSE b.price - h.value END, 2), 0) AS price \
         FROM price_list_rule_history h \
         LEFT JOIN companies co ON co.id = h.company_id \
         LEFT JOIN customer_groups g ON g.id = h.customer_group_id \
         LEFT JOIN LATERAL ( \
             SELECT ph.price FROM price_history ph \
             WHERE ph.product_id = $1 \
               AND ph.variant_id IS NOT DISTINCT FROM (CASE WHEN h.scope = 'variant' THEN h.target_id END) \
             ORDER BY (ph.valid_from <= h.valid_from) DESC, \
                      CASE WHEN ph.valid_from <= h.valid_from THEN ph.valid_from END DESC, ph.valid_from \
             LIMIT 1) b ON true \
         WHERE {TARGETS} \
           AND ($4::uuid IS NULL OR h.company_id = $4 OR h.customer_group_id = $5) \
           AND ($6::timestamptz IS NULL OR h.valid_to IS NULL OR h.valid_to > $6) \
           AND ($7::timestamptz IS NULL OR h.valid_from < $7) \
         ORDER BY h.valid_from DESC, h.id DESC LIMIT 500"
    );
    let context = match q.customer {
        Some(customer) => pricing_context(&mut *conn, customer).await?,
        None => Default::default(),
    };
    let company = context.company_id.or(q.customer.map(|_| Uuid::nil()));
    Ok(sqlx::query_as(&sql)
        .bind(t.product)
        .bind(&t.variants)
        .bind(&t.categories)
        .bind(company)
        .bind(context.customer_group_id)
        .bind(q.from)
        .bind(q.to)
        .fetch_all(conn)
        .await?)
}

/// A product's price history: catalogue prices, the promotions that target
/// it, negotiated rules over time and the prices actually charged
pub async fn product_history(
    conn: &mut PgConnection,
    product: Uuid,
    q: &HistoryQuery,
) -> Result<ProductPriceHistory> {
    let (title, sku): (String, Option<String>) =
        sqlx::query_as("SELECT title, sku FROM products WHERE id = $1")
            .bind(product)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| Error::not_found("Product not found"))?;
    let targets = targets_of(&mut *conn, product).await?;
    let lines = HistoryQuery {
        product: Some(product),
        ..q.clone()
    };
    Ok(ProductPriceHistory {
        title,
        sku,
        catalogue: catalogue_history(&mut *conn, product).await?,
        promotions: product_promotions(&mut *conn, &targets).await?,
        negotiated: product_rules(&mut *conn, &targets, q).await?,
        charged: charged_lines(conn, &lines).await?,
    })
}

/// The rules in force for a customer's company and group, with what they target
async fn customer_rules(
    conn: &mut PgConnection,
    company: Option<Uuid>,
    group: Option<Uuid>,
) -> Result<Vec<RulePeriod>> {
    Ok(sqlx::query_as(
        "SELECT h.price_list_id, h.price_list_name, h.company_id, co.name AS company_name, h.customer_group_id, \
                g.name AS group_name, h.list_active, h.list_starts_at, h.list_ends_at, h.scope, h.target_id, \
                COALESCE(p.title, vp.title || ' · ' || v.title, cat.name) AS target_name, h.adjustment, h.value, \
                h.min_quantity, h.valid_from, h.valid_to, NULL::numeric AS price \
         FROM price_list_rule_history h \
         LEFT JOIN companies co ON co.id = h.company_id \
         LEFT JOIN customer_groups g ON g.id = h.customer_group_id \
         LEFT JOIN products p ON h.scope = 'product' AND p.id = h.target_id \
         LEFT JOIN product_variants v ON h.scope = 'variant' AND v.id = h.target_id \
         LEFT JOIN products vp ON vp.id = v.product_id \
         LEFT JOIN product_categories cat ON h.scope = 'category' AND cat.id = h.target_id \
         WHERE h.valid_to IS NULL AND (h.company_id = $1 OR h.customer_group_id = $2) \
         ORDER BY h.price_list_name, h.scope, target_name, h.min_quantity",
    )
    .bind(company)
    .bind(group)
    .fetch_all(conn)
    .await?)
}

/// A customer's price history: what they were charged per product over time
/// (with where each price came from), and the negotiated rules in force
pub async fn customer_history(
    conn: &mut PgConnection,
    customer: Uuid,
    q: &HistoryQuery,
) -> Result<CustomerPriceHistory> {
    let (name, email): (String, String) = sqlx::query_as(
        "SELECT TRIM(CONCAT_WS(' ', first_name, last_name)), email FROM customers WHERE id = $1",
    )
    .bind(customer)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| Error::not_found("Customer not found"))?;
    let context = pricing_context(&mut *conn, customer).await?;
    let company_name: Option<String> = match context.company_id {
        Some(id) => {
            sqlx::query_scalar("SELECT name FROM companies WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?
        }
        None => None,
    };
    let lines = HistoryQuery {
        customer: Some(customer),
        ..q.clone()
    };
    Ok(CustomerPriceHistory {
        name,
        email,
        company_name,
        lines: charged_lines(&mut *conn, &lines).await?,
        negotiated: customer_rules(conn, context.company_id, context.customer_group_id).await?,
    })
}

/// One order a promotion was used on
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PromotionUse {
    pub order_id: Uuid,
    pub order_number: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub total: Decimal,
    pub customer_id: Option<Uuid>,
    pub customer_name: Option<String>,
    pub email: Option<String>,
    pub discount: Decimal,
}

/// A promotion's uses: totals, then the orders, newest first
#[derive(Debug, Clone, Serialize)]
pub struct PromotionUses {
    /// The promotion's name (as last used, once it is deleted)
    pub name: Option<String>,
    pub orders: i64,
    pub discount: Decimal,
    /// The orders' totals
    pub sales: Decimal,
    pub uses: Vec<PromotionUse>,
}

/// A promotion's uses, `per_page` orders from `page` (1-based)
pub async fn promotion_uses(
    conn: &mut PgConnection,
    promotion: Uuid,
    page: i64,
    per_page: i64,
) -> Result<PromotionUses> {
    let (name, orders, discount, sales): (Option<String>, i64, Decimal, Decimal) = sqlx::query_as(
        "SELECT COALESCE((SELECT name FROM promotions WHERE id = $1), MAX(u.name)), COUNT(*), \
                COALESCE(SUM(u.discount), 0), COALESCE(SUM(o.total), 0)::numeric \
         FROM promotion_uses u JOIN orders o ON o.id = u.order_id WHERE u.promotion_id = $1",
    )
    .bind(promotion)
    .fetch_one(&mut *conn)
    .await?;
    let uses = sqlx::query_as(
        "SELECT u.order_id, o.order_number, o.status::text AS status, u.created_at, o.total::numeric AS total, \
                u.customer_id, NULLIF(TRIM(CONCAT_WS(' ', c.first_name, c.last_name)), '') AS customer_name, \
                u.email, u.discount \
         FROM promotion_uses u JOIN orders o ON o.id = u.order_id LEFT JOIN customers c ON c.id = u.customer_id \
         WHERE u.promotion_id = $1 ORDER BY u.created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(promotion)
    .bind(per_page)
    .bind((page.max(1) - 1) * per_page)
    .fetch_all(conn)
    .await?;
    Ok(PromotionUses {
        name,
        orders,
        discount,
        sales,
        uses,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn promotion(kind: &str, value: i64, combinable: bool) -> Promotion {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(), "name": kind, "kind": kind, "audience": "both", "scope": "all",
            "target_id": null, "value": value.to_string(), "buy_quantity": null, "get_quantity": null,
            "tiers": null, "min_amount": null, "min_quantity": null, "first_order_only": false,
            "max_uses": null, "max_uses_per_customer": null, "combinable": combinable,
            "gift_product_id": null, "gift_quantity": 1, "starts_at": null, "ends_at": null,
            "is_active": true, "created_at": Utc::now(), "updated_at": Utc::now()
        }))
        .unwrap()
    }

    #[test]
    fn a_single_promotion_takes_the_whole_saving() {
        let p = promotion("percent_off", 20, false);
        let savings = chain_savings(&[&p], Decimal::from(10), Decimal::from(8), 3);
        assert_eq!(savings, vec![(p.id, Decimal::from(6))]);
    }

    #[test]
    fn a_chain_shares_the_saving_in_the_order_applied() {
        let amount = promotion("amount_off", 1, true);
        let percent = promotion("percent_off", 10, true);
        // 20 → -10 % = 18 → -1 = 17, on 2 units
        let savings = chain_savings(
            &[&amount, &percent],
            Decimal::from(20),
            Decimal::from(17),
            2,
        );
        assert_eq!(
            savings,
            vec![
                (percent.id, Decimal::from(4)),
                (amount.id, Decimal::from(2))
            ]
        );
    }

    #[test]
    fn no_saving_without_a_lower_price() {
        let p = promotion("amount_off", 1, false);
        assert_eq!(
            chain_savings(&[&p], Decimal::from(5), Decimal::from(5), 1),
            vec![(p.id, Decimal::ZERO)]
        );
        assert!(chain_savings(&[], Decimal::from(5), Decimal::from(4), 1).is_empty());
    }
}
