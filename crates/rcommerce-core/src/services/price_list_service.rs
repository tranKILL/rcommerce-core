//! Price lists: negotiated prices for a customer group or one company.
//!
//! A price list targets one customer group (every company in it, e.g. all
//! trade customers) or one company, and can be limited in time. Its rules
//! apply to every product, a category (and its sub-categories), a product or a
//! variant, from a minimum quantity: a fixed price, a percentage off or an
//! amount off the catalogue price. When several rules apply, the customer pays
//! the lowest price. Prices are excluding VAT, like catalogue prices.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::services::company_service::PricingContext;
use crate::services::promotion_service::{
    catalogue_price, live_promotions, AppliedPromotion, Buyer, CatalogueItem, Promotion,
};
use crate::{Error, Result};

/// A price list
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PriceList {
    pub id: Uuid,
    pub name: String,
    /// The customer group it applies to (or `company_id`)
    pub customer_group_id: Option<Uuid>,
    /// The company it applies to (or `customer_group_id`)
    pub company_id: Option<Uuid>,
    pub is_active: bool,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A rule of a price list
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PriceRule {
    pub id: Uuid,
    pub price_list_id: Uuid,
    /// `all`, `category`, `product` or `variant`
    pub scope: String,
    /// The category, product or variant (`None` for `all`)
    pub target_id: Option<Uuid>,
    /// `fixed` (price), `percent_off` or `amount_off`
    pub adjustment: String,
    pub value: Decimal,
    /// The rule applies from this quantity on a cart line
    pub min_quantity: i32,
    pub created_at: DateTime<Utc>,
}

/// A new price list
#[derive(Debug, Clone, Deserialize)]
pub struct NewPriceList {
    pub name: String,
    #[serde(default)]
    pub customer_group_id: Option<Uuid>,
    #[serde(default)]
    pub company_id: Option<Uuid>,
    #[serde(default)]
    pub starts_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub ends_at: Option<DateTime<Utc>>,
}

/// Changes to a price list; `None` leaves a field as it is
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PriceListUpdate {
    pub name: Option<String>,
    pub is_active: Option<bool>,
    /// `null` removes the start date
    #[serde(default, deserialize_with = "present")]
    pub starts_at: Option<Option<DateTime<Utc>>>,
    /// `null` removes the end date
    #[serde(default, deserialize_with = "present")]
    pub ends_at: Option<Option<DateTime<Utc>>>,
}

/// A field that is present, possibly `null` (an absent field stays `None`)
fn present<'de, D, T>(deserializer: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A new rule
#[derive(Debug, Clone, Deserialize)]
pub struct NewPriceRule {
    pub scope: String,
    #[serde(default)]
    pub target_id: Option<Uuid>,
    pub adjustment: String,
    /// A price, percentage or amount: a JSON number or a decimal string
    #[serde(deserialize_with = "number_or_text")]
    pub value: Decimal,
    #[serde(default = "one")]
    pub min_quantity: i32,
}

/// A decimal sent as a JSON number (`10`, `8.5`) or as text (`"8.50"`)
pub(crate) fn number_or_text<'de, D>(deserializer: D) -> std::result::Result<Decimal, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Text(String),
        Number(serde_json::Number),
    }
    let text = match Raw::deserialize(deserializer)? {
        Raw::Text(text) => text,
        Raw::Number(number) => number.to_string(),
    };
    text.trim()
        .parse::<Decimal>()
        .map_err(serde::de::Error::custom)
}

fn one() -> i32 {
    1
}

/// What to price: a product (its own price) or one of its variants
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceItem {
    pub product_id: Uuid,
    pub variant_id: Option<Uuid>,
    /// Catalogue price, excluding VAT
    pub base_price: Decimal,
    pub quantity: i32,
}

/// The price from a quantity on
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PriceTier {
    pub min_quantity: i32,
    pub price: Decimal,
}

/// The price a customer pays for an item
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedPrice {
    pub product_id: Uuid,
    pub variant_id: Option<Uuid>,
    pub base_price: Decimal,
    /// Price at the item's quantity (the base price when no rule applies)
    pub price: Decimal,
    /// The list the price comes from, when it is lower than the base price
    pub price_list_id: Option<Uuid>,
    /// Prices from larger quantities, cheapest last (only those below `price`)
    pub tiers: Vec<PriceTier>,
    /// The catalogue promotions behind `price` (empty without one)
    pub promotions: Vec<AppliedPromotion>,
}

/// An applicable rule, joined with what it targets
#[derive(Debug, Clone, sqlx::FromRow)]
struct LiveRule {
    price_list_id: Uuid,
    scope: String,
    target_id: Option<Uuid>,
    adjustment: String,
    value: Decimal,
    min_quantity: i32,
}

impl LiveRule {
    /// The price this rule gives for `base`, never below zero, in cents
    fn price(&self, base: Decimal) -> Decimal {
        let price = match self.adjustment.as_str() {
            "fixed" => self.value,
            "percent_off" => base * (Decimal::ONE_HUNDRED - self.value) / Decimal::ONE_HUNDRED,
            _ => base - self.value,
        };
        price.max(Decimal::ZERO).round_dp(2)
    }

    /// Whether the rule targets `item` (whose categories, ancestors included, are given)
    fn targets(&self, item: &PriceItem, categories: &HashSet<Uuid>) -> bool {
        match (self.scope.as_str(), self.target_id) {
            ("all", _) => true,
            ("product", Some(id)) => id == item.product_id,
            ("variant", Some(id)) => Some(id) == item.variant_id,
            ("category", Some(id)) => categories.contains(&id),
            _ => false,
        }
    }
}

/// The price of `item` among `rules`: the lowest price of the rules that
/// target it at its quantity, and the cheaper prices of larger quantities
fn resolve_item(item: &PriceItem, rules: &[LiveRule], categories: &HashSet<Uuid>) -> ResolvedPrice {
    let targeting: Vec<&LiveRule> = rules
        .iter()
        .filter(|r| r.targets(item, categories))
        .collect();
    let best_at = |quantity: i32| {
        targeting
            .iter()
            .filter(|r| r.min_quantity <= quantity)
            .map(|r| (r.price(item.base_price), r.price_list_id))
            .filter(|(price, _)| *price < item.base_price)
            .min_by_key(|(price, _)| *price)
    };
    let (price, price_list_id) = match best_at(item.quantity.max(1)) {
        Some((price, list)) => (price, Some(list)),
        None => (item.base_price, None),
    };
    let mut thresholds: Vec<i32> = targeting
        .iter()
        .map(|r| r.min_quantity)
        .filter(|q| *q > item.quantity.max(1))
        .collect();
    thresholds.sort_unstable();
    thresholds.dedup();
    let mut tiers = Vec::new();
    let mut last = price;
    for min_quantity in thresholds {
        if let Some((tier_price, _)) = best_at(min_quantity).filter(|(p, _)| *p < last) {
            tiers.push(PriceTier {
                min_quantity,
                price: tier_price,
            });
            last = tier_price;
        }
    }
    ResolvedPrice {
        product_id: item.product_id,
        variant_id: item.variant_id,
        base_price: item.base_price,
        price,
        price_list_id,
        tiers,
        promotions: Vec::new(),
    }
}

/// Apply catalogue promotions to a negotiated price (see promotion_service)
fn with_promotions(
    mut resolved: ResolvedPrice,
    item: &PriceItem,
    promotions: &[Promotion],
    categories: &HashSet<Uuid>,
    amounts: Option<&HashMap<Uuid, Decimal>>,
) -> ResolvedPrice {
    let catalogue_item = CatalogueItem {
        product_id: item.product_id,
        variant_id: item.variant_id,
        categories,
        quantity: item.quantity.max(1),
        base: item.base_price,
        negotiated: resolved.price,
    };
    if let Some((price, used)) = catalogue_price(promotions, &catalogue_item, amounts) {
        if used.iter().all(|p| !p.combinable) {
            resolved.price_list_id = None;
        }
        resolved.price = price;
        resolved.tiers.retain(|t| t.price < price);
        resolved.promotions = used.into_iter().map(AppliedPromotion::from).collect();
    }
    resolved
}

/// The buyer promotions are looked up for in a pricing context
pub fn buyer_of(context: &PricingContext, email: Option<&str>) -> Buyer {
    Buyer {
        pro: context.pro,
        customer_id: context.customer_id,
        email: email.map(str::to_string),
    }
}

/// The rules in force for `context` now that may target `items`
async fn live_rules(
    conn: &mut PgConnection,
    context: PricingContext,
    items: &[PriceItem],
) -> Result<Vec<LiveRule>> {
    if context.company_id.is_none() && context.customer_group_id.is_none() {
        return Ok(Vec::new());
    }
    let products: Vec<Uuid> = items.iter().map(|i| i.product_id).collect();
    let variants: Vec<Uuid> = items.iter().filter_map(|i| i.variant_id).collect();
    Ok(sqlx::query_as::<_, LiveRule>(
        "SELECT r.price_list_id, r.scope, r.target_id, r.adjustment, r.value, r.min_quantity \
         FROM price_list_rules r JOIN price_lists l ON l.id = r.price_list_id \
         WHERE l.is_active AND (l.starts_at IS NULL OR l.starts_at <= NOW()) \
           AND (l.ends_at IS NULL OR l.ends_at > NOW()) \
           AND (l.customer_group_id = $1 OR l.company_id = $2) \
           AND (r.scope IN ('all', 'category') \
                OR (r.scope = 'product' AND r.target_id = ANY($3)) \
                OR (r.scope = 'variant' AND r.target_id = ANY($4)))",
    )
    .bind(context.customer_group_id)
    .bind(context.company_id)
    .bind(&products)
    .bind(&variants)
    .fetch_all(conn)
    .await?)
}

/// Each product's categories with all their ancestors
pub(crate) async fn category_paths(
    conn: &mut PgConnection,
    products: &[Uuid],
) -> Result<HashMap<Uuid, HashSet<Uuid>>> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "WITH RECURSIVE up(product_id, category_id) AS ( \
             SELECT product_id, category_id FROM product_category_relations \
             WHERE product_id = ANY($1) \
           UNION \
             SELECT up.product_id, c.parent_id FROM up \
             JOIN product_categories c ON c.id = up.category_id \
             WHERE c.parent_id IS NOT NULL) \
         SELECT product_id, category_id FROM up",
    )
    .bind(products)
    .fetch_all(conn)
    .await?;
    let mut paths: HashMap<Uuid, HashSet<Uuid>> = HashMap::new();
    for (product, category) in rows {
        paths.entry(product).or_default().insert(category);
    }
    Ok(paths)
}

/// The prices `context` pays for `items`, in the same order, with the
/// catalogue promotions in force for them (listings: no cart amounts)
pub async fn resolve_prices(
    conn: &mut PgConnection,
    context: PricingContext,
    items: &[PriceItem],
) -> Result<Vec<ResolvedPrice>> {
    let promotions = live_promotions(&mut *conn, &buyer_of(&context, None)).await?;
    resolve_with(conn, context, items, &promotions, None).await
}

/// Each product's categories with their ancestors, when a rule or promotion
/// targets a category
async fn paths_if_needed(
    conn: &mut PgConnection,
    items: &[PriceItem],
    category_scoped: bool,
) -> Result<HashMap<Uuid, HashSet<Uuid>>> {
    if !category_scoped {
        return Ok(HashMap::new());
    }
    let products: Vec<Uuid> = items.iter().map(|i| i.product_id).collect();
    category_paths(conn, &products).await
}

/// The prices of `items` under `context`'s price lists and `promotions`
pub async fn resolve_with(
    conn: &mut PgConnection,
    context: PricingContext,
    items: &[PriceItem],
    promotions: &[Promotion],
    amounts: Option<&HashMap<Uuid, Decimal>>,
) -> Result<Vec<ResolvedPrice>> {
    let rules = live_rules(&mut *conn, context, items).await?;
    let category_scoped = rules.iter().any(|r| r.scope == "category")
        || promotions.iter().any(|p| p.scope == "category");
    let paths = paths_if_needed(&mut *conn, items, category_scoped).await?;
    let none = HashSet::new();
    Ok(items
        .iter()
        .map(|item| {
            let categories = paths.get(&item.product_id).unwrap_or(&none);
            let negotiated = resolve_item(item, &rules, categories);
            with_promotions(negotiated, item, promotions, categories, amounts)
        })
        .collect())
}

/// The pricing context of a customer: their active company and its group,
/// else their own group
pub async fn pricing_context(conn: &mut PgConnection, customer: Uuid) -> Result<PricingContext> {
    let row: Option<(Option<Uuid>, Option<Uuid>, Option<Uuid>, bool)> = sqlx::query_as(
        "SELECT co.id, co.customer_group_id, c.customer_group_id, COALESCE(g.prices_exclude_tax, false) \
         FROM customers c \
         LEFT JOIN company_members m ON m.customer_id = c.id \
         LEFT JOIN companies co ON co.id = m.company_id AND co.status = 'active' \
         LEFT JOIN customer_groups g ON g.id = CASE WHEN co.id IS NULL THEN c.customer_group_id \
                                                   ELSE co.customer_group_id END \
         WHERE c.id = $1",
    )
    .bind(customer)
    .fetch_optional(conn)
    .await?;
    Ok(match row {
        Some((company, company_group, own_group, net_prices)) => PricingContext {
            company_id: company,
            customer_group_id: if company.is_some() {
                company_group
            } else {
                own_group
            },
            pro: company.is_some() || net_prices,
            customer_id: Some(customer),
        },
        None => PricingContext::default(),
    })
}

/// The catalogue price (excluding VAT) of products and variants: a variant's
/// own price, else its product's
pub async fn catalogue_prices(
    conn: &mut PgConnection,
    products: &[Uuid],
    variants: &[Uuid],
) -> Result<(HashMap<Uuid, Decimal>, HashMap<Uuid, (Uuid, Decimal)>)> {
    let product_prices: Vec<(Uuid, Decimal)> =
        sqlx::query_as("SELECT id, price FROM products WHERE id = ANY($1) AND is_active")
            .bind(products)
            .fetch_all(&mut *conn)
            .await?;
    let variant_prices: Vec<(Uuid, Uuid, Decimal)> = sqlx::query_as(
        "SELECT v.id, v.product_id, COALESCE(v.price, p.price) FROM product_variants v \
         JOIN products p ON p.id = v.product_id WHERE v.id = ANY($1) AND p.is_active",
    )
    .bind(variants)
    .fetch_all(&mut *conn)
    .await?;
    Ok((
        product_prices.into_iter().collect(),
        variant_prices
            .into_iter()
            .map(|(id, product, price)| (id, (product, price)))
            .collect(),
    ))
}

/// A regular cart line (not a bundle line nor a gift)
#[derive(Debug, sqlx::FromRow)]
struct CartLine {
    id: Uuid,
    product_id: Uuid,
    variant_id: Option<Uuid>,
    quantity: i32,
}

/// The lines that still have a catalogue price, with what to price for each
fn price_items<'a>(
    lines: &'a [CartLine],
    products: &HashMap<Uuid, Decimal>,
    variants: &HashMap<Uuid, (Uuid, Decimal)>,
) -> (Vec<&'a CartLine>, Vec<PriceItem>) {
    lines
        .iter()
        .filter_map(|line| {
            let base = match line.variant_id {
                Some(v) => variants.get(&v).map(|(_, price)| *price),
                None => products.get(&line.product_id).copied(),
            }?;
            let item = PriceItem {
                product_id: line.product_id,
                variant_id: line.variant_id,
                base_price: base,
                quantity: line.quantity,
            };
            Some((line, item))
        })
        .unzip()
}

/// What repricing found, for the cart promotions that follow
pub struct RepricedCart {
    pub context: PricingContext,
    /// Promotions in force for the cart's buyer
    pub promotions: Vec<Promotion>,
    /// Each product's categories with their ancestors (when a scope needs them)
    pub paths: HashMap<Uuid, HashSet<Uuid>>,
}

/// Per catalogue promotion with a minimum amount: the cart's total, at
/// catalogue prices, of the lines it targets
fn targeted_amounts(
    promotions: &[Promotion],
    items: &[PriceItem],
    paths: &HashMap<Uuid, HashSet<Uuid>>,
) -> HashMap<Uuid, Decimal> {
    let none = HashSet::new();
    promotions
        .iter()
        .filter(|p| p.min_amount.is_some())
        .map(|p| {
            let amount = items
                .iter()
                .filter(|i| {
                    let categories = paths.get(&i.product_id).unwrap_or(&none);
                    crate::services::promotion_service::targets(
                        &p.scope, p.target_id, i.product_id, i.variant_id, categories,
                    )
                })
                .map(|i| i.base_price * Decimal::from(i.quantity))
                .sum();
            (p.id, amount)
        })
        .collect()
}

/// Write each line's price, list and promotions
async fn write_line_prices(
    conn: &mut PgConnection,
    lines: &[&CartLine],
    prices: Vec<ResolvedPrice>,
) -> Result<()> {
    for (line, price) in lines.iter().zip(prices) {
        let promotions: Vec<Uuid> = price.promotions.iter().map(|p| p.id).collect();
        sqlx::query(
            "UPDATE cart_items SET unit_price = $2, subtotal = $2 * quantity, price_list_id = $3, \
             original_price = $4, promotion_ids = $5, updated_at = NOW() WHERE id = $1",
        )
        .bind(line.id)
        .bind(price.price)
        .bind(price.price_list_id)
        .bind(price.base_price)
        .bind(&promotions)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Give a cart's regular lines (not bundle lines, not gifts) their price: the
/// catalogue price, the customer's negotiated price or a catalogue promotion,
/// whichever applies (see promotion_service). Call inside the cart's lock,
/// before its totals.
pub async fn reprice_cart_lines(
    conn: &mut PgConnection,
    cart_id: Uuid,
    customer: Option<Uuid>,
    email: Option<&str>,
) -> Result<RepricedCart> {
    let context = match customer {
        Some(id) => pricing_context(&mut *conn, id).await?,
        None => PricingContext::default(),
    };
    let promotions = live_promotions(&mut *conn, &buyer_of(&context, email)).await?;
    let lines: Vec<CartLine> = sqlx::query_as(
        "SELECT id, product_id, variant_id, quantity FROM cart_items WHERE cart_id = $1 \
           AND COALESCE((custom_attributes->>'is_bundle_component')::boolean, false) = false \
           AND COALESCE((custom_attributes->>'is_bundle_parent')::boolean, false) = false \
           AND custom_attributes->>'gift_promotion_id' IS NULL",
    )
    .bind(cart_id)
    .fetch_all(&mut *conn)
    .await?;
    let products: Vec<Uuid> = lines.iter().map(|l| l.product_id).collect();
    let variants: Vec<Uuid> = lines.iter().filter_map(|l| l.variant_id).collect();
    let (product_prices, variant_prices) = catalogue_prices(&mut *conn, &products, &variants).await?;
    let (priced, items) = price_items(&lines, &product_prices, &variant_prices);
    let paths = paths_if_needed(&mut *conn, &items, promotions.iter().any(|p| p.scope == "category")).await?;
    let amounts = targeted_amounts(&promotions, &items, &paths);
    let prices = resolve_with(&mut *conn, context, &items, &promotions, Some(&amounts)).await?;
    write_line_prices(&mut *conn, &priced, prices).await?;
    Ok(RepricedCart { context, promotions, paths })
}

fn check_rule(rule: &NewPriceRule) -> Result<()> {
    let scope_ok = match rule.scope.as_str() {
        "all" => rule.target_id.is_none(),
        "category" | "product" | "variant" => rule.target_id.is_some(),
        _ => false,
    };
    if !scope_ok {
        return Err(Error::validation(
            "scope must be all (no target_id), or category, product or variant with a target_id",
        ));
    }
    let value_ok = match rule.adjustment.as_str() {
        "fixed" | "amount_off" => rule.value >= Decimal::ZERO,
        "percent_off" => rule.value >= Decimal::ZERO && rule.value <= Decimal::ONE_HUNDRED,
        _ => false,
    };
    if !value_ok || rule.min_quantity < 1 {
        return Err(Error::validation(
            "adjustment must be fixed, percent_off (0 to 100) or amount_off with a value of at least 0, and min_quantity at least 1",
        ));
    }
    Ok(())
}

fn check_dates(starts_at: Option<DateTime<Utc>>, ends_at: Option<DateTime<Utc>>) -> Result<()> {
    match (starts_at, ends_at) {
        (Some(start), Some(end)) if end <= start => {
            Err(Error::validation("ends_at must be after starts_at"))
        }
        _ => Ok(()),
    }
}

/// Price lists for staff screens
#[derive(Clone)]
pub struct PriceListService {
    pool: PgPool,
}

impl PriceListService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The prices a customer pays for `items` now (catalogue prices when they
    /// have no price list)
    pub async fn prices_for(
        &self,
        customer: Option<Uuid>,
        items: &[PriceItem],
    ) -> Result<Vec<ResolvedPrice>> {
        let mut conn = self.pool.acquire().await?;
        let context = match customer {
            Some(id) => pricing_context(&mut conn, id).await?,
            None => PricingContext::default(),
        };
        resolve_prices(&mut conn, context, items).await
    }

    /// Price lists, newest first, optionally those of one group or company
    pub async fn list(&self, group: Option<Uuid>, company: Option<Uuid>) -> Result<Vec<PriceList>> {
        Ok(sqlx::query_as::<_, PriceList>(
            "SELECT * FROM price_lists WHERE ($1::uuid IS NULL OR customer_group_id = $1) \
             AND ($2::uuid IS NULL OR company_id = $2) ORDER BY created_at DESC",
        )
        .bind(group)
        .bind(company)
        .fetch_all(&self.pool)
        .await?)
    }

    /// One price list with its rules
    pub async fn get(&self, id: Uuid) -> Result<(PriceList, Vec<PriceRule>)> {
        let list = sqlx::query_as::<_, PriceList>("SELECT * FROM price_lists WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| Error::not_found("Price list not found"))?;
        let rules = sqlx::query_as::<_, PriceRule>(
            "SELECT * FROM price_list_rules WHERE price_list_id = $1 \
             ORDER BY scope, target_id, min_quantity",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok((list, rules))
    }

    /// Create a price list for one customer group or one company
    pub async fn create(&self, new: NewPriceList) -> Result<PriceList> {
        let name = new.name.trim();
        if name.is_empty() || name.chars().count() > 255 {
            return Err(Error::validation("name must be 1 to 255 characters"));
        }
        if new.customer_group_id.is_some() == new.company_id.is_some() {
            return Err(Error::validation(
                "a price list applies to either a customer_group_id or a company_id",
            ));
        }
        check_dates(new.starts_at, new.ends_at)?;
        sqlx::query_as::<_, PriceList>(
            "INSERT INTO price_lists (name, customer_group_id, company_id, starts_at, ends_at) \
             VALUES ($1, $2, $3, $4, $5) RETURNING *",
        )
        .bind(name)
        .bind(new.customer_group_id)
        .bind(new.company_id)
        .bind(new.starts_at)
        .bind(new.ends_at)
        .fetch_one(&self.pool)
        .await
        .map_err(missing_reference)
    }

    /// Rename, switch on or off, or change the dates of a price list
    pub async fn update(&self, id: Uuid, update: PriceListUpdate) -> Result<PriceList> {
        let (current, _) = self.get(id).await?;
        let name = update
            .name
            .as_deref()
            .map(str::trim)
            .unwrap_or(&current.name)
            .to_string();
        if name.is_empty() || name.chars().count() > 255 {
            return Err(Error::validation("name must be 1 to 255 characters"));
        }
        let starts_at = update.starts_at.unwrap_or(current.starts_at);
        let ends_at = update.ends_at.unwrap_or(current.ends_at);
        check_dates(starts_at, ends_at)?;
        Ok(sqlx::query_as::<_, PriceList>(
            "UPDATE price_lists SET name = $2, is_active = $3, starts_at = $4, ends_at = $5, \
             updated_at = NOW() WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(name)
        .bind(update.is_active.unwrap_or(current.is_active))
        .bind(starts_at)
        .bind(ends_at)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Delete a price list and its rules
    pub async fn delete(&self, id: Uuid) -> Result<()> {
        let deleted = sqlx::query("DELETE FROM price_lists WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(Error::not_found("Price list not found"));
        }
        Ok(())
    }

    /// Add a rule to a price list; its target must exist
    pub async fn add_rule(&self, list: Uuid, rule: NewPriceRule) -> Result<PriceRule> {
        check_rule(&rule)?;
        let table = match rule.scope.as_str() {
            "category" => Some("product_categories"),
            "product" => Some("products"),
            "variant" => Some("product_variants"),
            _ => None,
        };
        if let (Some(table), Some(target)) = (table, rule.target_id) {
            let exists: bool = sqlx::query_scalar(&format!(
                "SELECT EXISTS (SELECT 1 FROM {table} WHERE id = $1)"
            ))
            .bind(target)
            .fetch_one(&self.pool)
            .await?;
            if !exists {
                return Err(Error::validation(format!(
                    "target_id is not a {}",
                    rule.scope
                )));
            }
        }
        sqlx::query_as::<_, PriceRule>(
            "INSERT INTO price_list_rules (price_list_id, scope, target_id, adjustment, value, \
             min_quantity) VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
        )
        .bind(list)
        .bind(&rule.scope)
        .bind(rule.target_id)
        .bind(&rule.adjustment)
        .bind(rule.value)
        .bind(rule.min_quantity)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match missing_reference(e) {
            Error::Validation(_) => Error::not_found("Price list not found"),
            other => other,
        })
    }

    /// Remove a rule from a price list
    pub async fn remove_rule(&self, list: Uuid, rule: Uuid) -> Result<()> {
        let deleted =
            sqlx::query("DELETE FROM price_list_rules WHERE id = $1 AND price_list_id = $2")
                .bind(rule)
                .bind(list)
                .execute(&self.pool)
                .await?;
        if deleted.rows_affected() == 0 {
            return Err(Error::not_found("Price rule not found"));
        }
        Ok(())
    }
}

/// A foreign key that points nowhere is the client's mistake
fn missing_reference(e: sqlx::Error) -> Error {
    match e {
        sqlx::Error::Database(d) if d.is_foreign_key_violation() => {
            Error::validation("customer_group_id or company_id does not exist")
        }
        other => other.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn rule(
        scope: &str,
        target: Option<Uuid>,
        adjustment: &str,
        value: Decimal,
        min: i32,
    ) -> LiveRule {
        LiveRule {
            price_list_id: Uuid::nil(),
            scope: scope.into(),
            target_id: target,
            adjustment: adjustment.into(),
            value,
            min_quantity: min,
        }
    }

    #[test]
    fn the_lowest_applicable_price_wins_with_quantity_tiers() {
        let (product, category) = (Uuid::new_v4(), Uuid::new_v4());
        let item = PriceItem {
            product_id: product,
            variant_id: None,
            base_price: dec!(10.00),
            quantity: 1,
        };
        let rules = vec![
            rule("all", None, "percent_off", dec!(5), 1),
            rule("category", Some(category), "amount_off", dec!(1.50), 1),
            rule("product", Some(product), "fixed", dec!(8.00), 10),
            rule("product", Some(Uuid::new_v4()), "fixed", dec!(1.00), 1),
        ];
        let categories = HashSet::from([category]);
        let resolved = resolve_item(&item, &rules, &categories);
        assert_eq!(resolved.price, dec!(8.50), "1.50 off beats 5 %");
        assert_eq!(
            resolved.tiers,
            vec![PriceTier {
                min_quantity: 10,
                price: dec!(8.00)
            }]
        );
        let at_ten = resolve_item(
            &PriceItem {
                quantity: 12,
                ..item
            },
            &rules,
            &categories,
        );
        assert_eq!(at_ten.price, dec!(8.00));
        assert!(at_ten.tiers.is_empty());
        // Outside the category, only the 5 % applies
        assert_eq!(
            resolve_item(&item, &rules[..1], &HashSet::new()).price,
            dec!(9.50)
        );
    }

    #[test]
    fn a_rule_never_raises_the_price() {
        let item = PriceItem {
            product_id: Uuid::new_v4(),
            variant_id: None,
            base_price: dec!(5.00),
            quantity: 1,
        };
        let resolved = resolve_item(
            &item,
            &[rule("all", None, "fixed", dec!(6.00), 1)],
            &HashSet::new(),
        );
        assert_eq!(resolved.price, dec!(5.00));
        assert_eq!(resolved.price_list_id, None);
        let free = resolve_item(
            &item,
            &[rule("all", None, "amount_off", dec!(9), 1)],
            &HashSet::new(),
        );
        assert_eq!(free.price, Decimal::ZERO);
    }

    #[test]
    fn rule_values_are_numbers_or_text() {
        let rule: NewPriceRule =
            serde_json::from_str(r#"{"scope": "all", "adjustment": "percent_off", "value": 10}"#)
                .unwrap();
        assert_eq!(rule.value, dec!(10));
        let rule: NewPriceRule = serde_json::from_str(
            r#"{"scope": "all", "adjustment": "fixed", "value": "8.50", "min_quantity": 5}"#,
        )
        .unwrap();
        assert_eq!((rule.value, rule.min_quantity), (dec!(8.50), 5));
    }

    #[test]
    fn rules_are_checked() {
        let new =
            |scope: &str, target: Option<Uuid>, adjustment: &str, value: Decimal| NewPriceRule {
                scope: scope.into(),
                target_id: target,
                adjustment: adjustment.into(),
                value,
                min_quantity: 1,
            };
        assert!(check_rule(&new("all", None, "percent_off", dec!(10))).is_ok());
        assert!(check_rule(&new("all", Some(Uuid::nil()), "fixed", dec!(1))).is_err());
        assert!(check_rule(&new("product", None, "fixed", dec!(1))).is_err());
        assert!(check_rule(&new("product", Some(Uuid::nil()), "percent_off", dec!(101))).is_err());
        assert!(check_rule(&new("brand", Some(Uuid::nil()), "fixed", dec!(1))).is_err());
    }
}
