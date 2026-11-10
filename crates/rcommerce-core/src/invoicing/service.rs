//! Issuing invoices from orders and reading them back.

use chrono::{Duration, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{
    invoice_number, rate_of, structured_reference, vat_breakdown, Invoice, InvoiceLine, Party,
};
use crate::config::InvoicingConfig;
use crate::list_sort::{order_by, SortColumns};
use crate::notification::i18n;
use crate::{Error, Result};

/// The order facts an invoice is made from
#[derive(Debug, sqlx::FromRow)]
struct OrderFacts {
    order_number: String,
    customer_id: Option<Uuid>,
    currency: String,
    tax_total: Decimal,
    shipping_total: Decimal,
    discount_total: Decimal,
    total: Decimal,
    payment_status: String,
    customer_email: String,
    shipping_address: Option<serde_json::Value>,
    billing_address: Option<serde_json::Value>,
    placed_locale: Option<String>,
    customer_locale: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct OrderLine {
    name: String,
    sku: Option<String>,
    quantity: i32,
    price: Decimal,
    tax_amount: Decimal,
}

/// The buyer's company, when the customer buys for one
#[derive(Debug, sqlx::FromRow)]
struct BuyerCompany {
    id: Uuid,
    name: String,
    vat_number: String,
    country: String,
    company_number: Option<String>,
    peppol_id: Option<String>,
}

/// Everything an invoice is written from, before it gets its number
struct Draft<'a> {
    order_id: Uuid,
    facts: &'a OrderFacts,
    buyer: Party,
    company: Option<Uuid>,
    lines: Vec<InvoiceLine>,
    locale: &'a str,
}

/// Staff search of invoices
/// Columns the invoice list can be sorted on
const INVOICE_SORTS: SortColumns = &[
    ("date", "issue_date"),
    ("number", "number"),
    ("customer", "lower(buyer->>'name')"),
    ("total", "total"),
    ("due", "total - paid_amount"),
    ("due_date", "due_date"),
    ("peppol", "peppol_status"),
];

#[derive(Debug, Clone, Default, Deserialize)]
pub struct InvoiceQuery {
    /// Part of the number, the order number or the buyer's name
    pub q: Option<String>,
    /// Peppol status (`pending`, `sent`, `delivered`, `failed`, `not_on_peppol`)
    pub peppol_status: Option<String>,
    pub company_id: Option<Uuid>,
    /// `date` (default `-date`), `number`, `customer`, `total`, `due`,
    /// `due_date` or `peppol`; `-` in front for descending order
    pub sort: Option<String>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

/// A text field of an address snapshot, trying each key in turn
fn field(address: Option<&serde_json::Value>, keys: &[&str]) -> String {
    address
        .and_then(|a| keys.iter().find_map(|k| a.get(*k)?.as_str()))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The street, city, postcode and country of an address snapshot; the
/// country as a two-letter code (`fallback` when the snapshot has none)
fn address_parts(
    address: Option<&serde_json::Value>,
    fallback: &str,
) -> (String, String, String, String) {
    let street = [
        field(address, &["address1", "address_1", "street"]),
        field(address, &["address2", "address_2"]),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(", ");
    let country = field(address, &["country", "country_code"]);
    let country = if country.len() == 2 {
        country.to_ascii_uppercase()
    } else {
        fallback.to_string()
    };
    (
        street,
        field(address, &["city"]),
        field(address, &["zip", "postcode", "postal_code"]),
        country,
    )
}

/// What an online payment of `charged` settles of an invoice of `total`: all of
/// it when they differ by VAT rounding only (two cents at most)
fn paid_amount(paid: bool, total: Decimal, charged: Decimal) -> Decimal {
    if !paid {
        Decimal::ZERO
    } else if (total - charged).abs() <= Decimal::new(2, 2) {
        total
    } else {
        charged.min(total)
    }
}

/// Spread `discount` over the VAT rates of `lines` in proportion to their
/// totals: one negative line per rate
fn discount_lines(lines: &[InvoiceLine], discount: Decimal, label: &str) -> Vec<InvoiceLine> {
    let total: Decimal = lines.iter().map(|l| l.net).sum();
    if discount <= Decimal::ZERO || total <= Decimal::ZERO {
        return Vec::new();
    }
    let vat = vat_breakdown(lines);
    let mut left = discount;
    vat.iter()
        .enumerate()
        .map(|(i, rate)| {
            let share = if i + 1 == vat.len() {
                left
            } else {
                (discount * rate.taxable / total).round_dp(2)
            };
            left -= share;
            InvoiceLine {
                description: label.to_string(),
                sku: None,
                quantity: 1,
                unit_price: -share,
                vat_rate: rate.rate,
                net: -share,
            }
        })
        .collect()
}

/// Invoices: issue them from orders, read them back
#[derive(Clone)]
pub struct InvoiceService {
    pool: PgPool,
    config: InvoicingConfig,
    /// Language when neither the order nor the customer gives one
    default_locale: String,
    /// Whether invoices to companies wait for Peppol
    peppol_enabled: bool,
}

impl InvoiceService {
    pub fn new(
        pool: PgPool,
        config: InvoicingConfig,
        default_locale: String,
        peppol_enabled: bool,
    ) -> Self {
        Self {
            pool,
            config,
            default_locale,
            peppol_enabled,
        }
    }

    /// Whether the store issues invoices (`[invoicing] enabled`)
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// The seller identity printed on invoices
    pub fn seller(&self) -> &Party {
        &self.config.seller
    }

    /// Issue the invoice of an order (once: a second call returns it)
    pub async fn issue_for_order(&self, order_id: Uuid) -> Result<Invoice> {
        if let Some(existing) = self.for_order(order_id).await? {
            return Ok(existing);
        }
        let mut tx = self.pool.begin().await?;
        // One issue at a time per order
        sqlx::query("SELECT id FROM orders WHERE id = $1 FOR UPDATE")
            .bind(order_id)
            .execute(&mut *tx)
            .await?;
        if let Some(existing) = Self::invoice_of_order(&mut tx, order_id).await? {
            return Ok(existing);
        }
        let facts = Self::order_facts(&mut tx, order_id).await?;
        let locale = i18n::pick_locale([
            facts.placed_locale.as_deref(),
            facts.customer_locale.as_deref(),
            Some(self.default_locale.as_str()),
        ]);
        let (buyer, company) = self.buyer(&mut tx, &facts).await?;
        let lines = self.lines(&mut tx, order_id, &facts, &locale).await?;
        let draft = Draft {
            order_id,
            facts: &facts,
            buyer,
            company,
            lines,
            locale: &locale,
        };
        let invoice = self.insert(&mut tx, draft).await?;
        tx.commit().await?;
        Ok(invoice)
    }

    async fn invoice_of_order(conn: &mut PgConnection, order_id: Uuid) -> Result<Option<Invoice>> {
        Ok(sqlx::query_as::<_, Invoice>(
            "SELECT * FROM invoices WHERE order_id = $1 AND kind = 'invoice'",
        )
        .bind(order_id)
        .fetch_optional(conn)
        .await?)
    }

    async fn order_facts(conn: &mut PgConnection, order_id: Uuid) -> Result<OrderFacts> {
        sqlx::query_as::<_, OrderFacts>(
            "SELECT o.order_number, o.customer_id, o.currency::text AS currency, o.tax_total, \
             o.shipping_total, o.discount_total, o.total, o.payment_status::text AS payment_status, \
             o.customer_email, o.shipping_address, o.billing_address, \
             o.metadata->>'locale' AS placed_locale, c.locale AS customer_locale \
             FROM orders o LEFT JOIN customers c ON c.id = o.customer_id WHERE o.id = $1",
        )
        .bind(order_id)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| Error::not_found("Order not found"))
    }

    /// The buyer: the customer's company (legal name, VAT, company number,
    /// Peppol id) with the order's billing address, else the customer
    async fn buyer(
        &self,
        conn: &mut PgConnection,
        facts: &OrderFacts,
    ) -> Result<(Party, Option<Uuid>)> {
        let company: Option<BuyerCompany> = match facts.customer_id {
            Some(customer) => sqlx::query_as(
                "SELECT co.id, co.name, co.vat_number, co.country, co.company_number, co.peppol_id \
                 FROM company_members m JOIN companies co ON co.id = m.company_id WHERE m.customer_id = $1",
            )
            .bind(customer)
            .fetch_optional(&mut *conn)
            .await?,
            None => None,
        };
        let address = facts
            .billing_address
            .as_ref()
            .or(facts.shipping_address.as_ref());
        let seller_country = self.config.seller.country.clone();
        let (street, city, postcode, country) = address_parts(address, &seller_country);
        let person = format!(
            "{} {}",
            field(address, &["first_name"]),
            field(address, &["last_name"])
        );
        let mut party = Party {
            name: person.trim().to_string(),
            street,
            city,
            postcode,
            country,
            email: Some(facts.customer_email.clone()).filter(|e| !e.is_empty()),
            ..Default::default()
        };
        let company_id = company.as_ref().map(|c| c.id);
        if let Some(company) = company {
            party.name = company.name;
            party.vat_number = Some(company.vat_number);
            party.company_number = company.company_number;
            party.peppol_id = company.peppol_id;
            party.country = company.country;
        }
        if party.name.is_empty() {
            party.name = facts.customer_email.clone();
        }
        Ok((party, company_id))
    }

    /// The order's items, its delivery charge and its discount as invoice lines
    async fn lines(
        &self,
        conn: &mut PgConnection,
        order_id: Uuid,
        facts: &OrderFacts,
        locale: &str,
    ) -> Result<Vec<InvoiceLine>> {
        let items: Vec<OrderLine> = sqlx::query_as(
            "SELECT title AS name, sku, quantity, price, tax_amount FROM order_items \
             WHERE order_id = $1 AND COALESCE(is_bundle_component, false) = false ORDER BY created_at, id",
        )
        .bind(order_id)
        .fetch_all(conn)
        .await?;
        let mut lines: Vec<InvoiceLine> = items
            .iter()
            .map(|item| {
                let net = (item.price * Decimal::from(item.quantity)).round_dp(2);
                InvoiceLine {
                    description: item.name.clone(),
                    sku: item.sku.clone(),
                    quantity: item.quantity,
                    unit_price: item.price,
                    vat_rate: rate_of(net, item.tax_amount),
                    net,
                }
            })
            .collect();
        if facts.shipping_total > Decimal::ZERO {
            let item_tax: Decimal = items.iter().map(|i| i.tax_amount).sum();
            let shipping_tax = (facts.tax_total - item_tax).max(Decimal::ZERO);
            lines.push(InvoiceLine {
                description: i18n::text(locale, "invoice_document", "delivery"),
                sku: None,
                quantity: 1,
                unit_price: facts.shipping_total,
                vat_rate: rate_of(facts.shipping_total, shipping_tax),
                net: facts.shipping_total,
            });
        }
        let discount = discount_lines(
            &lines,
            facts.discount_total,
            &i18n::text(locale, "invoice_document", "discount"),
        );
        lines.extend(discount);
        Ok(lines)
    }

    /// The next number of the year, allocated inside the issuing transaction:
    /// numbers have no gaps, as the counter row stays locked until commit
    async fn next_sequence(conn: &mut PgConnection, prefix: &str, year: i32) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "INSERT INTO invoice_counters (prefix, year, last_number) VALUES ($1, $2, 1) \
             ON CONFLICT (prefix, year) DO UPDATE SET last_number = invoice_counters.last_number + 1 \
             RETURNING last_number",
        )
        .bind(prefix)
        .bind(year)
        .fetch_one(conn)
        .await?)
    }

    /// Today in the store's time zone
    fn today(&self) -> NaiveDate {
        let zone: chrono_tz::Tz = self
            .config
            .timezone
            .parse()
            .unwrap_or(chrono_tz::Europe::Brussels);
        Utc::now().with_timezone(&zone).date_naive()
    }

    /// What is already paid, and when the rest is due
    fn settlement(
        &self,
        facts: &OrderFacts,
        total: Decimal,
        issue_date: NaiveDate,
    ) -> (Decimal, NaiveDate) {
        let paid = facts.payment_status == "paid";
        let due = if paid {
            issue_date
        } else {
            issue_date + Duration::days(self.config.payment_terms_days)
        };
        (paid_amount(paid, total, facts.total), due)
    }

    async fn insert(&self, conn: &mut PgConnection, draft: Draft<'_>) -> Result<Invoice> {
        use chrono::Datelike;
        let Draft {
            order_id,
            facts,
            buyer,
            company,
            lines,
            locale,
        } = draft;
        let issue_date = self.today();
        let sequence =
            Self::next_sequence(&mut *conn, &self.config.number_prefix, issue_date.year()).await?;
        let vat = vat_breakdown(&lines);
        let subtotal: Decimal = vat.iter().map(|v| v.taxable).sum();
        let tax_total: Decimal = vat.iter().map(|v| v.tax).sum();
        let total = subtotal + tax_total;
        let (paid_amount, due_date) = self.settlement(facts, total, issue_date);
        let peppol_status =
            (self.peppol_enabled && buyer.vat_number.is_some()).then_some("pending");
        Ok(sqlx::query_as::<_, Invoice>(
            "INSERT INTO invoices (number, kind, order_id, order_number, customer_id, company_id, issue_date, \
             due_date, currency, seller, buyer, lines, vat, subtotal, tax_total, total, paid_amount, \
             payment_reference, locale, peppol_status) \
             VALUES ($1, 'invoice', $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19) \
             RETURNING *",
        )
        .bind(invoice_number(&self.config.number_prefix, issue_date.year(), sequence))
        .bind(order_id)
        .bind(&facts.order_number)
        .bind(facts.customer_id)
        .bind(company)
        .bind(issue_date)
        .bind(due_date)
        .bind(&facts.currency)
        .bind(sqlx::types::Json(&self.config.seller))
        .bind(sqlx::types::Json(&buyer))
        .bind(sqlx::types::Json(&lines))
        .bind(sqlx::types::Json(&vat))
        .bind(subtotal)
        .bind(tax_total)
        .bind(total)
        .bind(paid_amount)
        .bind(structured_reference(issue_date.year(), sequence))
        .bind(locale)
        .bind(peppol_status)
        .fetch_one(conn)
        .await?)
    }

    /// The invoice of an order, if issued
    pub async fn for_order(&self, order_id: Uuid) -> Result<Option<Invoice>> {
        let mut conn = self.pool.acquire().await?;
        Self::invoice_of_order(&mut conn, order_id).await
    }

    /// One invoice by id
    pub async fn get(&self, id: Uuid) -> Result<Option<Invoice>> {
        Ok(
            sqlx::query_as::<_, Invoice>("SELECT * FROM invoices WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Whether `customer` may see `invoice`: theirs, or their company's
    pub async fn visible_to(&self, invoice: &Invoice, customer: Uuid) -> Result<bool> {
        if invoice.customer_id == Some(customer) {
            return Ok(true);
        }
        let Some(company) = invoice.company_id else {
            return Ok(false);
        };
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM company_members WHERE company_id = $1 AND customer_id = $2)",
        )
        .bind(company)
        .bind(customer)
        .fetch_one(&self.pool)
        .await?)
    }

    /// A customer's invoices and their company's, newest first
    pub async fn list_for_customer(
        &self,
        customer: Uuid,
        order: Option<Uuid>,
    ) -> Result<Vec<Invoice>> {
        Ok(sqlx::query_as::<_, Invoice>(
            "SELECT * FROM invoices WHERE (customer_id = $1 OR company_id IN \
             (SELECT company_id FROM company_members WHERE customer_id = $1)) \
             AND ($2::uuid IS NULL OR order_id = $2) ORDER BY issue_date DESC, number DESC LIMIT 200",
        )
        .bind(customer)
        .bind(order)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Invoices for staff, newest first; the page and the total count
    pub async fn list(&self, query: &InvoiceQuery) -> Result<(Vec<Invoice>, i64)> {
        let per_page = query.per_page.unwrap_or(50).clamp(1, 100);
        let page = query.page.unwrap_or(1).max(1);
        let q = query
            .q
            .as_deref()
            .map(|q| format!("%{}%", q.trim().replace(['%', '_'], "")));
        let filter = "WHERE ($1::text IS NULL OR number ILIKE $1 OR order_number ILIKE $1 OR buyer->>'name' ILIKE $1) \
                      AND ($2::text IS NULL OR peppol_status = $2) AND ($3::uuid IS NULL OR company_id = $3)";
        let order = order_by(query.sort.as_deref(), INVOICE_SORTS, "-date", "number DESC")?;
        let invoices = sqlx::query_as::<_, Invoice>(&format!(
            "SELECT * FROM invoices {filter} ORDER BY {order} LIMIT $4 OFFSET $5"
        ))
        .bind(&q)
        .bind(&query.peppol_status)
        .bind(query.company_id)
        .bind(per_page)
        .bind((page - 1) * per_page)
        .fetch_all(&self.pool)
        .await?;
        let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM invoices {filter}"))
            .bind(&q)
            .bind(&query.peppol_status)
            .bind(query.company_id)
            .fetch_one(&self.pool)
            .await?;
        Ok((invoices, total))
    }

    /// Record what Peppol did with an invoice
    pub async fn set_peppol(
        &self,
        id: Uuid,
        status: &str,
        message_id: Option<&str>,
        recipient: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE invoices SET peppol_status = $2, peppol_message_id = COALESCE($3, peppol_message_id), \
             peppol_recipient = COALESCE($4, peppol_recipient), peppol_error = $5, peppol_updated_at = NOW() \
             WHERE id = $1",
        )
        .bind(id)
        .bind(status)
        .bind(message_id)
        .bind(recipient)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Invoices whose Peppol delivery is still in progress, oldest first
    pub async fn awaiting_peppol(&self, statuses: &[&str], limit: i64) -> Result<Vec<Invoice>> {
        let statuses: Vec<String> = statuses.iter().map(|s| s.to_string()).collect();
        Ok(sqlx::query_as::<_, Invoice>(
            "SELECT * FROM invoices WHERE peppol_status = ANY($1) ORDER BY created_at LIMIT $2",
        )
        .bind(&statuses)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Remember a company's Peppol id once found
    pub async fn remember_peppol_id(&self, company: Uuid, participant: &str) -> Result<()> {
        sqlx::query("UPDATE companies SET peppol_id = $2, updated_at = NOW() WHERE id = $1")
            .bind(company)
            .bind(participant)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn line(net: Decimal, rate: Decimal) -> InvoiceLine {
        InvoiceLine {
            description: "x".into(),
            sku: None,
            quantity: 1,
            unit_price: net,
            vat_rate: rate,
            net,
        }
    }

    #[test]
    fn a_discount_is_spread_over_the_rates() {
        let lines = [line(dec!(75), dec!(21)), line(dec!(25), dec!(6))];
        let discount = discount_lines(&lines, dec!(10), "Discount");
        assert_eq!(discount.len(), 2);
        assert_eq!(
            (discount[0].vat_rate, discount[0].net),
            (dec!(21), dec!(-7.50))
        );
        assert_eq!(
            (discount[1].vat_rate, discount[1].net),
            (dec!(6), dec!(-2.50))
        );
        assert!(discount_lines(&lines, Decimal::ZERO, "Discount").is_empty());
    }

    #[test]
    fn address_snapshots_give_their_parts() {
        let address = serde_json::json!({"address1": "Rue Neuve 1", "city": "Liège", "zip": "4000", "country": "be"});
        assert_eq!(
            address_parts(Some(&address), "BE"),
            (
                "Rue Neuve 1".into(),
                "Liège".into(),
                "4000".into(),
                "BE".into()
            )
        );
        let named =
            serde_json::json!({"street": "Main St 2", "postcode": "1000", "country": "Belgium"});
        assert_eq!(address_parts(Some(&named), "BE").3, "BE");
    }
}
