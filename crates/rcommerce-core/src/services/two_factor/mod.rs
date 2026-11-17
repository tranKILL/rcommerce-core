//! Two-step sign-in: passkeys (WebAuthn), authenticator apps (TOTP, RFC 6238) and
//! single-use recovery codes.
//!
//! - A **passkey** signs in on its own: it proves the device and the person (biometric
//!   or PIN, user verification is required), so it counts as two factors.
//! - After a **password**, an account with a second factor answers a one-time
//!   challenge with an authenticator code, a recovery code or one of its passkeys.
//! - With `[security.mfa] staff = "required"`, staff get no token without a second
//!   factor; those without one are sent to add one first.
//!
//! Every change is recorded in `security_events` ([`events`]).

pub mod challenge;
pub mod events;
pub mod lockout;
pub mod passkeys;
pub mod recovery;
pub mod totp;

use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::CustomerRole;
use crate::Result;

/// Seconds a sign-in or re-verification counts as recent for sensitive changes
/// (adding or removing a factor, exports, staff resets)
pub const FRESH_SECS: i64 = 300;

/// What an account can use as a second step
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Factors {
    /// An authenticator app is set up and confirmed
    pub totp: bool,
    /// Number of passkeys
    pub passkeys: i64,
    /// Recovery codes not used yet
    pub recovery_codes_left: i64,
}

impl Factors {
    /// Whether the account has a second factor (recovery codes alone do not count)
    pub fn any(&self) -> bool {
        self.totp || self.passkeys > 0
    }
}

/// The second factors of `customer_id`, in one query
pub async fn factors(pool: &PgPool, customer_id: Uuid) -> Result<Factors> {
    let (totp, passkeys, recovery_codes_left): (bool, i64, i64) = sqlx::query_as(
        "SELECT \
           EXISTS (SELECT 1 FROM customer_totp WHERE customer_id = $1 AND confirmed_at IS NOT NULL), \
           (SELECT COUNT(*) FROM customer_passkeys WHERE customer_id = $1), \
           (SELECT COUNT(*) FROM mfa_recovery_codes WHERE customer_id = $1 AND used_at IS NULL)",
    )
    .bind(customer_id)
    .fetch_one(pool)
    .await?;
    Ok(Factors {
        totp,
        passkeys,
        recovery_codes_left,
    })
}

/// Whether the account is staff: the Admin or Manager role, or any staff role
pub async fn is_staff(pool: &PgPool, customer_id: Uuid, role: &CustomerRole) -> Result<bool> {
    if matches!(role, CustomerRole::Admin | CustomerRole::Manager) {
        return Ok(true);
    }
    let assigned: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM staff_role_assignments WHERE customer_id = $1)",
    )
    .bind(customer_id)
    .fetch_one(pool)
    .await?;
    Ok(assigned)
}

/// A second factor to remove
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Factor {
    /// The authenticator app (set up or not)
    Totp,
    /// One passkey, by its credential id
    Passkey(String),
}

/// What happened to a removal
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    Removed,
    /// The account has no such factor
    NotFound,
    /// It was the last factor of an account that must keep one: nothing removed
    LastOne,
}

/// Remove one factor. With `keep_one`, the account must keep a second factor: the
/// account row is locked, so two removals at once cannot leave it with none.
pub async fn remove(
    pool: &PgPool,
    customer_id: Uuid,
    factor: &Factor,
    keep_one: bool,
) -> Result<Removal> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM customers WHERE id = $1 FOR UPDATE")
        .bind(customer_id)
        .execute(&mut *tx)
        .await?;
    let removed = match factor {
        Factor::Totp => sqlx::query("DELETE FROM customer_totp WHERE customer_id = $1")
            .bind(customer_id)
            .execute(&mut *tx)
            .await?
            .rows_affected(),
        Factor::Passkey(id) => sqlx::query(
            "DELETE FROM customer_passkeys WHERE customer_id = $1 AND credential_id = $2",
        )
        .bind(customer_id)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected(),
    };
    if removed == 0 {
        return Ok(Removal::NotFound);
    }
    if keep_one {
        let left: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM customer_totp WHERE customer_id = $1 AND confirmed_at IS NOT NULL) \
                 OR EXISTS (SELECT 1 FROM customer_passkeys WHERE customer_id = $1)",
        )
        .bind(customer_id)
        .fetch_one(&mut *tx)
        .await?;
        if !left {
            return Ok(Removal::LastOne); // dropping the transaction rolls the removal back
        }
    }
    tx.commit().await?;
    Ok(Removal::Removed)
}

/// Remove every second factor of `customer_id` (an admin's reset for a lost device).
/// Returns whether anything was removed.
pub async fn reset(pool: &PgPool, customer_id: Uuid) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let mut removed = 0;
    for table in ["customer_totp", "customer_passkeys", "mfa_recovery_codes"] {
        let sql = format!("DELETE FROM {table} WHERE customer_id = $1");
        removed += sqlx::query(&sql)
            .bind(customer_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    tx.commit().await?;
    Ok(removed > 0)
}

/// Constant-time equality of two short secrets (codes, hashes)
pub(crate) fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_codes_alone_are_not_a_second_factor() {
        let only_codes = Factors {
            recovery_codes_left: 10,
            ..Default::default()
        };
        assert!(!only_codes.any());
        let app = Factors {
            totp: true,
            ..Default::default()
        };
        assert!(app.any());
    }

    #[test]
    fn secrets_compare_whole() {
        assert!(same(b"123456", b"123456"));
        assert!(!same(b"123456", b"123457"));
        assert!(!same(b"12345", b"123456"));
    }
}
