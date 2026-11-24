//! What a promotion would do, before staff save it: how many products it
//! covers, sample prices before and after (catalogue kinds), example carts at
//! its thresholds (cart kinds, or a minimum amount), the live promotions on
//! the same products, and warnings. Nothing is stored: the draft is priced
//! with the same functions as listings and carts.

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::services::promotion_service::{
    catalogue_price, check, evaluate_cart, targets, CartLineView, CatalogueItem, NewPromotion,
    Promotion, CART_KINDS, CATALOGUE_KINDS,
};
use crate::Result;

/// Products sampled for prices and overlaps
const COVERED_SAMPLE: i64 = 200;
/// Sample prices shown
const SAMPLES: usize = 8;

/// A product's price before and after the promotion (excluding VAT)
#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub product_id: Uuid,
    pub title: String,
    pub sku: Option<String>,
    pub base: Decimal,
    pub price: Decimal,
}

/// A line of an example cart
#[derive(Debug, Clone, Serialize)]
pub struct Line {
    pub title: String,
    pub quantity: i32,
    pub unit_price: Decimal,
}

/// An example cart and what the promotion does to it (excluding VAT)
#[derive(Debug, Clone, Serialize)]
pub struct ExampleCart {
    /// `one`, `below_min`, `at_min`, `below_tier`, `at_tier`, `buy_only`,
    /// `buy_and_get`
    pub label: String,
    /// The threshold the cart is set against (minimum amount or tier)
    pub threshold: Option<Decimal>,
    pub lines: Vec<Line>,
    pub subtotal: Decimal,
    pub discount: Decimal,
    pub free_delivery: bool,
    pub gift_quantity: i32,
}

/// A live promotion on some of the same products
#[derive(Debug, Clone, Serialize)]
pub struct Overlap {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub combinable: bool,
}

/// The preview
#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    /// Active products covered
    pub reach: i64,
    pub samples: Vec<Sample>,
    pub carts: Vec<ExampleCart>,
    pub overlaps: Vec<Overlap>,
    /// `first_order_only`, `starts_in_past`, `already_ended`, `inactive`,
    /// `no_products`, `cart_amount_needed`
    pub warnings: Vec<String>,
}

/// The draft as a promotion (not stored)
fn as_promotion(p: &NewPromotion) -> Promotion {
    let now = Utc::now();
    Promotion {
        id: Uuid::new_v4(),
        name: p.name.clone(),
        kind: p.kind.clone(),
        audience: p.audience.clone(),
        scope: p.scope.clone(),
        target_id: p.target_id,
        value: p.value,
        buy_quantity: p.buy_quantity,
        get_quantity: p.get_quantity,
        tiers: p.tiers.clone().map(sqlx::types::Json),
        min_amount: p.min_amount,
        min_quantity: p.min_quantity,
        first_order_only: p.first_order_only,
        max_uses: p.max_uses,
        max_uses_per_customer: p.max_uses_per_customer,
        combinable: p.combinable,
        gift_product_id: p.gift_product_id,
        gift_quantity: p.gift_quantity,
        starts_at: p.starts_at,
        ends_at: p.ends_at,
        is_active: p.is_active,
        created_at: now,
        updated_at: now,
    }
}

/// A product the promotion covers, with its categories and their ancestors
struct Covered {
    id: Uuid,
    title: String,
    sku: Option<String>,
    price: Decimal,
    categories: HashSet<Uuid>,
}

/// The SQL condition on `p` (products) for the draft's scope, `$1` its target
fn scope_filter(scope: &str) -> &'static str {
    match scope {
        "category" => {
            "p.id IN (SELECT r.product_id FROM product_category_relations r WHERE r.category_id IN ( \
               WITH RECURSIVE below(id) AS (SELECT $1::uuid UNION \
                 SELECT c.id FROM product_categories c JOIN below b ON c.parent_id = b.id) \
               SELECT id FROM below))"
        }
        "product" => "p.id = $1",
        "variant" => "p.id IN (SELECT product_id FROM product_variants WHERE id = $1)",
        _ => "$1::uuid IS NULL",
    }
}

/// How many active products the draft covers, and a sample of them
async fn covered(pool: &PgPool, p: &NewPromotion) -> Result<(i64, Vec<Covered>)> {
    let filter = scope_filter(&p.scope);
    let reach: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM products p WHERE p.is_active AND {filter}"
    ))
    .bind(p.target_id)
    .fetch_one(pool)
    .await?;
    let rows: Vec<(Uuid, String, Option<String>, Decimal)> = sqlx::query_as(&format!(
        "SELECT p.id, p.title, p.sku, p.price FROM products p WHERE p.is_active AND {filter} \
         ORDER BY p.created_at DESC, p.id LIMIT $2"
    ))
    .bind(p.target_id)
    .bind(COVERED_SAMPLE)
    .fetch_all(pool)
    .await?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    let categories = categories_of(pool, &ids).await?;
    Ok((
        reach,
        rows.into_iter()
            .map(|(id, title, sku, price)| Covered {
                categories: categories.get(&id).cloned().unwrap_or_default(),
                id,
                title,
                sku,
                price,
            })
            .collect(),
    ))
}

/// Each product's categories with their ancestors
async fn categories_of(pool: &PgPool, products: &[Uuid]) -> Result<HashMap<Uuid, HashSet<Uuid>>> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "WITH RECURSIVE up(product_id, category_id) AS ( \
           SELECT product_id, category_id FROM product_category_relations WHERE product_id = ANY($1) \
           UNION SELECT u.product_id, c.parent_id FROM up u JOIN product_categories c \
             ON c.id = u.category_id WHERE c.parent_id IS NOT NULL) \
         SELECT product_id, category_id FROM up",
    )
    .bind(products)
    .fetch_all(pool)
    .await?;
    let mut map: HashMap<Uuid, HashSet<Uuid>> = HashMap::new();
    for (product, category) in rows {
        map.entry(product).or_default().insert(category);
    }
    Ok(map)
}

/// Sample prices: the catalogue price for one unit (at the minimum quantity)
fn samples(promo: &Promotion, covered: &[Covered]) -> Vec<Sample> {
    let catalogue = CATALOGUE_KINDS.contains(&promo.kind.as_str());
    covered
        .iter()
        .take(SAMPLES)
        .map(|c| {
            let item = CatalogueItem {
                product_id: c.id,
                variant_id: promo.target_id.filter(|_| promo.scope == "variant"),
                categories: &c.categories,
                quantity: promo.min_quantity.unwrap_or(1).max(1),
                base: c.price,
                negotiated: c.price,
            };
            let price = if catalogue {
                catalogue_price(std::slice::from_ref(promo), &item, None)
                    .map(|(p, _)| p)
                    .unwrap_or(c.price)
            } else {
                c.price
            };
            Sample {
                product_id: c.id,
                title: c.title.clone(),
                sku: c.sku.clone(),
                base: c.price,
                price,
            }
        })
        .collect()
}

/// `quantity` units of `product` as a cart
fn cart_of(promo: &Promotion, product: &Covered, quantity: i32) -> Vec<CartLineView> {
    vec![CartLineView {
        id: Uuid::new_v4(),
        product_id: product.id,
        variant_id: promo.target_id.filter(|_| promo.scope == "variant"),
        quantity,
        unit_price: product.price,
        categories: product.categories.clone(),
        promotions: Vec::new(),
    }]
}

/// What the promotion does to a cart of `quantity` units of `product`
fn example(
    promo: &Promotion,
    product: &Covered,
    quantity: i32,
    label: &str,
    threshold: Option<Decimal>,
) -> ExampleCart {
    let lines = cart_of(promo, product, quantity.max(1));
    let subtotal: Decimal = lines
        .iter()
        .map(|l| l.unit_price * Decimal::from(l.quantity))
        .sum();
    let (discount, free_delivery, gift_quantity) = if CART_KINDS.contains(&promo.kind.as_str()) {
        let out = evaluate_cart(std::slice::from_ref(promo), &lines);
        let gifts = out.gifts.iter().map(|g| g.2).sum();
        (out.discount(), out.free_delivery, gifts)
    } else {
        let l = &lines[0];
        let amounts = HashMap::from([(promo.id, subtotal)]);
        let item = CatalogueItem {
            product_id: l.product_id,
            variant_id: l.variant_id,
            categories: &l.categories,
            quantity: l.quantity,
            base: l.unit_price,
            negotiated: l.unit_price,
        };
        let unit = catalogue_price(std::slice::from_ref(promo), &item, Some(&amounts))
            .map(|(p, _)| p)
            .unwrap_or(l.unit_price);
        ((l.unit_price - unit) * Decimal::from(l.quantity), false, 0)
    };
    ExampleCart {
        label: label.into(),
        threshold,
        lines: lines
            .iter()
            .map(|l| Line {
                title: product.title.clone(),
                quantity: l.quantity,
                unit_price: l.unit_price,
            })
            .collect(),
        subtotal,
        discount,
        free_delivery,
        gift_quantity,
    }
}

/// Units of `price` to just reach `amount`, and the units just below it
fn units_around(amount: Decimal, price: Decimal) -> (i32, i32) {
    if price <= Decimal::ZERO {
        return (1, 1);
    }
    let at = amount
        .checked_div(price)
        .unwrap_or(Decimal::MAX)
        .ceil()
        .to_i32()
        .unwrap_or(i32::MAX)
        .clamp(1, 9_999);
    (at, (at - 1).max(1))
}

/// Example carts at the promotion's thresholds
fn carts(promo: &Promotion, product: Option<&Covered>) -> Vec<ExampleCart> {
    let Some(product) = product else {
        return Vec::new();
    };
    let catalogue = CATALOGUE_KINDS.contains(&promo.kind.as_str());
    let mut out = Vec::new();
    if promo.kind == "buy_x_get_y" {
        let (buy, get) = (
            promo.buy_quantity.unwrap_or(1),
            promo.get_quantity.unwrap_or(1),
        );
        out.push(example(promo, product, buy, "buy_only", None));
        out.push(example(
            promo,
            product,
            buy.saturating_add(get),
            "buy_and_get",
            None,
        ));
    } else if promo.kind == "spend_tiers" {
        let mut tiers: Vec<Decimal> = promo
            .tiers
            .iter()
            .flat_map(|t| t.0.iter().map(|x| x.min_amount))
            .collect();
        tiers.sort();
        tiers.dedup();
        for tier in tiers {
            let (at, below) = units_around(tier, product.price);
            if Decimal::from(below) * product.price < tier {
                out.push(example(promo, product, below, "below_tier", Some(tier)));
            }
            out.push(example(promo, product, at, "at_tier", Some(tier)));
        }
    }
    if let Some(min) = promo.min_amount.filter(|m| *m > Decimal::ZERO) {
        if promo.kind != "spend_tiers" {
            let (at, below) = units_around(min, product.price);
            if Decimal::from(below) * product.price < min {
                out.push(example(promo, product, below, "below_min", Some(min)));
            }
            out.push(example(promo, product, at, "at_min", Some(min)));
        }
    } else if !catalogue && out.is_empty() {
        let units = promo.min_quantity.unwrap_or(1);
        out.push(example(promo, product, units, "one", None));
    }
    out
}

/// Live promotions (other than `exclude`) that reach some covered product
/// for the same audience
async fn overlaps(
    pool: &PgPool,
    promo: &Promotion,
    covered: &[Covered],
    exclude: Option<Uuid>,
) -> Result<Vec<Overlap>> {
    let live: Vec<Promotion> = sqlx::query_as(
        "SELECT * FROM promotions p WHERE p.is_active AND (p.starts_at IS NULL OR p.starts_at <= NOW()) \
         AND (p.ends_at IS NULL OR p.ends_at > NOW()) AND ($1::uuid IS NULL OR p.id <> $1) \
         ORDER BY p.created_at",
    )
    .bind(exclude)
    .fetch_all(pool)
    .await?;
    let audiences_meet = |a: &str, b: &str| a == "both" || b == "both" || a == b;
    Ok(live
        .into_iter()
        .filter(|other| audiences_meet(&other.audience, &promo.audience))
        .filter(|other| {
            covered.iter().any(|c| {
                let variant = promo.target_id.filter(|_| promo.scope == "variant");
                targets(&other.scope, other.target_id, c.id, variant, &c.categories)
            })
        })
        .map(|o| Overlap {
            id: o.id,
            name: o.name,
            kind: o.kind,
            combinable: o.combinable,
        })
        .collect())
}

fn warnings(promo: &Promotion, reach: i64) -> Vec<String> {
    let now = Utc::now();
    let mut out = Vec::new();
    if promo.first_order_only {
        out.push("first_order_only");
    }
    if promo
        .starts_at
        .is_some_and(|s| s < now - chrono::Duration::hours(1))
    {
        out.push("starts_in_past");
    }
    if promo.ends_at.is_some_and(|e| e <= now) {
        out.push("already_ended");
    }
    if !promo.is_active {
        out.push("inactive");
    }
    if reach == 0 {
        out.push("no_products");
    }
    if CATALOGUE_KINDS.contains(&promo.kind.as_str())
        && promo.min_amount.is_some_and(|m| m > Decimal::ZERO)
    {
        out.push("cart_amount_needed");
    }
    out.into_iter().map(str::to_string).collect()
}

/// The preview of a draft (`exclude`: the promotion being edited)
pub async fn preview(
    pool: &PgPool,
    draft: &NewPromotion,
    exclude: Option<Uuid>,
) -> Result<Preview> {
    check(draft)?;
    let promo = as_promotion(draft);
    let (reach, covered) = covered(pool, draft).await?;
    Ok(Preview {
        reach,
        samples: samples(&promo, &covered),
        carts: carts(&promo, covered.first()),
        overlaps: overlaps(pool, &promo, &covered, exclude).await?,
        warnings: warnings(&promo, reach),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn product(price: &str) -> Covered {
        Covered {
            id: Uuid::new_v4(),
            title: "Cement 25 kg".into(),
            sku: None,
            price: price.parse().unwrap(),
            categories: HashSet::new(),
        }
    }

    fn draft(kind: &str) -> NewPromotion {
        serde_json::from_value(serde_json::json!({"name": "Test", "kind": kind, "value": "10"}))
            .unwrap()
    }

    #[test]
    fn samples_show_the_price_after() {
        let promo = as_promotion(&draft("percent_off"));
        let s = samples(&promo, &[product("8.50")]);
        assert_eq!(
            (s[0].base, s[0].price),
            ("8.50".parse().unwrap(), "7.65".parse().unwrap())
        );
    }

    #[test]
    fn carts_sit_around_the_thresholds() {
        let mut d = draft("order_amount_off");
        d.min_amount = Some("100".parse().unwrap());
        let promo = as_promotion(&d);
        let carts = carts(&promo, Some(&product("30")));
        let labels: Vec<_> = carts
            .iter()
            .map(|c| (c.label.as_str(), c.subtotal, c.discount))
            .collect();
        assert_eq!(
            labels,
            [
                ("below_min", "90".parse().unwrap(), Decimal::ZERO),
                ("at_min", "120".parse().unwrap(), "10".parse().unwrap())
            ]
        );
        let mut bxgy = draft("buy_x_get_y");
        (bxgy.buy_quantity, bxgy.get_quantity, bxgy.value) = (Some(2), Some(1), None);
        let carts = carts_for(&bxgy);
        assert_eq!(carts[1].label, "buy_and_get");
        assert_eq!(
            carts[1].discount,
            "30".parse::<Decimal>().unwrap(),
            "the third unit free"
        );
    }

    fn carts_for(d: &NewPromotion) -> Vec<ExampleCart> {
        carts(&as_promotion(d), Some(&product("30")))
    }

    #[test]
    fn warnings_name_what_staff_may_miss() {
        let mut d = draft("percent_off");
        d.first_order_only = true;
        d.min_amount = Some("50".parse().unwrap());
        d.ends_at = Some(Utc::now() - chrono::Duration::days(1));
        let w = warnings(&as_promotion(&d), 0);
        for code in [
            "first_order_only",
            "already_ended",
            "no_products",
            "cart_amount_needed",
        ] {
            assert!(w.iter().any(|x| x == code), "{code} in {w:?}");
        }
    }
}
