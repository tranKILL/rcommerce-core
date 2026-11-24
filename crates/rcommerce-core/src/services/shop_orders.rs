//! Web orders sent to the shop that prepares them.
//!
//! Each shop (inventory location) keeps a list of addresses that receive its
//! web orders: staff members, or any address such as an e-mail printer's. A
//! job picks up the paid orders not yet sent (`metadata.shop_notified`) and
//! mails each recipient the order with its slip as a PDF; an order whose shop
//! has nobody listed goes to the store's operator address, unless the operator
//! was already told. Orders older than a day are left alone, so turning the
//! feature on does not mail old orders.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::invoice::slip::{OrderSlip, SlipFormat, SlipLine, SlipMode};
use crate::{Error, Result};

/// Most addresses a shop may list
pub const MAX_RECIPIENTS: usize = 20;
/// Orders picked up per run
pub const BATCH: i64 = 50;

/// An address that receives a shop's web orders
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Recipient {
    pub id: Uuid,
    pub email: String,
    /// The staff member, when picked from the staff list
    pub customer_id: Option<Uuid>,
    /// Language of the e-mails (`None`: the store's)
    pub locale: Option<String>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

/// One address as staff set it
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewRecipient {
    pub email: String,
    #[serde(default)]
    pub customer_id: Option<Uuid>,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default = "yes")]
    pub active: bool,
}

fn yes() -> bool {
    true
}

/// An address mail can go to: valid, at most 254 characters, a domain name
/// with a dot (no IP literal such as `x@[10.0.0.5]`, no bare host name)
fn deliverable(email: &str) -> bool {
    let domain = email.rsplit_once('@').map(|(_, d)| d).unwrap_or_default();
    validator::validate_email(email)
        && email.len() <= 254
        && domain.contains('.')
        && !domain.starts_with('[')
        && !domain.ends_with('.')
}

/// The addresses checked and normalised: valid e-mails, once each, a known
/// language, at most [`MAX_RECIPIENTS`]
pub fn check(list: &[NewRecipient]) -> Result<Vec<NewRecipient>> {
    if list.len() > MAX_RECIPIENTS {
        return Err(Error::validation(format!(
            "A shop lists at most {MAX_RECIPIENTS} addresses"
        )));
    }
    let mut out: Vec<NewRecipient> = Vec::new();
    for r in list {
        let email = r.email.trim().to_lowercase();
        if !deliverable(&email) {
            return Err(Error::validation(format!(
                "Invalid e-mail address: {email}"
            )));
        }
        if out.iter().any(|o| o.email == email) {
            return Err(Error::validation(format!("{email} is listed twice")));
        }
        let locale = match r.locale.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
            None => None,
            Some(l) => Some(
                crate::common::locale::normalize_locale(l)
                    .filter(|code| crate::notification::i18n::available_locales().contains(code))
                    .ok_or_else(|| Error::validation(format!("Unknown language: {l}")))?,
            ),
        };
        out.push(NewRecipient {
            email,
            customer_id: r.customer_id,
            locale,
            active: r.active,
        });
    }
    Ok(out)
}

/// A shop's addresses, in the order they were added
pub async fn recipients(pool: &PgPool, location: Uuid) -> Result<Vec<Recipient>> {
    Ok(sqlx::query_as(
        "SELECT id, email, customer_id, locale, active, created_at \
         FROM location_order_recipients WHERE location_id = $1 ORDER BY created_at, email",
    )
    .bind(location)
    .fetch_all(pool)
    .await?)
}

/// Replace a shop's addresses with `list` (already [`check`]ed), each row
/// recording `actor` as who set it; `None` when the shop does not exist
pub async fn replace(
    pool: &PgPool,
    location: Uuid,
    list: &[NewRecipient],
    actor: Uuid,
) -> Result<Option<Vec<Recipient>>> {
    let accounts: Vec<Uuid> = list.iter().filter_map(|r| r.customer_id).collect();
    let known: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM customers WHERE id = ANY($1)")
        .bind(&accounts)
        .fetch_one(pool)
        .await?;
    if known != accounts.len() as i64 {
        return Err(Error::validation("An address names an unknown account"));
    }
    let mut tx = pool.begin().await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM inventory_locations WHERE id = $1)")
            .bind(location)
            .fetch_one(&mut *tx)
            .await?;
    if !exists {
        return Ok(None);
    }
    sqlx::query("DELETE FROM location_order_recipients WHERE location_id = $1")
        .bind(location)
        .execute(&mut *tx)
        .await?;
    for r in list {
        sqlx::query(
            "INSERT INTO location_order_recipients \
               (location_id, email, customer_id, locale, active, added_by) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(location)
        .bind(&r.email)
        .bind(r.customer_id)
        .bind(&r.locale)
        .bind(r.active)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Some(recipients(pool, location).await?))
}

/// A paid web order its shop has not been sent yet
#[derive(Debug, Clone, PartialEq)]
pub struct PendingOrder {
    pub id: Uuid,
    pub location_id: Option<Uuid>,
    /// The operator already got the new-order e-mail
    pub admin_notified: bool,
}

/// Statuses of an order whose payment is confirmed
const PAID: &str = "('confirmed', 'processing', 'shipped', 'delivered', 'completed')";

/// SQL condition, on `orders`, of a sale at the till: marked once sold, and
/// recognised before that by its basket (`session_token` `pos:<session>`).
/// Its goods leave at the counter, so its shop is not sent it. Never NULL, so
/// `NOT` it keeps the web orders (which have no `source`).
pub const TILL_SALE: &str = "(COALESCE(orders.metadata->>'source' = 'pos', FALSE) OR EXISTS ( \
     SELECT 1 FROM carts c WHERE c.id::text = orders.metadata->>'cart_id' \
       AND c.session_token LIKE 'pos:%'))";

/// Most characters of the customer's notes printed on a slip
pub const NOTES_MAX: usize = 500;

/// The notes as printed: trimmed, at most [`NOTES_MAX`] characters
fn printed_notes(notes: Option<String>) -> Option<String> {
    let notes = notes?.trim().to_string();
    if notes.is_empty() {
        return None;
    }
    if notes.chars().count() <= NOTES_MAX {
        return Some(notes);
    }
    Some(notes.chars().take(NOTES_MAX).collect::<String>() + "…")
}

/// Claim the right to send `location` a test e-mail: at most one a minute per
/// shop. False when one went out in the last minute (or no such shop).
pub async fn claim_test(pool: &PgPool, location: Uuid) -> Result<bool> {
    let claimed: Option<Uuid> = sqlx::query_scalar(
        "UPDATE inventory_locations SET order_test_sent_at = NOW() WHERE id = $1 \
           AND (order_test_sent_at IS NULL OR order_test_sent_at < NOW() - INTERVAL '1 minute') \
         RETURNING id",
    )
    .bind(location)
    .fetch_optional(pool)
    .await?;
    Ok(claimed.is_some())
}

/// The orders to send, oldest first
pub async fn pending(pool: &PgPool, limit: i64) -> Result<Vec<PendingOrder>> {
    let rows = sqlx::query(&format!(
        "SELECT id, metadata->>'fulfillment_location_id' AS location, \
                COALESCE(metadata->>'admin_notified', 'false') = 'true' AS admin_notified \
         FROM orders WHERE NOT draft AND status::text IN {PAID} AND NOT {TILL_SALE} \
           AND metadata->>'shop_notified' IS NULL AND created_at > NOW() - INTERVAL '1 day' \
         ORDER BY created_at LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| PendingOrder {
            id: row.get("id"),
            location_id: row
                .get::<Option<String>, _>("location")
                .and_then(|l| l.parse().ok()),
            admin_notified: row.get("admin_notified"),
        })
        .collect())
}

/// Record that the order's shop was sent it (`recipients`: how many
/// addresses; 0 when it went to the operator)
pub async fn mark_sent(pool: &PgPool, order: Uuid, recipients: usize) -> Result<()> {
    sqlx::query(
        "UPDATE orders SET metadata = jsonb_set(CASE WHEN jsonb_typeof(metadata) = 'object' \
           THEN metadata ELSE '{}'::jsonb END, '{shop_notified}', to_jsonb(NOW())) WHERE id = $1",
    )
    .bind(order)
    .execute(pool)
    .await?;
    let detail = serde_json::json!({ "recipients": recipients });
    crate::services::order_events::record_quietly(
        pool,
        order,
        "shop_notified",
        crate::services::order_events::Actor::System,
        detail,
    )
    .await;
    Ok(())
}

/// A shop's name, time zone and slip paper
#[derive(Debug, Clone)]
pub struct ShopInfo {
    pub name: String,
    pub timezone: chrono_tz::Tz,
    pub format: SlipFormat,
}

/// The shop `location`, if it exists
pub async fn shop(pool: &PgPool, location: Uuid) -> Result<Option<ShopInfo>> {
    let row: Option<(String, String, String)> =
        sqlx::query_as("SELECT name, timezone, slip_format FROM inventory_locations WHERE id = $1")
            .bind(location)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(name, timezone, format)| ShopInfo {
        name,
        timezone: timezone.parse().unwrap_or(chrono_tz::Europe::Brussels),
        format: SlipFormat::parse(&format).unwrap_or_default(),
    }))
}

/// Text at `key` of a JSON object, when not empty
fn field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Name, e-mail, phone and company of the customer from the address snapshot
fn customer_lines(address: &serde_json::Value, email: &str) -> Vec<String> {
    let name = [field(address, "first_name"), field(address, "last_name")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    [
        Some(name).filter(|n| !n.is_empty()),
        Some(email.to_string()),
        field(address, "phone"),
        field(address, "company"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The street, town and country lines of an address snapshot
fn address_lines(address: &serde_json::Value) -> Vec<String> {
    let town = [
        field(address, "zip").or_else(|| field(address, "postcode")),
        field(address, "city"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    [
        field(address, "address1").or_else(|| field(address, "address_1")),
        field(address, "address2"),
        Some(town).filter(|t| !t.is_empty()),
        field(address, "country"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Pickup at the shop (with the ready time the customer was told) or delivery
fn mode(metadata: &serde_json::Value) -> SlipMode {
    match metadata.get("pickup") {
        Some(pickup) if pickup.is_object() => SlipMode::Pickup {
            location: field(pickup, "location_name").unwrap_or_default(),
            ready_at: field(pickup, "ready_at").and_then(|at| at.parse().ok()),
        },
        _ => SlipMode::Delivery {
            service: field(metadata, "shipping_service_name")
                .or_else(|| field(metadata, "shipping_carrier")),
        },
    }
}

/// An order line as the slip reads it: quantity, title, variant, SKU, bundle part
type LineRow = (i32, String, Option<String>, Option<String>, Option<bool>);

/// The lines to prepare; a bundle's parts follow it, marked with a dash
async fn slip_lines(pool: &PgPool, order: Uuid) -> Result<Vec<SlipLine>> {
    let rows: Vec<LineRow> = sqlx::query_as(
        "SELECT quantity, title, variant_title, sku, is_bundle_component FROM order_items \
         WHERE order_id = $1 ORDER BY created_at, id",
    )
    .bind(order)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(quantity, name, variant, sku, part)| SlipLine {
            quantity,
            name: if part == Some(true) {
                format!("– {name}")
            } else {
                name
            },
            variant,
            sku,
        })
        .collect())
}

/// The slip of `order` (`None`: unknown order); `store_name` heads it
pub async fn slip(
    pool: &PgPool,
    order: Uuid,
    store_name: &str,
) -> Result<Option<(OrderSlip, Option<Uuid>)>> {
    let row = sqlx::query(
        "SELECT order_number, created_at, payment_status::text AS paid, test, total, currency, notes, \
                customer_email, COALESCE(shipping_address, '{}'::jsonb) AS address, \
                COALESCE(metadata, '{}'::jsonb) AS metadata \
         FROM orders WHERE id = $1",
    )
    .bind(order)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let metadata: serde_json::Value = row.get("metadata");
    let address: serde_json::Value = row.get("address");
    let location: Option<Uuid> =
        field(&metadata, "fulfillment_location_id").and_then(|l| l.parse().ok());
    let timezone = match location {
        Some(id) => shop(pool, id).await?.map(|s| s.timezone),
        None => None,
    };
    let mode = mode(&metadata);
    let delivery = matches!(mode, SlipMode::Delivery { .. });
    let slip = OrderSlip {
        store_name: store_name.to_string(),
        order_number: row.get("order_number"),
        placed_at: row.get("created_at"),
        timezone: timezone.unwrap_or(chrono_tz::Europe::Brussels),
        mode,
        paid: row.get::<String, _>("paid") == "paid",
        test: row.get("test"),
        customer: customer_lines(&address, &row.get::<String, _>("customer_email")),
        address: if delivery {
            address_lines(&address)
        } else {
            Vec::new()
        },
        notes: printed_notes(row.get("notes")),
        lines: slip_lines(pool, order).await?,
        total: row.get::<Decimal, _>("total"),
        currency: row.get("currency"),
    };
    Ok(Some((slip, location)))
}

/// A sample slip for a test e-mail to `shop`
pub fn sample(shop: &ShopInfo, store_name: &str) -> OrderSlip {
    OrderSlip {
        store_name: store_name.to_string(),
        order_number: "TEST-0001".into(),
        placed_at: Utc::now(),
        timezone: shop.timezone,
        mode: SlipMode::Pickup {
            location: shop.name.clone(),
            ready_at: Some(Utc::now()),
        },
        paid: true,
        test: true,
        customer: vec!["A. Customer".into(), "customer@example.com".into()],
        address: Vec::new(),
        notes: None,
        lines: vec![
            SlipLine {
                quantity: 2,
                name: "Sample product".into(),
                variant: None,
                sku: Some("SAMPLE-1".into()),
            },
            SlipLine {
                quantity: 1,
                name: "Another product".into(),
                variant: Some("25 kg".into()),
                sku: None,
            },
        ],
        total: Decimal::new(4250, 2),
        currency: "EUR".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(email: &str) -> NewRecipient {
        NewRecipient {
            email: email.into(),
            customer_id: None,
            locale: Some("NL-be".into()),
            active: true,
        }
    }

    #[test]
    fn addresses_are_normalised_and_checked() {
        let list = check(&[new(" Shop@Example.com "), new("printer@print.example")]).unwrap();
        assert_eq!(list[0].email, "shop@example.com");
        assert_eq!(list[0].locale.as_deref(), Some("nl"));
        assert!(check(&[new("not an address")]).is_err());
        assert!(
            check(&[new("a@example.com"), new("A@example.com")]).is_err(),
            "twice"
        );
        let many: Vec<_> = (0..=MAX_RECIPIENTS)
            .map(|i| new(&format!("s{i}@example.com")))
            .collect();
        assert!(check(&many).is_err());
        let mut odd = new("a@example.com");
        odd.locale = Some("xx-unknown".into());
        assert!(check(&[odd]).is_err());
    }

    #[test]
    fn addresses_must_have_a_mail_domain() {
        assert!(deliverable("shop@print.example"));
        for bad in [
            "x@[10.0.0.5]",
            "x@internal-host",
            "x@example.",
            "not-an-address",
        ] {
            assert!(!deliverable(bad), "{bad}");
        }
    }

    #[test]
    fn long_notes_are_cut_on_the_slip() {
        assert_eq!(printed_notes(Some("  ".into())), None);
        assert_eq!(
            printed_notes(Some(" Call ".into())).as_deref(),
            Some("Call")
        );
        let long = printed_notes(Some("é".repeat(NOTES_MAX * 4))).unwrap();
        assert_eq!(long.chars().count(), NOTES_MAX + 1, "cut, with an ellipsis");
    }

    #[test]
    fn a_pickup_and_a_delivery_read_from_the_metadata() {
        let pickup = serde_json::json!({"pickup": {"location_name": "North", "ready_at": "2026-11-21T09:00:00Z"}});
        assert!(
            matches!(mode(&pickup), SlipMode::Pickup { ref location, ready_at: Some(_) } if location == "North")
        );
        let delivery = serde_json::json!({"shipping_carrier": "Store delivery"});
        assert_eq!(
            mode(&delivery),
            SlipMode::Delivery {
                service: Some("Store delivery".into())
            }
        );
    }

    #[test]
    fn the_customer_and_address_lines_skip_empty_fields() {
        let address = serde_json::json!({"first_name": "Ann", "last_name": "", "phone": "0470 00 00 00",
            "address1": "Rue Haute 1", "zip": "1000", "city": "Town", "country": "BE"});
        assert_eq!(
            customer_lines(&address, "ann@example.com"),
            ["Ann", "ann@example.com", "0470 00 00 00"]
        );
        assert_eq!(address_lines(&address), ["Rue Haute 1", "1000 Town", "BE"]);
    }
}
