//! Recovery codes: 10 single-use codes shown once, for when the phone or the passkey
//! is lost. Each has about 59 random bits (12 characters of a 31-letter alphabet
//! without look-alikes), so only an Argon2id hash with its own random salt is stored
//! (ASVS asks for a password hash below 112 bits).

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use password_hash::rand_core::OsRng;
use rand::Rng;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{Error, Result};

/// Codes issued at a time
pub const COUNT: usize = 10;
/// Letters and digits without 0/O, 1/I/L
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const LENGTH: usize = 12;

/// One new code, grouped by four for reading: `ABCD-EFGH-JKMN`
pub fn new_code() -> String {
    let mut rng = rand::thread_rng();
    let raw: String = (0..LENGTH)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect();
    format!("{}-{}-{}", &raw[0..4], &raw[4..8], &raw[8..12])
}

/// The code as stored: upper case, without separators or spaces
pub fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn hash(code: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(normalize(code).as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| Error::internal(format!("Failed to hash a recovery code: {e}")))
}

fn matches(code: &str, stored: &str) -> bool {
    PasswordHash::new(stored).is_ok_and(|h| {
        Argon2::default()
            .verify_password(normalize(code).as_bytes(), &h)
            .is_ok()
    })
}

/// Replace every recovery code of `customer_id` with [`COUNT`] new ones, returned once
pub async fn replace(pool: &PgPool, customer_id: Uuid) -> Result<Vec<String>> {
    let codes: Vec<String> = (0..COUNT).map(|_| new_code()).collect();
    let to_hash = codes.clone();
    let hashes: Vec<String> = tokio::task::spawn_blocking(move || {
        to_hash.iter().map(|c| hash(c)).collect::<Result<Vec<_>>>()
    })
    .await
    .map_err(|e| Error::internal(format!("Recovery code hashing stopped: {e}")))??;
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM mfa_recovery_codes WHERE customer_id = $1")
        .bind(customer_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO mfa_recovery_codes (customer_id, code_hash) \
         SELECT $1, h FROM unnest($2::text[]) AS h",
    )
    .bind(customer_id)
    .bind(&hashes)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(codes)
}

/// Use up `code` if it is one of the account's unused codes (checked against each
/// unused hash, at most [`COUNT`], off the async threads)
pub async fn consume(pool: &PgPool, customer_id: Uuid, code: &str) -> Result<bool> {
    if normalize(code).len() != LENGTH {
        return Ok(false);
    }
    let unused: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, code_hash FROM mfa_recovery_codes WHERE customer_id = $1 AND used_at IS NULL",
    )
    .bind(customer_id)
    .fetch_all(pool)
    .await?;
    let code = code.to_string();
    let found = tokio::task::spawn_blocking(move || {
        unused
            .into_iter()
            .find(|(_, stored)| matches(&code, stored))
            .map(|(id, _)| id)
    })
    .await
    .map_err(|e| Error::internal(format!("Recovery code check stopped: {e}")))?;
    let Some(id) = found else {
        return Ok(false);
    };
    let used = sqlx::query(
        "UPDATE mfa_recovery_codes SET used_at = NOW() WHERE id = $1 AND used_at IS NULL",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(used.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_read_in_groups_of_four_from_the_alphabet() {
        let code = new_code();
        assert_eq!(code.len(), 14);
        assert_eq!(code.matches('-').count(), 2);
        assert!(normalize(&code).bytes().all(|b| ALPHABET.contains(&b)));
    }

    #[test]
    fn typing_variations_match_the_stored_hash() {
        let stored = hash("abcd-efgh-jkmn").unwrap();
        assert!(stored.starts_with("$argon2id$"));
        assert!(matches("ABCD EFGH JKMN", &stored));
        assert!(!matches("ABCD-EFGH-JKMP", &stored));
        // each code has its own salt
        assert_ne!(stored, hash("abcd-efgh-jkmn").unwrap());
    }
}
