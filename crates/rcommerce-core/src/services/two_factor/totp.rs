//! Authenticator apps (TOTP, RFC 6238): 6 digits, 30-second steps, SHA-1 (what every
//! authenticator app supports), one step of clock drift each way. The shared secret is
//! encrypted at rest with `[security.mfa] encryption_key` (AES-256-GCM), and a code is
//! never accepted twice: the last accepted step is stored.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::RngCore;
use sqlx::PgPool;
use totp_rs::{Algorithm, Builder, Totp};
use uuid::Uuid;

use crate::config::MfaConfig;
use crate::{Error, Result};

/// Seconds per code
pub const STEP: u64 = 30;
const DIGITS: usize = 6;
const SECRET_BYTES: usize = 20;
const NONCE_BYTES: usize = 12;

/// The key that seals authenticator secrets
#[derive(Clone)]
pub struct SecretKey(Aes256Gcm);

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(..)")
    }
}

impl SecretKey {
    /// The key from `[security.mfa] encryption_key` (base64 of 32 bytes); None when unset
    pub fn from_config(config: &MfaConfig) -> Result<Option<Self>> {
        let Some(encoded) = config.encryption_key.as_deref().filter(|k| !k.is_empty()) else {
            return Ok(None);
        };
        let bytes = STANDARD
            .decode(encoded.trim())
            .map_err(|_| Error::Config("security.mfa.encryption_key is not base64".into()))?;
        let key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| Error::Config("security.mfa.encryption_key must be 32 bytes".into()))?;
        Ok(Some(Self(Aes256Gcm::new(&Key::<Aes256Gcm>::from(key)))))
    }

    /// nonce || ciphertext
    fn seal(&self, secret: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_BYTES];
        rand::thread_rng().fill_bytes(&mut nonce);
        let sealed = self
            .0
            .encrypt(&Nonce::from(nonce), secret)
            .map_err(|_| Error::internal("Failed to encrypt the authenticator secret"))?;
        Ok([nonce.as_slice(), &sealed].concat())
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() <= NONCE_BYTES {
            return Err(Error::internal("Stored authenticator secret is too short"));
        }
        let (nonce, body) = sealed.split_at(NONCE_BYTES);
        let nonce: [u8; NONCE_BYTES] = nonce.try_into().expect("split at NONCE_BYTES");
        self.0
            .decrypt(&Nonce::from(nonce), body)
            .map_err(|_| Error::internal("Failed to decrypt the authenticator secret"))
    }
}

/// What the authenticator app needs: the secret to type, or the QR code's URI
#[derive(Debug, Clone, serde::Serialize)]
pub struct Enrolment {
    /// Base32 secret, for manual entry
    pub secret: String,
    /// `otpauth://totp/...`, drawn as a QR code by the apps
    pub otpauth_uri: String,
}

fn settings(secret: Vec<u8>) -> Builder {
    Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(DIGITS as u8)
        .with_skew(1)
        .with_step_duration(STEP)
        .with_secret(secret)
}

fn generator(secret: Vec<u8>, issuer: &str, account: &str) -> Result<Totp> {
    // the otpauth label refuses ':' in either part
    let clean = |s: &str| s.replace(':', " ");
    settings(secret)
        .with_account_name(clean(account))
        .with_issuer(Some(clean(issuer)))
        .build()
        .map_err(|e| Error::internal(format!("Invalid authenticator settings: {e:?}")))
}

/// Start (or restart) setting up an authenticator app for `customer_id`. Refused
/// while a confirmed app exists: it must be removed first.
pub async fn begin(
    pool: &PgPool,
    key: &SecretKey,
    customer_id: Uuid,
    issuer: &str,
    account: &str,
) -> Result<Enrolment> {
    let mut secret = vec![0u8; SECRET_BYTES];
    rand::thread_rng().fill_bytes(&mut secret);
    let totp = generator(secret.clone(), issuer, account)?;
    let stored = sqlx::query(
        "INSERT INTO customer_totp (customer_id, secret_enc) VALUES ($1, $2) \
         ON CONFLICT (customer_id) DO UPDATE SET secret_enc = EXCLUDED.secret_enc, \
           last_step = 0, created_at = NOW() \
         WHERE customer_totp.confirmed_at IS NULL",
    )
    .bind(customer_id)
    .bind(key.seal(&secret)?)
    .execute(pool)
    .await?;
    if stored.rows_affected() == 0 {
        return Err(Error::validation(
            "An authenticator app is already set up; remove it first",
        ));
    }
    let otpauth_uri = totp
        .to_url()
        .map_err(|e| Error::internal(format!("Invalid authenticator label: {e:?}")))?;
    Ok(Enrolment {
        secret: totp.secret().to_base32(),
        otpauth_uri,
    })
}

/// The 30-second step `code` belongs to, if it is valid at `now` (unix seconds),
/// in the current or the previous step (a code lives 30 seconds, plus one step for a
/// clock or a person that is a little late), and later than `after` (the last
/// accepted step)
pub fn matching_step(secret: &[u8], code: &str, now: u64, after: i64) -> Option<i64> {
    let code = code.trim();
    if code.len() != DIGITS || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let totp = settings(secret.to_vec()).build_noncompliant();
    let current = (now / STEP) as i64;
    (current - 1..=current)
        .filter(|step| *step >= 0 && *step > after)
        .find(|step| {
            let expected = totp.generate(*step as u64 * STEP).to_string();
            super::same(expected.as_bytes(), code.as_bytes())
        })
}

/// RFC 4648 base32 (the form authenticator apps take), without padding
fn base32_decode(text: &str) -> Option<Vec<u8>> {
    const LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let (mut bits, mut value, mut out) = (0u32, 0u32, Vec::new());
    for c in text.trim_end_matches('=').bytes() {
        let index = LETTERS.iter().position(|l| *l == c.to_ascii_uppercase())? as u32;
        value = (value << 5) | index;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((value >> bits) as u8);
            value &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The code an authenticator app shows at `time` (unix seconds) for a base32 secret
/// (tests and development tools sign in with it)
pub fn code_at(secret_base32: &str, time: u64) -> Option<String> {
    let secret = base32_decode(secret_base32)?;
    Some(
        settings(secret)
            .build_noncompliant()
            .generate(time)
            .to_string(),
    )
}

/// Check a code of the confirmed app (`confirm = false`) or of the app being set up
/// (`confirm = true`, which confirms it). Accepting a code moves the last step forward
/// in the same statement, so a code works once, even under concurrent attempts.
pub async fn check(
    pool: &PgPool,
    key: &SecretKey,
    customer_id: Uuid,
    code: &str,
    confirm: bool,
) -> Result<bool> {
    // a setup left unconfirmed expires after 10 minutes
    let row: Option<(Vec<u8>, i64)> = sqlx::query_as(
        "SELECT secret_enc, last_step FROM customer_totp \
         WHERE customer_id = $1 AND (confirmed_at IS NULL) = $2 \
           AND (NOT $2 OR created_at > NOW() - INTERVAL '10 minutes')",
    )
    .bind(customer_id)
    .bind(confirm)
    .fetch_optional(pool)
    .await?;
    let Some((sealed, last_step)) = row else {
        return Ok(false);
    };
    let now = chrono::Utc::now().timestamp() as u64;
    let Some(step) = matching_step(&key.open(&sealed)?, code, now, last_step) else {
        return Ok(false);
    };
    let accepted = sqlx::query(
        "UPDATE customer_totp SET last_step = $2, confirmed_at = COALESCE(confirmed_at, NOW()) \
         WHERE customer_id = $1 AND last_step < $2",
    )
    .bind(customer_id)
    .bind(step)
    .execute(pool)
    .await?;
    Ok(accepted.rows_affected() == 1)
}

/// Remove the authenticator app (set up or not); returns whether one was confirmed
pub async fn remove(pool: &PgPool, customer_id: Uuid) -> Result<bool> {
    let confirmed: Option<bool> = sqlx::query_scalar(
        "DELETE FROM customer_totp WHERE customer_id = $1 RETURNING confirmed_at IS NOT NULL",
    )
    .bind(customer_id)
    .fetch_optional(pool)
    .await?;
    Ok(confirmed.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 appendix B, SHA-1 seed, 6-digit truncation of the 8-digit vectors
    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn rfc_6238_vectors_match_their_step() {
        // T = 59 s -> 94287082, T = 1111111109 -> 07081804
        assert_eq!(matching_step(RFC_SECRET, "287082", 59, 0), Some(1));
        assert_eq!(
            matching_step(RFC_SECRET, "081804", 1_111_111_109, 0),
            Some(1_111_111_109 / 30)
        );
    }

    #[test]
    fn a_code_is_valid_in_its_step_and_the_next_only() {
        // the code of step 1 (t = 30..59)
        assert_eq!(matching_step(RFC_SECRET, "287082", 89, 0), Some(1));
        assert_eq!(
            matching_step(RFC_SECRET, "287082", 0, -1),
            None,
            "not before its time"
        );
        assert_eq!(matching_step(RFC_SECRET, "287082", 120, 0), None);
    }

    #[test]
    fn codes_follow_the_base32_secret() {
        // "12345678901234567890" in base32
        let secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        assert_eq!(base32_decode(secret).unwrap(), RFC_SECRET);
        assert_eq!(code_at(secret, 59).unwrap(), "287082");
        assert!(code_at("not base32!", 59).is_none());
    }

    #[test]
    fn a_used_step_is_refused() {
        assert_eq!(matching_step(RFC_SECRET, "287082", 59, 1), None);
    }

    #[test]
    fn malformed_codes_are_refused() {
        for code in ["", "28708", "2870821", "28708a", "28 082"] {
            assert_eq!(matching_step(RFC_SECRET, code, 59, 0), None, "{code}");
        }
    }

    #[test]
    fn secrets_round_trip_through_the_key() {
        let config = MfaConfig {
            encryption_key: Some(STANDARD.encode([7u8; 32])),
            ..Default::default()
        };
        let key = SecretKey::from_config(&config).unwrap().unwrap();
        let sealed = key.seal(b"secret").unwrap();
        assert_ne!(&sealed[NONCE_BYTES..], b"secret");
        assert_eq!(key.open(&sealed).unwrap(), b"secret");
    }

    #[test]
    fn a_wrong_size_key_is_a_config_error() {
        let config = MfaConfig {
            encryption_key: Some(STANDARD.encode([7u8; 16])),
            ..Default::default()
        };
        assert!(SecretKey::from_config(&config).is_err());
        assert!(SecretKey::from_config(&MfaConfig::default())
            .unwrap()
            .is_none());
    }
}
