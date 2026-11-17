//! One-time challenges: the second step after a password, and the WebAuthn state
//! kept between a passkey's options and the browser's answer. A challenge expires
//! after minutes and is deleted when used (or after too many wrong answers).

use chrono::{Duration, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::Result;

/// Wrong answers a second-step challenge takes before it is deleted
pub const MAX_ATTEMPTS: i32 = 5;

/// What a challenge is for
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeKind {
    /// The second step after a correct password (5 minutes)
    SecondFactor,
    /// A passkey sign-in without a password
    PasskeySignIn,
    /// Adding a passkey to the signed-in account
    PasskeyRegistration,
    /// A passkey as the second step after a password
    PasskeySecondFactor,
    /// Re-verifying the signed-in person with a passkey
    PasskeyReauth,
}

impl ChallengeKind {
    /// Value stored in `auth_challenges.kind`
    pub fn as_str(self) -> &'static str {
        match self {
            ChallengeKind::SecondFactor => "second_factor",
            ChallengeKind::PasskeySignIn => "passkey_sign_in",
            ChallengeKind::PasskeyRegistration => "passkey_registration",
            ChallengeKind::PasskeySecondFactor => "passkey_second_factor",
            ChallengeKind::PasskeyReauth => "passkey_reauth",
        }
    }

    /// How long the challenge can be answered
    pub fn lifetime(self) -> Duration {
        Duration::minutes(5)
    }
}

/// A stored challenge
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Challenge {
    pub id: Uuid,
    pub customer_id: Option<Uuid>,
    pub data: Value,
    pub attempts: i32,
}

/// Store a challenge and return its id. Expired challenges are swept on the way, on
/// about one call in 32 (the sweep is a table scan).
pub async fn create(
    pool: &PgPool,
    kind: ChallengeKind,
    customer_id: Option<Uuid>,
    data: Value,
) -> Result<Uuid> {
    if rand::random::<u8>() < 8 {
        sqlx::query("DELETE FROM auth_challenges WHERE expires_at < NOW()")
            .execute(pool)
            .await?;
    }
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO auth_challenges (id, customer_id, kind, data, expires_at) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(customer_id)
    .bind(kind.as_str())
    .bind(data)
    .bind(Utc::now() + kind.lifetime())
    .execute(pool)
    .await?;
    Ok(id)
}

/// Count an attempt on the live challenge `id` of `kind` and return it, before the
/// answer is checked: parallel answers each use one of its [`MAX_ATTEMPTS`]
pub async fn reserve(pool: &PgPool, id: Uuid, kind: ChallengeKind) -> Result<Option<Challenge>> {
    let found = sqlx::query_as::<_, Challenge>(
        "UPDATE auth_challenges SET attempts = attempts + 1 \
         WHERE id = $1 AND kind = $2 AND expires_at > NOW() AND attempts < $3 \
         RETURNING id, customer_id, data, attempts",
    )
    .bind(id)
    .bind(kind.as_str())
    .bind(MAX_ATTEMPTS)
    .fetch_optional(pool)
    .await?;
    Ok(found)
}

/// Use up the live challenge `id` of `kind`: of two concurrent uses, one gets it.
/// (A challenge whose last attempt was the right answer can still be used up.)
pub async fn take(pool: &PgPool, id: Uuid, kind: ChallengeKind) -> Result<Option<Challenge>> {
    let taken = sqlx::query_as::<_, Challenge>(
        "DELETE FROM auth_challenges \
         WHERE id = $1 AND kind = $2 AND expires_at > NOW() AND attempts <= $3 \
         RETURNING id, customer_id, data, attempts",
    )
    .bind(id)
    .bind(kind.as_str())
    .bind(MAX_ATTEMPTS)
    .fetch_optional(pool)
    .await?;
    Ok(taken)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_fit_their_column() {
        for kind in [
            ChallengeKind::SecondFactor,
            ChallengeKind::PasskeySignIn,
            ChallengeKind::PasskeyRegistration,
            ChallengeKind::PasskeySecondFactor,
            ChallengeKind::PasskeyReauth,
        ] {
            assert!(kind.as_str().len() <= 32);
            assert!(kind.lifetime() <= Duration::minutes(15));
        }
    }
}
