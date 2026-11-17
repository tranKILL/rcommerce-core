//! Wrong codes per account, whatever the address they come from. Every code or
//! recovery code attempt is counted before it is checked (so parallel guesses all
//! count); a right one resets the count. After [`MAX_FAILURES`] in a row, codes are
//! refused for 15 minutes, doubling each time after that (24 hours at most). Passkeys
//! are not counted: they cannot be guessed.

use chrono::Duration;
use sqlx::PgPool;
use uuid::Uuid;

use crate::Result;

/// Wrong codes in a row before the account's codes are refused for a while
pub const MAX_FAILURES: i32 = 10;

/// Whether a code may be tried now
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    Allowed,
    /// Codes are refused for this many more seconds
    Locked(i64),
}

/// How long codes are refused after `failures` wrong ones in a row
pub fn lock_for(failures: i32) -> Duration {
    let doublings = (failures - MAX_FAILURES - 1).clamp(0, 7) as u32;
    (Duration::minutes(15) * 2i32.pow(doublings)).min(Duration::hours(24))
}

/// Count an attempt before checking it; refused while the account is locked
pub async fn reserve(pool: &PgPool, customer_id: Uuid) -> Result<Attempt> {
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO mfa_lockouts (customer_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(customer_id)
        .execute(&mut *tx)
        .await?;
    let (failures, locked): (i32, Option<f64>) = sqlx::query_as(
        "SELECT failures, EXTRACT(EPOCH FROM (locked_until - NOW()))::float8 \
         FROM mfa_lockouts WHERE customer_id = $1 FOR UPDATE",
    )
    .bind(customer_id)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(left) = locked.filter(|s| *s > 0.0) {
        tx.commit().await?;
        return Ok(Attempt::Locked(left.ceil() as i64));
    }
    let failures = failures + 1;
    let wait = (failures > MAX_FAILURES).then(|| lock_for(failures));
    sqlx::query(
        "UPDATE mfa_lockouts SET failures = $2, updated_at = NOW(), \
           locked_until = CASE WHEN $3::bigint IS NULL THEN NULL \
                               ELSE NOW() + make_interval(secs => $3::bigint) END \
         WHERE customer_id = $1",
    )
    .bind(customer_id)
    .bind(failures)
    .bind(wait.map(|w| w.num_seconds()))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(match wait {
        Some(w) => Attempt::Locked(w.num_seconds()),
        None => Attempt::Allowed,
    })
}

/// A right code: the count starts again
pub async fn succeeded(pool: &PgPool, customer_id: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE mfa_lockouts SET failures = 0, locked_until = NULL, updated_at = NOW() \
         WHERE customer_id = $1",
    )
    .bind(customer_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_grow_from_15_minutes_to_a_day() {
        assert_eq!(lock_for(MAX_FAILURES + 1), Duration::minutes(15));
        assert_eq!(lock_for(MAX_FAILURES + 2), Duration::minutes(30));
        assert_eq!(lock_for(MAX_FAILURES + 50), Duration::hours(24));
    }
}
