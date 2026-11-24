//! Checking the outside links the store shows: product documents, links in
//! article bodies and in the footer. Each address is checked once a week, and
//! every day while it fails. A link is:
//! - **broken** on 404 or 410, when its host cannot be reached (name, TLS,
//!   connection), or after three server errors or timeouts in a row;
//! - **moved** when it answers with a permanent redirect (its target is
//!   suggested as the replacement);
//! - **uncertain** on 403 or 429 (some sites refuse robots), or after a first
//!   server error or timeout;
//! - **ok** otherwise.
//!
//! Requests go only to public addresses: each host is resolved first, private,
//! loopback and link-local addresses are refused, the request is sent to the
//! address checked (no second lookup), and each redirect is checked the same
//! way. HEAD first, then a one-byte GET for sites that refuse HEAD; 10 s per
//! request, 5 redirects at most, one request per host per second.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde::Serialize;
use sqlx::PgPool;

use crate::{Error, Result};

/// What one check found
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 2xx, after temporary redirects only
    Ok(u16),
    /// A permanent redirect (301, 308) on the way, to this address
    Moved {
        status: u16,
        to: String,
    },
    /// 404 or 410
    Gone(u16),
    /// 403 or 429
    Refused(u16),
    /// 5xx (or another status no rule names)
    ServerError(u16),
    Timeout,
    /// Name, TLS or connection failure, too many redirects
    Unreachable(String),
    /// Not http(s), or a private address
    NotAllowed(String),
}

/// Who may be asked: tests also allow the loopback address of their local
/// server; everything private stays refused
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub allow_loopback: bool,
    pub timeout: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            allow_loopback: false,
            timeout: Duration::from_secs(10),
        }
    }
}

/// Whether `ip` is a public address
fn public(ip: IpAddr, allow_loopback: bool) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if v4.is_loopback() {
                return allow_loopback;
            }
            !(v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (64..128).contains(&o[1])) // carrier-grade NAT
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public(IpAddr::V4(v4), allow_loopback);
            }
            if v6.is_loopback() {
                return allow_loopback;
            }
            let first = v6.segments()[0];
            !(v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00 // unique local
                || (first & 0xffc0) == 0xfe80 // link local
                || first == 0x2001 && v6.segments()[1] == 0x0db8) // documentation
        }
    }
}

/// A public address of `url`'s host, or why not
async fn resolve(url: &reqwest::Url, policy: Policy) -> std::result::Result<SocketAddr, Outcome> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Outcome::NotAllowed(format!(
            "{} addresses are not checked",
            url.scheme()
        )));
    }
    let host = url
        .host_str()
        .ok_or_else(|| Outcome::NotAllowed("no host".into()))?;
    let port = url.port_or_known_default().unwrap_or(443);
    let found: Vec<SocketAddr> = tokio::net::lookup_host((host.trim_matches(['[', ']']), port))
        .await
        .map_err(|e| Outcome::Unreachable(format!("name lookup failed: {e}")))?
        .collect();
    if found.is_empty() {
        return Err(Outcome::Unreachable("the name has no address".into()));
    }
    // Every address must be public: a name may mix public and private ones
    if let Some(bad) = found
        .iter()
        .find(|a| !public(a.ip(), policy.allow_loopback))
    {
        return Err(Outcome::NotAllowed(format!(
            "{} is not a public address",
            bad.ip()
        )));
    }
    Ok(found[0])
}

/// One request to `url`, sent to the address checked; no redirects followed
async fn request(
    url: &reqwest::Url,
    addr: SocketAddr,
    method: reqwest::Method,
    policy: Policy,
) -> std::result::Result<reqwest::Response, Outcome> {
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_matches(['[', ']'])
        .to_string();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(policy.timeout)
        .user_agent("Mozilla/5.0 (compatible; StoreLinkCheck/1.0)")
        .resolve(&host, addr)
        .build()
        .map_err(|e| Outcome::Unreachable(e.to_string()))?;
    let mut builder = client.request(method.clone(), url.clone());
    if method == reqwest::Method::GET {
        builder = builder.header(reqwest::header::RANGE, "bytes=0-0");
    }
    builder.send().await.map_err(|e| {
        if e.is_timeout() {
            Outcome::Timeout
        } else {
            Outcome::Unreachable(e.to_string())
        }
    })
}

/// HEAD, then a one-byte GET when the site refuses HEAD
async fn probe(
    url: &reqwest::Url,
    policy: Policy,
) -> std::result::Result<reqwest::Response, Outcome> {
    let addr = resolve(url, policy).await?;
    let head = request(url, addr, reqwest::Method::HEAD, policy).await?;
    if matches!(head.status().as_u16(), 403 | 405 | 501) {
        return request(url, addr, reqwest::Method::GET, policy).await;
    }
    Ok(head)
}

/// Check one address
pub async fn check(url: &str, policy: Policy) -> Outcome {
    let Ok(mut current) = reqwest::Url::parse(url) else {
        return Outcome::NotAllowed("not an address".into());
    };
    let mut moved: Option<u16> = None;
    for _ in 0..=5 {
        let response = match probe(&current, policy).await {
            Ok(r) => r,
            Err(outcome) => return outcome,
        };
        let status = response.status();
        if !status.is_redirection() {
            return verdict(status, moved, &current);
        }
        let Some(next) = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|l| l.to_str().ok())
            .and_then(|l| current.join(l).ok())
        else {
            return Outcome::ServerError(status.as_u16());
        };
        if moved.is_none() && matches!(status.as_u16(), 301 | 308) {
            moved = Some(status.as_u16());
        }
        current = next;
    }
    Outcome::Unreachable("more than 5 redirects".into())
}

/// The outcome of the final answer (`moved`: a permanent redirect on the way)
fn verdict(status: StatusCode, moved: Option<u16>, at: &reqwest::Url) -> Outcome {
    let code = status.as_u16();
    match code {
        200..=299 => match moved {
            Some(status) => Outcome::Moved {
                status,
                to: at.to_string(),
            },
            None => Outcome::Ok(code),
        },
        404 | 410 => Outcome::Gone(code),
        403 | 429 => Outcome::Refused(code),
        _ => Outcome::ServerError(code),
    }
}

/// The state to store after `outcome`, given the failures in a row before it
pub fn next_state(
    outcome: &Outcome,
    failures_before: i32,
) -> (&'static str, i32, chrono::Duration) {
    let week = chrono::Duration::days(7);
    let day = chrono::Duration::days(1);
    match outcome {
        Outcome::Ok(_) => ("ok", 0, week),
        Outcome::Moved { .. } => ("moved", 0, week),
        Outcome::Gone(_) | Outcome::Unreachable(_) | Outcome::NotAllowed(_) => {
            ("broken", failures_before + 1, day)
        }
        Outcome::Refused(_) => ("uncertain", failures_before, day),
        Outcome::ServerError(_) | Outcome::Timeout => {
            let failures = failures_before + 1;
            (
                if failures >= 3 { "broken" } else { "uncertain" },
                failures,
                day,
            )
        }
    }
}

fn status_of(outcome: &Outcome) -> Option<i32> {
    match outcome {
        Outcome::Ok(s) | Outcome::Gone(s) | Outcome::Refused(s) | Outcome::ServerError(s) => {
            Some(i32::from(*s))
        }
        Outcome::Moved { status, .. } => Some(i32::from(*status)),
        _ => None,
    }
}

fn error_of(outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::Unreachable(e) | Outcome::NotAllowed(e) => Some(e.chars().take(500).collect()),
        Outcome::Timeout => Some("no answer within the time limit".into()),
        _ => None,
    }
}

/// Addresses in `<a href="...">` of an article body
fn body_links(html: &str) -> Vec<String> {
    let re = regex::Regex::new(r#"href="(https?://[^"]+)""#).expect("valid pattern");
    re.captures_iter(html)
        .map(|c| c[1].replace("&amp;", "&"))
        .collect()
}

/// Addresses of `Label | https://...` lines (footer links)
fn line_links(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.split_once('|').map(|(_, a)| a.trim().to_string()))
        .filter(|a| a.starts_with("https://") || a.starts_with("http://"))
        .collect()
}

/// A place a link is shown
#[derive(Debug, Clone, Serialize)]
pub struct UsedIn {
    /// `product`, `article` or `footer`
    pub kind: String,
    pub id: Option<uuid::Uuid>,
    pub title: String,
}

/// A link that needs attention, with where it is shown
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CheckedLink {
    pub url: String,
    pub state: String,
    pub status: Option<i32>,
    pub error: Option<String>,
    pub final_url: Option<String>,
    pub consecutive_failures: i32,
    pub failing_since: Option<DateTime<Utc>>,
    pub last_checked_at: Option<DateTime<Utc>>,
    #[sqlx(skip)]
    pub used_in: Vec<UsedIn>,
}

/// The link checker's records
#[derive(Clone)]
pub struct LinkChecks {
    pool: PgPool,
}

impl LinkChecks {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Every outside address the store shows now
    async fn addresses(&self) -> Result<Vec<String>> {
        let mut found: Vec<String> = sqlx::query_scalar("SELECT DISTINCT url FROM product_links")
            .fetch_all(&self.pool)
            .await?;
        let bodies: Vec<String> = sqlx::query_scalar("SELECT body_html FROM article_translations")
            .fetch_all(&self.pool)
            .await?;
        found.extend(bodies.iter().flat_map(|b| body_links(b)));
        let footers: Vec<sqlx::types::Json<std::collections::BTreeMap<String, String>>> =
            sqlx::query_scalar("SELECT content FROM content_blocks WHERE key = 'footer'")
                .fetch_all(&self.pool)
                .await?;
        found.extend(
            footers
                .iter()
                .flat_map(|f| f.0.values().flat_map(|t| line_links(t)).collect::<Vec<_>>()),
        );
        found.sort();
        found.dedup();
        Ok(found)
    }

    /// Add the addresses shown now (to check) and forget those no longer shown
    pub async fn refresh(&self) -> Result<usize> {
        let found = self.addresses().await?;
        sqlx::query(
            "INSERT INTO link_checks (url) SELECT unnest($1::text[]) ON CONFLICT (url) DO NOTHING",
        )
        .bind(&found)
        .execute(&self.pool)
        .await?;
        sqlx::query("DELETE FROM link_checks WHERE NOT (url = ANY($1))")
            .bind(&found)
            .execute(&self.pool)
            .await?;
        Ok(found.len())
    }

    /// Store what a check found
    pub async fn record(&self, url: &str, outcome: &Outcome) -> Result<()> {
        let before: i32 =
            sqlx::query_scalar("SELECT consecutive_failures FROM link_checks WHERE url = $1")
                .bind(url)
                .fetch_optional(&self.pool)
                .await?
                .unwrap_or(0);
        let (state, failures, wait) = next_state(outcome, before);
        let final_url = match outcome {
            Outcome::Moved { to, .. } => Some(to.clone()),
            _ => None,
        };
        sqlx::query(
            "INSERT INTO link_checks (url, state, status, error, final_url, consecutive_failures, \
               failing_since, ok_at, last_checked_at, next_check_at) \
             VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $6 > 0 THEN NOW() END, \
               CASE WHEN $2 = 'ok' THEN NOW() END, NOW(), NOW() + $7) \
             ON CONFLICT (url) DO UPDATE SET state = $2, status = $3, error = $4, final_url = $5, \
               consecutive_failures = $6, \
               failing_since = CASE WHEN $6 = 0 THEN NULL \
                 ELSE COALESCE(link_checks.failing_since, NOW()) END, \
               ok_at = CASE WHEN $2 = 'ok' THEN NOW() ELSE link_checks.ok_at END, \
               last_checked_at = NOW(), next_check_at = NOW() + $7",
        )
        .bind(url)
        .bind(state)
        .bind(status_of(outcome))
        .bind(error_of(outcome))
        .bind(final_url)
        .bind(failures)
        .bind(wait)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Check the addresses that are due, one request per host per second;
    /// how many were checked
    pub async fn run(&self, limit: i64, policy: Policy) -> Result<usize> {
        self.refresh().await?;
        let due: Vec<String> = sqlx::query_scalar(
            "SELECT url FROM link_checks WHERE next_check_at <= NOW() ORDER BY next_check_at LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut last_by_host: HashMap<String, Instant> = HashMap::new();
        for url in &due {
            let host = reqwest::Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_default();
            if let Some(at) = last_by_host.get(&host) {
                let wait = Duration::from_secs(1).saturating_sub(at.elapsed());
                tokio::time::sleep(wait).await;
            }
            let outcome = check(url, policy).await;
            last_by_host.insert(host, Instant::now());
            self.record(url, &outcome).await?;
        }
        Ok(due.len())
    }

    /// Links in a state (`broken`, `moved`, `uncertain`), with where they are shown
    pub async fn report(&self, state: &str) -> Result<Vec<CheckedLink>> {
        if !matches!(state, "broken" | "moved" | "uncertain" | "pending" | "ok") {
            return Err(Error::validation(
                "state must be broken, moved, uncertain, pending or ok",
            ));
        }
        let mut rows = sqlx::query_as::<_, CheckedLink>(
            "SELECT url, state, status, error, final_url, consecutive_failures, failing_since, \
               last_checked_at FROM link_checks WHERE state = $1 ORDER BY failing_since NULLS LAST, url \
             LIMIT 500",
        )
        .bind(state)
        .fetch_all(&self.pool)
        .await?;
        for row in &mut rows {
            row.used_in = self.used_in(&row.url).await?;
        }
        Ok(rows)
    }

    /// Where an address is shown
    pub async fn used_in(&self, url: &str) -> Result<Vec<UsedIn>> {
        let products: Vec<(uuid::Uuid, String)> = sqlx::query_as(
            "SELECT DISTINCT p.id, p.title FROM product_links l JOIN products p ON p.id = l.product_id \
             WHERE l.url = $1 LIMIT 50",
        )
        .bind(url)
        .fetch_all(&self.pool)
        .await?;
        let attribute = format!("href=\"{}\"", url.replace('&', "&amp;"));
        let articles: Vec<(uuid::Uuid, String)> = sqlx::query_as(
            "SELECT DISTINCT ON (t.article_id) t.article_id, t.title FROM article_translations t \
             WHERE strpos(t.body_html, $1) > 0 ORDER BY t.article_id LIMIT 50",
        )
        .bind(&attribute)
        .fetch_all(&self.pool)
        .await?;
        let footers: Vec<String> = sqlx::query_scalar(
            "SELECT locale FROM content_blocks WHERE key = 'footer' AND strpos(content::text, $1) > 0",
        )
        .bind(url)
        .fetch_all(&self.pool)
        .await?;
        let mut used: Vec<UsedIn> = products
            .into_iter()
            .map(|(id, title)| UsedIn {
                kind: "product".into(),
                id: Some(id),
                title,
            })
            .collect();
        used.extend(articles.into_iter().map(|(id, title)| UsedIn {
            kind: "article".into(),
            id: Some(id),
            title,
        }));
        used.extend(footers.into_iter().map(|l| UsedIn {
            kind: "footer".into(),
            id: None,
            title: l,
        }));
        Ok(used)
    }

    /// Replace an address everywhere it is shown (product links, article
    /// bodies, footers); how many places changed
    pub async fn replace(&self, old: &str, new: &str) -> Result<u64> {
        if !crate::services::content_rules::link_address(new) || !new.starts_with("https://") {
            return Err(Error::validation(
                "the new address must be an https address",
            ));
        }
        let mut tx = self.pool.begin().await?;
        let products = sqlx::query("UPDATE product_links SET url = $2 WHERE url = $1")
            .bind(old)
            .bind(new)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let (old_attr, new_attr) = (
            format!("href=\"{}\"", old.replace('&', "&amp;")),
            format!("href=\"{}\"", new.replace('&', "&amp;")),
        );
        let articles = sqlx::query(
            "UPDATE article_translations SET body_html = replace(body_html, $1, $2) \
             WHERE strpos(body_html, $1) > 0",
        )
        .bind(&old_attr)
        .bind(&new_attr)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let footers = sqlx::query(
            "UPDATE content_blocks SET content = replace(content::text, $1, $2)::jsonb, updated_at = NOW() \
             WHERE key = 'footer' AND strpos(content::text, $1) > 0",
        )
        .bind(format!("| {old}"))
        .bind(format!("| {new}"))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query("DELETE FROM link_checks WHERE url = $1")
            .bind(old)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO link_checks (url) VALUES ($1) ON CONFLICT (url) DO NOTHING")
            .bind(new)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(products + articles + footers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn only_public_addresses_are_asked() {
        for private in [
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "127.0.0.1",
        ] {
            let ip: Ipv4Addr = private.parse().unwrap();
            assert!(!public(IpAddr::V4(ip), false), "{private}");
        }
        assert!(public(IpAddr::V4("93.184.216.34".parse().unwrap()), false));
        assert!(
            public(IpAddr::V4(Ipv4Addr::LOCALHOST), true),
            "tests reach their local server"
        );
        for private in ["::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1", "::"] {
            let ip: Ipv6Addr = private.parse().unwrap();
            assert!(!public(IpAddr::V6(ip), false), "{private}");
        }
        assert!(public(
            IpAddr::V6("2606:4700::1111".parse().unwrap()),
            false
        ));
    }

    #[test]
    fn failures_turn_broken_by_their_kind() {
        assert_eq!(next_state(&Outcome::Ok(200), 2).0, "ok");
        assert_eq!(next_state(&Outcome::Gone(404), 0).0, "broken");
        assert_eq!(next_state(&Outcome::Refused(403), 0).0, "uncertain");
        assert_eq!(
            next_state(&Outcome::ServerError(503), 0),
            ("uncertain", 1, chrono::Duration::days(1))
        );
        assert_eq!(
            next_state(&Outcome::Timeout, 2).0,
            "broken",
            "third failure in a row"
        );
        let moved = Outcome::Moved {
            status: 301,
            to: "https://new.example/".into(),
        };
        assert_eq!(
            next_state(&moved, 0),
            ("moved", 0, chrono::Duration::days(7))
        );
    }

    #[test]
    fn links_are_found_in_bodies_and_footers() {
        let body =
            r#"<p><a href="https://maker.example/a?x=1&amp;y=2">x</a> <a href="/fr/p/x">y</a></p>"#;
        assert_eq!(body_links(body), ["https://maker.example/a?x=1&y=2"]);
        let footer = "Terms | https://shop.example/terms\nBranches | /fr/depots";
        assert_eq!(line_links(footer), ["https://shop.example/terms"]);
    }
}
