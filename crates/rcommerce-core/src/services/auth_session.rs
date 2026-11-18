//! Refresh-token sessions.
//!
//! Every login opens a session row. A refresh token names its session (`sid`) and the
//! id of the one token the session currently accepts (`jti`). Each refresh swaps that
//! id for a new one, so a refresh token works once. Presenting an older token of a live
//! session means a token was copied: the session is revoked, which also ends the
//! copy's access to it.
//!
//! Access tokens name their session too (`sid`): a request with one is refused once
//! the session has ended (sign-out, password change, reset, refresh reuse). Whether a
//! session is live is cached for 30 seconds per process, and ending a session here
//! clears its entry at once.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use uuid::Uuid;

use crate::Result;

/// How long a liveness answer is trusted without asking the database again
const LIVE_FOR: std::time::Duration = std::time::Duration::from_secs(30);
/// Entries kept at most (the cache is emptied beyond)
const LIVE_MAX: usize = 100_000;

/// Cached liveness answers, and a counter every revocation bumps: an answer read
/// from the database before a revocation is not cached after it
#[derive(Default)]
struct LiveCache {
    /// session id -> (live, owner, checked at)
    answers: HashMap<Uuid, (bool, Uuid, Instant)>,
    generation: u64,
}

static LIVE: OnceLock<Mutex<LiveCache>> = OnceLock::new();

fn live_cache() -> &'static Mutex<LiveCache> {
    LIVE.get_or_init(Mutex::default)
}

/// Forget cached answers for one session, or for every session of an account
fn forget(session_id: Option<Uuid>, customer_id: Uuid) {
    if let Ok(mut cache) = live_cache().lock() {
        cache.generation += 1;
        match session_id {
            Some(id) => {
                cache.answers.remove(&id);
            }
            None => cache
                .answers
                .retain(|_, (_, owner, _)| *owner != customer_id),
        }
    }
}

/// Whether session `session_id` of `customer_id` is live: not ended, not expired
pub async fn is_live(pool: &PgPool, customer_id: Uuid, session_id: Uuid) -> Result<bool> {
    let mut generation = None;
    if let Ok(cache) = live_cache().lock() {
        if let Some((answer, owner, at)) = cache.answers.get(&session_id) {
            if *owner == customer_id && at.elapsed() < LIVE_FOR {
                return Ok(*answer);
            }
        }
        generation = Some(cache.generation);
    }
    let answer: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM auth_sessions WHERE id = $1 AND customer_id = $2 \
           AND revoked_at IS NULL AND expires_at > NOW())",
    )
    .bind(session_id)
    .bind(customer_id)
    .fetch_one(pool)
    .await?;
    if let Ok(mut cache) = live_cache().lock() {
        if Some(cache.generation) == generation {
            if cache.answers.len() >= LIVE_MAX {
                cache.answers.clear();
            }
            let entry = (answer, customer_id, Instant::now());
            cache.answers.insert(session_id, entry);
        }
    }
    Ok(answer)
}

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
    forget(Some(session_id), customer_id);
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
    // Before too: a check that reads the session meanwhile does not cache it as live
    forget(Some(session_id), customer_id);
    sqlx::query(
        "UPDATE auth_sessions SET revoked_at = NOW(), revoked_reason = $3 \
         WHERE id = $1 AND customer_id = $2 AND revoked_at IS NULL",
    )
    .bind(session_id)
    .bind(customer_id)
    .bind(reason.as_str())
    .execute(pool)
    .await?;
    forget(Some(session_id), customer_id);
    Ok(())
}

/// End the live sessions of `customer_id` opened without a second factor (a
/// password possibly known to someone else); returns how many were ended
pub async fn revoke_without_mfa(
    pool: &PgPool,
    customer_id: Uuid,
    reason: RevokeReason,
) -> Result<u64> {
    forget(None, customer_id);
    let done = sqlx::query(
        "UPDATE auth_sessions SET revoked_at = NOW(), revoked_reason = $2 \
         WHERE customer_id = $1 AND NOT mfa AND revoked_at IS NULL AND expires_at > NOW()",
    )
    .bind(customer_id)
    .bind(reason.as_str())
    .execute(pool)
    .await?;
    end_setups(pool, customer_id).await?;
    forget(None, customer_id);
    Ok(done.rows_affected())
}

/// End every live session of `customer_id`, and its setup tokens; returns how many
/// sessions were ended
pub async fn revoke_all(pool: &PgPool, customer_id: Uuid, reason: RevokeReason) -> Result<u64> {
    forget(None, customer_id);
    let done = sqlx::query(
        "UPDATE auth_sessions SET revoked_at = NOW(), revoked_reason = $2 \
         WHERE customer_id = $1 AND revoked_at IS NULL AND expires_at > NOW()",
    )
    .bind(customer_id)
    .bind(reason.as_str())
    .execute(pool)
    .await?;
    end_setups(pool, customer_id).await?;
    // A lost or stolen phone stops getting the account's notifications too; the
    // apps register again after signing in
    if !matches!(
        reason,
        RevokeReason::SecondFactorRequired | RevokeReason::SecondFactorAdded
    ) {
        sqlx::query("DELETE FROM push_devices WHERE customer_id = $1")
            .bind(customer_id)
            .execute(pool)
            .await?;
    }
    forget(None, customer_id);
    Ok(done.rows_affected())
}

/// Delete the setup tokens of `customer_id` (staff adding a first factor): a
/// password change or reset, an admin's reset or a first factor ends them
async fn end_setups(pool: &PgPool, customer_id: Uuid) -> Result<()> {
    use crate::services::two_factor::challenge::ChallengeKind;
    sqlx::query("DELETE FROM auth_challenges WHERE customer_id = $1 AND kind = $2")
        .bind(customer_id)
        .bind(ChallengeKind::Setup.as_str())
        .execute(pool)
        .await?;
    Ok(())
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
