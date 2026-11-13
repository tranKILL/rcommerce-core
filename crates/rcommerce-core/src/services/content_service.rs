//! Content blocks: short storefront texts staff edit per language (the home
//! page banner's title and text, ...). A block is a JSON object of text
//! fields under a key; each language has its own, and a storefront falls back
//! to its built-in texts when a language has none.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::locale::normalize_locale;
use crate::{Error, Result};

/// Text fields of a block: field name -> text
pub type Fields = BTreeMap<String, String>;

/// A block in one language
#[derive(Debug, Clone, Serialize)]
pub struct ContentBlock {
    pub key: String,
    pub locale: String,
    pub content: Fields,
    pub updated_at: DateTime<Utc>,
}

/// Longest text of a field
const MAX_FIELD: usize = 2000;

fn check_key(key: &str) -> Result<()> {
    let ok = (1..=50).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(Error::validation(
            "key must be 1 to 50 lowercase letters, digits or _",
        ))
    }
}

/// The fields to store: trimmed, empty ones dropped, each name a key-like
/// word and each text at most 2000 characters
fn clean(fields: &Fields) -> Result<Fields> {
    let mut kept = Fields::new();
    for (name, text) in fields {
        check_key(name)?;
        if text.chars().count() > MAX_FIELD {
            return Err(Error::validation(format!(
                "{name} is longer than {MAX_FIELD} characters"
            )));
        }
        if !text.trim().is_empty() {
            kept.insert(name.clone(), text.trim().to_string());
        }
    }
    Ok(kept)
}

/// Content blocks
#[derive(Clone)]
pub struct ContentService {
    pool: PgPool,
}

impl ContentService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Block `key` in the first of `locales` that has it
    pub async fn get(&self, key: &str, locales: &[&str]) -> Result<Option<ContentBlock>> {
        check_key(key)?;
        for locale in locales.iter().filter_map(|l| normalize_locale(l)) {
            let row: Option<(sqlx::types::Json<Fields>, DateTime<Utc>)> = sqlx::query_as(
                "SELECT content, updated_at FROM content_blocks WHERE key = $1 AND locale = $2",
            )
            .bind(key)
            .bind(&locale)
            .fetch_optional(&self.pool)
            .await?;
            if let Some((content, updated_at)) = row {
                return Ok(Some(ContentBlock {
                    key: key.into(),
                    locale,
                    content: content.0,
                    updated_at,
                }));
            }
        }
        Ok(None)
    }

    /// Block `key` in every language that has it
    pub async fn all_languages(&self, key: &str) -> Result<BTreeMap<String, Fields>> {
        check_key(key)?;
        let rows: Vec<(String, sqlx::types::Json<Fields>)> =
            sqlx::query_as("SELECT locale, content FROM content_blocks WHERE key = $1")
                .bind(key)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|(l, c)| (l, c.0)).collect())
    }

    /// Replace block `key` in the languages given (a language with only
    /// empty fields loses its block)
    pub async fn save(
        &self,
        key: &str,
        languages: &BTreeMap<String, Fields>,
        staff: Uuid,
    ) -> Result<BTreeMap<String, Fields>> {
        check_key(key)?;
        let mut tx = self.pool.begin().await?;
        for (locale, fields) in languages {
            let locale = normalize_locale(locale)
                .ok_or_else(|| Error::validation(format!("unknown language {locale}")))?;
            let fields = clean(fields)?;
            if fields.is_empty() {
                sqlx::query("DELETE FROM content_blocks WHERE key = $1 AND locale = $2")
                    .bind(key)
                    .bind(&locale)
                    .execute(&mut *tx)
                    .await?;
                continue;
            }
            sqlx::query(
                "INSERT INTO content_blocks (key, locale, content, updated_by) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (key, locale) DO UPDATE SET content = EXCLUDED.content, \
                 updated_by = EXCLUDED.updated_by, updated_at = NOW()",
            )
            .bind(key)
            .bind(&locale)
            .bind(sqlx::types::Json(&fields))
            .bind(staff)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.all_languages(key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_fields_are_checked_and_trimmed() {
        assert!(check_key("home_hero").is_ok());
        assert!(check_key("Home Hero").is_err());
        let fields = Fields::from([
            ("title".into(), "  Tout pour construire ".into()),
            ("text".into(), " ".into()),
        ]);
        let kept = clean(&fields).unwrap();
        assert_eq!(
            kept.get("title").map(String::as_str),
            Some("Tout pour construire")
        );
        assert!(!kept.contains_key("text"), "empty fields are dropped");
        let long = Fields::from([("text".into(), "x".repeat(2001))]);
        assert!(clean(&long).is_err());
    }
}
