//! Promotions: reductions and offers for retail customers, trade customers or
//! both (retail by default).
//!
//! Catalogue kinds lower the price of what they target, shown on listings and
//! product pages and charged in the cart: `percent_off`, `amount_off` and
//! `fixed_price`. Cart kinds act on the cart:
//! - `buy_x_get_y`: on each targeted line, for every `buy_quantity` units the
//!   next `get_quantity` are free (`value` 100, the default) or reduced by
//!   `value` percent ("1 bought = 1 free", "3 + 1", "2nd at -50 %");
//! - `spend_tiers`: a percentage off the targeted lines from a spending tier
//!   ("5 % from 50 €, 10 % from 100 €");
//! - `free_delivery`: delivery costs nothing;
//! - `gift`: `gift_quantity` of `gift_product_id` added free.
//!
//! Every promotion can be limited in time (`starts_at`, `ends_at`: flash and
//! seasonal sales) and have conditions: `min_amount` and `min_quantity` of the
//! targeted lines, `first_order_only`, and usage limits (`max_uses` orders in
//! all, `max_uses_per_customer`).
//!
//! **Adding up.** A line or cart promotion is added to another only when both
//! are `combinable`; a non-combinable one applies only where no other
//! promotion does. For a catalogue price the cheapest of these wins: the
//! negotiated price (price lists, else the catalogue price), the best single
//! promotion on the catalogue price, or every combinable promotion stacked on
//! the negotiated price.
//!
//! Amounts are excluding VAT, like catalogue prices. A reduced price is shown
//! with the lowest price of the 30 days before the promotion started (EU
//! Omnibus rule), read from the price history.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::list_sort::{order_by, SortColumns};
use crate::services::price_list_service::number_or_text;
use crate::{Error, Result};

/// Kinds that lower a catalogue price
pub const CATALOGUE_KINDS: [&str; 3] = ["percent_off", "amount_off", "fixed_price"];
/// Kinds that act on the cart
pub const CART_KINDS: [&str; 4] = ["buy_x_get_y", "spend_tiers", "free_delivery", "gift"];

/// A spending tier: `percent` off from `min_amount`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendTier {
    #[serde(deserialize_with = "number_or_text")]
    pub min_amount: Decimal,
    #[serde(deserialize_with = "number_or_text")]
    pub percent: Decimal,
}

/// A promotion
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Promotion {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    /// `customer` (retail customers and guests), `pro` or `both`
    pub audience: String,
    /// `all`, `category`, `product` or `variant`
    pub scope: String,
    pub target_id: Option<Uuid>,
    pub value: Option<Decimal>,
    pub buy_quantity: Option<i32>,
    pub get_quantity: Option<i32>,
    pub tiers: Option<Json<Vec<SpendTier>>>,
    pub min_amount: Option<Decimal>,
    pub min_quantity: Option<i32>,
    pub first_order_only: bool,
    pub max_uses: Option<i32>,
    pub max_uses_per_customer: Option<i32>,
    pub combinable: bool,
    pub gift_product_id: Option<Uuid>,
    pub gift_quantity: i32,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A decimal that may be absent, sent as a JSON number or as text
pub(crate) fn optional_number<'de, D>(deserializer: D) -> std::result::Result<Option<Decimal>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Wrap(#[serde(deserialize_with = "number_or_text")] Decimal);
    Ok(Option::<Wrap>::deserialize(deserializer)?.map(|w| w.0))
}

fn customer() -> String {
    "customer".into()
}
fn all() -> String {
    "all".into()
}
fn one() -> i32 {
    1
}
fn yes() -> bool {
    true
}

/// A promotion as staff create or replace it
#[derive(Debug, Clone, Deserialize)]
pub struct NewPromotion {
    pub name: String,
    pub kind: String,
    #[serde(default = "customer")]
    pub audience: String,
    #[serde(default = "all")]
    pub scope: String,
    #[serde(default)]
    pub target_id: Option<Uuid>,
    #[serde(default, deserialize_with = "optional_number")]
    pub value: Option<Decimal>,
    #[serde(default)]
    pub buy_quantity: Option<i32>,
    #[serde(default)]
    pub get_quantity: Option<i32>,
    #[serde(default)]
    pub tiers: Option<Vec<SpendTier>>,
    #[serde(default, deserialize_with = "optional_number")]
    pub min_amount: Option<Decimal>,
    #[serde(default)]
    pub min_quantity: Option<i32>,
    #[serde(default)]
    pub first_order_only: bool,
    #[serde(default)]
    pub max_uses: Option<i32>,
    #[serde(default)]
    pub max_uses_per_customer: Option<i32>,
    #[serde(default)]
    pub combinable: bool,
    #[serde(default)]
    pub gift_product_id: Option<Uuid>,
    #[serde(default = "one")]
    pub gift_quantity: i32,
    #[serde(default)]
    pub starts_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub ends_at: Option<DateTime<Utc>>,
    #[serde(default = "yes")]
    pub is_active: bool,
}

fn invalid(message: &str) -> Error {
    Error::validation(message.to_string())
}

fn check_percent(value: Option<Decimal>) -> Result<()> {
    match value {
        Some(v) if v > Decimal::ZERO && v <= Decimal::ONE_HUNDRED => Ok(()),
        _ => Err(invalid("value must be a percentage above 0 and at most 100")),
    }
}

/// The fields each kind needs
fn check_kind(p: &NewPromotion) -> Result<()> {
    let at_least = |v: Option<Decimal>, min: Decimal| v.is_some_and(|v| v >= min);
    match p.kind.as_str() {
        "percent_off" => check_percent(p.value),
        "amount_off" if at_least(p.value, Decimal::new(1, 2)) => Ok(()),
        "fixed_price" if at_least(p.value, Decimal::ZERO) => Ok(()),
        "amount_off" | "fixed_price" => Err(invalid("value must be an amount of at least 0")),
        "buy_x_get_y" => match (p.buy_quantity, p.get_quantity) {
            (Some(b), Some(g)) if b >= 1 && g >= 1 => {
                check_percent(p.value.or(Some(Decimal::ONE_HUNDRED)))
            }
            _ => Err(invalid("buy_quantity and get_quantity must be at least 1")),
        },
        "spend_tiers" => check_tiers(p.tiers.as_deref()),
        "free_delivery" => Ok(()),
        "gift" if p.gift_product_id.is_some() && p.gift_quantity >= 1 => Ok(()),
        "gift" => Err(invalid("gift needs gift_product_id and a gift_quantity of at least 1")),
        _ => Err(invalid(
            "kind must be percent_off, amount_off, fixed_price, buy_x_get_y, spend_tiers, free_delivery or gift",
        )),
    }
}

fn check_tiers(tiers: Option<&[SpendTier]>) -> Result<()> {
    let tiers = tiers
        .filter(|t| !t.is_empty())
        .ok_or_else(|| invalid("spend_tiers needs at least one tier"))?;
    let ok = tiers.iter().all(|t| {
        t.min_amount > Decimal::ZERO
            && t.percent > Decimal::ZERO
            && t.percent <= Decimal::ONE_HUNDRED
    });
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "each tier needs a min_amount above 0 and a percent above 0 and at most 100",
        ))
    }
}

/// Conditions and dates
fn check_conditions(p: &NewPromotion) -> Result<()> {
    if p.min_amount.is_some_and(|m| m < Decimal::ZERO) {
        return Err(invalid("min_amount must be at least 0"));
    }
    let below_one = |v: Option<i32>| v.is_some_and(|v| v < 1);
    if below_one(p.min_quantity) || below_one(p.max_uses) || below_one(p.max_uses_per_customer) {
        return Err(invalid(
            "min_quantity, max_uses and max_uses_per_customer must be at least 1",
        ));
    }
    if let (Some(start), Some(end)) = (p.starts_at, p.ends_at) {
        if end <= start {
            return Err(invalid("ends_at must be after starts_at"));
        }
    }
    Ok(())
}

/// Everything a promotion must satisfy before it is stored
pub fn check(p: &NewPromotion) -> Result<()> {
    if p.name.trim().is_empty() || p.name.len() > 150 {
        return Err(invalid("name must have 1 to 150 characters"));
    }
    if !matches!(p.audience.as_str(), "customer" | "pro" | "both") {
        return Err(invalid("audience must be customer, pro or both"));
    }
    let scope_ok = match p.scope.as_str() {
        "all" => p.target_id.is_none(),
        "category" | "product" | "variant" => p.target_id.is_some(),
        _ => false,
    };
    if !scope_ok {
        return Err(invalid(
            "scope must be all (no target_id), or category, product or variant with a target_id",
        ));
    }
    check_conditions(p)?;
    check_kind(p)
}

/// Whether a scope targets a product or variant (with its categories and
/// their ancestors)
pub fn targets(
    scope: &str,
    target: Option<Uuid>,
    product: Uuid,
    variant: Option<Uuid>,
    categories: &HashSet<Uuid>,
) -> bool {
    match (scope, target) {
        ("all", _) => true,
        ("product", Some(id)) => id == product,
        ("variant", Some(id)) => Some(id) == variant,
        ("category", Some(id)) => categories.contains(&id),
        _ => false,
    }
}

/// The price a catalogue promotion gives for `price`, never below zero
pub fn promo_price(promotion: &Promotion, price: Decimal) -> Decimal {
    let value = promotion.value.unwrap_or_default();
    let reduced = match promotion.kind.as_str() {
        "percent_off" => price * (Decimal::ONE_HUNDRED - value) / Decimal::ONE_HUNDRED,
        "amount_off" => price - value,
        "fixed_price" => value.min(price),
        _ => price,
    };
    reduced.max(Decimal::ZERO).round_dp(2)
}

/// Whether the quantity and amount conditions hold (`amount` unknown on
/// listings: promotions with a minimum amount then wait for the cart)
pub fn conditions_met(p: &Promotion, quantity: i32, amount: Option<Decimal>) -> bool {
    quantity >= p.min_quantity.unwrap_or(1)
        && match p.min_amount.filter(|m| *m > Decimal::ZERO) {
            None => true,
            Some(min) => amount.is_some_and(|a| a >= min),
        }
}

/// Whether `p` may be added to a line already carrying `existing`
/// (promotion, combinable) pairs
pub fn can_add(p: &Promotion, existing: &[(Uuid, bool)]) -> bool {
    existing.is_empty() || (p.combinable && existing.iter().all(|(_, c)| *c))
}

/// The promotion shown and charged with a price
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppliedPromotion {
    pub id: Uuid,
    pub kind: String,
    pub value: Option<Decimal>,
    pub combinable: bool,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
}

impl From<&Promotion> for AppliedPromotion {
    fn from(p: &Promotion) -> Self {
        Self {
            id: p.id,
            kind: p.kind.clone(),
            value: p.value,
            combinable: p.combinable,
            starts_at: p.starts_at,
            ends_at: p.ends_at,
        }
    }
}

/// An item to price with catalogue promotions
#[derive(Debug, Clone)]
pub struct CatalogueItem<'a> {
    pub product_id: Uuid,
    pub variant_id: Option<Uuid>,
    pub categories: &'a HashSet<Uuid>,
    pub quantity: i32,
    /// Catalogue price
    pub base: Decimal,
    /// The price without promotions (negotiated, else `base`)
    pub negotiated: Decimal,
}

/// The cheapest catalogue price for `item` (see the module docs), when a
/// promotion makes it lower than the negotiated price. `amounts`: per
/// promotion, the cart's total of the lines it targets (`None` on listings).
pub fn catalogue_price<'a>(
    promotions: &'a [Promotion],
    item: &CatalogueItem<'_>,
    amounts: Option<&HashMap<Uuid, Decimal>>,
) -> Option<(Decimal, Vec<&'a Promotion>)> {
    let amount_of = |p: &Promotion| amounts.map(|a| a.get(&p.id).copied().unwrap_or_default());
    let candidates: Vec<&Promotion> = promotions
        .iter()
        .filter(|p| CATALOGUE_KINDS.contains(&p.kind.as_str()))
        .filter(|p| targets(&p.scope, p.target_id, item.product_id, item.variant_id, item.categories))
        .filter(|p| conditions_met(p, item.quantity, amount_of(p)))
        .collect();
    let mut options: Vec<(Decimal, Vec<&Promotion>)> = candidates
        .iter()
        .filter(|p| !p.combinable)
        .map(|p| (promo_price(p, item.base), vec![*p]))
        .collect();
    let mut chain: Vec<&Promotion> = candidates.iter().copied().filter(|p| p.combinable).collect();
    if !chain.is_empty() {
        // a fixed price first, then percentages, then amounts off
        chain.sort_by_key(|p| ["fixed_price", "percent_off", "amount_off"].iter().position(|k| *k == p.kind));
        let stacked = chain.iter().fold(item.negotiated, |price, p| promo_price(p, price));
        options.push((stacked, chain));
    }
    options
        .into_iter()
        .filter(|(price, _)| *price < item.negotiated)
        .min_by_key(|(price, _)| *price)
}

const LIVE: &str = "p.is_active AND (p.starts_at IS NULL OR p.starts_at <= NOW()) \
                    AND (p.ends_at IS NULL OR p.ends_at > NOW())";

/// Who promotions are looked up for
#[derive(Debug, Clone, Default)]
pub struct Buyer {
    /// A trade customer (active company, or a group with prices excluding VAT)
    pub pro: bool,
    pub customer_id: Option<Uuid>,
    /// A guest's e-mail, for first-order and per-customer conditions
    pub email: Option<String>,
}

/// The promotions in force now that reach `buyer`: their audience, usage
/// limits not reached, first-order ones only for a buyer without an order
pub async fn live_promotions(conn: &mut PgConnection, buyer: &Buyer) -> Result<Vec<Promotion>> {
    let audiences: &[&str] = if buyer.pro { &["pro", "both"] } else { &["customer", "both"] };
    let known = "($2::uuid IS NOT NULL OR $3::text IS NOT NULL)";
    let same = "(x.customer_id = $2 OR (x.{email} IS NOT NULL AND lower(x.{email}) = lower($3)))";
    let sql = format!(
        "SELECT p.* FROM promotions p WHERE {LIVE} AND p.audience = ANY($1) \
         AND (p.max_uses IS NULL OR (SELECT COUNT(*) FROM promotion_uses u WHERE u.promotion_id = p.id) < p.max_uses) \
         AND (p.max_uses_per_customer IS NULL OR NOT {known} OR (SELECT COUNT(*) FROM promotion_uses x \
              WHERE x.promotion_id = p.id AND {used}) < p.max_uses_per_customer) \
         AND (NOT p.first_order_only OR ({known} AND NOT EXISTS (SELECT 1 FROM orders x WHERE {ordered} \
              AND x.status::text NOT IN ('cancelled', 'refunded') AND NOT x.draft))) \
         ORDER BY p.created_at",
        used = same.replace("{email}", "email"),
        ordered = same.replace("{email}", "customer_email"),
    );
    Ok(sqlx::query_as::<_, Promotion>(&sql)
        .bind(audiences)
        .bind(buyer.customer_id)
        .bind(&buyer.email)
        .fetch_all(conn)
        .await?)
}

/// Record that an order used promotions (for usage limits)
pub async fn record_uses(
    conn: &mut PgConnection,
    order_id: Uuid,
    customer_id: Option<Uuid>,
    email: &str,
    promotions: &[Uuid],
) -> Result<()> {
    sqlx::query(
        "INSERT INTO promotion_uses (promotion_id, order_id, customer_id, email) \
         SELECT p, $2, $3, $4 FROM UNNEST($1::uuid[]) p \
         WHERE EXISTS (SELECT 1 FROM promotions WHERE id = p) ON CONFLICT DO NOTHING",
    )
    .bind(promotions)
    .bind(order_id)
    .bind(customer_id)
    .bind(email)
    .execute(conn)
    .await?;
    Ok(())
}

/// A cancelled or refunded order gives its promotion uses back
pub async fn release_uses(conn: &mut PgConnection, order_id: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM promotion_uses WHERE order_id = $1")
        .bind(order_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// The lowest catalogue price each item had in the 30 days before `since`
/// (its promotion's start, or now): (product, variant) -> price
pub async fn reference_prices(
    conn: &mut PgConnection,
    items: &[(Uuid, Option<Uuid>, DateTime<Utc>)],
) -> Result<HashMap<(Uuid, Option<Uuid>), Decimal>> {
    let mut found = HashMap::new();
    for (product, variant, since) in items {
        let lowest: Option<Decimal> = sqlx::query_scalar(
            "SELECT MIN(price) FROM ( \
               SELECT price FROM price_history WHERE product_id = $1 AND variant_id IS NOT DISTINCT FROM $2 \
                 AND valid_from > $3 AND valid_from <= $4 \
               UNION ALL \
               (SELECT price FROM price_history WHERE product_id = $1 AND variant_id IS NOT DISTINCT FROM $2 \
                 AND valid_from <= $3 ORDER BY valid_from DESC LIMIT 1)) h",
        )
        .bind(product)
        .bind(variant)
        .bind(*since - Duration::days(30))
        .bind(since)
        .fetch_one(&mut *conn)
        .await?;
        if let Some(price) = lowest {
            found.insert((*product, *variant), price.round_dp(2));
        }
    }
    Ok(found)
}

/// An offer shown on a product card or page: a cart promotion or a
/// catalogue promotion with conditions, that targets the product
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Offer {
    pub id: Uuid,
    pub kind: String,
    pub value: Option<Decimal>,
    pub buy_quantity: Option<i32>,
    pub get_quantity: Option<i32>,
    pub min_quantity: Option<i32>,
    pub min_amount: Option<Decimal>,
    pub tiers: Option<Json<Vec<SpendTier>>>,
    pub ends_at: Option<DateTime<Utc>>,
}

impl From<&Promotion> for Offer {
    fn from(p: &Promotion) -> Self {
        Self {
            id: p.id,
            kind: p.kind.clone(),
            value: p.value,
            buy_quantity: p.buy_quantity,
            get_quantity: p.get_quantity,
            min_quantity: p.min_quantity,
            min_amount: p.min_amount,
            tiers: p.tiers.clone(),
            ends_at: p.ends_at,
        }
    }
}

/// A product's promotions as a listing shows them
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProductOffers {
    /// The reduced price of one unit (catalogue promotions without conditions)
    pub price: Option<Decimal>,
    /// The lowest price of the 30 days before the reduction started
    pub reference_price: Option<Decimal>,
    pub promotions: Vec<AppliedPromotion>,
    /// Offers with conditions, and cart offers aimed at this product
    pub offers: Vec<Offer>,
}

/// The offers of each product in force for `buyer`; `items`: (product,
/// catalogue price). Products without any are left out.
pub async fn product_offers(
    conn: &mut PgConnection,
    buyer: &Buyer,
    items: &[(Uuid, Decimal)],
) -> Result<HashMap<Uuid, ProductOffers>> {
    let promotions = live_promotions(&mut *conn, buyer).await?;
    if promotions.is_empty() || items.is_empty() {
        return Ok(HashMap::new());
    }
    let ids: Vec<Uuid> = items.iter().map(|(id, _)| *id).collect();
    let paths = if promotions.iter().any(|p| p.scope == "category") {
        crate::services::price_list_service::category_paths(&mut *conn, &ids).await?
    } else {
        HashMap::new()
    };
    let mut found = HashMap::new();
    let none = HashSet::new();
    for (product, base) in items {
        let categories = paths.get(product).unwrap_or(&none);
        let offers = offers_for(&promotions, *product, categories, *base);
        if offers != ProductOffers::default() {
            found.insert(*product, offers);
        }
    }
    let started: Vec<(Uuid, Option<Uuid>, DateTime<Utc>)> = found
        .iter()
        .filter(|(_, o)| o.price.is_some())
        .map(|(id, o)| (*id, None, o.promotions.iter().filter_map(|p| p.starts_at).max().unwrap_or_else(Utc::now)))
        .collect();
    let references = reference_prices(&mut *conn, &started).await?;
    for (product, offers) in found.iter_mut() {
        offers.reference_price = references.get(&(*product, None)).copied();
    }
    Ok(found)
}

/// One product's reduced price and offers among `promotions`
fn offers_for(promotions: &[Promotion], product: Uuid, categories: &HashSet<Uuid>, base: Decimal) -> ProductOffers {
    let item = CatalogueItem { product_id: product, variant_id: None, categories, quantity: 1, base, negotiated: base };
    let mut result = ProductOffers::default();
    if let Some((price, used)) = catalogue_price(promotions, &item, None) {
        result.price = Some(price);
        result.promotions = used.into_iter().map(AppliedPromotion::from).collect();
    }
    result.offers = promotions
        .iter()
        .filter(|p| targets(&p.scope, p.target_id, product, None, categories))
        .filter(|p| !result.promotions.iter().any(|a| a.id == p.id))
        .filter(|p| p.scope != "all" || !CART_KINDS.contains(&p.kind.as_str()))
        .map(Offer::from)
        .collect();
    result
}

/// A cart line as cart promotions see it
#[derive(Debug, Clone)]
pub struct CartLineView {
    pub id: Uuid,
    pub product_id: Uuid,
    pub variant_id: Option<Uuid>,
    pub quantity: i32,
    pub unit_price: Decimal,
    /// Its product's categories with their ancestors
    pub categories: HashSet<Uuid>,
    /// Promotions already on the line (catalogue ones): (id, combinable)
    pub promotions: Vec<(Uuid, bool)>,
}

/// A promotion that applies to a cart, as the cart shows it
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CartPromotion {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    /// What it takes off the cart (zero for free delivery and gifts)
    pub discount: Decimal,
    /// Loyalty points redeemed (the `loyalty` line only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<i32>,
}

impl CartPromotion {
    /// The points a cart redeems: those of its loyalty line
    pub fn redeemed(promotions: &[CartPromotion]) -> i32 {
        promotions.iter().filter_map(|p| p.points).sum()
    }
}

/// What cart promotions do to a cart
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CartOutcome {
    /// Discount per cart line
    pub line_discounts: HashMap<Uuid, Decimal>,
    pub applied: Vec<CartPromotion>,
    pub free_delivery: bool,
    /// Gift lines to have: (promotion, product, quantity)
    pub gifts: Vec<(Uuid, Uuid, i32)>,
}

impl CartOutcome {
    /// Everything cart promotions take off
    pub fn discount(&self) -> Decimal {
        self.line_discounts.values().copied().sum()
    }
}

/// The lines `p` targets and may be added to
fn eligible<'a>(p: &'a Promotion, lines: &'a [CartLineView]) -> impl Iterator<Item = &'a CartLineView> {
    lines.iter().filter(move |l| {
        targets(&p.scope, p.target_id, l.product_id, l.variant_id, &l.categories)
            && can_add(p, &l.promotions)
    })
}

/// What a line still costs after the discounts so far
fn left(line: &CartLineView, out: &CartOutcome) -> Decimal {
    line.unit_price * Decimal::from(line.quantity)
        - out.line_discounts.get(&line.id).copied().unwrap_or_default()
}

/// Amount and quantity of the lines `p` may apply to
fn eligible_totals(p: &Promotion, lines: &[CartLineView], out: &CartOutcome) -> (Decimal, i32) {
    eligible(p, lines).fold((Decimal::ZERO, 0), |(a, q), l| (a + left(l, out), q + l.quantity))
}

/// What buy X get Y saves on a line
pub fn buy_x_get_y_discount(p: &Promotion, line: &CartLineView) -> Decimal {
    let (buy, get) = (p.buy_quantity.unwrap_or(1), p.get_quantity.unwrap_or(1));
    let units = (line.quantity / (buy + get)) * get;
    let percent = p.value.unwrap_or(Decimal::ONE_HUNDRED);
    (Decimal::from(units) * line.unit_price * percent / Decimal::ONE_HUNDRED).round_dp(2)
}

fn mark(lines: &mut [CartLineView], line: Uuid, p: &Promotion) {
    if let Some(l) = lines.iter_mut().find(|l| l.id == line) {
        l.promotions.push((p.id, p.combinable));
    }
}

fn applied(p: &Promotion, discount: Decimal) -> CartPromotion {
    CartPromotion { id: p.id, name: p.name.clone(), kind: p.kind.clone(), discount, points: None }
}

/// The best buy X get Y on each line
fn apply_buy_x_get_y(promotions: &[&Promotion], lines: &mut [CartLineView], out: &mut CartOutcome) {
    let mut best: HashMap<Uuid, (Decimal, &Promotion)> = HashMap::new();
    for p in promotions.iter().filter(|p| p.kind == "buy_x_get_y") {
        let (amount, _) = eligible_totals(p, lines, out);
        for line in eligible(p, lines) {
            let saved = buy_x_get_y_discount(p, line);
            let ok = saved > Decimal::ZERO && conditions_met(p, line.quantity, Some(amount));
            if ok && best.get(&line.id).is_none_or(|(s, _)| saved > *s) {
                best.insert(line.id, (saved, *p));
            }
        }
    }
    let mut by_promotion: HashMap<Uuid, (Decimal, &Promotion)> = HashMap::new();
    for (line, (saved, p)) in best {
        *out.line_discounts.entry(line).or_default() += saved;
        mark(lines, line, p);
        by_promotion.entry(p.id).or_insert((Decimal::ZERO, p)).0 += saved;
    }
    let mut found: Vec<CartPromotion> = by_promotion.into_values().map(|(s, p)| applied(p, s)).collect();
    found.sort_by_key(|c| c.id);
    out.applied.extend(found);
}

/// The discount of the best tier `p` reaches, with the amount it is taken from
fn tier_discount(p: &Promotion, lines: &[CartLineView], out: &CartOutcome) -> Option<(Decimal, Decimal)> {
    let (amount, quantity) = eligible_totals(p, lines, out);
    if amount <= Decimal::ZERO || !conditions_met(p, quantity, Some(amount)) {
        return None;
    }
    let percent = p.tiers.as_ref()?.iter().filter(|t| t.min_amount <= amount).map(|t| t.percent).max()?;
    Some((amount, (amount * percent / Decimal::ONE_HUNDRED).round_dp(2)))
}

/// The best spending tier among spend_tiers promotions, spread over its lines
fn apply_spend_tiers(promotions: &[&Promotion], lines: &mut [CartLineView], out: &mut CartOutcome) {
    let best = promotions
        .iter()
        .filter(|p| p.kind == "spend_tiers")
        .filter_map(|p| tier_discount(p, lines, out).map(|(a, d)| (*p, a, d)))
        .max_by_key(|(_, _, discount)| *discount);
    let Some((p, amount, discount)) = best else {
        return;
    };
    let shares: Vec<(Uuid, Decimal)> = eligible(p, lines).map(|l| (l.id, left(l, out))).collect();
    let mut rest = discount;
    for (i, (line, cost)) in shares.iter().enumerate() {
        let share = if i + 1 == shares.len() { rest } else { (discount * cost / amount).round_dp(2) };
        rest -= share;
        *out.line_discounts.entry(*line).or_default() += share;
        mark(lines, *line, p);
    }
    out.applied.push(applied(p, discount));
}

/// Free delivery and gifts once their conditions hold
fn apply_thresholds(promotions: &[&Promotion], lines: &[CartLineView], out: &mut CartOutcome) {
    for p in promotions.iter().filter(|p| matches!(p.kind.as_str(), "free_delivery" | "gift")) {
        let (amount, quantity) = eligible_totals(p, lines, out);
        if quantity == 0 || !conditions_met(p, quantity, Some(amount)) {
            continue;
        }
        match (p.kind.as_str(), p.gift_product_id) {
            ("free_delivery", _) => out.free_delivery = true,
            (_, Some(gift)) => out.gifts.push((p.id, gift, p.gift_quantity)),
            _ => continue,
        }
        out.applied.push(applied(p, Decimal::ZERO));
    }
}

/// What the cart promotions among `promotions` do to `lines` (gift lines
/// excluded by the caller): buy X get Y first, then spending tiers on what is
/// left, then free delivery and gifts
pub fn evaluate_cart(promotions: &[Promotion], lines: &[CartLineView]) -> CartOutcome {
    let cart: Vec<&Promotion> = promotions
        .iter()
        .filter(|p| CART_KINDS.contains(&p.kind.as_str()))
        .collect();
    let mut lines = lines.to_vec();
    let mut out = CartOutcome::default();
    apply_buy_x_get_y(&cart, &mut lines, &mut out);
    apply_spend_tiers(&cart, &mut lines, &mut out);
    apply_thresholds(&cart, &lines, &mut out);
    out
}

/// Columns staff lists sort on
const PROMOTION_SORTS: SortColumns = &[
    ("created", "created_at"),
    ("name", "lower(name)"),
    ("kind", "kind"),
    ("audience", "audience"),
    ("starts", "starts_at"),
    ("ends", "ends_at"),
    ("active", "is_active"),
];

/// Filters of the staff list
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PromotionQuery {
    pub q: Option<String>,
    pub kind: Option<String>,
    /// `live` (in force now), `scheduled`, `ended` or `off`
    pub state: Option<String>,
    pub sort: Option<String>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

fn state_filter(state: Option<&str>) -> Result<String> {
    Ok(match state {
        None => "TRUE".to_string(),
        Some("live") => LIVE.to_string(),
        Some("scheduled") => "p.is_active AND p.starts_at > NOW()".to_string(),
        Some("ended") => "p.ends_at <= NOW()".to_string(),
        Some("off") => "NOT p.is_active".to_string(),
        Some(_) => return Err(invalid("state must be live, scheduled, ended or off")),
    })
}

/// A promotion with how many orders used it
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PromotionWithUses {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub promotion: Promotion,
    pub uses: i64,
}

/// Promotions for staff screens
#[derive(Clone)]
pub struct PromotionService {
    pool: PgPool,
}

impl PromotionService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Promotions matching `query`, newest first by default, with their count
    pub async fn list(&self, query: &PromotionQuery) -> Result<(Vec<PromotionWithUses>, i64)> {
        let per_page = query.per_page.unwrap_or(50).clamp(1, 100);
        let page = query.page.unwrap_or(1).max(1);
        let q = query.q.as_deref().map(|q| format!("%{}%", q.trim().replace(['%', '_'], "")));
        let filter = format!(
            "WHERE ($1::text IS NULL OR p.name ILIKE $1) AND ($2::text IS NULL OR p.kind = $2) AND {}",
            state_filter(query.state.as_deref())?
        );
        let order = order_by(query.sort.as_deref(), PROMOTION_SORTS, "-created", "id")?;
        let rows = sqlx::query_as::<_, PromotionWithUses>(&format!(
            "SELECT p.*, (SELECT COUNT(*) FROM promotion_uses u WHERE u.promotion_id = p.id) AS uses \
             FROM promotions p {filter} ORDER BY {order} LIMIT $3 OFFSET $4"
        ))
        .bind(&q)
        .bind(&query.kind)
        .bind(per_page)
        .bind((page - 1) * per_page)
        .fetch_all(&self.pool)
        .await?;
        let total = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM promotions p {filter}"))
            .bind(&q)
            .bind(&query.kind)
            .fetch_one(&self.pool)
            .await?;
        Ok((rows, total))
    }

    pub async fn get(&self, id: Uuid) -> Result<Promotion> {
        sqlx::query_as::<_, Promotion>("SELECT * FROM promotions WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| Error::not_found("Promotion not found"))
    }

    /// Store a new promotion, or replace promotion `id`
    pub async fn save(&self, id: Option<Uuid>, p: NewPromotion) -> Result<Promotion> {
        check(&p)?;
        let sql = match id {
            None => "INSERT INTO promotions (name, kind, audience, scope, target_id, value, buy_quantity, \
                     get_quantity, tiers, min_amount, min_quantity, first_order_only, max_uses, \
                     max_uses_per_customer, combinable, gift_product_id, gift_quantity, starts_at, ends_at, \
                     is_active) VALUES ($2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, \
                     $17, $18, $19, $20, $21) RETURNING *",
            Some(_) => "UPDATE promotions SET name = $2, kind = $3, audience = $4, scope = $5, target_id = $6, \
                        value = $7, buy_quantity = $8, get_quantity = $9, tiers = $10, min_amount = $11, \
                        min_quantity = $12, first_order_only = $13, max_uses = $14, max_uses_per_customer = $15, \
                        combinable = $16, gift_product_id = $17, gift_quantity = $18, starts_at = $19, \
                        ends_at = $20, is_active = $21, updated_at = NOW() WHERE id = $1 RETURNING *",
        };
        sqlx::query_as::<_, Promotion>(sql)
            .bind(id)
            .bind(p.name.trim())
            .bind(&p.kind)
            .bind(&p.audience)
            .bind(&p.scope)
            .bind(p.target_id)
            .bind(p.value)
            .bind(p.buy_quantity)
            .bind(p.get_quantity)
            .bind(p.tiers.map(Json))
            .bind(p.min_amount)
            .bind(p.min_quantity)
            .bind(p.first_order_only)
            .bind(p.max_uses)
            .bind(p.max_uses_per_customer)
            .bind(p.combinable)
            .bind(p.gift_product_id)
            .bind(p.gift_quantity)
            .bind(p.starts_at)
            .bind(p.ends_at)
            .bind(p.is_active)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| Error::not_found("Promotion not found"))
    }

    /// Switch a promotion on or off
    pub async fn set_active(&self, id: Uuid, active: bool) -> Result<Promotion> {
        sqlx::query_as::<_, Promotion>(
            "UPDATE promotions SET is_active = $2, updated_at = NOW() WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(active)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| Error::not_found("Promotion not found"))
    }

    pub async fn delete(&self, id: Uuid) -> Result<()> {
        let done = sqlx::query("DELETE FROM promotions WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if done.rows_affected() == 0 {
            return Err(Error::not_found("Promotion not found"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn promotion(kind: &str) -> Promotion {
        Promotion {
            id: Uuid::new_v4(),
            name: kind.into(),
            kind: kind.into(),
            audience: "customer".into(),
            scope: "all".into(),
            target_id: None,
            value: None,
            buy_quantity: None,
            get_quantity: None,
            tiers: None,
            min_amount: None,
            min_quantity: None,
            first_order_only: false,
            max_uses: None,
            max_uses_per_customer: None,
            combinable: false,
            gift_product_id: None,
            gift_quantity: 1,
            starts_at: None,
            ends_at: None,
            is_active: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn valued(kind: &str, value: Decimal, combinable: bool) -> Promotion {
        Promotion { value: Some(value), combinable, ..promotion(kind) }
    }

    fn line(quantity: i32, unit_price: Decimal) -> CartLineView {
        CartLineView {
            id: Uuid::new_v4(),
            product_id: Uuid::new_v4(),
            variant_id: None,
            quantity,
            unit_price,
            categories: HashSet::new(),
            promotions: Vec::new(),
        }
    }

    fn item(categories: &HashSet<Uuid>, quantity: i32, base: Decimal, negotiated: Decimal) -> CatalogueItem<'_> {
        CatalogueItem { product_id: Uuid::new_v4(), variant_id: None, categories, quantity, base, negotiated }
    }

    #[test]
    fn the_cheapest_option_wins_and_only_combinable_ones_add_up() {
        let none = HashSet::new();
        let ten = valued("percent_off", dec!(10), false);
        let five = valued("amount_off", dec!(5), false);
        let all = [ten.clone(), five.clone()];
        let (price, used) = catalogue_price(&all, &item(&none, 1, dec!(64), dec!(64)), None).unwrap();
        assert_eq!((price, used[0].id), (dec!(57.60), ten.id), "not added up: the best one");
        let stack = [valued("percent_off", dec!(10), true), valued("amount_off", dec!(5), true)];
        let (price, used) = catalogue_price(&stack, &item(&none, 1, dec!(64), dec!(64)), None).unwrap();
        assert_eq!((price, used.len()), (dec!(52.60), 2), "-10 % then -5 €");
        // combinable ones stack on a negotiated price; a lower negotiated price wins
        let (price, _) = catalogue_price(&stack, &item(&none, 1, dec!(64), dec!(60)), None).unwrap();
        assert_eq!(price, dec!(49));
        assert!(catalogue_price(&all, &item(&none, 1, dec!(64), dec!(50)), None).is_none());
    }

    #[test]
    fn conditions_hold_back_a_promotion() {
        let none = HashSet::new();
        let mut from_three = valued("percent_off", dec!(10), false);
        from_three.min_quantity = Some(3);
        let mut from_100 = valued("percent_off", dec!(20), false);
        from_100.min_amount = Some(dec!(100));
        let all = [from_three.clone(), from_100.clone()];
        assert!(catalogue_price(&all, &item(&none, 2, dec!(10), dec!(10)), None).is_none());
        let (price, _) = catalogue_price(&all, &item(&none, 3, dec!(10), dec!(10)), None).unwrap();
        assert_eq!(price, dec!(9), "the amount is unknown on a listing");
        let amounts = HashMap::from([(from_100.id, dec!(120))]);
        let (price, _) = catalogue_price(&all, &item(&none, 3, dec!(10), dec!(10)), Some(&amounts)).unwrap();
        assert_eq!(price, dec!(8), "in a cart of 120 €");
    }

    #[test]
    fn buy_x_get_y_frees_whole_groups_only() {
        let mut three_plus_one = promotion("buy_x_get_y");
        (three_plus_one.buy_quantity, three_plus_one.get_quantity) = (Some(3), Some(1));
        assert_eq!(buy_x_get_y_discount(&three_plus_one, &line(7, dec!(10))), dec!(10));
        assert_eq!(buy_x_get_y_discount(&three_plus_one, &line(8, dec!(10))), dec!(20));
        let mut second_half = promotion("buy_x_get_y");
        (second_half.buy_quantity, second_half.get_quantity, second_half.value) =
            (Some(1), Some(1), Some(dec!(50)));
        assert_eq!(buy_x_get_y_discount(&second_half, &line(3, dec!(8))), dec!(4));
    }

    #[test]
    fn cart_promotions_add_up_only_when_combinable() {
        let tiers = Some(Json(vec![
            SpendTier { min_amount: dec!(50), percent: dec!(5) },
            SpendTier { min_amount: dec!(100), percent: dec!(10) },
        ]));
        let mut one_plus_one = promotion("buy_x_get_y");
        (one_plus_one.buy_quantity, one_plus_one.get_quantity) = (Some(1), Some(1));
        let lines = [line(2, dec!(60))];
        // not combinable: the tier skips the line buy X get Y took
        let spend = Promotion { tiers: tiers.clone(), ..promotion("spend_tiers") };
        let out = evaluate_cart(&[spend, one_plus_one.clone()], &lines);
        assert_eq!(out.discount(), dec!(60));
        // both combinable: 120 - 60 free = 60 left, the 5 % tier
        let spend = Promotion { tiers, combinable: true, ..promotion("spend_tiers") };
        let one_plus_one = Promotion { combinable: true, ..one_plus_one };
        let out = evaluate_cart(&[spend, one_plus_one], &lines);
        assert_eq!(out.discount(), dec!(63));
        // a line priced by a non-combinable catalogue promotion takes nothing more
        let mut reduced = line(2, dec!(60));
        reduced.promotions.push((Uuid::new_v4(), false));
        let gift = Promotion { gift_product_id: Some(Uuid::new_v4()), ..promotion("gift") };
        assert!(evaluate_cart(&[gift], &[reduced]).gifts.is_empty());
    }

    #[test]
    fn thresholds_unlock_free_delivery_and_gifts() {
        let delivery = Promotion { min_amount: Some(dec!(80)), ..promotion("free_delivery") };
        assert!(!evaluate_cart(&[delivery.clone()], &[line(1, dec!(50))]).free_delivery);
        let out = evaluate_cart(&[delivery], &[line(2, dec!(50))]);
        assert!(out.free_delivery && out.applied.len() == 1);
        let gift_product = Uuid::new_v4();
        let gift = Promotion {
            min_quantity: Some(10),
            gift_product_id: Some(gift_product),
            gift_quantity: 2,
            ..promotion("gift")
        };
        assert_eq!(evaluate_cart(&[gift], &[line(12, dec!(1))]).gifts[0].1, gift_product);
    }

    #[test]
    fn checks_refuse_incomplete_promotions() {
        let new = |kind: &str| NewPromotion {
            name: "Sale".into(),
            kind: kind.into(),
            audience: "customer".into(),
            scope: "all".into(),
            target_id: None,
            value: None,
            buy_quantity: None,
            get_quantity: None,
            tiers: None,
            min_amount: None,
            min_quantity: None,
            first_order_only: false,
            max_uses: None,
            max_uses_per_customer: None,
            combinable: false,
            gift_product_id: None,
            gift_quantity: 1,
            starts_at: None,
            ends_at: None,
            is_active: true,
        };
        assert!(check(&new("percent_off")).is_err());
        assert!(check(&NewPromotion { value: Some(dec!(20)), ..new("percent_off") }).is_ok());
        assert!(check(&NewPromotion { value: Some(dec!(120)), ..new("percent_off") }).is_err());
        let two_plus_one = NewPromotion { buy_quantity: Some(2), get_quantity: Some(1), ..new("buy_x_get_y") };
        assert!(check(&two_plus_one).is_ok());
        assert!(check(&NewPromotion { scope: "product".into(), ..new("free_delivery") }).is_err());
        assert!(check(&NewPromotion { audience: "vip".into(), value: Some(dec!(5)), ..new("amount_off") }).is_err());
        assert!(check(&NewPromotion { max_uses: Some(0), ..new("free_delivery") }).is_err());
        assert!(check(&new("gift")).is_err());
    }
}
