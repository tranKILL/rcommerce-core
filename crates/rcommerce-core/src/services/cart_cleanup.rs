//! Carts nobody touched for months are deleted, with their lines.
//!
//! A cart's `updated_at` moves with every change to it (lines, coupon,
//! address, totals). With `[carts] delete_inactive_after_months` set, carts
//! untouched for that long go: abandoned ones and those already turned into
//! orders (orders keep their own copy of the lines). Off when unset.

use chrono::{DateTime, Months, Utc};
use sqlx::PgPool;

use crate::Result;

/// Carts deleted per statement, so a long backlog never locks the table long
const BATCH: i64 = 1000;

/// The moment before which a cart counts as inactive, `months` ago
pub fn cutoff(now: DateTime<Utc>, months: u32) -> DateTime<Utc> {
    now.checked_sub_months(Months::new(months)).unwrap_or(now)
}

/// Delete the carts not updated since `before` (their lines go with them);
/// the number deleted
pub async fn delete_inactive_carts(pool: &PgPool, before: DateTime<Utc>) -> Result<u64> {
    let mut deleted = 0;
    loop {
        let done = sqlx::query(
            "DELETE FROM carts WHERE id IN \
             (SELECT id FROM carts WHERE updated_at < $1 ORDER BY updated_at LIMIT $2)",
        )
        .bind(before)
        .bind(BATCH)
        .execute(pool)
        .await?;
        deleted += done.rows_affected();
        if (done.rows_affected() as i64) < BATCH {
            return Ok(deleted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn six_months_back_keeps_the_day_of_the_month() {
        let now = Utc.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap();
        assert_eq!(
            cutoff(now, 6),
            Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap()
        );
        // the 31st of a month without one: the last day instead
        let end = Utc.with_ymd_and_hms(2026, 8, 31, 0, 0, 0).unwrap();
        assert_eq!(
            cutoff(end, 6),
            Utc.with_ymd_and_hms(2026, 2, 28, 0, 0, 0).unwrap()
        );
    }
}
