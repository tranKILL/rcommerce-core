//! Reminders of carts left without an order: commercial e-mails, so only for
//! signed-in customers who accept them (with a consent on record) and confirmed
//! their address.
//!
//! `[carts] reminder_after_hours` (`[4, 48]`) sets the schedule, counted from the
//! customer's last change to the cart. Each step is sent at most once per idle
//! period; a change to the cart starts a new period, an order (any order of the
//! customer since the period started) stops it. When the job was off for a while,
//! only the latest step due is sent. A customer gets at most one reminder in
//! [`QUIET_HOURS`]. The cart is repriced before each reminder (see
//! `cart_seen_at` in migration 051).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::Result;

/// Failed sends after which a step is given up
pub const MAX_ATTEMPTS: i32 = 3;
/// A customer gets at most one reminder in this many hours
pub const QUIET_HOURS: i32 = 20;
/// Lines shown in a reminder; the others are counted
pub const SHOWN_LINES: i64 = 6;

/// The schedule in hours, sorted, without zeros or repeats
pub fn schedule(hours: &[u32]) -> Vec<i32> {
    let mut steps: Vec<i32> = hours
        .iter()
        .filter(|h| **h > 0)
        .map(|h| (*h).min(24 * 365) as i32)
        .collect();
    steps.sort_unstable();
    steps.dedup();
    steps
}

/// A cart whose reminder is due
#[derive(Debug, Clone, FromRow)]
pub struct DueReminder {
    pub cart_id: Uuid,
    pub customer_id: Uuid,
    /// Where its idle period started
    pub period_start: DateTime<Utc>,
    /// 1 for the first delay of the schedule
    pub step: i32,
}

/// A line of a reminded cart, in the customer's language
#[derive(Debug, Clone, FromRow)]
pub struct ReminderLine {
    /// The cart line
    pub id: Uuid,
    pub title: String,
    pub variant_title: Option<String>,
    pub quantity: i32,
    /// After price lists and promotions, excluding VAT
    pub total: Decimal,
    pub image_url: Option<String>,
}

/// Carts due for a reminder under `schedule` (from [`schedule`]), oldest first
pub async fn due(
    pool: &PgPool,
    schedule: &[i32],
    min_subtotal: Decimal,
    limit: i64,
) -> Result<Vec<DueReminder>> {
    let Some(last) = schedule.last() else {
        return Ok(Vec::new());
    };
    Ok(sqlx::query_as::<_, DueReminder>(
        "WITH c AS ( \
           SELECT c.id, c.customer_id, COALESCE(prev.period_start, c.updated_at) AS anchor \
           FROM carts c \
           LEFT JOIN LATERAL (SELECT r.period_start FROM cart_reminders r \
               WHERE r.cart_id = c.id AND r.cart_seen_at = c.updated_at \
               ORDER BY r.created_at DESC LIMIT 1) prev ON TRUE \
           WHERE c.customer_id IS NOT NULL AND NOT c.converted_to_order AND c.order_id IS NULL \
             AND c.subtotal >= $3 \
             AND c.updated_at > NOW() - make_interval(hours => $2 + 24)) \
         SELECT c.id AS cart_id, c.customer_id, c.anchor AS period_start, d.step \
         FROM c \
         JOIN customers cu ON cu.id = c.customer_id AND cu.email_verified AND cu.accepts_marketing \
         CROSS JOIN LATERAL (SELECT MAX(s.i)::int AS step \
             FROM unnest($1::int[]) WITH ORDINALITY s(h, i) \
             WHERE c.anchor <= NOW() - make_interval(hours => s.h)) d \
         LEFT JOIN cart_reminders r \
             ON r.cart_id = c.id AND r.period_start = c.anchor AND r.step = d.step \
         WHERE d.step IS NOT NULL \
           AND c.anchor > NOW() - make_interval(hours => $2 + 24) \
           AND EXISTS (SELECT 1 FROM cart_items i WHERE i.cart_id = c.id) \
           AND COALESCE((SELECT mc.granted FROM marketing_consents mc \
               WHERE mc.customer_id = c.customer_id \
               ORDER BY mc.created_at DESC LIMIT 1), FALSE) \
           AND NOT EXISTS (SELECT 1 FROM cart_reminders x \
               WHERE x.cart_id = c.id AND x.period_start = c.anchor AND x.step >= d.step \
                 AND (x.sent_at IS NOT NULL OR x.attempts >= $4)) \
           AND (r.id IS NULL OR r.next_attempt_at IS NULL OR r.next_attempt_at <= NOW()) \
           AND NOT EXISTS (SELECT 1 FROM cart_reminders y \
               WHERE y.customer_id = c.customer_id \
                 AND y.sent_at > NOW() - make_interval(hours => $6)) \
           AND NOT EXISTS (SELECT 1 FROM orders o \
               WHERE o.customer_id = c.customer_id AND o.created_at >= c.anchor AND NOT o.draft) \
         ORDER BY c.anchor LIMIT $5",
    )
    .bind(schedule)
    .bind(last)
    .bind(min_subtotal)
    .bind(MAX_ATTEMPTS)
    .bind(limit)
    .bind(QUIET_HOURS)
    .fetch_all(pool)
    .await?)
}

/// The cart's `updated_at` now (after repricing), to record with the reminder
pub async fn seen_at(pool: &PgPool, cart_id: Uuid) -> Result<Option<DateTime<Utc>>> {
    Ok(
        sqlx::query_scalar("SELECT updated_at FROM carts WHERE id = $1")
            .bind(cart_id)
            .fetch_optional(pool)
            .await?,
    )
}

/// The first [`SHOWN_LINES`] lines of a cart (no bundle parts) in `locale`
pub async fn lines(pool: &PgPool, cart_id: Uuid, locale: &str) -> Result<Vec<ReminderLine>> {
    Ok(sqlx::query_as::<_, ReminderLine>(
        "SELECT i.id, COALESCE(t.title, i.title) AS title, i.variant_title, i.quantity, i.total, \
           COALESCE(i.image_url, (SELECT im.src FROM product_images im \
               WHERE im.product_id = i.product_id ORDER BY im.position LIMIT 1)) AS image_url \
         FROM cart_items i \
         LEFT JOIN product_translations t ON t.product_id = i.product_id AND t.locale = $2 \
         WHERE i.cart_id = $1 \
           AND NOT COALESCE((i.custom_attributes->>'is_bundle_component')::boolean, FALSE) \
         ORDER BY i.created_at, i.id LIMIT $3",
    )
    .bind(cart_id)
    .bind(locale)
    .bind(SHOWN_LINES)
    .fetch_all(pool)
    .await?)
}

/// Record a sent reminder
pub async fn mark_sent(pool: &PgPool, due: &DueReminder, seen: DateTime<Utc>) -> Result<()> {
    sqlx::query(
        "INSERT INTO cart_reminders (cart_id, customer_id, step, period_start, cart_seen_at, sent_at) \
         VALUES ($1, $2, $3, $4, $5, NOW()) \
         ON CONFLICT (cart_id, period_start, step) DO UPDATE \
           SET sent_at = NOW(), cart_seen_at = EXCLUDED.cart_seen_at \
           WHERE cart_reminders.sent_at IS NULL",
    )
    .bind(due.cart_id)
    .bind(due.customer_id)
    .bind(due.step as i16)
    .bind(due.period_start)
    .bind(seen)
    .execute(pool)
    .await?;
    Ok(())
}

/// Count a failed send: tried again 15 minutes later, then 30; given up at
/// [`MAX_ATTEMPTS`]
pub async fn failed(pool: &PgPool, due: &DueReminder, seen: DateTime<Utc>) -> Result<()> {
    sqlx::query(
        "INSERT INTO cart_reminders \
           (cart_id, customer_id, step, period_start, cart_seen_at, attempts, next_attempt_at) \
         VALUES ($1, $2, $3, $4, $5, 1, NOW() + INTERVAL '15 minutes') \
         ON CONFLICT (cart_id, period_start, step) DO UPDATE \
           SET attempts = cart_reminders.attempts + 1, cart_seen_at = EXCLUDED.cart_seen_at, \
               next_attempt_at = NOW() + INTERVAL '15 minutes' * POWER(2, cart_reminders.attempts) \
           WHERE cart_reminders.sent_at IS NULL",
    )
    .bind(due.cart_id)
    .bind(due.customer_id)
    .bind(due.step as i16)
    .bind(due.period_start)
    .bind(seen)
    .execute(pool)
    .await?;
    Ok(())
}

/// What the reminders sent in a period brought
#[derive(Debug, Clone, Serialize, FromRow, PartialEq)]
pub struct ReminderStats {
    /// Reminders sent
    pub sent: i64,
    /// Customers reminded
    pub customers: i64,
    /// Orders placed within 7 days of a reminder by the customer reminded
    pub orders: i64,
    /// Their total, VAT included
    pub revenue: Decimal,
}

/// The reminders sent from `from` (included) to `to` (excluded), and the orders
/// (not drafts, not cancelled or refunded) placed within 7 days of one
pub async fn stats(pool: &PgPool, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<ReminderStats> {
    Ok(sqlx::query_as::<_, ReminderStats>(
        "WITH s AS (SELECT customer_id, sent_at FROM cart_reminders \
                    WHERE sent_at >= $1 AND sent_at < $2), \
              o AS (SELECT DISTINCT o.id, o.total FROM orders o JOIN s \
                    ON o.customer_id = s.customer_id AND o.created_at > s.sent_at \
                   AND o.created_at <= s.sent_at + INTERVAL '7 days' \
                    WHERE NOT o.draft AND o.status::text NOT IN ('cancelled', 'refunded')) \
         SELECT (SELECT COUNT(*) FROM s) AS sent, \
                (SELECT COUNT(DISTINCT customer_id) FROM s) AS customers, \
                (SELECT COUNT(*) FROM o) AS orders, \
                (SELECT COALESCE(SUM(total), 0) FROM o)::NUMERIC(20, 2) AS revenue",
    )
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schedule_is_sorted_without_zeros_or_repeats() {
        assert_eq!(schedule(&[48, 4, 0, 4]), vec![4, 48]);
        assert!(schedule(&[]).is_empty());
        assert!(schedule(&[0]).is_empty());
    }
}
