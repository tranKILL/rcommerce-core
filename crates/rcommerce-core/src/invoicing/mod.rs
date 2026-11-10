//! Legal invoices: numbered without gaps, issued from an order once its goods
//! are handed over (collected or delivered), and immutable afterwards.
//!
//! An invoice keeps a snapshot of the seller (from `[invoicing.seller]`), the
//! buyer (a company account's legal details, else the customer), the lines and
//! the VAT per rate, so later changes to the order, the customer or the store
//! never change an issued invoice. The VAT of each rate is computed on the
//! rate's taxable total (EN 16931), which electronic invoicing requires.

pub mod service;

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use service::InvoiceService;

/// The legal identity of the seller or the buyer, as printed on the invoice
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Party {
    /// Legal name (company) or full name (person)
    pub name: String,
    /// VAT number with country prefix (`BE0123456749`)
    #[serde(default)]
    pub vat_number: Option<String>,
    /// National company number (Belgium: the VAT number's ten digits)
    #[serde(default)]
    pub company_number: Option<String>,
    #[serde(default)]
    pub street: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub postcode: String,
    /// ISO 3166-1 alpha-2 country code
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub email: Option<String>,
    /// Peppol participant id (`0208:0123456749`) when known
    #[serde(default)]
    pub peppol_id: Option<String>,
    /// Bank account the buyer pays to (seller only)
    #[serde(default)]
    pub iban: Option<String>,
    #[serde(default)]
    pub bic: Option<String>,
}

/// One invoiced line, excluding VAT
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvoiceLine {
    pub description: String,
    #[serde(default)]
    pub sku: Option<String>,
    pub quantity: i32,
    pub unit_price: Decimal,
    /// VAT percentage (21, 12, 6, 0)
    pub vat_rate: Decimal,
    /// quantity × unit price (negative for a discount line)
    pub net: Decimal,
}

/// The VAT of one rate
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VatSubtotal {
    pub rate: Decimal,
    pub taxable: Decimal,
    pub tax: Decimal,
}

/// An issued invoice (or credit note)
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Invoice {
    pub id: Uuid,
    /// `F2026-00001`
    pub number: String,
    /// `invoice` or `credit_note`
    pub kind: String,
    pub order_id: Uuid,
    pub order_number: String,
    pub customer_id: Option<Uuid>,
    pub company_id: Option<Uuid>,
    pub issue_date: NaiveDate,
    pub due_date: NaiveDate,
    pub currency: String,
    pub seller: sqlx::types::Json<Party>,
    pub buyer: sqlx::types::Json<Party>,
    pub lines: sqlx::types::Json<Vec<InvoiceLine>>,
    pub vat: sqlx::types::Json<Vec<VatSubtotal>>,
    pub subtotal: Decimal,
    pub tax_total: Decimal,
    pub total: Decimal,
    /// Already paid (online payment); the rest is due
    pub paid_amount: Decimal,
    /// Belgian structured communication (`+++123/4567/89012+++`)
    pub payment_reference: String,
    /// Language of the invoice document
    pub locale: String,
    /// `pending`, `sent`, `delivered`, `failed`, `not_on_peppol` (`None`: not for Peppol)
    pub peppol_status: Option<String>,
    pub peppol_message_id: Option<String>,
    pub peppol_recipient: Option<String>,
    pub peppol_error: Option<String>,
    pub peppol_updated_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl Invoice {
    /// What remains to pay
    pub fn amount_due(&self) -> Decimal {
        (self.total - self.paid_amount).max(Decimal::ZERO)
    }
}

/// The VAT per rate of `lines`: each rate's tax is computed on its taxable
/// total and rounded to cents (EN 16931 BR-CO-17), rates in decreasing order
pub fn vat_breakdown(lines: &[InvoiceLine]) -> Vec<VatSubtotal> {
    let mut rates: Vec<Decimal> = lines.iter().map(|l| l.vat_rate.normalize()).collect();
    rates.sort_by(|a, b| b.cmp(a));
    rates.dedup();
    rates
        .into_iter()
        .map(|rate| {
            let taxable: Decimal = lines
                .iter()
                .filter(|l| l.vat_rate.normalize() == rate)
                .map(|l| l.net)
                .sum();
            let tax = (taxable * rate / Decimal::ONE_HUNDRED).round_dp(2);
            VatSubtotal {
                rate,
                taxable: taxable.round_dp(2),
                tax,
            }
        })
        .collect()
}

/// A line's VAT rate from its net amount and the tax charged on it, rounded
/// to the nearest whole percent (Belgian rates are 21, 12, 6 and 0)
pub fn rate_of(net: Decimal, tax: Decimal) -> Decimal {
    if net.is_zero() {
        return Decimal::ZERO;
    }
    (tax / net * Decimal::ONE_HUNDRED).round_dp(0)
}

/// The Belgian structured communication for invoice `sequence` of `year`:
/// ten digits (year, then the sequence) and a modulo-97 check (97 for 0)
pub fn structured_reference(year: i32, sequence: i64) -> String {
    let base = format!(
        "{:04}{:06}",
        year.rem_euclid(10_000),
        sequence.rem_euclid(1_000_000)
    );
    let number: u64 = base.parse().unwrap_or(0);
    let check = match number % 97 {
        0 => 97,
        rest => rest,
    };
    let digits = format!("{base}{check:02}");
    format!("+++{}/{}/{}+++", &digits[..3], &digits[3..7], &digits[7..])
}

/// The invoice number for `sequence` of `year`: `{prefix}{year}-{sequence:05}`
pub fn invoice_number(prefix: &str, year: i32, sequence: i64) -> String {
    format!("{prefix}{year}-{sequence:05}")
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
    fn vat_is_computed_per_rate_on_the_rate_total() {
        // Three lines at 21 %: per-line rounding would give 0.69 + 0.69 + 0.69 = 2.07
        let lines = [
            line(dec!(3.30), dec!(21)),
            line(dec!(3.30), dec!(21)),
            line(dec!(3.30), dec!(21)),
            line(dec!(10), dec!(6)),
        ];
        let vat = vat_breakdown(&lines);
        assert_eq!(vat.len(), 2);
        assert_eq!(
            (vat[0].rate, vat[0].taxable, vat[0].tax),
            (dec!(21), dec!(9.90), dec!(2.08))
        );
        assert_eq!(
            (vat[1].rate, vat[1].taxable, vat[1].tax),
            (dec!(6), dec!(10.00), dec!(0.60))
        );
        assert_eq!(rate_of(dec!(64.00), dec!(13.44)), dec!(21));
        assert_eq!(rate_of(dec!(0), dec!(0)), dec!(0));
    }

    #[test]
    fn numbers_and_structured_references() {
        assert_eq!(invoice_number("F", 2026, 42), "F2026-00042");
        // 2026000042 mod 97 = 36
        assert_eq!(structured_reference(2026, 42), "+++202/6000/04236+++");
        let digits: String = structured_reference(2026, 1)
            .chars()
            .filter(char::is_ascii_digit)
            .collect();
        let (base, check) = digits.split_at(10);
        let expected = match base.parse::<u64>().unwrap() % 97 {
            0 => 97,
            r => r,
        };
        assert_eq!(check.parse::<u64>().unwrap(), expected);
    }
}
