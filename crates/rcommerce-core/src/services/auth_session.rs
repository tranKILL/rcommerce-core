//! Refresh-token sessions.
//!
//! Every login opens a session row. A refresh token names its session (`sid`) and the
//! id of the one token the session currently accepts (`jti`). Each refresh swaps that
//! id for a new one, so a refresh token works once. Presenting an older token of a live
//! session means a token was copied: the session is revoked, which also ends the
//! copy's access to it.
//!
//! Access tokens stay stateless (no lookup per request); they end at their own expiry.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::Result;

/// The token a session currently accepts, and when the session ends
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionToken {
    /// Session id (`sid` claim)
    pub session_id: Uuid,
    /// Id of the refresh token the session accepts next (`jti` claim)
    pub token_id: Uuid,
    /// Absolute end of the session; rotation does not extend it
    pub expires_at: DateTime<Utc>,
    /// The sign-in used a second factor (or a passkey): refreshed tokens keep it
    pub mfa: bool,
}

/// Why a session was ended
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeReason {
    /// The customer signed out
    Logout,
    /// An already-used refresh token was presented again
    Reuse,
    /// The password was changed or reset
    PasswordChange,
    /// An admin removed the account's second factors
    FactorsReset,
    /// Staff must sign in with a second factor; this session had none
    SecondFactorRequired,
    /// The account added a second factor: sessions opened without one end
    SecondFactorAdded,
}

impl RevokeReason {
    /// Value stored in `auth_sessions.revoked_reason`
    pub fn as_str(self) -> &'static str {
        match self {
            RevokeReason::Logout => "logout",
            RevokeReason::Reuse => "reuse",
            RevokeReason::PasswordChange => "password_change",
            RevokeReason::FactorsReset => "factors_reset",
            RevokeReason::SecondFactorRequired => "second_factor_required",
            RevokeReason::SecondFactorAdded => "second_factor_added",
        }
    }
}

/// Result of presenting a refresh token
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// The token was the current one; here is its successor
    Rotated(SessionToken),
    /// The token was used before: the session is now revoked
    Reused,
    /// The session is unknown, expired or already revoked
    Ended,
}

/// Open a session for `customer_id` that lasts `lifetime`; `mfa`: the sign-in used a
/// second factor
pub async fn start(
    pool: &PgPool,
    customer_id: Uuid,
    lifetime: Duration,
    mfa: bool,
) -> Result<SessionToken> {
    let token = SessionToken {
        session_id: Uuid::new_v4(),
        token_id: Uuid::new_v4(),
        expires_at: Utc::now() + lifetime,
        mfa,
    };
    sqlx::query(
        "INSERT INTO auth_sessions (id, customer_id, current_token_id, expires_at, mfa) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(token.session_id)
    .bind(customer_id)
    .bind(token.token_id)
    .bind(token.expires_at)
    .bind(token.mfa)
    .execute(pool)
    .await?;
    Ok(token)
}

/// Accept refresh token `token_id` of session `session_id` once and name its successor.
///
/// The swap is a single conditional update, so two concurrent refreshes with the same
/// token cannot both succeed: the second one counts as reuse.
pub async fn rotate(
    pool: &PgPool,
    customer_id: Uuid,
    session_id: Uuid,
    token_id: Uuid,
) -> Result<Rotation> {
    let next = Uuid::new_v4();
    let rotated: Option<(DateTime<Utc>, bool)> = sqlx::query_as(
        "UPDATE auth_sessions SET current_token_id = $4, last_used_at = NOW() \
         WHERE id = $1 AND customer_id = $2 AND current_token_id = $3 \
           AND revoked_at IS NULL AND expires_at > NOW() \
         RETURNING expires_at, mfa",
    )
    .bind(session_id)
    .bind(customer_id)
    .bind(token_id)
    .bind(next)
    .fetch_optional(pool)
    .await?;
    if let Some((expires_at, mfa)) = rotated {
        return Ok(Rotation::Rotated(SessionToken {
            session_id,
            token_id: next,
            expires_at,
            mfa,
        }));
    }
    let revoked = sqlx::query(
        "UPDATE auth_sessions SET revoked_at = NOW(), revoked_reason = $3 \
         WHERE id = $1 AND customer_id = $2 AND revoked_at IS NULL AND expires_at > NOW()",
    )
    .bind(session_id)
    .bind(customer_id)
    .bind(RevokeReason::Reuse.as_str())
    .execute(pool)
    .await?;
    Ok(if revoked.rows_affected() > 0 {
        Rotation::Reused
    } else {
        Rotation::Ended
    })
}

/// End one session of `customer_id`. Ending an unknown or ended session is a no-op.
pub async fn revoke(
    pool: &PgPool,
    customer_id: Uuid,
    session_id: Uuid,
    reason: RevokeReason,
) -> Result<()> {
    sqlx::query(
        "UPDATE auth_sessions SET revoked_at = NOW(), revoked_reason = $3 \
         WHERE id = $1 AND customer_id = $2 AND revoked_at IS NULL",
    )
    .bind(session_id)
    .bind(customer_id)
    .bind(reason.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

/// End the live sessions of `customer_id` opened without a second factor (a
/// password possibly known to someone else); returns how many were ended
pub async fn revoke_without_mfa(
    pool: &PgPool,
    customer_id: Uuid,
    reason: RevokeReason,
) -> Result<u64> {
    let done = sqlx::query(
        "UPDATE auth_sessions SET revoked_at = NOW(), revoked_reason = $2 \
         WHERE customer_id = $1 AND NOT mfa AND revoked_at IS NULL AND expires_at > NOW()",
    )
    .bind(customer_id)
    .bind(reason.as_str())
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

/// End every live session of `customer_id`; returns how many were ended
pub async fn revoke_all(pool: &PgPool, customer_id: Uuid, reason: RevokeReason) -> Result<u64> {
    let done = sqlx::query(
        "UPDATE auth_sessions SET revoked_at = NOW(), revoked_reason = $2 \
         WHERE customer_id = $1 AND revoked_at IS NULL AND expires_at > NOW()",
    )
    .bind(customer_id)
    .bind(reason.as_str())
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revoke_reasons_fit_their_column() {
        for reason in [
            RevokeReason::Logout,
            RevokeReason::Reuse,
            RevokeReason::PasswordChange,
            RevokeReason::FactorsReset,
            RevokeReason::SecondFactorRequired,
            RevokeReason::SecondFactorAdded,
        ] {
            assert!(reason.as_str().len() <= 32);
        }
    }
}
