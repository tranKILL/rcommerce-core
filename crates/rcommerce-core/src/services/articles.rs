//! News and building guides (wiki). An article has a kind (`news` or `wiki`),
//! an optional category of that kind, a slug shared by every language, a cover
//! picture, and a title, summary, body and search texts per language. Shoppers
//! see published articles from their publication time, newest first; news with
//! an end date leave the lists after it. Guides link to the products they are
//! about, and product pages list their guides.
//!
//! Bodies come from a rich text editor and are cleaned on save: only headings,
//! paragraphs, emphasis, lists, quotes, tables, links (site pages, https,
//! mailto:, tel:) and pictures (https or uploaded `/media/` files) remain.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::common::locale::normalize_locale;
use crate::services::content_rules::link_address;
use crate::{Error, Result};

/// Longest body accepted from the editor, before cleaning
const MAX_RAW_BODY: usize = 300_000;
/// Longest body stored, after cleaning
const MAX_BODY: usize = 200_000;
/// Most products one guide links to
const MAX_PRODUCTS: usize = 50;

/// Text of an article in one language
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArticleText {
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub body_html: String,
    #[serde(default)]
    pub seo_title: Option<String>,
    #[serde(default)]
    pub seo_description: Option<String>,
}

/// An article as staff write it
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArticleInput {
    pub kind: String,
    #[serde(default)]
    pub category_id: Option<Uuid>,
    pub slug: String,
    #[serde(default)]
    pub cover_image_url: Option<String>,
    #[serde(default)]
    pub is_published: bool,
    #[serde(default)]
    pub published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// Locale -> texts; at least one
    pub texts: BTreeMap<String, ArticleText>,
    /// Products the article is about, in order
    #[serde(default)]
    pub product_ids: Vec<Uuid>,
}

/// An article as staff edit it, every language included
#[derive(Debug, Clone, Serialize)]
pub struct AdminArticle {
    pub id: Uuid,
    pub kind: String,
    pub category_id: Option<Uuid>,
    pub slug: String,
    pub cover_image_url: Option<String>,
    pub is_published: bool,
    pub published_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub texts: BTreeMap<String, ArticleText>,
    pub product_ids: Vec<Uuid>,
    pub updated_at: DateTime<Utc>,
}

/// An article in a list (no body), in one language
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ArticleSummary {
    pub id: Uuid,
    pub kind: String,
    pub slug: String,
    pub locale: String,
    pub title: String,
    pub summary: Option<String>,
    pub cover_image_url: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub category_slug: Option<String>,
    pub category_name: Option<String>,
}

/// A product a guide links to
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct LinkedProduct {
    pub id: Uuid,
    pub slug: String,
    pub title: String,
    pub image: Option<String>,
}

/// One article to read, in one language
#[derive(Debug, Clone, Serialize)]
pub struct Article {
    #[serde(flatten)]
    pub summary: ArticleSummary,
    pub body_html: String,
    pub seo_title: Option<String>,
    pub seo_description: Option<String>,
    /// Languages the article is written in
    pub locales: Vec<String>,
    pub products: Vec<LinkedProduct>,
}

/// A category with its name in every language
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    #[serde(default)]
    pub id: Option<Uuid>,
    pub kind: String,
    pub slug: String,
    #[serde(default)]
    pub position: i32,
    /// Locale -> name
    pub names: BTreeMap<String, String>,
}

fn check_kind(kind: &str) -> Result<()> {
    if matches!(kind, "news" | "wiki") {
        Ok(())
    } else {
        Err(Error::validation("kind must be news or wiki"))
    }
}

/// Lower-case letters and digits in words joined by single hyphens
fn check_slug(slug: &str, max: usize) -> Result<()> {
    let ok = (1..=max).contains(&slug.len())
        && slug.split('-').all(|w| {
            !w.is_empty()
                && w.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        });
    if ok {
        Ok(())
    } else {
        Err(Error::validation(
            "slug must be lower-case letters and digits joined by hyphens",
        ))
    }
}

fn too_long(name: &str, text: Option<&str>, max: usize) -> Result<()> {
    if text.is_some_and(|t| t.chars().count() > max) {
        return Err(Error::validation(format!(
            "{name} is longer than {max} characters"
        )));
    }
    Ok(())
}

/// Keep what an article body may hold, and drop everything else (scripts,
/// styles, event handlers, unknown tags and addresses)
pub fn clean_html(html: &str) -> String {
    let tags: HashSet<&str> = [
        "h2",
        "h3",
        "h4",
        "p",
        "br",
        "hr",
        "strong",
        "b",
        "em",
        "i",
        "u",
        "s",
        "sub",
        "sup",
        "ul",
        "ol",
        "li",
        "blockquote",
        "a",
        "img",
        "figure",
        "figcaption",
        "table",
        "thead",
        "tbody",
        "tr",
        "th",
        "td",
    ]
    .into_iter()
    .collect();
    let attributes = std::collections::HashMap::from([
        ("a", ["href", "title"].into_iter().collect::<HashSet<_>>()),
        (
            "img",
            ["src", "alt", "title", "width", "height"]
                .into_iter()
                .collect(),
        ),
        ("th", ["colspan", "rowspan"].into_iter().collect()),
        ("td", ["colspan", "rowspan"].into_iter().collect()),
        ("ol", ["start"].into_iter().collect()),
    ]);
    ammonia::Builder::default()
        .tags(tags)
        .tag_attributes(attributes)
        .generic_attributes(HashSet::new())
        .url_schemes(["https", "mailto", "tel"].into_iter().collect())
        .url_relative(ammonia::UrlRelative::PassThrough)
        .attribute_filter(|tag, attribute, value| match (tag, attribute) {
            ("a", "href") => link_address(value).then(|| value.into()),
            ("img", "src") => crate::models::location::image_address(value).then(|| value.into()),
            _ => Some(value.into()),
        })
        .clean(html)
        .to_string()
}

/// Check an article and clean its bodies; the texts to store
fn checked(input: &ArticleInput) -> Result<BTreeMap<String, ArticleText>> {
    check_kind(&input.kind)?;
    check_slug(&input.slug, 150)?;
    if let Some(url) = input.cover_image_url.as_deref().filter(|u| !u.is_empty()) {
        if !crate::models::location::image_address(url) {
            return Err(Error::validation(
                "cover_image_url must be an https address or an uploaded /media/ file",
            ));
        }
    }
    if let (Some(from), Some(to)) = (input.published_at, input.expires_at) {
        if to <= from {
            return Err(Error::validation("expires_at must come after published_at"));
        }
    }
    if input.product_ids.len() > MAX_PRODUCTS {
        return Err(Error::validation(format!(
            "an article links to at most {MAX_PRODUCTS} products"
        )));
    }
    let mut texts = BTreeMap::new();
    for (locale, text) in &input.texts {
        let locale = normalize_locale(locale)
            .ok_or_else(|| Error::validation(format!("unknown language {locale}")))?;
        texts.insert(locale, checked_text(text)?);
    }
    if texts.is_empty() {
        return Err(Error::validation(
            "an article needs a title in one language",
        ));
    }
    Ok(texts)
}

fn checked_text(text: &ArticleText) -> Result<ArticleText> {
    let title = text.title.trim().to_string();
    if title.is_empty() {
        return Err(Error::validation("each language needs a title"));
    }
    too_long("title", Some(&title), 200)?;
    let trimmed = |t: &Option<String>| {
        t.as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    let (summary, seo_title, seo_description) = (
        trimmed(&text.summary),
        trimmed(&text.seo_title),
        trimmed(&text.seo_description),
    );
    too_long("summary", summary.as_deref(), 500)?;
    too_long("seo_title", seo_title.as_deref(), 70)?;
    too_long("seo_description", seo_description.as_deref(), 200)?;
    if text.body_html.len() > MAX_RAW_BODY {
        return Err(Error::validation("the text is too long"));
    }
    let body_html = clean_html(&text.body_html);
    if body_html.len() > MAX_BODY {
        return Err(Error::validation("the text is too long"));
    }
    Ok(ArticleText {
        title,
        summary,
        body_html,
        seo_title,
        seo_description,
    })
}

/// A text row: locale, title, summary, body, search title and description
type TextRow = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
);

/// Columns of a summary, the article as `a`, its text as `t` and its
/// category as `c` with its name as `cn`
const SUMMARY_COLUMNS: &str = "a.id, a.kind, a.slug, t.locale, t.title, t.summary, \
    a.cover_image_url, a.published_at, a.expires_at, c.slug AS category_slug, \
    cn.name AS category_name";

/// What shoppers may see now
const VISIBLE: &str = "a.is_published AND a.published_at <= NOW() \
    AND (a.expires_at IS NULL OR a.expires_at > NOW())";

/// The text of `a` in `$1` (the language asked for) else `$2` (the store's),
/// else any; and the category name the same way
fn joins() -> &'static str {
    "JOIN LATERAL (SELECT * FROM article_translations x WHERE x.article_id = a.id \
       ORDER BY (x.locale = $1) DESC, (x.locale = $2) DESC, x.locale LIMIT 1) t ON true \
     LEFT JOIN article_categories c ON c.id = a.category_id \
     LEFT JOIN LATERAL (SELECT name FROM article_category_translations y \
       WHERE y.category_id = c.id \
       ORDER BY (y.locale = $1) DESC, (y.locale = $2) DESC, y.locale LIMIT 1) cn ON true"
}

/// Staff's filter: articles of kind `$kind` (all when NULL) whose title in
/// some language holds `$q` (all when NULL)
fn admin_filter(kind: usize, q: usize) -> String {
    format!(
        "(${kind}::text IS NULL OR a.kind = ${kind}) AND (${q}::text IS NULL OR EXISTS ( \
           SELECT 1 FROM article_translations s WHERE s.article_id = a.id \
           AND s.title ILIKE '%' || ${q} || '%'))"
    )
}

/// Shoppers' filter: visible articles of kind `$kind` in category `$category`
/// (each optional), with the category joined as `c`
fn list_filter(kind: usize, category: usize) -> String {
    format!(
        "{VISIBLE} AND (${kind}::text IS NULL OR a.kind = ${kind}) \
         AND (${category}::text IS NULL OR c.slug = ${category})"
    )
}

/// A page of published articles
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub kind: Option<String>,
    pub category: Option<String>,
    pub page: i64,
    pub per_page: i64,
}

/// News and guides
#[derive(Clone)]
pub struct Articles {
    pool: PgPool,
}

impl Articles {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create an article; its id
    pub async fn create(&self, input: &ArticleInput, staff: Uuid) -> Result<Uuid> {
        let texts = checked(input)?;
        let mut tx = self.pool.begin().await?;
        self.check_links(&mut tx, input).await?;
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO articles (kind, category_id, slug, cover_image_url, is_published, \
               published_at, expires_at, author_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
        )
        .bind(&input.kind)
        .bind(input.category_id)
        .bind(&input.slug)
        .bind(cover(input))
        .bind(input.is_published)
        .bind(published_at(input))
        .bind(input.expires_at)
        .bind(staff)
        .fetch_one(&mut *tx)
        .await
        .map_err(slug_taken)?;
        write_parts(&mut tx, id, &texts, &input.product_ids).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Replace an article; `false` when there is none
    pub async fn update(&self, id: Uuid, input: &ArticleInput) -> Result<bool> {
        let texts = checked(input)?;
        let mut tx = self.pool.begin().await?;
        self.check_links(&mut tx, input).await?;
        let done = sqlx::query(
            "UPDATE articles SET kind = $2, category_id = $3, slug = $4, cover_image_url = $5, \
               is_published = $6, published_at = $7, expires_at = $8, updated_at = NOW() \
             WHERE id = $1",
        )
        .bind(id)
        .bind(&input.kind)
        .bind(input.category_id)
        .bind(&input.slug)
        .bind(cover(input))
        .bind(input.is_published)
        .bind(published_at(input))
        .bind(input.expires_at)
        .execute(&mut *tx)
        .await
        .map_err(slug_taken)?;
        if done.rows_affected() == 0 {
            return Ok(false);
        }
        write_parts(&mut tx, id, &texts, &input.product_ids).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The category must be of the article's kind, and the products must exist
    async fn check_links(&self, conn: &mut PgConnection, input: &ArticleInput) -> Result<()> {
        if let Some(category) = input.category_id {
            let kind: Option<String> =
                sqlx::query_scalar("SELECT kind FROM article_categories WHERE id = $1")
                    .bind(category)
                    .fetch_optional(&mut *conn)
                    .await?;
            if kind.as_deref() != Some(input.kind.as_str()) {
                return Err(Error::validation(
                    "category_id must be a category of the same kind",
                ));
            }
        }
        let wanted: HashSet<Uuid> = input.product_ids.iter().copied().collect();
        let found: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM products WHERE id = ANY($1)")
            .bind(wanted.iter().copied().collect::<Vec<_>>())
            .fetch_one(&mut *conn)
            .await?;
        if found != wanted.len() as i64 {
            return Err(Error::validation("product_ids holds an unknown product"));
        }
        Ok(())
    }

    /// Remove an article; `false` when there is none
    pub async fn remove(&self, id: Uuid) -> Result<bool> {
        let done = sqlx::query("DELETE FROM articles WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// An article with every language, for staff
    pub async fn admin_get(&self, id: Uuid) -> Result<Option<AdminArticle>> {
        type Row = (
            Uuid,
            String,
            Option<Uuid>,
            String,
            Option<String>,
            bool,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            DateTime<Utc>,
        );
        let row: Option<Row> = sqlx::query_as(
            "SELECT id, kind, category_id, slug, cover_image_url, is_published, published_at, \
               expires_at, updated_at FROM articles WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((
            id,
            kind,
            category_id,
            slug,
            cover_image_url,
            is_published,
            published_at,
            expires_at,
            updated_at,
        )) = row
        else {
            return Ok(None);
        };
        Ok(Some(AdminArticle {
            id,
            kind,
            category_id,
            slug,
            cover_image_url,
            is_published,
            published_at,
            expires_at,
            texts: self.texts(id).await?,
            product_ids: self.product_ids(id).await?,
            updated_at,
        }))
    }

    async fn texts(&self, id: Uuid) -> Result<BTreeMap<String, ArticleText>> {
        let rows: Vec<TextRow> = sqlx::query_as(
            "SELECT locale, title, summary, body_html, seo_title, seo_description \
                 FROM article_translations WHERE article_id = $1",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(locale, title, summary, body_html, seo_title, seo_description)| {
                    let text = ArticleText {
                        title,
                        summary,
                        body_html,
                        seo_title,
                        seo_description,
                    };
                    (locale, text)
                },
            )
            .collect())
    }

    async fn product_ids(&self, id: Uuid) -> Result<Vec<Uuid>> {
        Ok(sqlx::query_scalar(
            "SELECT product_id FROM product_articles WHERE article_id = $1 ORDER BY position",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Articles for staff, drafts included, newest first, with a title in
    /// `locale` when there is one; `q` searches the titles
    pub async fn admin_list(
        &self,
        kind: Option<&str>,
        q: Option<&str>,
        locale: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ArticleSummary>, i64)> {
        let q = q
            .map(|q| q.replace(['%', '_'], ""))
            .filter(|q| !q.trim().is_empty());
        let rows = sqlx::query_as::<_, ArticleSummary>(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM articles a {} WHERE {} \
             ORDER BY COALESCE(a.published_at, a.updated_at) DESC LIMIT $5 OFFSET $6",
            joins(),
            admin_filter(3, 4)
        ))
        .bind(locale)
        .bind(locale)
        .bind(kind)
        .bind(q.as_deref())
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let total: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM articles a WHERE {}",
            admin_filter(1, 2)
        ))
        .bind(kind)
        .bind(q.as_deref())
        .fetch_one(&self.pool)
        .await?;
        Ok((rows, total))
    }

    /// Published articles shoppers see now, newest first, in `locale` else
    /// `fallback`; with the count of all of them
    pub async fn list(
        &self,
        query: &ListQuery,
        locale: &str,
        fallback: &str,
    ) -> Result<(Vec<ArticleSummary>, i64)> {
        let rows = sqlx::query_as::<_, ArticleSummary>(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM articles a {} WHERE {} \
             ORDER BY a.published_at DESC, a.id LIMIT $5 OFFSET $6",
            joins(),
            list_filter(3, 4)
        ))
        .bind(locale)
        .bind(fallback)
        .bind(query.kind.as_deref())
        .bind(query.category.as_deref())
        .bind(query.per_page)
        .bind((query.page - 1) * query.per_page)
        .fetch_all(&self.pool)
        .await?;
        let total: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM articles a \
             LEFT JOIN article_categories c ON c.id = a.category_id WHERE {}",
            list_filter(1, 2)
        ))
        .bind(query.kind.as_deref())
        .bind(query.category.as_deref())
        .fetch_one(&self.pool)
        .await?;
        Ok((rows, total))
    }

    /// One article shoppers may see now, by slug
    pub async fn get(&self, slug: &str, locale: &str, fallback: &str) -> Result<Option<Article>> {
        let summary = sqlx::query_as::<_, ArticleSummary>(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM articles a {} WHERE {VISIBLE} AND a.slug = $3",
            joins()
        ))
        .bind(locale)
        .bind(fallback)
        .bind(slug)
        .fetch_optional(&self.pool)
        .await?;
        let Some(summary) = summary else {
            return Ok(None);
        };
        let (body_html, seo_title, seo_description): (String, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT body_html, seo_title, seo_description FROM article_translations \
                 WHERE article_id = $1 AND locale = $2",
            )
            .bind(summary.id)
            .bind(&summary.locale)
            .fetch_one(&self.pool)
            .await?;
        let locales = sqlx::query_scalar(
            "SELECT locale FROM article_translations WHERE article_id = $1 ORDER BY locale",
        )
        .bind(summary.id)
        .fetch_all(&self.pool)
        .await?;
        let products = self.linked_products(summary.id, locale, fallback).await?;
        Ok(Some(Article {
            summary,
            body_html,
            seo_title,
            seo_description,
            locales,
            products,
        }))
    }

    /// The active products an article links to, with a title in `locale`
    async fn linked_products(
        &self,
        article: Uuid,
        locale: &str,
        fallback: &str,
    ) -> Result<Vec<LinkedProduct>> {
        Ok(sqlx::query_as::<_, LinkedProduct>(
            "SELECT p.id, COALESCE(tr.slug, p.slug) AS slug, COALESCE(tr.title, p.title) AS title, \
               (SELECT src FROM product_images i WHERE i.product_id = p.id \
                  ORDER BY i.position LIMIT 1) AS image \
             FROM product_articles pa JOIN products p ON p.id = pa.product_id AND p.is_active \
             LEFT JOIN LATERAL (SELECT slug, title FROM product_translations x \
               WHERE x.product_id = p.id AND x.locale IN ($2, $3) \
               ORDER BY (x.locale = $2) DESC LIMIT 1) tr ON true \
             WHERE pa.article_id = $1 ORDER BY pa.position",
        )
        .bind(article)
        .bind(locale)
        .bind(fallback)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The guides and news shoppers see now that link to a product
    pub async fn for_product(
        &self,
        product: Uuid,
        locale: &str,
        fallback: &str,
    ) -> Result<Vec<ArticleSummary>> {
        Ok(sqlx::query_as::<_, ArticleSummary>(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM product_articles pa JOIN articles a ON a.id = pa.article_id \
             {} WHERE pa.product_id = $3 AND {VISIBLE} ORDER BY pa.position, a.published_at DESC \
             LIMIT 20",
            joins()
        ))
        .bind(locale)
        .bind(fallback)
        .bind(product)
        .fetch_all(&self.pool)
        .await?)
    }
}

/// The cover, or `None` when left empty
fn cover(input: &ArticleInput) -> Option<&str> {
    input.cover_image_url.as_deref().filter(|u| !u.is_empty())
}

/// A published article without a time is published now
fn published_at(input: &ArticleInput) -> Option<DateTime<Utc>> {
    input
        .published_at
        .or_else(|| input.is_published.then(Utc::now))
}

fn slug_taken(e: sqlx::Error) -> Error {
    match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            Error::validation("Another article already uses this slug")
        }
        _ => e.into(),
    }
}

/// Replace an article's texts and product links
async fn write_parts(
    conn: &mut PgConnection,
    id: Uuid,
    texts: &BTreeMap<String, ArticleText>,
    products: &[Uuid],
) -> Result<()> {
    sqlx::query("DELETE FROM article_translations WHERE article_id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    for (locale, t) in texts {
        sqlx::query(
            "INSERT INTO article_translations (article_id, locale, title, summary, body_html, \
               seo_title, seo_description) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(locale)
        .bind(&t.title)
        .bind(&t.summary)
        .bind(&t.body_html)
        .bind(&t.seo_title)
        .bind(&t.seo_description)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query("DELETE FROM product_articles WHERE article_id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO product_articles (product_id, article_id, position) \
         SELECT p, $1, (ord - 1)::int FROM unnest($2::uuid[]) WITH ORDINALITY AS u(p, ord) \
         ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(products)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_keep_text_and_safe_links_only() {
        let dirty = r#"<h2 onclick="x()">Mix</h2><script>alert(1)</script>
            <p style="color:red">Use <a href="javascript:alert(1)">this</a>,
            <a href="/fr/p/ciment">cement</a> and <a href="//evil.example">that</a>.</p>
            <img src="http://insecure.example/a.jpg"><img src="/media/1.jpg" alt="Wall" onerror="x()">
            <iframe src="https://evil.example"></iframe><table><tr><td colspan="2">x</td></tr></table>"#;
        let clean = clean_html(dirty);
        for gone in [
            "script",
            "onclick",
            "style",
            "javascript",
            "evil.example",
            "iframe",
            "insecure",
            "onerror",
        ] {
            assert!(!clean.contains(gone), "{gone} left in {clean}");
        }
        for kept in [
            "<h2>Mix</h2>",
            "href=\"/fr/p/ciment\"",
            "src=\"/media/1.jpg\"",
            "alt=\"Wall\"",
            "colspan=\"2\"",
        ] {
            assert!(clean.contains(kept), "{kept} missing from {clean}");
        }
    }

    fn input() -> ArticleInput {
        ArticleInput {
            kind: "wiki".into(),
            category_id: None,
            slug: "build-a-wall".into(),
            cover_image_url: None,
            is_published: true,
            published_at: None,
            expires_at: None,
            texts: BTreeMap::from([(
                "en-GB".into(),
                ArticleText {
                    title: " Build a wall ".into(),
                    body_html: "<p>Lay blocks</p>".into(),
                    ..Default::default()
                },
            )]),
            product_ids: vec![],
        }
    }

    #[test]
    fn articles_are_checked_before_saving() {
        let texts = checked(&input()).unwrap();
        assert_eq!(texts["en"].title, "Build a wall", "locale and title tidied");
        let mut bad = input();
        bad.kind = "blog".into();
        assert!(checked(&bad).is_err());
        let mut bad = input();
        bad.slug = "Build a wall".into();
        assert!(checked(&bad).is_err());
        let mut bad = input();
        bad.cover_image_url = Some("javascript:alert(1)".into());
        assert!(checked(&bad).is_err());
        let mut bad = input();
        bad.texts.clear();
        assert!(checked(&bad).is_err());
        let mut bad = input();
        let now = Utc::now();
        (bad.published_at, bad.expires_at) = (Some(now), Some(now));
        assert!(checked(&bad).is_err(), "ends after it starts");
        let mut bad = input();
        bad.texts.get_mut("en-GB").unwrap().seo_title = Some("x".repeat(71));
        assert!(checked(&bad).is_err());
    }
}
