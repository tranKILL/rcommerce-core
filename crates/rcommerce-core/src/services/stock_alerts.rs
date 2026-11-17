//! Stock alerts: a signed-in customer asks to be told when a product is in stock at
//! a branch (or at any branch) in the quantity they want.
//!
//! Only stock that is at the branch counts: its saleable quantity, as shown per
//! branch ([`super::stock_availability`]: free stock minus active cart holds).
//! Stock expected from a supplier does not. Each alert is sent once, then closed.
//! A customer keeps at most [`MAX_ACTIVE`] alerts open or sent in the last day, and
//! cannot ask for what is in stock now. Closed alerts are deleted after 90 days.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::{Error, Result};

/// Open alerts a customer may keep
pub const MAX_ACTIVE: i64 = 50;
/// Failed sends after which an alert closes
pub const MAX_ATTEMPTS: i32 = 5;

/// What a customer asks for
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewAlert {
    pub product_id: Uuid,
    #[serde(default)]
    pub variant_id: Option<Uuid>,
    /// The branch; none: any public branch
    #[serde(default)]
    pub location_id: Option<Uuid>,
    /// The branch by its slug, instead of `location_id`
    #[serde(default)]
    pub location: Option<String>,
    /// Units wanted (1 when not given)
    #[serde(default = "one")]
    pub quantity: i32,
}

fn one() -> i32 {
    1
}

/// One of a customer's alerts, with the names to show
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct StockAlert {
    pub id: Uuid,
    pub product_id: Uuid,
    pub variant_id: Option<Uuid>,
    pub location_id: Option<Uuid>,
    pub quantity: i32,
    pub created_at: DateTime<Utc>,
    /// When the e-mail went (the alert is closed)
    pub notified_at: Option<DateTime<Utc>>,
    pub product_title: String,
    pub product_slug: String,
    pub location_name: Option<String>,
}

/// An open alert whose stock has arrived, with what its e-mail needs
#[derive(Debug, Clone, FromRow)]
pub struct DueAlert {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub product_id: Uuid,
    pub quantity: i32,
    /// The branch that has the stock (the one asked for, or the first that has it)
    pub location_name: String,
}

const SELECT_ALERT: &str = "SELECT a.id, a.product_id, a.variant_id, a.location_id, a.quantity, \
     a.created_at, a.notified_at, p.title AS product_title, p.slug AS product_slug, \
     l.name AS location_name \
     FROM stock_alerts a JOIN products p ON p.id = a.product_id \
     LEFT JOIN inventory_locations l ON l.id = a.location_id";

/// Open an alert, or change the quantity of the same open alert (same product,
/// variant and branch). Refused for products that do not track stock, unknown or
/// private branches, and beyond [`MAX_ACTIVE`] open alerts.
pub async fn create(pool: &PgPool, customer_id: Uuid, new: &NewAlert) -> Result<StockAlert> {
    if !(1..=9999).contains(&new.quantity) {
        return Err(Error::validation("quantity must be between 1 and 9999"));
    }
    let location_id = match (&new.location_id, &new.location) {
        (Some(_), Some(_)) => {
            return Err(Error::validation("Give location_id or location, not both"));
        }
        (Some(id), None) => Some(*id),
        (None, Some(slug)) => Some(branch_by_slug(pool, slug).await?),
        (None, None) => None,
    };
    let new = &NewAlert {
        location_id,
        location: None,
        ..new.clone()
    };
    check_target(pool, new).await?;
    if in_stock_now(pool, new).await? {
        return Err(Error::HttpError(
            http::StatusCode::CONFLICT,
            "In stock now: it can be ordered".to_string(),
        ));
    }
    // Parallel requests of one customer queue here, so the cap holds
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("stock_alerts:{customer_id}"))
        .execute(&mut *tx)
        .await?;
    if counted(&mut tx, customer_id, new).await? >= MAX_ACTIVE {
        return Err(Error::validation(format!(
            "At most {MAX_ACTIVE} stock alerts can be open or sent in a day"
        )));
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO stock_alerts (customer_id, product_id, variant_id, location_id, quantity) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (customer_id, product_id, \
             COALESCE(variant_id, '00000000-0000-0000-0000-000000000000'::uuid), \
             COALESCE(location_id, '00000000-0000-0000-0000-000000000000'::uuid)) \
           WHERE notified_at IS NULL AND cancelled_at IS NULL \
         DO UPDATE SET quantity = EXCLUDED.quantity \
         RETURNING id",
    )
    .bind(customer_id)
    .bind(new.product_id)
    .bind(new.variant_id)
    .bind(new.location_id)
    .bind(new.quantity)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let query = format!("{SELECT_ALERT} WHERE a.id = $1");
    Ok(sqlx::query_as::<_, StockAlert>(&query)
        .bind(id)
        .fetch_one(pool)
        .await?)
}

/// The customer's alerts counted against [`MAX_ACTIVE`]: open, or sent in the last
/// day (the same open alert asked again is a change, not counted)
async fn counted(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    customer_id: Uuid,
    new: &NewAlert,
) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM stock_alerts \
         WHERE customer_id = $1 AND cancelled_at IS NULL \
           AND (notified_at IS NULL OR notified_at > NOW() - INTERVAL '1 day') \
           AND NOT (notified_at IS NULL AND product_id = $2 \
                    AND variant_id IS NOT DISTINCT FROM $3 \
                    AND location_id IS NOT DISTINCT FROM $4)",
    )
    .bind(customer_id)
    .bind(new.product_id)
    .bind(new.variant_id)
    .bind(new.location_id)
    .fetch_one(&mut **tx)
    .await?)
}

/// Whether the branch asked for (or, without one, any public branch) holds the
/// quantity now
async fn in_stock_now(pool: &PgPool, new: &NewAlert) -> Result<bool> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM inventory_locations l \
         WHERE l.is_public AND l.is_active AND ($2::uuid IS NULL OR l.id = $2) \
           AND {SALEABLE} >= $4)"
    ))
    .bind(new.product_id)
    .bind(new.location_id)
    .bind(new.variant_id)
    .bind(new.quantity)
    .fetch_one(pool)
    .await?)
}

/// Saleable quantity of product `$1` (variant `$3`, or all) at branch `l`: free stock
/// minus active cart holds
const SALEABLE: &str = "((SELECT COALESCE(SUM(lv.available_quantity), 0) FROM inventory_levels lv \
       WHERE lv.location_id = l.id AND lv.product_id = $1 \
         AND ($3::uuid IS NULL OR lv.variant_id = $3)) \
     - (SELECT COALESCE(SUM(r.quantity), 0) FROM stock_reservations r \
       WHERE r.product_id = $1 AND r.location_id = l.id \
         AND ($3::uuid IS NULL OR r.variant_id = $3) \
         AND r.status::text = 'active' AND r.expires_at > NOW()))";

/// The id of the public, active branch with this slug
async fn branch_by_slug(pool: &PgPool, slug: &str) -> Result<Uuid> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM inventory_locations WHERE slug = $1 AND is_public AND is_active",
    )
    .bind(slug)
    .fetch_optional(pool)
    .await?;
    id.ok_or_else(|| Error::not_found("Branch not found"))
}

/// The product must exist, be active and track stock; the variant must be its own;
/// the branch must be public and active
async fn check_target(pool: &PgPool, new: &NewAlert) -> Result<()> {
    let tracked: Option<bool> =
        sqlx::query_scalar("SELECT inventory_management FROM products WHERE id = $1 AND is_active")
            .bind(new.product_id)
            .fetch_optional(pool)
            .await?;
    match tracked {
        None => return Err(Error::not_found("Product not found")),
        Some(false) => return Err(Error::validation("This product is always in stock")),
        Some(true) => {}
    }
    if let Some(variant) = new.variant_id {
        let found: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM product_variants WHERE id = $1 AND product_id = $2)",
        )
        .bind(variant)
        .bind(new.product_id)
        .fetch_one(pool)
        .await?;
        if !found {
            return Err(Error::not_found("Variant not found"));
        }
    }
    if let Some(location) = new.location_id {
        let found: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM inventory_locations \
             WHERE id = $1 AND is_public AND is_active)",
        )
        .bind(location)
        .fetch_one(pool)
        .await?;
        if !found {
            return Err(Error::not_found("Branch not found"));
        }
    }
    Ok(())
}

/// A customer's open alerts, then those sent in the last 30 days (newest first)
pub async fn list(pool: &PgPool, customer_id: Uuid) -> Result<Vec<StockAlert>> {
    let query = format!(
        "{SELECT_ALERT} WHERE a.customer_id = $1 AND a.cancelled_at IS NULL \
           AND (a.notified_at IS NULL OR a.notified_at > NOW() - INTERVAL '30 days') \
         ORDER BY a.notified_at IS NOT NULL, a.created_at DESC"
    );
    Ok(sqlx::query_as::<_, StockAlert>(&query)
        .bind(customer_id)
        .fetch_all(pool)
        .await?)
}

/// Close one of the customer's open alerts; false when there is none with that id
pub async fn cancel(pool: &PgPool, customer_id: Uuid, id: Uuid) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE stock_alerts SET cancelled_at = NOW() \
         WHERE id = $1 AND customer_id = $2 AND notified_at IS NULL AND cancelled_at IS NULL",
    )
    .bind(id)
    .bind(customer_id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Open alerts whose branch (or, without one, any public branch) now has at least
/// the quantity wanted, oldest first, at most `limit`
pub async fn due(pool: &PgPool, limit: i64) -> Result<Vec<DueAlert>> {
    Ok(sqlx::query_as::<_, DueAlert>(
        "SELECT a.id, a.customer_id, a.product_id, a.quantity, found.name AS location_name \
         FROM stock_alerts a \
         JOIN products p ON p.id = a.product_id AND p.is_active AND p.inventory_management \
         JOIN customers c ON c.id = a.customer_id AND c.email_verified \
         CROSS JOIN LATERAL ( \
           SELECT l.name FROM inventory_locations l \
           WHERE l.is_public AND l.is_active \
             AND (a.location_id IS NULL OR l.id = a.location_id) \
             AND (SELECT COALESCE(SUM(lv.available_quantity), 0) FROM inventory_levels lv \
                  WHERE lv.location_id = l.id AND lv.product_id = a.product_id \
                    AND (a.variant_id IS NULL OR lv.variant_id = a.variant_id)) \
               - (SELECT COALESCE(SUM(r.quantity), 0) FROM stock_reservations r \
                  WHERE r.product_id = a.product_id AND r.location_id = l.id \
                    AND (a.variant_id IS NULL OR r.variant_id = a.variant_id) \
                    AND r.status::text = 'active' AND r.expires_at > NOW()) \
               >= a.quantity \
           ORDER BY l.sort_order, l.name LIMIT 1) found \
         WHERE a.notified_at IS NULL AND a.cancelled_at IS NULL AND a.attempts < $2 \
           AND (a.next_attempt_at IS NULL OR a.next_attempt_at <= NOW()) \
         ORDER BY a.attempts, a.created_at LIMIT $1",
    )
    .bind(limit)
    .bind(MAX_ATTEMPTS)
    .fetch_all(pool)
    .await?)
}

/// Count a failed send: the next try waits 15 minutes, doubling each time (the
/// five tries span about 8 hours); the alert closes at [`MAX_ATTEMPTS`]
pub async fn failed(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE stock_alerts SET attempts = attempts + 1, \
           next_attempt_at = NOW() + INTERVAL '15 minutes' * POWER(2, attempts), \
           cancelled_at = CASE WHEN attempts + 1 >= $2 THEN NOW() END \
         WHERE id = $1 AND notified_at IS NULL AND cancelled_at IS NULL",
    )
    .bind(id)
    .bind(MAX_ATTEMPTS)
    .execute(pool)
    .await?;
    Ok(())
}

/// Close the open alerts that can no longer fire: their product is gone from sale or
/// stops tracking stock, or their branch is no longer public and active. Returns how
/// many were closed.
pub async fn close_unreachable(pool: &PgPool) -> Result<u64> {
    let done = sqlx::query(
        "UPDATE stock_alerts a SET cancelled_at = NOW() \
         WHERE a.notified_at IS NULL AND a.cancelled_at IS NULL AND ( \
           NOT EXISTS (SELECT 1 FROM products p \
                       WHERE p.id = a.product_id AND p.is_active AND p.inventory_management) \
           OR (a.location_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM inventory_locations l \
                       WHERE l.id = a.location_id AND l.is_public AND l.is_active)))",
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

/// Delete the alerts closed more than 90 days ago (sent or stopped); returns how many
pub async fn purge_closed(pool: &PgPool) -> Result<u64> {
    let done = sqlx::query(
        "DELETE FROM stock_alerts \
         WHERE notified_at < NOW() - INTERVAL '90 days' \
            OR cancelled_at < NOW() - INTERVAL '90 days'",
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

/// Close an alert once its e-mail went; false when it was already closed
pub async fn mark_sent(pool: &PgPool, id: Uuid) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE stock_alerts SET notified_at = NOW() \
         WHERE id = $1 AND notified_at IS NULL AND cancelled_at IS NULL",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}
