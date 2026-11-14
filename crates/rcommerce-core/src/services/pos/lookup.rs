//! What a code scanned or typed at the till names: a loyalty card (13 digits
//! starting with `29`, with a valid EAN-13 check digit), a product or variant
//! (GTIN or SKU), or an order (its number). USB scanners type the code and
//! Enter, so the till sends whatever was typed.

use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::services::loyalty_service::ean13_check;
use crate::Result;

/// What a code names
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Lookup {
    Product(ProductHit),
    Customer(CustomerHit),
    Order(OrderHit),
    NotFound { code: String },
}

/// A product or one of its variants
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ProductHit {
    pub product_id: Uuid,
    pub variant_id: Option<Uuid>,
    pub title: String,
    pub variant_title: Option<String>,
    pub sku: Option<String>,
    pub gtin: Option<String>,
    /// Catalogue price excl. VAT (the basket prices it for the customer)
    pub price: Decimal,
    pub is_active: bool,
    /// Free stock at the till's branch
    pub available: i64,
}

/// A customer, found by their loyalty card
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CustomerHit {
    pub customer_id: Uuid,
    pub email: String,
    pub name: String,
    pub loyalty_card: Option<String>,
    pub company_id: Option<Uuid>,
    pub company_name: Option<String>,
}

/// An order, found by its number
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OrderHit {
    pub order_id: Uuid,
    pub order_number: String,
    pub status: String,
    pub payment_status: String,
    pub total: Decimal,
    pub customer_email: String,
}

/// Whether `code` is one of our loyalty cards: 13 digits, `29` first, a valid
/// check digit
pub fn is_loyalty_card(code: &str) -> bool {
    code.len() == 13
        && code.starts_with("29")
        && code.bytes().all(|b| b.is_ascii_digit())
        && code[12..].parse::<u32>().ok() == Some(ean13_check(&code[..12]))
}

/// A GTIN in its 13-digit form when it has 8, 12, 13 or 14 digits (leading
/// zeros added or dropped), so a UPC-A scan finds an EAN-13 product
pub fn gtin13(code: &str) -> Option<String> {
    if !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match code.len() {
        8 | 12 | 13 => Some(format!("{code:0>13}")),
        14 if code.starts_with('0') => Some(code[1..].to_string()),
        _ => None,
    }
}

/// Find what `code` names, with stock counted at `location`
pub async fn lookup(pool: &PgPool, code: &str, location: Uuid) -> Result<Lookup> {
    let code = code.trim();
    if code.is_empty() {
        return Ok(Lookup::NotFound {
            code: String::new(),
        });
    }
    if is_loyalty_card(code) {
        if let Some(customer) = card_holder(pool, code).await? {
            return Ok(Lookup::Customer(customer));
        }
    }
    if let Some(product) = product(pool, code, location).await? {
        return Ok(Lookup::Product(product));
    }
    if let Some(order) = order(pool, code).await? {
        return Ok(Lookup::Order(order));
    }
    Ok(Lookup::NotFound {
        code: code.to_string(),
    })
}

async fn card_holder(pool: &PgPool, card: &str) -> Result<Option<CustomerHit>> {
    let hit = sqlx::query_as(
        "SELECT c.id AS customer_id, c.email, \
         TRIM(COALESCE(c.first_name, '') || ' ' || COALESCE(c.last_name, '')) AS name, \
         c.loyalty_card, m.company_id, co.name AS company_name \
         FROM customers c LEFT JOIN company_members m ON m.customer_id = c.id \
         LEFT JOIN companies co ON co.id = m.company_id \
         WHERE c.loyalty_card = $1 LIMIT 1",
    )
    .bind(card)
    .fetch_optional(pool)
    .await?;
    Ok(hit)
}

/// A variant first (its own GTIN or SKU), then a product
async fn product(pool: &PgPool, code: &str, location: Uuid) -> Result<Option<ProductHit>> {
    let gtin = gtin13(code);
    let hit = sqlx::query_as(
        "WITH found AS ( \
           SELECT v.product_id, v.id AS variant_id, v.title AS variant_title, v.sku, v.gtin, \
                  COALESCE(v.price, p.price) AS price, 0 AS rank \
           FROM product_variants v JOIN products p ON p.id = v.product_id \
           WHERE v.is_active AND ((v.gtin IS NOT NULL AND lpad(v.gtin, 13, '0') = $1) \
                 OR lower(v.sku) = lower($2)) \
           UNION ALL \
           SELECT p.id, NULL, NULL, p.sku, p.gtin, p.price, 1 FROM products p \
           WHERE (p.gtin IS NOT NULL AND lpad(p.gtin, 13, '0') = $1) OR lower(p.sku) = lower($2)) \
         SELECT f.product_id, f.variant_id, p.title, f.variant_title, f.sku, f.gtin, f.price, \
                p.is_active, COALESCE((SELECT SUM(l.available_quantity) FROM inventory_levels l \
                  WHERE l.product_id = f.product_id AND l.location_id = $3 \
                    AND l.variant_id IS NOT DISTINCT FROM f.variant_id), 0)::bigint AS available \
         FROM found f JOIN products p ON p.id = f.product_id \
         ORDER BY f.rank, p.is_active DESC LIMIT 1",
    )
    .bind(gtin)
    .bind(code)
    .bind(location)
    .fetch_optional(pool)
    .await?;
    Ok(hit)
}

async fn order(pool: &PgPool, code: &str) -> Result<Option<OrderHit>> {
    let hit = sqlx::query_as(
        "SELECT id AS order_id, order_number, status::text AS status, \
         payment_status::text AS payment_status, total, customer_email \
         FROM orders WHERE upper(order_number) = upper($1) LIMIT 1",
    )
    .bind(code)
    .fetch_optional(pool)
    .await?;
    Ok(hit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loyalty_cards_need_the_prefix_and_a_valid_check_digit() {
        let body = "290000012345";
        let card = format!("{body}{}", ean13_check(body));
        assert!(is_loyalty_card(&card));
        let wrong = format!("{body}{}", (ean13_check(body) + 1) % 10);
        assert!(!is_loyalty_card(&wrong));
        assert!(!is_loyalty_card("5410000000001"));
        assert!(!is_loyalty_card("29000001234"));
    }

    #[test]
    fn gtins_compare_in_their_13_digit_form() {
        assert_eq!(gtin13("036000291452").as_deref(), Some("0036000291452"));
        assert_eq!(gtin13("5449000000996").as_deref(), Some("5449000000996"));
        assert_eq!(gtin13("05449000000996").as_deref(), Some("5449000000996"));
        assert_eq!(gtin13("96385074").as_deref(), Some("0000096385074"));
        assert_eq!(gtin13("ST-0001"), None);
        assert_eq!(gtin13("12345"), None);
    }
}
