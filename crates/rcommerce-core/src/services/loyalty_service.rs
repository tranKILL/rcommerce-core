//! Loyalty points, switched on or off by the store (off by default).
//!
//! Customers in the programme's audience (retail customers by default, trade
//! customers, or both) earn `points_per_euro` points per euro of goods
//! (excluding VAT, delivery and discounts) when an order is handed over
//! (collected or delivered). They redeem them in the cart, from `min_redeem`
//! points, each point taking `point_value` euros off. A cancelled or refunded
//! order takes its earned points back and gives its redeemed points back.
//! Points lapse after `expiry_months` months without earning or redeeming.
//! Staff can add or remove points with a note. Every change is a ledger entry.

use chrono::{DateTime, Months, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{Error, Result};

/// The programme's settings
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct LoyaltySettings {
    pub enabled: bool,
    /// `customer`, `pro` or `both`
    pub audience: String,
    pub points_per_euro: Decimal,
    /// Euros one point takes off when redeemed
    pub point_value: Decimal,
    pub min_redeem: i32,
    /// Months without activity after which points lapse (`None`: never)
    pub expiry_months: Option<i32>,
    pub updated_at: DateTime<Utc>,
}

impl LoyaltySettings {
    /// Whether a customer (trade or not) takes part
    pub fn reaches(&self, pro: bool) -> bool {
        self.enabled
            && match self.audience.as_str() {
                "both" => true,
                "pro" => pro,
                _ => !pro,
            }
    }

    /// Points earned on `amount` euros
    pub fn points_for(&self, amount: Decimal) -> i32 {
        (amount.max(Decimal::ZERO) * self.points_per_euro)
            .floor()
            .to_i32()
            .unwrap_or(0)
    }

    /// The points (and their value) to redeem when `requested` are asked for,
    /// with `balance` available and `room` euros left to pay: zero below the
    /// minimum, never more than the balance or than what is left to pay
    pub fn redeemable(&self, requested: i32, balance: i64, room: Decimal) -> (i32, Decimal) {
        let affordable = (room.max(Decimal::ZERO) / self.point_value)
            .floor()
            .to_i64()
            .unwrap_or(0);
        let points = i64::from(requested.max(0)).min(balance).min(affordable);
        if points < i64::from(self.min_redeem) {
            return (0, Decimal::ZERO);
        }
        let points = points as i32;
        (points, (Decimal::from(points) * self.point_value).round_dp(2))
    }
}

/// Changes to the settings; `None` leaves a field as it is
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SettingsUpdate {
    pub enabled: Option<bool>,
    pub audience: Option<String>,
    #[serde(default, deserialize_with = "crate::services::promotion_service::optional_number")]
    pub points_per_euro: Option<Decimal>,
    #[serde(default, deserialize_with = "crate::services::promotion_service::optional_number")]
    pub point_value: Option<Decimal>,
    pub min_redeem: Option<i32>,
    /// `0` means never
    pub expiry_months: Option<i32>,
}

/// A ledger entry
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct LoyaltyEntry {
    pub id: Uuid,
    pub points: i32,
    /// `earn`, `redeem`, `reverse`, `refund`, `adjust` or `expire`
    pub kind: String,
    pub order_id: Option<Uuid>,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// A customer's points as their account shows them
#[derive(Debug, Clone, Serialize)]
pub struct LoyaltyAccount {
    /// The programme is on and reaches this customer
    pub enabled: bool,
    pub points: i64,
    /// What the points are worth
    pub value: Decimal,
    pub point_value: Decimal,
    pub points_per_euro: Decimal,
    pub min_redeem: i32,
    /// When the points lapse without activity
    pub expires_at: Option<DateTime<Utc>>,
    pub entries: Vec<LoyaltyEntry>,
    /// The loyalty card number (shown as a QR code at the counter)
    pub card: String,
}

/// The programme's settings
pub async fn settings(conn: &mut PgConnection) -> Result<LoyaltySettings> {
    Ok(sqlx::query_as::<_, LoyaltySettings>(
        "SELECT enabled, audience, points_per_euro, point_value, min_redeem, expiry_months, updated_at \
         FROM loyalty_settings",
    )
    .fetch_one(conn)
    .await?)
}

/// The last earn, redeem or staff change of a customer
async fn last_activity(conn: &mut PgConnection, customer: Uuid) -> Result<Option<DateTime<Utc>>> {
    Ok(sqlx::query_scalar(
        "SELECT MAX(created_at) FROM loyalty_entries WHERE customer_id = $1 \
         AND kind IN ('earn', 'redeem', 'adjust')",
    )
    .bind(customer)
    .fetch_one(conn)
    .await?)
}

/// A customer's balance, after letting points lapse when they were inactive
/// for longer than the settings allow
pub async fn balance(conn: &mut PgConnection, customer: Uuid, settings: &LoyaltySettings) -> Result<i64> {
    let points: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(points), 0)::bigint FROM loyalty_entries WHERE customer_id = $1",
    )
    .bind(customer)
    .fetch_one(&mut *conn)
    .await?;
    let lapsed = match (settings.expiry_months, last_activity(&mut *conn, customer).await?) {
        (Some(months), Some(last)) => last
            .checked_add_months(Months::new(months.max(1) as u32))
            .is_some_and(|until| until < Utc::now()),
        _ => false,
    };
    if lapsed && points > 0 {
        add_entry(&mut *conn, customer, -(points as i32), "expire", None, None, None).await?;
        return Ok(0);
    }
    Ok(points.max(0))
}

/// Write a ledger entry (an order's award or redemption only once)
async fn add_entry(
    conn: &mut PgConnection,
    customer: Uuid,
    points: i32,
    kind: &str,
    order: Option<Uuid>,
    note: Option<&str>,
    staff: Option<Uuid>,
) -> Result<bool> {
    let done = sqlx::query(
        "INSERT INTO loyalty_entries (customer_id, points, kind, order_id, note, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
    )
    .bind(customer)
    .bind(points)
    .bind(kind)
    .bind(order)
    .bind(note)
    .bind(staff)
    .execute(conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Serialise changes to one customer's points until the transaction ends
async fn lock_customer(conn: &mut PgConnection, customer: Uuid) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('loyalty:' || $1::text, 0))")
        .bind(customer)
        .execute(conn)
        .await?;
    Ok(())
}

/// Take redeemed points for an order placed from a cart (inside the order's
/// transaction); fails when the balance no longer covers them
pub async fn redeem_for_order(conn: &mut PgConnection, customer: Uuid, order: Uuid, points: i32) -> Result<()> {
    if points <= 0 {
        return Ok(());
    }
    lock_customer(&mut *conn, customer).await?;
    let settings = settings(&mut *conn).await?;
    if balance(&mut *conn, customer, &settings).await? < i64::from(points) {
        return Err(Error::validation("Not enough loyalty points for this order"));
    }
    add_entry(conn, customer, -points, "redeem", Some(order), None, None).await?;
    Ok(())
}

/// The EAN-13 check digit of 12 digits
pub fn ean13_check(digits: &str) -> u32 {
    let sum: u32 = digits
        .chars()
        .filter_map(|c| c.to_digit(10))
        .enumerate()
        .map(|(i, d)| if i % 2 == 0 { d } else { d * 3 })
        .sum();
    (10 - sum % 10) % 10
}

/// A new card number: 29 (in-store range), 10 random digits, check digit
fn new_card_number() -> String {
    let random = (uuid::Uuid::new_v4().as_u128() % 10_000_000_000) as u64;
    let body = format!("29{random:010}");
    format!("{body}{}", ean13_check(&body))
}

/// A customer's loyalty card number, given on first use
pub async fn card_number(conn: &mut PgConnection, customer: Uuid) -> Result<String> {
    for _ in 0..5 {
        let card: Option<String> = sqlx::query_scalar(
            "UPDATE customers SET loyalty_card = COALESCE(loyalty_card, $2) WHERE id = $1 RETURNING loyalty_card",
        )
        .bind(customer)
        .bind(new_card_number())
        .fetch_optional(&mut *conn)
        .await
        .or_else(|e| match &e {
            // another customer already has that number: draw again
            sqlx::Error::Database(db) if db.is_unique_violation() => Ok(None),
            _ => Err(e),
        })?;
        if let Some(card) = card {
            return Ok(card);
        }
    }
    Err(Error::internal("Could not give a loyalty card number"))
}

/// The customer a card number belongs to: id, name, e-mail
pub async fn find_card(conn: &mut PgConnection, card: &str) -> Result<Option<(Uuid, String, String)>> {
    Ok(sqlx::query_as(
        "SELECT id, btrim(first_name || ' ' || last_name), email FROM customers WHERE loyalty_card = $1",
    )
    .bind(card.trim())
    .fetch_optional(conn)
    .await?)
}

/// Whether a customer is a trade customer (see PricingContext::pro)
async fn is_pro(conn: &mut PgConnection, customer: Uuid) -> Result<bool> {
    Ok(crate::services::price_list_service::pricing_context(conn, customer).await?.pro)
}

/// Loyalty points for staff, customers and orders
#[derive(Clone)]
pub struct LoyaltyService {
    pool: PgPool,
}

impl LoyaltyService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn settings(&self) -> Result<LoyaltySettings> {
        settings(&mut *self.pool.acquire().await?).await
    }

    /// Change the settings
    pub async fn update_settings(&self, update: SettingsUpdate) -> Result<LoyaltySettings> {
        check_update(&update)?;
        sqlx::query(
            "UPDATE loyalty_settings SET enabled = COALESCE($1, enabled), audience = COALESCE($2, audience), \
             points_per_euro = COALESCE($3, points_per_euro), point_value = COALESCE($4, point_value), \
             min_redeem = COALESCE($5, min_redeem), \
             expiry_months = CASE WHEN $6::int IS NULL THEN expiry_months WHEN $6 = 0 THEN NULL ELSE $6 END, \
             updated_at = NOW()",
        )
        .bind(update.enabled)
        .bind(&update.audience)
        .bind(update.points_per_euro)
        .bind(update.point_value)
        .bind(update.min_redeem)
        .bind(update.expiry_months)
        .execute(&self.pool)
        .await?;
        self.settings().await
    }

    /// A customer's points, value, lapse date and latest entries
    pub async fn account(&self, customer: Uuid, history: i64) -> Result<LoyaltyAccount> {
        let mut conn = self.pool.acquire().await?;
        let settings = settings(&mut conn).await?;
        let enabled = settings.reaches(is_pro(&mut conn, customer).await?);
        let points = balance(&mut conn, customer, &settings).await?;
        let expires_at = match (settings.expiry_months, last_activity(&mut conn, customer).await?) {
            (Some(months), Some(last)) if points > 0 => last.checked_add_months(Months::new(months as u32)),
            _ => None,
        };
        let card = card_number(&mut conn, customer).await?;
        let entries = sqlx::query_as::<_, LoyaltyEntry>(
            "SELECT id, points, kind, order_id, note, created_at FROM loyalty_entries \
             WHERE customer_id = $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(customer)
        .bind(history.clamp(1, 200))
        .fetch_all(&mut *conn)
        .await?;
        Ok(LoyaltyAccount {
            enabled,
            points,
            value: (Decimal::from(points) * settings.point_value).round_dp(2),
            point_value: settings.point_value,
            points_per_euro: settings.points_per_euro,
            min_redeem: settings.min_redeem,
            expires_at,
            entries,
            card,
        })
    }

    /// Staff add (or remove, with negative points) points with a note
    pub async fn adjust(&self, customer: Uuid, points: i32, note: &str, staff: Uuid) -> Result<LoyaltyAccount> {
        if points == 0 || note.trim().is_empty() {
            return Err(Error::validation("points must not be 0 and a note is required"));
        }
        let mut tx = self.pool.begin().await?;
        lock_customer(&mut tx, customer).await?;
        let settings = settings(&mut tx).await?;
        if points < 0 && balance(&mut tx, customer, &settings).await? + i64::from(points) < 0 {
            return Err(Error::validation("The customer does not have that many points"));
        }
        add_entry(&mut tx, customer, points, "adjust", None, Some(note.trim()), Some(staff)).await?;
        tx.commit().await?;
        self.account(customer, 20).await
    }

    /// Award an order's points once it is handed over; the points earned
    /// (zero when the programme is off or does not reach the customer)
    pub async fn award_for_order(&self, order: Uuid) -> Result<i32> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(Option<Uuid>, Decimal, Decimal)> =
            sqlx::query_as("SELECT customer_id, subtotal, discount_total FROM orders WHERE id = $1")
                .bind(order)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((Some(customer), subtotal, discount)) = row else {
            return Ok(0);
        };
        let settings = settings(&mut tx).await?;
        if !settings.reaches(is_pro(&mut tx, customer).await?) {
            return Ok(0);
        }
        let points = settings.points_for(subtotal - discount);
        if points <= 0 {
            return Ok(0);
        }
        lock_customer(&mut tx, customer).await?;
        let added = add_entry(&mut tx, customer, points, "earn", Some(order), None, None).await?;
        tx.commit().await?;
        Ok(if added { points } else { 0 })
    }

    /// A cancelled or refunded order takes back the points it earned and
    /// gives back the points it used
    pub async fn undo_for_order(&self, order: Uuid) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let entries: Vec<(Uuid, i32, String)> = sqlx::query_as(
            "SELECT customer_id, points, kind FROM loyalty_entries WHERE order_id = $1 AND kind IN ('earn', 'redeem')",
        )
        .bind(order)
        .fetch_all(&mut *tx)
        .await?;
        for (customer, points, kind) in entries {
            lock_customer(&mut tx, customer).await?;
            let undo = if kind == "earn" { "reverse" } else { "refund" };
            add_entry(&mut tx, customer, -points, undo, Some(order), None, None).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

fn check_update(update: &SettingsUpdate) -> Result<()> {
    let positive = |v: Option<Decimal>| v.is_none_or(|v| v > Decimal::ZERO);
    let audience_ok = update
        .audience
        .as_deref()
        .is_none_or(|a| matches!(a, "customer" | "pro" | "both"));
    let ok = audience_ok
        && positive(update.points_per_euro)
        && positive(update.point_value)
        && update.min_redeem.is_none_or(|m| m >= 1)
        && update.expiry_months.is_none_or(|m| m >= 0);
    if ok {
        Ok(())
    } else {
        Err(Error::validation(
            "audience must be customer, pro or both; points_per_euro and point_value above 0; \
             min_redeem at least 1; expiry_months at least 0 (0: never)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn settings() -> LoyaltySettings {
        LoyaltySettings {
            enabled: true,
            audience: "customer".into(),
            points_per_euro: dec!(1),
            point_value: dec!(0.05),
            min_redeem: 100,
            expiry_months: Some(24),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn points_are_earned_per_whole_euro_for_the_audience() {
        let s = settings();
        assert_eq!(s.points_for(dec!(129.99)), 129);
        assert_eq!(s.points_for(dec!(-5)), 0);
        assert!(s.reaches(false) && !s.reaches(true));
        let off = LoyaltySettings { enabled: false, ..settings() };
        assert!(!off.reaches(false));
        let both = LoyaltySettings { audience: "both".into(), ..settings() };
        assert!(both.reaches(true));
    }

    #[test]
    fn redemption_respects_minimum_balance_and_what_is_left_to_pay() {
        let s = settings();
        assert_eq!(s.redeemable(300, 1000, dec!(100)), (300, dec!(15)));
        assert_eq!(s.redeemable(300, 250, dec!(100)), (250, dec!(12.50)));
        assert_eq!(s.redeemable(300, 1000, dec!(6)), (120, dec!(6)), "never more than the cart");
        assert_eq!(s.redeemable(50, 1000, dec!(100)), (0, dec!(0)), "below the minimum");
    }

    #[test]
    fn card_numbers_carry_an_ean13_check_digit() {
        assert_eq!(ean13_check("400638133393"), 1, "4006381333931");
        let card = new_card_number();
        assert_eq!(card.len(), 13);
        assert!(card.starts_with("29"));
        assert_eq!(card[12..].parse::<u32>().unwrap(), ean13_check(&card[..12]));
    }

    #[test]
    fn settings_changes_are_checked() {
        assert!(check_update(&SettingsUpdate { audience: Some("vip".into()), ..Default::default() }).is_err());
        assert!(check_update(&SettingsUpdate { point_value: Some(dec!(0)), ..Default::default() }).is_err());
        assert!(check_update(&SettingsUpdate { enabled: Some(true), expiry_months: Some(0), ..Default::default() }).is_ok());
    }
}
