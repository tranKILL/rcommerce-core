//! Passkeys (WebAuthn) with `webauthn-rs`: registering one on the signed-in account,
//! signing in with one (discoverable credentials: no e-mail typed), and using one as
//! the second step or to re-verify. User verification (biometric or PIN) is always
//! required, so a passkey counts as two factors.
//!
//! The ceremony state between the options and the browser's answer is kept in
//! `auth_challenges` (5 minutes, single use).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use url::Url;
use uuid::Uuid;
use webauthn_rs::prelude::*;

/// The option and answer types, for clients written in Rust (tests, tools)
pub use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse};

use super::challenge::{self, ChallengeKind};
use crate::config::WebauthnConfig;
use crate::{Error, Result};

/// The store as a WebAuthn relying party (`[security.webauthn]`)
#[derive(Clone)]
pub struct Passkeys {
    webauthn: std::sync::Arc<Webauthn>,
    rp_id: String,
}

impl std::fmt::Debug for Passkeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Passkeys({})", self.rp_id)
    }
}

/// A passkey as the account's owner sees it
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PasskeyInfo {
    /// base64url credential id
    pub id: String,
    pub name: String,
    pub aaguid: Option<Uuid>,
    /// Synced by a password manager (iCloud Keychain, Google, 1Password...)
    pub backed_up: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Options for the browser and the challenge that answers them
#[derive(Debug, Clone, Serialize)]
pub struct Ceremony<T> {
    pub state_id: Uuid,
    pub options: T,
}

fn invalid(e: WebauthnError) -> Error {
    tracing::info!("Passkey refused: {e:?}");
    Error::validation("The passkey could not be verified")
}

impl Passkeys {
    /// The relying party from config; None when `rp_id` is unset (passkeys off)
    pub fn from_config(config: &WebauthnConfig) -> Result<Option<Self>> {
        let Some(rp_id) = config.rp_id.as_deref().filter(|id| !id.is_empty()) else {
            return Ok(None);
        };
        let parse = |origin: &str| {
            Url::parse(origin).map_err(|_| {
                Error::Config(format!("security.webauthn origin {origin} is not a URL"))
            })
        };
        let origins: Vec<Url> = config
            .origins
            .iter()
            .map(|o| parse(o))
            .collect::<Result<_>>()?;
        let first = origins
            .first()
            .ok_or_else(|| Error::Config("security.webauthn.origins is empty".into()))?;
        let mut builder = WebauthnBuilder::new(rp_id, first)
            .map_err(|e| Error::Config(format!("security.webauthn: {e:?}")))?
            .rp_name(&config.rp_name);
        for origin in &origins[1..] {
            builder = builder.append_allowed_origin(origin);
        }
        let webauthn = builder
            .build()
            .map_err(|e| Error::Config(format!("security.webauthn: {e:?}")))?;
        Ok(Some(Self {
            webauthn: std::sync::Arc::new(webauthn),
            rp_id: rp_id.to_string(),
        }))
    }

    /// The relying party id the browser must use (never derived from the page's host)
    pub fn rp_id(&self) -> &str {
        &self.rp_id
    }
}

/// The account's id towards passkey providers, created on first use (read first: the
/// row is only written once)
pub async fn user_handle(pool: &PgPool, customer_id: Uuid) -> Result<Uuid> {
    let known: Option<Uuid> =
        sqlx::query_scalar("SELECT webauthn_user_id FROM customers WHERE id = $1")
            .bind(customer_id)
            .fetch_one(pool)
            .await?;
    if let Some(handle) = known {
        return Ok(handle);
    }
    let handle: Uuid = sqlx::query_scalar(
        "UPDATE customers SET webauthn_user_id = COALESCE(webauthn_user_id, $2) \
         WHERE id = $1 RETURNING webauthn_user_id",
    )
    .bind(customer_id)
    .bind(Uuid::new_v4())
    .fetch_one(pool)
    .await?;
    Ok(handle)
}

async fn stored(pool: &PgPool, customer_id: Uuid) -> Result<Vec<(String, Passkey)>> {
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT credential_id, passkey FROM customer_passkeys WHERE customer_id = $1",
    )
    .bind(customer_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(id, value)| Ok((id, serde_json::from_value(value)?)))
        .collect()
}

/// The account's passkeys, newest first
pub async fn list(pool: &PgPool, customer_id: Uuid) -> Result<Vec<PasskeyInfo>> {
    let rows = sqlx::query_as::<_, PasskeyInfo>(
        "SELECT credential_id AS id, name, aaguid, backed_up, created_at, last_used_at \
         FROM customer_passkeys WHERE customer_id = $1 ORDER BY created_at DESC",
    )
    .bind(customer_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Rename one of the account's passkeys
pub async fn rename(pool: &PgPool, customer_id: Uuid, id: &str, name: &str) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE customer_passkeys SET name = $3 WHERE customer_id = $1 AND credential_id = $2",
    )
    .bind(customer_id)
    .bind(id)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Remove one of the account's passkeys
pub async fn remove(pool: &PgPool, customer_id: Uuid, id: &str) -> Result<bool> {
    let done =
        sqlx::query("DELETE FROM customer_passkeys WHERE customer_id = $1 AND credential_id = $2")
            .bind(customer_id)
            .bind(id)
            .execute(pool)
            .await?;
    Ok(done.rows_affected() == 1)
}

/// The account a passkey belongs to, by its credential id
pub async fn owner_of(pool: &PgPool, id: &str) -> Result<Option<Uuid>> {
    let owner =
        sqlx::query_scalar("SELECT customer_id FROM customer_passkeys WHERE credential_id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(owner)
}

/// What the browser reports about a new passkey besides what is verified: the
/// authenticator model (AAGUID, for display only) and the backup flags, read from
/// `response.authenticatorData` when the browser sends it (WebAuthn Level 3 `toJSON`)
fn registration_details(credential: &Value) -> (Option<Uuid>, bool, bool) {
    let data = credential["response"]["authenticatorData"]
        .as_str()
        .and_then(|s| URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok());
    let Some(data) = data.filter(|d| d.len() >= 53) else {
        return (None, false, false);
    };
    let flags = data[32];
    let aaguid = Uuid::from_slice(&data[37..53])
        .ok()
        .filter(|id| !id.is_nil());
    (aaguid, flags & 0x08 != 0, flags & 0x10 != 0)
}

impl Passkeys {
    /// Options to add a passkey to `customer_id` (`email` and `display_name` label it
    /// in the password manager); existing passkeys are excluded
    pub async fn start_registration(
        &self,
        pool: &PgPool,
        customer_id: Uuid,
        email: &str,
        display_name: &str,
    ) -> Result<Ceremony<Value>> {
        let handle = user_handle(pool, customer_id).await?;
        let exclude: Vec<CredentialID> = stored(pool, customer_id)
            .await?
            .iter()
            .map(|(_, key)| key.cred_id().clone())
            .collect();
        let (options, state) = self
            .webauthn
            .start_passkey_registration(handle, email, display_name, Some(exclude))
            .map_err(invalid)?;
        // A discoverable credential: the passkey must sign in without an e-mail typed
        // (webauthn-rs leaves it optional to spare the slots of hardware keys)
        let mut options = serde_json::to_value(&options)?;
        let selection = &mut options["publicKey"]["authenticatorSelection"];
        selection["residentKey"] = json!("required");
        selection["requireResidentKey"] = json!(true);
        let data = json!({ "state": serde_json::to_value(&state)? });
        let state_id = challenge::create(
            pool,
            ChallengeKind::PasskeyRegistration,
            Some(customer_id),
            data,
        )
        .await?;
        Ok(Ceremony { state_id, options })
    }

    /// Verify the browser's new credential and store it; returns the stored passkey
    pub async fn finish_registration(
        &self,
        pool: &PgPool,
        customer_id: Uuid,
        state_id: Uuid,
        credential: &Value,
        name: &str,
    ) -> Result<PasskeyInfo> {
        let taken = challenge::take(pool, state_id, ChallengeKind::PasskeyRegistration)
            .await?
            .filter(|c| c.customer_id == Some(customer_id))
            .ok_or_else(|| Error::validation("The passkey request has expired; try again"))?;
        let state: PasskeyRegistration = serde_json::from_value(taken.data["state"].clone())?;
        let reg: RegisterPublicKeyCredential = serde_json::from_value(credential.clone())
            .map_err(|_| Error::validation("Malformed passkey credential"))?;
        let passkey = self
            .webauthn
            .finish_passkey_registration(&reg, &state)
            .map_err(invalid)?;
        let id = URL_SAFE_NO_PAD.encode::<&[u8]>(passkey.cred_id().as_ref());
        let (aaguid, eligible, backed_up) = registration_details(credential);
        let info = sqlx::query_as::<_, PasskeyInfo>(
            "INSERT INTO customer_passkeys \
               (credential_id, customer_id, passkey, name, aaguid, backup_eligible, backed_up) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             RETURNING credential_id AS id, name, aaguid, backed_up, created_at, last_used_at",
        )
        .bind(&id)
        .bind(customer_id)
        .bind(serde_json::to_value(&passkey)?)
        .bind(name)
        .bind(aaguid)
        .bind(eligible)
        .bind(backed_up)
        .fetch_one(pool)
        .await?;
        Ok(info)
    }

    /// Options for a sign-in with any passkey of this store (the browser lists them)
    pub async fn start_sign_in(&self, pool: &PgPool) -> Result<Ceremony<RequestChallengeResponse>> {
        let (options, state) = self
            .webauthn
            .start_discoverable_authentication()
            .map_err(invalid)?;
        let data = json!({ "state": serde_json::to_value(&state)? });
        let state_id = challenge::create(pool, ChallengeKind::PasskeySignIn, None, data).await?;
        Ok(Ceremony { state_id, options })
    }

    /// Verify a sign-in with any passkey; returns the account. An unknown credential
    /// is `NotFound`, so the page can tell the password manager it is gone.
    pub async fn finish_sign_in(
        &self,
        pool: &PgPool,
        state_id: Uuid,
        credential: &Value,
    ) -> Result<Uuid> {
        let taken = challenge::take(pool, state_id, ChallengeKind::PasskeySignIn)
            .await?
            .ok_or_else(|| Error::validation("The passkey request has expired; try again"))?;
        let state: DiscoverableAuthentication =
            serde_json::from_value(taken.data["state"].clone())?;
        let auth: PublicKeyCredential = serde_json::from_value(credential.clone())
            .map_err(|_| Error::validation("Malformed passkey credential"))?;
        let (handle, raw_id) = self
            .webauthn
            .identify_discoverable_authentication(&auth)
            .map_err(invalid)?;
        let id = URL_SAFE_NO_PAD.encode(raw_id);
        let found: Option<(Uuid, Value)> = sqlx::query_as(
            "SELECT p.customer_id, p.passkey FROM customer_passkeys p \
             JOIN customers c ON c.id = p.customer_id \
             WHERE p.credential_id = $1 AND c.webauthn_user_id = $2",
        )
        .bind(&id)
        .bind(handle)
        .fetch_optional(pool)
        .await?;
        let (customer_id, value) = found.ok_or_else(|| Error::not_found("Unknown passkey"))?;
        let mut passkey: Passkey = serde_json::from_value(value)?;
        let result = self
            .webauthn
            .finish_discoverable_authentication_v2(&auth, state, &passkey)
            .map_err(invalid)?;
        save_use(pool, &id, &mut passkey, &result).await?;
        Ok(customer_id)
    }

    /// Options for a passkey of `customer_id` only: the second step after a password,
    /// or re-verification (`kind`)
    pub async fn start_for(
        &self,
        pool: &PgPool,
        customer_id: Uuid,
        kind: ChallengeKind,
    ) -> Result<Ceremony<RequestChallengeResponse>> {
        let keys: Vec<Passkey> = stored(pool, customer_id)
            .await?
            .into_iter()
            .map(|(_, key)| key)
            .collect();
        if keys.is_empty() {
            return Err(Error::validation("This account has no passkey"));
        }
        let (options, state) = self
            .webauthn
            .start_passkey_authentication(&keys)
            .map_err(invalid)?;
        let data = json!({ "state": serde_json::to_value(&state)? });
        let state_id = challenge::create(pool, kind, Some(customer_id), data).await?;
        Ok(Ceremony { state_id, options })
    }

    /// Verify a passkey of `customer_id` answering [`Self::start_for`]. A passkey of
    /// another account is refused, even a valid one.
    pub async fn finish_for(
        &self,
        pool: &PgPool,
        customer_id: Uuid,
        kind: ChallengeKind,
        state_id: Uuid,
        credential: &Value,
    ) -> Result<()> {
        let taken = challenge::take(pool, state_id, kind)
            .await?
            .filter(|c| c.customer_id == Some(customer_id))
            .ok_or_else(|| Error::validation("The passkey request has expired; try again"))?;
        let state: PasskeyAuthentication = serde_json::from_value(taken.data["state"].clone())?;
        let auth: PublicKeyCredential = serde_json::from_value(credential.clone())
            .map_err(|_| Error::validation("Malformed passkey credential"))?;
        let result = self
            .webauthn
            .finish_passkey_authentication(&auth, &state)
            .map_err(invalid)?;
        let id = URL_SAFE_NO_PAD.encode::<&[u8]>(result.cred_id().as_ref());
        let mut keys = stored(pool, customer_id).await?;
        let (_, passkey) = keys
            .iter_mut()
            .find(|(stored_id, _)| *stored_id == id)
            .ok_or_else(|| Error::not_found("Unknown passkey"))?;
        save_use(pool, &id, passkey, &result).await
    }
}

/// Store the new signature counter and backup state of a passkey just used
async fn save_use(
    pool: &PgPool,
    id: &str,
    passkey: &mut Passkey,
    result: &AuthenticationResult,
) -> Result<()> {
    passkey.update_credential(result);
    sqlx::query(
        "UPDATE customer_passkeys SET passkey = $2, backed_up = $3, last_used_at = NOW() \
         WHERE credential_id = $1",
    )
    .bind(id)
    .bind(serde_json::to_value(&*passkey)?)
    .bind(result.backup_state())
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passkeys_are_off_without_an_rp_id() {
        assert!(Passkeys::from_config(&WebauthnConfig::default())
            .unwrap()
            .is_none());
    }

    #[test]
    fn the_relying_party_takes_every_origin() {
        let config = WebauthnConfig {
            rp_id: Some("example.com".into()),
            rp_name: "Example".into(),
            origins: vec![
                "https://www.example.com".into(),
                "https://admin.example.com".into(),
            ],
        };
        let rp = Passkeys::from_config(&config).unwrap().unwrap();
        assert_eq!(rp.rp_id(), "example.com");
    }

    #[test]
    fn registration_details_read_the_authenticator_data() {
        let mut data = vec![0u8; 55];
        data[32] = 0x08 | 0x10 | 0x45; // backup eligible, backed up, UP, UV, AT
        data[37..53].copy_from_slice(Uuid::from_u128(0xfbfc3007).as_bytes());
        let credential =
            json!({ "response": { "authenticatorData": URL_SAFE_NO_PAD.encode(&data) } });
        let (aaguid, eligible, backed_up) = registration_details(&credential);
        assert_eq!(aaguid, Some(Uuid::from_u128(0xfbfc3007)));
        assert!(eligible && backed_up);
        assert_eq!(registration_details(&json!({})), (None, false, false));
    }
}
