//! Marketing consent: whether a customer accepts commercial e-mails
//! (`customers.accepts_marketing`), with the proof of each choice (GDPR art. 7):
//! when, where (sign-up, account, unsubscribe link), under which wording, from
//! which address. Staff export the e-mail list of the customers who said yes, each
//! row with its own unsubscribe link.

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;
use sqlx::{FromRow, PgPool};
use std::net::IpAddr;
use uuid::Uuid;

use crate::{Error, Result};

/// Where a choice was made
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The sign-up form (an unticked box the customer ticked)
    Register,
    /// The customer's account settings
    Account,
    /// The unsubscribe link of an e-mail
    Unsubscribe,
}

impl Source {
    /// Value stored in `marketing_consents.source`
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Register => "register",
            Source::Account => "account",
            Source::Unsubscribe => "unsubscribe",
        }
    }
}

/// A choice and its context
#[derive(Debug, Clone)]
pub struct Choice<'a> {
    pub granted: bool,
    pub source: Source,
    /// The version of the text the customer was shown (at most 32 characters)
    pub wording: Option<&'a str>,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<&'a str>,
}

/// One recorded choice
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Consent {
    pub granted: bool,
    pub source: String,
    pub wording_version: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Record a customer's choice and set `accepts_marketing` to it, in one
/// transaction. A choice equal to the current one changes and records nothing.
/// Returns whether it changed.
pub async fn choose(pool: &PgPool, customer_id: Uuid, choice: &Choice<'_>) -> Result<bool> {
    if choice.wording.is_some_and(|w| w.chars().count() > 32) {
        return Err(Error::validation(
            "marketing_wording: at most 32 characters",
        ));
    }
    // A "yes" is proof only with the words the customer was shown
    if choice.granted && choice.wording.map_or(true, |w| w.trim().is_empty()) {
        return Err(Error::validation(
            "marketing_wording: the version of the text shown is needed with a yes",
        ));
    }
    let mut tx = pool.begin().await?;
    let changed = sqlx::query(
        "UPDATE customers SET accepts_marketing = $2, updated_at = NOW() \
         WHERE id = $1 AND accepts_marketing IS DISTINCT FROM $2",
    )
    .bind(customer_id)
    .bind(choice.granted)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if changed {
        sqlx::query(
            "INSERT INTO marketing_consents \
               (customer_id, granted, source, wording_version, ip, user_agent) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(customer_id)
        .bind(choice.granted)
        .bind(choice.source.as_str())
        .bind(choice.wording)
        .bind(choice.ip.map(|ip| ip.to_string()))
        .bind(
            choice
                .user_agent
                .map(|ua| ua.chars().take(500).collect::<String>()),
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// A customer's choices, newest first
pub async fn history(pool: &PgPool, customer_id: Uuid) -> Result<Vec<Consent>> {
    Ok(sqlx::query_as::<_, Consent>(
        "SELECT granted, source, wording_version, created_at FROM marketing_consents \
         WHERE customer_id = $1 ORDER BY created_at DESC",
    )
    .bind(customer_id)
    .fetch_all(pool)
    .await?)
}

type HmacSha256 = Hmac<Sha256>;

/// The signature of a customer's unsubscribe link (base64url)
fn signature(secret: &[u8], customer_id: Uuid) -> Result<Vec<u8>> {
    let mut mac =
        HmacSha256::new_from_slice(secret).map_err(|_| Error::internal("unsubscribe key"))?;
    mac.update(b"unsubscribe:");
    mac.update(customer_id.as_bytes());
    Ok(mac.finalize().into_bytes().to_vec())
}

/// The token of a customer's unsubscribe link: `<customer id>.<signature>`. It
/// does not expire (an old e-mail's link still works) and can only turn
/// commercial e-mails off.
pub fn unsubscribe_token(secret: &[u8], customer_id: Uuid) -> Result<String> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let sig = URL_SAFE_NO_PAD.encode(signature(secret, customer_id)?);
    Ok(format!("{customer_id}.{sig}"))
}

/// The customer an unsubscribe token belongs to, if its signature is right
pub fn unsubscribe_customer(secret: &[u8], token: &str) -> Option<Uuid> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let (id, sig) = token.split_once('.')?;
    let id = Uuid::parse_str(id).ok()?;
    let sig = URL_SAFE_NO_PAD.decode(sig).ok()?;
    let mut mac = HmacSha256::new_from_slice(secret).ok()?;
    mac.update(b"unsubscribe:");
    mac.update(id.as_bytes());
    mac.verify_slice(&sig).ok()?;
    Some(id)
}

/// Who an export is for
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    /// Customers without a company
    Retail,
    /// Members of a company
    Trade,
    All,
}

impl Audience {
    /// `retail`, `trade` or `all` (default)
    pub fn parse(text: Option<&str>) -> Result<Self> {
        match text.unwrap_or("all") {
            "retail" => Ok(Audience::Retail),
            "trade" => Ok(Audience::Trade),
            "all" => Ok(Audience::All),
            _ => Err(Error::validation("audience: retail, trade or all")),
        }
    }

    fn condition(self) -> &'static str {
        match self {
            Audience::Retail => "m.company_id IS NULL",
            Audience::Trade => "m.company_id IS NOT NULL",
            Audience::All => "TRUE",
        }
    }
}

/// One customer of the e-mail list
#[derive(Debug, Clone, FromRow)]
pub struct Recipient {
    pub id: Uuid,
    pub email: String,
    pub first_name: String,
    pub last_name: String,
    pub locale: Option<String>,
    pub company: Option<String>,
    /// The latest "yes" (none for consents older than the record)
    pub consented_at: Option<DateTime<Utc>>,
    pub consent_source: Option<String>,
}

/// The customers to write to: they accept commercial e-mails, their consent is on
/// record (a "yes" set before the record existed is not proof) and they confirmed
/// their address; `lang`: only those whose language starts with it
fn list_query(audience: Audience, select: &str) -> String {
    format!(
        "SELECT {select} FROM customers c \
         LEFT JOIN company_members m ON m.customer_id = c.id \
         LEFT JOIN companies co ON co.id = m.company_id \
         LEFT JOIN LATERAL (SELECT k.created_at, k.source FROM marketing_consents k \
             WHERE k.customer_id = c.id AND k.granted ORDER BY k.created_at DESC LIMIT 1) k ON TRUE \
         WHERE c.accepts_marketing AND c.email_verified AND k.created_at IS NOT NULL AND {} \
           AND ($1::text IS NULL OR lower(COALESCE(c.locale, $2)) LIKE lower($1) || '%')",
        audience.condition()
    )
}

/// How many customers an export would hold
pub async fn count(
    pool: &PgPool,
    audience: Audience,
    lang: Option<&str>,
    default_locale: &str,
) -> Result<i64> {
    let query = list_query(audience, "COUNT(*)");
    Ok(sqlx::query_scalar(&query)
        .bind(lang)
        .bind(default_locale)
        .fetch_one(pool)
        .await?)
}

/// The customers an export holds, by e-mail
pub async fn recipients(
    pool: &PgPool,
    audience: Audience,
    lang: Option<&str>,
    default_locale: &str,
) -> Result<Vec<Recipient>> {
    let select = "c.id, c.email, c.first_name, c.last_name, c.locale, co.name AS company, \
                  k.created_at AS consented_at, k.source AS consent_source";
    let query = format!("{} ORDER BY lower(c.email)", list_query(audience, select));
    Ok(sqlx::query_as::<_, Recipient>(&query)
        .bind(lang)
        .bind(default_locale)
        .fetch_all(pool)
        .await?)
}

/// The e-mail list as CSV (cells guarded against spreadsheet formulas): e-mail,
/// names, language, retail or trade, company, consent date and source, and the
/// row's unsubscribe link (`unsubscribe_url` of the customer id)
pub fn recipients_csv(
    recipients: &[Recipient],
    default_locale: &str,
    unsubscribe_url: impl Fn(Uuid) -> Result<String>,
) -> Result<String> {
    let header = [
        "email",
        "first_name",
        "last_name",
        "language",
        "audience",
        "company",
        "consented_at",
        "consent_source",
        "unsubscribe_url",
    ];
    let mut rows = Vec::with_capacity(recipients.len());
    for r in recipients {
        rows.push(vec![
            r.email.clone(),
            r.first_name.clone(),
            r.last_name.clone(),
            r.locale
                .clone()
                .unwrap_or_else(|| default_locale.to_string()),
            if r.company.is_some() {
                "trade"
            } else {
                "retail"
            }
            .to_string(),
            r.company.clone().unwrap_or_default(),
            r.consented_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
            r.consent_source.clone().unwrap_or_default(),
            unsubscribe_url(r.id)?,
        ]);
    }
    // A plain address (`-sales@example.com`) stays as it is: no formula can be made of
    // letters, digits and `.`, `_`, `+`, `-`; anything else is guarded like every cell
    super::analytics::export::write_csv_keeping(&header, rows, |column, cell| {
        column == 0 && plain_address(cell)
    })
}

/// Letters, digits, `.`, `_`, `+`, `-` and one `@`
fn plain_address(cell: &str) -> bool {
    cell.matches('@').count() == 1
        && cell
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-' | '@'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsubscribe_tokens_name_their_customer_and_cannot_be_forged() {
        let id = Uuid::new_v4();
        let token = unsubscribe_token(b"secret-one", id).unwrap();
        assert_eq!(unsubscribe_customer(b"secret-one", &token), Some(id));
        assert_eq!(unsubscribe_customer(b"secret-two", &token), None);
        let other = Uuid::new_v4();
        let (_, sig) = token.split_once('.').unwrap();
        assert_eq!(
            unsubscribe_customer(b"secret-one", &format!("{other}.{sig}")),
            None
        );
        for bad in [
            "",
            "x",
            "not-a-uuid.abc",
            &format!("{id}."),
            &format!("{id}.!!"),
        ] {
            assert_eq!(unsubscribe_customer(b"secret-one", bad), None, "{bad}");
        }
    }

    #[test]
    fn plain_addresses_stay_as_they_are_in_the_list() {
        let at = chrono::Utc::now();
        let row = |email: &str| Recipient {
            id: Uuid::new_v4(),
            email: email.to_string(),
            first_name: "=1+2".to_string(),
            last_name: "Lee".to_string(),
            locale: None,
            company: None,
            consented_at: Some(at),
            consent_source: Some("account".to_string()),
        };
        let list = [row("-sales@example.com"), row("=cmd|'/c'!A0@example.com")];
        let csv = recipients_csv(&list, "fr", |_| Ok("u".to_string())).unwrap();
        assert!(csv.contains("\n-sales@example.com,'=1+2,"), "{csv}");
        assert!(csv.contains("'=cmd|"), "{csv}");
    }

    #[test]
    fn audiences_parse() {
        assert_eq!(Audience::parse(None).unwrap(), Audience::All);
        assert_eq!(Audience::parse(Some("trade")).unwrap(), Audience::Trade);
        assert!(Audience::parse(Some("everyone")).is_err());
    }
}
