//! A till basket: a cart that belongs to a till session, priced by the same
//! engine as the web shop (price lists, promotions, coupon, points), plus
//! manual discounts the cashier gives within their cap. How a sale is paid:
//! cash and approved card terminal transactions, with the change due.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{Error, Result};

/// One line of a basket
#[derive(Debug, Clone, Deserialize)]
pub struct BasketLine {
    pub product_id: Uuid,
    #[serde(default)]
    pub variant_id: Option<Uuid>,
    pub quantity: i32,
    /// A manual discount, in percent, within the cashier's cap (a number or text)
    #[serde(default, deserialize_with = "crate::models::staff::percent")]
    pub discount_percent: Option<Decimal>,
    /// Why it is given (required with a discount)
    #[serde(default)]
    pub discount_reason: Option<String>,
}

/// The cart token of a session's baskets
pub fn till_token(session: Uuid) -> String {
    format!("pos:{session}")
}

/// Check the lines: 1 to 200 lines, quantities 1 to 9999, each discount
/// above 0, at most `cap` percent and with a reason
pub fn check_lines(lines: &[BasketLine], cap: Decimal) -> Result<()> {
    if lines.is_empty() || lines.len() > 200 {
        return Err(Error::validation("A basket has 1 to 200 lines"));
    }
    for line in lines {
        if !(1..=9999).contains(&line.quantity) {
            return Err(Error::validation("Quantities go from 1 to 9999"));
        }
        let Some(percent) = line.discount_percent else {
            continue;
        };
        if percent <= Decimal::ZERO || percent > Decimal::ONE_HUNDRED {
            return Err(Error::validation(
                "A discount is a percentage above 0, at most 100",
            ));
        }
        if percent > cap {
            return Err(Error::forbidden(format!(
                "A manual discount of {} % is above your limit of {} %",
                percent.normalize(),
                cap.normalize()
            )));
        }
        let reason = line.discount_reason.as_deref().map(str::trim).unwrap_or("");
        if reason.is_empty() || reason.chars().count() > 200 {
            return Err(Error::validation(
                "Give the reason of each manual discount (at most 200 characters)",
            ));
        }
    }
    Ok(())
}

/// Lines with the same product and variant, merged (quantities added, the
/// first line's discount kept)
fn merged(lines: &[BasketLine]) -> Vec<BasketLine> {
    let mut out: Vec<BasketLine> = Vec::new();
    for line in lines {
        match out
            .iter_mut()
            .find(|l| l.product_id == line.product_id && l.variant_id == line.variant_id)
        {
            Some(existing) => existing.quantity = (existing.quantity + line.quantity).min(9999),
            None => out.push(line.clone()),
        }
    }
    out
}

/// Who buys and the basket to write
pub struct BasketWrite<'a> {
    pub session: Uuid,
    pub basket: Option<Uuid>,
    pub customer: Option<Uuid>,
    pub email: Option<&'a str>,
    pub currency: &'a str,
    pub lines: &'a [BasketLine],
    /// Points to redeem (customers only)
    pub points: i32,
}

/// Create the session's basket or replace the lines of one it owns; its id
pub async fn write_basket(pool: &PgPool, basket: BasketWrite<'_>) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let token = till_token(basket.session);
    let id = match basket.basket {
        Some(id) => {
            let owned: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM carts WHERE id = $1 AND session_token = $2 \
                 AND NOT converted_to_order FOR UPDATE",
            )
            .bind(id)
            .bind(&token)
            .fetch_optional(&mut *tx)
            .await?;
            owned.ok_or_else(|| Error::not_found("Basket not found in this till session"))?
        }
        None => sqlx::query_scalar(
            "INSERT INTO carts (id, session_token, currency, subtotal, discount_total, tax_total, \
                 shipping_total, total, created_at, updated_at, expires_at, converted_to_order) \
                 VALUES (gen_random_uuid(), $1, $2::currency, 0, 0, 0, 0, 0, NOW(), NOW(), \
                 NOW() + INTERVAL '1 day', false) RETURNING id",
        )
        .bind(&token)
        .bind(basket.currency)
        .fetch_one(&mut *tx)
        .await?,
    };
    sqlx::query("UPDATE carts SET customer_id = $2, email = $3, loyalty_points = $4, updated_at = NOW() WHERE id = $1")
        .bind(id)
        .bind(basket.customer)
        .bind(basket.email)
        .bind(if basket.customer.is_some() { basket.points.max(0) } else { 0 })
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM cart_items WHERE cart_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for line in merged(basket.lines) {
        insert_line(&mut tx, id, &line).await?;
    }
    tx.commit().await?;
    Ok(id)
}

async fn insert_line(conn: &mut PgConnection, cart: Uuid, line: &BasketLine) -> Result<()> {
    let inserted = sqlx::query(
        "INSERT INTO cart_items (id, cart_id, product_id, variant_id, quantity, unit_price, \
           original_price, subtotal, discount_amount, total, sku, title, variant_title, image_url, \
           requires_shipping, is_gift_card, created_at, updated_at, manual_discount_percent, \
           manual_discount_reason) \
         SELECT gen_random_uuid(), $1, p.id, v.id, $4, COALESCE(v.price, p.price), \
           COALESCE(v.price, p.price), COALESCE(v.price, p.price) * $4, 0, \
           COALESCE(v.price, p.price) * $4, COALESCE(v.sku, p.sku), p.title, v.title, NULL, \
           p.requires_shipping, false, NOW(), NOW(), $5, $6 \
         FROM products p LEFT JOIN product_variants v ON v.id = $3 AND v.product_id = p.id \
         WHERE p.id = $2 AND p.is_active AND ($3::uuid IS NULL OR v.id IS NOT NULL)",
    )
    .bind(cart)
    .bind(line.product_id)
    .bind(line.variant_id)
    .bind(line.quantity)
    .bind(line.discount_percent)
    .bind(line.discount_reason.as_deref().map(str::trim))
    .execute(conn)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(Error::not_found("Product not found or not for sale"));
    }
    Ok(())
}

/// How a sale is paid
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Settlement {
    /// Paid by card (approved terminal transactions)
    pub card: Decimal,
    /// Cash kept for the sale
    pub cash: Decimal,
    /// Cash handed over by the customer
    pub cash_given: Decimal,
    /// Cash given back
    pub change: Decimal,
}

impl Settlement {
    /// `cash`, `card` or `split`
    pub fn method(&self) -> &'static str {
        match (self.cash > Decimal::ZERO, self.card > Decimal::ZERO) {
            (true, true) => "split",
            (false, true) => "card",
            _ => "cash",
        }
    }
}

/// How `total` is paid with `cash_given` and approved card amounts: card first,
/// cash for the rest, the change given back. Card cannot exceed the total;
/// cash must cover what the card did not.
pub fn settle(total: Decimal, cash_given: Decimal, card: &[Decimal]) -> Result<Settlement> {
    let card: Decimal = card.iter().sum();
    if cash_given < Decimal::ZERO {
        return Err(Error::validation("Cash given cannot be negative"));
    }
    if card > total {
        return Err(Error::validation("The card payments exceed the total"));
    }
    let cash = total - card;
    if cash_given < cash {
        return Err(Error::validation(format!(
            "{} is still to pay",
            (cash - cash_given).round_dp(2)
        )));
    }
    Ok(Settlement {
        card,
        cash,
        cash_given,
        change: cash_given - cash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(quantity: i32, percent: Option<i64>, reason: Option<&str>) -> BasketLine {
        BasketLine {
            product_id: Uuid::nil(),
            variant_id: None,
            quantity,
            discount_percent: percent.map(Decimal::from),
            discount_reason: reason.map(String::from),
        }
    }

    #[test]
    fn discounts_stay_within_the_cap_and_have_a_reason() {
        let cap = Decimal::from(10);
        assert!(check_lines(&[line(2, None, None)], cap).is_ok());
        assert!(check_lines(&[line(2, Some(10), Some("Damaged box"))], cap).is_ok());
        let over = check_lines(&[line(2, Some(15), Some("Friend"))], cap).unwrap_err();
        assert_eq!(over.status_code(), 403);
        for bad in [
            vec![line(2, Some(5), None)],
            vec![line(2, Some(5), Some("  "))],
            vec![line(0, None, None)],
            vec![line(2, Some(0), Some("x"))],
            vec![],
        ] {
            assert_eq!(check_lines(&bad, cap).unwrap_err().status_code(), 400);
        }
    }

    #[test]
    fn the_same_product_twice_is_one_line() {
        let lines = merged(&[line(2, Some(5), Some("a")), line(3, None, None)]);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            (lines[0].quantity, lines[0].discount_percent),
            (5, Some(Decimal::from(5)))
        );
    }

    #[test]
    fn cash_covers_what_the_card_did_not_and_gives_change() {
        let total = Decimal::new(12_345, 2);
        let s = settle(total, Decimal::new(5_000, 2), &[Decimal::new(8_000, 2)]).unwrap();
        assert_eq!(
            (s.card, s.cash, s.change),
            (
                Decimal::new(8_000, 2),
                Decimal::new(4_345, 2),
                Decimal::new(655, 2)
            )
        );
        assert_eq!(s.method(), "split");
        let cash = settle(total, Decimal::from(200), &[]).unwrap();
        assert_eq!(
            (cash.method(), cash.change),
            ("cash", Decimal::new(7_655, 2))
        );
        assert_eq!(
            settle(total, Decimal::ZERO, &[total]).unwrap().method(),
            "card"
        );
        assert!(settle(total, Decimal::from(100), &[]).is_err());
        assert!(settle(total, Decimal::ZERO, &[Decimal::from(200)]).is_err());
    }
}
