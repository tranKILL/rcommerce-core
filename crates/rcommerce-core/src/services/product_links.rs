//! Documents and pages about a product: technical sheets, declarations of
//! performance, safety data sheets, manuals, the maker's page, videos. Each
//! link has a kind, an address (http or https), an optional language and
//! label. Shoppers see the links in their language first, then the others;
//! links the link checker found broken are left out.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::locale::normalize_locale;
use crate::{Error, Result};

/// Kinds of product links
pub const KINDS: [&str; 7] = [
    "technical_sheet",
    "performance_declaration",
    "safety_sheet",
    "manual",
    "maker_page",
    "video",
    "other",
];
/// Most links on one product
const MAX_LINKS: usize = 30;

/// A link as staff enter it
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkInput {
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// A stored link
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ProductLink {
    pub id: Uuid,
    pub kind: String,
    pub url: String,
    pub locale: Option<String>,
    pub label: Option<String>,
    pub position: i32,
}

/// An outside address a link may point to: http or https with a host, at
/// most 2000 characters, nothing that could break out of an attribute
pub fn outside_address(url: &str) -> bool {
    url.len() <= 2000
        && !url.contains(['"', '<', '>', '\\', ' ', '\t', '\n', '\r'])
        && ["http://", "https://"].iter().any(|p| {
            url.strip_prefix(p)
                .is_some_and(|rest| !rest.starts_with('/'))
        })
        && url::Url::parse(url).is_ok_and(|u| u.host_str().is_some())
}

/// The links to store: trimmed, checked, in order
pub fn check(links: &[LinkInput]) -> Result<Vec<LinkInput>> {
    if links.len() > MAX_LINKS {
        return Err(Error::validation(format!(
            "a product has at most {MAX_LINKS} links"
        )));
    }
    links
        .iter()
        .map(|l| {
            if !KINDS.contains(&l.kind.as_str()) {
                return Err(Error::validation(format!(
                    "kind must be one of {}",
                    KINDS.join(", ")
                )));
            }
            let url = l.url.trim().to_string();
            if !outside_address(&url) {
                return Err(Error::validation(format!(
                    "{url} is not an http or https address"
                )));
            }
            let label = l
                .label
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            if label.as_deref().is_some_and(|t| t.chars().count() > 120) {
                return Err(Error::validation("a label has at most 120 characters"));
            }
            let locale = match l.locale.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                None => None,
                Some(raw) => Some(
                    normalize_locale(raw)
                        .ok_or_else(|| Error::validation(format!("unknown language {raw}")))?,
                ),
            };
            Ok(LinkInput {
                kind: l.kind.clone(),
                url,
                locale,
                label,
            })
        })
        .collect()
}

/// Product links
#[derive(Clone)]
pub struct ProductLinks {
    pool: PgPool,
}

impl ProductLinks {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Every link of a product, in order (staff)
    pub async fn list(&self, product: Uuid) -> Result<Vec<ProductLink>> {
        Ok(sqlx::query_as::<_, ProductLink>(
            "SELECT id, kind, url, locale, label, position FROM product_links \
             WHERE product_id = $1 ORDER BY position, created_at",
        )
        .bind(product)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Replace a product's links; `None` when there is no such product
    pub async fn replace(
        &self,
        product: Uuid,
        links: &[LinkInput],
    ) -> Result<Option<Vec<ProductLink>>> {
        let links = check(links)?;
        let mut tx = self.pool.begin().await?;
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM products WHERE id = $1)")
                .bind(product)
                .fetch_one(&mut *tx)
                .await?;
        if !exists {
            return Ok(None);
        }
        sqlx::query("DELETE FROM product_links WHERE product_id = $1")
            .bind(product)
            .execute(&mut *tx)
            .await?;
        for (position, l) in links.iter().enumerate() {
            sqlx::query(
                "INSERT INTO product_links (product_id, kind, url, locale, label, position) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(product)
            .bind(&l.kind)
            .bind(&l.url)
            .bind(&l.locale)
            .bind(&l.label)
            .bind(position as i32)
            .execute(&mut *tx)
            .await?;
        }
        // New addresses are checked on the link checker's next run
        sqlx::query(
            "INSERT INTO link_checks (url) SELECT DISTINCT url FROM product_links \
             WHERE product_id = $1 ON CONFLICT (url) DO NOTHING",
        )
        .bind(product)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(self.list(product).await?))
    }

    /// The links shoppers see (active products only): in `locale` first, then
    /// those for any language, then the others; broken ones left out
    pub async fn public(&self, product: Uuid, locale: &str) -> Result<Vec<ProductLink>> {
        Ok(sqlx::query_as::<_, ProductLink>(
            "SELECT l.id, l.kind, l.url, l.locale, l.label, l.position FROM product_links l \
             JOIN products p ON p.id = l.product_id AND p.is_active \
             LEFT JOIN link_checks c ON c.url = l.url \
             WHERE l.product_id = $1 AND COALESCE(c.state, 'pending') <> 'broken' \
             ORDER BY COALESCE(l.locale = $2, false) DESC, (l.locale IS NULL) DESC, l.position",
        )
        .bind(product)
        .bind(locale)
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(kind: &str, url: &str) -> LinkInput {
        LinkInput {
            kind: kind.into(),
            url: url.into(),
            locale: Some("fr-BE".into()),
            label: Some("  Fiche  ".into()),
        }
    }

    #[test]
    fn links_are_checked_and_tidied() {
        let ok = check(&[link("technical_sheet", " https://maker.example/sheet.pdf ")]).unwrap();
        assert_eq!(ok[0].url, "https://maker.example/sheet.pdf");
        assert_eq!(ok[0].locale.as_deref(), Some("fr"));
        assert_eq!(ok[0].label.as_deref(), Some("Fiche"));
        assert!(check(&[link("technical_sheet", "http://old-maker.example/a.pdf")]).is_ok());
        for bad in [
            "javascript:alert(1)",
            "//maker.example",
            "https:///x",
            "ftp://maker.example/a",
            "https://a.example/\"x",
        ] {
            assert!(check(&[link("manual", bad)]).is_err(), "{bad}");
        }
        assert!(check(&[link("brochure", "https://maker.example")]).is_err());
        let many: Vec<_> = (0..31)
            .map(|i| link("other", &format!("https://a.example/{i}")))
            .collect();
        assert!(check(&many).is_err());
    }
}
