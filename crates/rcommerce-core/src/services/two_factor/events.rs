//! The security log: sign-ins, failed second steps, factors added or removed,
//! recovery codes used, staff resets and exports. One row per event, never updated.

use serde_json::Value;
use sqlx::PgPool;
use std::net::IpAddr;
use uuid::Uuid;

use crate::Result;

/// A sign-in that ended with tokens (`detail.method`)
pub const SIGN_IN: &str = "sign_in";
/// A wrong second step (code, recovery code or passkey; `detail.flow`: sign_in, reauth
/// or setup)
pub const SECOND_FACTOR_FAILED: &str = "second_factor_failed";
/// Too many wrong codes in a row: codes are refused for a while (`detail.seconds`)
pub const SECOND_FACTOR_LOCKED: &str = "second_factor_locked";
/// An authenticator app was confirmed
pub const TOTP_ADDED: &str = "totp_added";
/// The authenticator app was removed
pub const TOTP_REMOVED: &str = "totp_removed";
/// A passkey was added (`detail.credential_id`)
pub const PASSKEY_ADDED: &str = "passkey_added";
/// A passkey was removed (`detail.credential_id`)
pub const PASSKEY_REMOVED: &str = "passkey_removed";
/// A recovery code was used to sign in or re-verify
pub const RECOVERY_CODE_USED: &str = "recovery_code_used";
/// New recovery codes replaced the old ones
pub const RECOVERY_CODES_RENEWED: &str = "recovery_codes_renewed";
/// An admin removed every factor of the account (`actor_id` = the admin)
pub const FACTORS_RESET: &str = "factors_reset";
/// The person re-verified themselves for a sensitive change
pub const REAUTHENTICATED: &str = "reauthenticated";
/// The password was changed or reset (`detail.how`: change or reset)
pub const PASSWORD_CHANGED: &str = "password_changed";

/// Events the account's owner is told about by e-mail: changes to how they sign in
pub const NOTIFIED: [&str; 8] = [
    PASSWORD_CHANGED,
    TOTP_ADDED,
    TOTP_REMOVED,
    PASSKEY_ADDED,
    PASSKEY_REMOVED,
    RECOVERY_CODE_USED,
    RECOVERY_CODES_RENEWED,
    FACTORS_RESET,
];

/// One event to record
#[derive(Debug, Clone)]
pub struct Event<'a> {
    /// The account concerned
    pub customer_id: Uuid,
    /// Who acted, when someone else than the account's owner did (staff)
    pub actor_id: Option<Uuid>,
    /// One of the constants of this module
    pub kind: &'a str,
    pub ip: Option<IpAddr>,
    pub detail: Value,
}

impl<'a> Event<'a> {
    /// An event the account's owner caused
    pub fn own(customer_id: Uuid, kind: &'a str, ip: Option<IpAddr>, detail: Value) -> Self {
        Self {
            customer_id,
            actor_id: None,
            kind,
            ip,
            detail,
        }
    }
}

/// Record `event`
pub async fn record(pool: &PgPool, event: &Event<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO security_events (customer_id, actor_id, kind, ip, detail) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(event.customer_id)
    .bind(event.actor_id)
    .bind(event.kind)
    .bind(event.ip.map(|ip| ip.to_string()))
    .bind(&event.detail)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record `event`; a failure is logged, never shown to the caller
pub async fn record_quietly(pool: &PgPool, event: Event<'_>) {
    if let Err(e) = record(pool, &event).await {
        tracing::error!("Failed to record security event {}: {}", event.kind, e);
    }
}
