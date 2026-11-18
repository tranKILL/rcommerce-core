//! The reports as CSV files: comma-separated, a dot for decimals, UTF-8 with
//! a byte order mark so spreadsheets pick the encoding up. Every cell is guarded
//! against spreadsheet formulas ([`formula_safe`]).

use rust_decimal::{Decimal, RoundingStrategy};

use super::finance::FinanceReport;
use super::marketing::MarketingReport;
use crate::{Error, Result};

/// The parts of the marketing report that can be exported
pub const MARKETING_SECTIONS: [&str; 6] = [
    "customers",
    "companies",
    "customer_months",
    "seasonality",
    "restock",
    "promotions",
];

const MONTH_NAMES: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// A cell a spreadsheet will not run as a formula (OWASP CSV injection): text
/// starting with `=`, `+`, `-`, `@`, a tab or a carriage return gets a `'` in
/// front, and so does each part after a `;` or a tab (spreadsheets set to `;` as
/// separator split there); plain numbers (`-12.30`) stay as they are
pub fn formula_safe(cell: &str) -> std::borrow::Cow<'_, str> {
    if cell.parse::<f64>().is_ok() {
        return std::borrow::Cow::Borrowed(cell);
    }
    let risky = |c: char| matches!(c, '=' | '+' | '-' | '@' | '\t' | '\r');
    let mut out = String::with_capacity(cell.len() + 2);
    // At the start of the cell, or of a part after `;` or a tab (spaces and quotes
    // skipped): a spreadsheet using `;` as separator starts a new cell there
    let mut at_start = true;
    for c in cell.chars() {
        if at_start && risky(c) {
            out.push('\'');
            at_start = false;
        } else if !matches!(c, ' ' | '"' | '\'') {
            at_start = false;
        }
        out.push(c);
        if matches!(c, ';' | '\t') {
            at_start = true;
        }
    }
    if out.len() == cell.len() {
        std::borrow::Cow::Borrowed(cell)
    } else {
        std::borrow::Cow::Owned(out)
    }
}

/// Rows written to a CSV text, each cell guarded by [`formula_safe`]
pub(crate) fn write_csv(header: &[&str], rows: Vec<Vec<String>>) -> Result<String> {
    write_csv_keeping(header, rows, |_, _| false)
}

/// Rows written to a CSV text, each cell guarded by [`formula_safe`] unless `keep`
/// (column index, cell) says it is safe as it is
pub(crate) fn write_csv_keeping(
    header: &[&str],
    rows: Vec<Vec<String>>,
    keep: impl Fn(usize, &str) -> bool,
) -> Result<String> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer
        .write_record(header)
        .map_err(|e| Error::internal(format!("CSV: {e}")))?;
    for row in rows {
        let row: Vec<_> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                if keep(i, cell) {
                    std::borrow::Cow::Borrowed(cell.as_str())
                } else {
                    formula_safe(cell)
                }
            })
            .collect();
        writer
            .write_record(row.iter().map(|cell| cell.as_ref()))
            .map_err(|e| Error::internal(format!("CSV: {e}")))?;
    }
    let bytes = writer
        .into_inner()
        .map_err(|e| Error::internal(format!("CSV: {e}")))?;
    let text = String::from_utf8(bytes).map_err(|e| Error::internal(format!("CSV: {e}")))?;
    Ok(format!("\u{feff}{text}"))
}

fn amount(value: Decimal) -> String {
    value
        .round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
        .to_string()
}

/// One row per month of the finance report
pub fn finance_csv(report: &FinanceReport) -> Result<String> {
    let header = [
        "month",
        "orders",
        "sales_excl_vat",
        "sales_incl_vat",
        "vat",
        "discounts",
        "goods",
        "costed_goods",
        "cost",
        "margin",
    ];
    let rows = report
        .months
        .iter()
        .map(|m| {
            vec![
                m.month.format("%Y-%m").to_string(),
                m.totals.orders.to_string(),
                amount(m.totals.sales),
                amount(m.totals.sales_incl_vat),
                amount(m.totals.vat),
                amount(m.totals.discounts),
                amount(m.goods),
                amount(m.costed),
                amount(m.cost),
                amount(m.margin),
            ]
        })
        .collect();
    write_csv(&header, rows)
}

/// One part of the marketing report (see [`MARKETING_SECTIONS`])
pub fn marketing_csv(report: &MarketingReport, section: &str) -> Result<String> {
    match section {
        "customers" => customers_csv(report),
        "companies" => companies_csv(report),
        "customer_months" => customer_months_csv(report),
        "seasonality" => seasonality_csv(report),
        "restock" => restock_csv(report),
        "promotions" => promotions_csv(report),
        _ => Err(Error::validation(format!(
            "Unknown section '{section}'; expected one of {}",
            MARKETING_SECTIONS.join(", ")
        ))),
    }
}

fn customers_csv(report: &MarketingReport) -> Result<String> {
    let header = [
        "name",
        "email",
        "company",
        "orders",
        "sales_excl_vat",
        "first_order",
        "last_order",
    ];
    let rows = report
        .top_customers
        .iter()
        .map(|c| {
            vec![
                c.name.clone(),
                c.email.clone(),
                c.company_name.clone().unwrap_or_default(),
                c.orders.to_string(),
                amount(c.sales),
                c.first_order.date_naive().to_string(),
                c.last_order.date_naive().to_string(),
            ]
        })
        .collect();
    write_csv(&header, rows)
}

fn companies_csv(report: &MarketingReport) -> Result<String> {
    let header = [
        "company",
        "buyers",
        "orders",
        "sales_excl_vat",
        "last_order",
    ];
    let rows = report
        .top_companies
        .iter()
        .map(|c| {
            vec![
                c.name.clone(),
                c.buyers.to_string(),
                c.orders.to_string(),
                amount(c.sales),
                c.last_order.date_naive().to_string(),
            ]
        })
        .collect();
    write_csv(&header, rows)
}

fn customer_months_csv(report: &MarketingReport) -> Result<String> {
    let header = [
        "month",
        "new_customers",
        "new_sales",
        "returning_customers",
        "returning_sales",
        "guest_orders",
        "guest_sales",
    ];
    let rows = report
        .customer_months
        .iter()
        .map(|m| {
            vec![
                m.month.format("%Y-%m").to_string(),
                m.new_customers.to_string(),
                amount(m.new_sales),
                m.returning_customers.to_string(),
                amount(m.returning_sales),
                m.guest_orders.to_string(),
                amount(m.guest_sales),
            ]
        })
        .collect();
    write_csv(&header, rows)
}

fn seasonality_csv(report: &MarketingReport) -> Result<String> {
    let mut header = vec!["category", "sales_excl_vat"];
    header.extend(MONTH_NAMES);
    let rows = report
        .seasonality
        .iter()
        .map(|s| {
            let mut row = vec![s.name.clone(), amount(s.sales)];
            row.extend(s.index.iter().map(|i| i.to_string()));
            row
        })
        .collect();
    write_csv(&header, rows)
}

fn restock_csv(report: &MarketingReport) -> Result<String> {
    let header = [
        "product",
        "sku",
        "last_year_coming",
        "last_year_before",
        "recent",
        "expected",
        "available",
        "suggested",
    ];
    let rows = report
        .restock
        .iter()
        .map(|r| {
            vec![
                r.title.clone(),
                r.sku.clone().unwrap_or_default(),
                r.last_year_coming.to_string(),
                r.last_year_before.to_string(),
                r.recent.to_string(),
                r.expected.to_string(),
                r.available.to_string(),
                r.suggested.to_string(),
            ]
        })
        .collect();
    write_csv(&header, rows)
}

fn promotions_csv(report: &MarketingReport) -> Result<String> {
    let header = [
        "promotion",
        "kind",
        "orders",
        "discount",
        "sales_excl_vat",
        "daily_before",
        "daily_during",
    ];
    let optional = |v: Option<Decimal>| v.map(amount).unwrap_or_default();
    let rows = report
        .promotions
        .iter()
        .map(|p| {
            vec![
                p.name.clone().unwrap_or_default(),
                p.kind.clone().unwrap_or_default(),
                p.orders.to_string(),
                amount(p.discount),
                amount(p.sales),
                optional(p.daily_before),
                optional(p.daily_during),
            ]
        })
        .collect();
    write_csv(&header, rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_that_look_like_formulas_are_kept_as_text() {
        for (cell, safe) in [
            ("=HYPERLINK(\"http://x\")", "'=HYPERLINK(\"http://x\")"),
            ("+1+1", "'+1+1"),
            ("@SUM(A1)", "'@SUM(A1)"),
            ("-2+3", "'-2+3"),
            ("\tcmd", "'\tcmd"),
            ("-12.30", "-12.30"),
            ("+32", "+32"),
            ("Anne", "Anne"),
            ("x;=1+1;", "x;'=1+1;"),
            ("a; =HYPERLINK(1)", "a; '=HYPERLINK(1)"),
            ("Smith; Jr", "Smith; Jr"),
            ("", ""),
        ] {
            assert_eq!(formula_safe(cell), safe, "{cell}");
        }
        let csv = write_csv(&["name"], vec![vec!["=1+2".to_string()]]).unwrap();
        assert!(csv.contains("'=1+2"), "{csv}");
    }

    #[test]
    fn csv_has_a_bom_a_header_and_quotes_commas() {
        let text = write_csv(
            &["name", "sales"],
            vec![vec!["Martin, fils".into(), amount(Decimal::new(12345, 3))]],
        )
        .unwrap();
        assert!(text.starts_with('\u{feff}'));
        assert_eq!(
            text.trim_start_matches('\u{feff}'),
            "name,sales\n\"Martin, fils\",12.35\n"
        );
    }
}
