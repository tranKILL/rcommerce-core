//! The order slip a shop prints to prepare a web order: how it leaves (pickup
//! at the shop with its ready time, or delivery), whether it is paid, the
//! customer, the lines with their references and the total. A4, or an 80 mm
//! ticket as long as the order needs, in the reader's language.

use chrono::{DateTime, Utc};
use printpdf::{Mm, Op, ParsedFont, PdfDocument, PdfFontHandle, PdfPage, PdfSaveOptions};
use rust_decimal::Decimal;

use super::{draw_line, light_gray, write_text, HELVETICA_BOLD, HELVETICA_REGULAR};
use crate::notification::i18n;
use crate::{Error, Result};

const MM: f32 = 72.0 / 25.4;

/// Paper of a slip
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SlipFormat {
    /// A4 pages
    #[default]
    A4,
    /// An 80 mm receipt roll, one page as long as the slip
    Ticket80,
}

impl SlipFormat {
    /// `a4` or `ticket80`
    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "a4" => Some(Self::A4),
            "ticket80" => Some(Self::Ticket80),
            _ => None,
        }
    }

    /// The stored spelling
    pub fn code(self) -> &'static str {
        match self {
            Self::A4 => "a4",
            Self::Ticket80 => "ticket80",
        }
    }

    /// Page width and side margin, in points
    fn page(self) -> (f32, f32) {
        match self {
            Self::A4 => (210.0 * MM, 50.0),
            Self::Ticket80 => (80.0 * MM, 4.0 * MM),
        }
    }
}

/// How the order leaves the shop
#[derive(Debug, Clone, PartialEq)]
pub enum SlipMode {
    /// Collected at `location`; `ready_at` as the customer was told
    Pickup {
        location: String,
        ready_at: Option<DateTime<Utc>>,
    },
    /// Delivered or shipped (`service`: the delivery option chosen)
    Delivery { service: Option<String> },
}

/// One line to prepare
#[derive(Debug, Clone, PartialEq)]
pub struct SlipLine {
    pub quantity: i32,
    pub name: String,
    pub variant: Option<String>,
    pub sku: Option<String>,
}

/// What a slip shows; texts and amounts are written in the reader's language
#[derive(Debug, Clone, PartialEq)]
pub struct OrderSlip {
    pub store_name: String,
    pub order_number: String,
    pub placed_at: DateTime<Utc>,
    /// The shop's time zone, for dates and times (`Europe/Brussels`)
    pub timezone: chrono_tz::Tz,
    pub mode: SlipMode,
    pub paid: bool,
    /// A test order (or the sample of a test e-mail)
    pub test: bool,
    /// Name, e-mail, phone, company: the lines known
    pub customer: Vec<String>,
    /// The delivery address, for deliveries
    pub address: Vec<String>,
    pub notes: Option<String>,
    pub lines: Vec<SlipLine>,
    pub total: Decimal,
    pub currency: String,
}

/// One row of the slip: text on the left, optionally a text ending at the
/// right margin, `gap` points below the previous row
#[derive(Debug, Clone)]
struct Row {
    size: f32,
    bold: bool,
    left: String,
    right: Option<String>,
    gap: f32,
    rule: bool,
}

impl Row {
    fn text(size: f32, bold: bool, left: impl Into<String>) -> Self {
        Row {
            size,
            bold,
            left: left.into(),
            right: None,
            gap: size * 1.35,
            rule: false,
        }
    }

    fn rule() -> Self {
        Row {
            size: 0.0,
            bold: false,
            left: String::new(),
            right: None,
            gap: 8.0,
            rule: true,
        }
    }

    fn spaced(mut self, extra: f32) -> Self {
        self.gap += extra;
        self
    }
}

fn label(locale: &str, key: &str) -> String {
    i18n::text(locale, "order_slip", key)
}

/// "12 novembre 2026 14:05" in the shop's time zone
fn when(locale: &str, at: DateTime<Utc>, zone: chrono_tz::Tz) -> String {
    let local = at.with_timezone(&zone);
    format!(
        "{} {}",
        i18n::date(locale, local.date_naive()),
        local.format("%H:%M")
    )
}

/// The heading rows: store, title and number, date, test warning, how it
/// leaves and whether it is paid
fn heading_rows(slip: &OrderSlip, locale: &str) -> Vec<Row> {
    let mut rows = vec![
        Row::text(11.0, false, slip.store_name.clone()),
        Row::text(
            15.0,
            true,
            format!("{} {}", label(locale, "title"), slip.order_number),
        )
        .spaced(4.0),
        Row::text(
            9.0,
            false,
            format!(
                "{} {}",
                label(locale, "placed"),
                when(locale, slip.placed_at, slip.timezone)
            ),
        ),
    ];
    if slip.test {
        rows.push(Row::text(10.0, true, label(locale, "test")).spaced(2.0));
    }
    let (mode, detail) = match &slip.mode {
        SlipMode::Pickup { location, ready_at } => (
            label(locale, "pickup").replace("{location}", location),
            ready_at.map(|at| {
                format!(
                    "{} {}",
                    label(locale, "ready"),
                    when(locale, at, slip.timezone)
                )
            }),
        ),
        SlipMode::Delivery { service } => (label(locale, "delivery"), service.clone()),
    };
    rows.push(Row::text(12.0, true, mode).spaced(4.0));
    if let Some(detail) = detail {
        rows.push(Row::text(10.0, false, detail));
    }
    let paid = if slip.paid { "paid" } else { "to_pay" };
    rows.push(Row::text(11.0, true, label(locale, paid)).spaced(2.0));
    rows
}

/// The customer, delivery address and notes
fn customer_rows(slip: &OrderSlip, locale: &str) -> Vec<Row> {
    let mut rows = vec![
        Row::rule(),
        Row::text(8.5, true, label(locale, "customer").to_uppercase()),
    ];
    rows.extend(
        slip.customer
            .iter()
            .map(|l| Row::text(10.0, false, l.clone())),
    );
    if !slip.address.is_empty() {
        rows.push(Row::text(8.5, true, label(locale, "address").to_uppercase()).spaced(4.0));
        rows.extend(
            slip.address
                .iter()
                .map(|l| Row::text(10.0, false, l.clone())),
        );
    }
    if let Some(notes) = slip.notes.as_deref().filter(|n| !n.trim().is_empty()) {
        rows.push(Row::text(8.5, true, label(locale, "notes").to_uppercase()).spaced(4.0));
        rows.push(Row::text(10.0, false, notes.trim().to_string()));
    }
    rows
}

/// The lines (quantity and name, then reference and variant), the count and
/// the total
fn line_rows(slip: &OrderSlip, locale: &str) -> Vec<Row> {
    let mut rows = vec![Row::rule()];
    for line in &slip.lines {
        rows.push(Row::text(11.0, true, format!("{} × {}", line.quantity, line.name)).spaced(2.0));
        let below: Vec<String> = [
            line.sku
                .as_ref()
                .map(|s| format!("{} {s}", label(locale, "sku"))),
            line.variant.clone(),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !below.is_empty() {
            rows.push(Row::text(9.0, false, below.join(" · ")));
        }
    }
    let units: i32 = slip.lines.iter().map(|l| l.quantity).sum();
    rows.push(Row::rule());
    let mut total = Row::text(12.0, true, label(locale, "total"));
    total.right = Some(i18n::money(locale, slip.total, &slip.currency));
    rows.push(total);
    rows.push(Row::text(
        9.0,
        false,
        label(locale, "units").replace("{count}", &units.to_string()),
    ));
    rows
}

/// Long texts cut to the printable width (an estimate from the font size)
fn wrapped(rows: Vec<Row>, width: f32) -> Vec<Row> {
    let mut out = Vec::new();
    for row in rows {
        if row.rule || row.right.is_some() {
            out.push(row);
            continue;
        }
        for (i, piece) in super::wrap_text(&row.left, row.size, width)
            .into_iter()
            .enumerate()
        {
            let mut next = row.clone();
            next.left = piece;
            if i > 0 {
                next.gap = row.size * 1.25;
            }
            out.push(next);
        }
    }
    out
}

/// The rows of a slip in reading order
fn rows(slip: &OrderSlip, locale: &str, width: f32) -> Vec<Row> {
    let mut rows = heading_rows(slip, locale);
    rows.extend(customer_rows(slip, locale));
    rows.extend(line_rows(slip, locale));
    wrapped(rows, width)
}

/// Fonts of a document, and their metrics to align text on the right
struct Fonts {
    regular: PdfFontHandle,
    bold: PdfFontHandle,
    metrics: (ParsedFont, ParsedFont),
}

fn fonts(doc: &mut PdfDocument) -> Result<Fonts> {
    let mut warnings = Vec::new();
    let regular = ParsedFont::from_bytes(HELVETICA_REGULAR, 0, &mut warnings)
        .ok_or_else(|| Error::internal("Failed to parse embedded regular font"))?;
    let bold = ParsedFont::from_bytes(HELVETICA_BOLD, 0, &mut warnings)
        .ok_or_else(|| Error::internal("Failed to parse embedded bold font"))?;
    Ok(Fonts {
        regular: PdfFontHandle::External(doc.add_font(&regular)),
        bold: PdfFontHandle::External(doc.add_font(&bold)),
        metrics: (regular, bold),
    })
}

/// Draw `row` with its baseline at `y`
fn draw(ops: &mut Vec<Op>, fonts: &Fonts, row: &Row, y: f32, (left, right): (f32, f32)) {
    if row.rule {
        draw_line(ops, left, y + 4.0, right, y + 4.0, 0.6, light_gray());
        return;
    }
    let font = if row.bold {
        &fonts.bold
    } else {
        &fonts.regular
    };
    write_text(ops, font.clone(), row.size, left, y, &row.left);
    if let Some(text) = &row.right {
        let metrics = if row.bold {
            &fonts.metrics.1
        } else {
            &fonts.metrics.0
        };
        let x = right - super::legal::measure(metrics, row.size, text);
        write_text(ops, font.clone(), row.size, x, y, text);
    }
}

/// The slip's PDF in `locale`
pub fn order_slip_pdf(slip: &OrderSlip, format: SlipFormat, locale: &str) -> Result<Vec<u8>> {
    let (width, margin) = format.page();
    let rows = rows(slip, locale, width - 2.0 * margin);
    let mut doc = PdfDocument::new(&slip.order_number);
    let fonts = fonts(&mut doc)?;
    let content: f32 = rows.iter().map(|r| r.gap).sum();
    let (height, top, bottom) = match format {
        SlipFormat::A4 => (297.0 * MM, 50.0, 60.0),
        SlipFormat::Ticket80 => (content + 2.0 * margin + 8.0, margin, margin),
    };
    let mut pages = Vec::new();
    let mut ops = Vec::new();
    let mut y = height - top;
    for row in &rows {
        if y - row.gap < bottom {
            pages.push(std::mem::take(&mut ops));
            y = height - top;
        }
        y -= row.gap;
        draw(&mut ops, &fonts, row, y, (margin, width - margin));
    }
    pages.push(ops);
    for ops in pages {
        doc.pages
            .push(PdfPage::new(Mm(width / MM), Mm(height / MM), ops));
    }
    let mut warnings = Vec::new();
    Ok(doc.save(&PdfSaveOptions::default(), &mut warnings))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A pickup slip with `lines` lines
    pub(crate) fn sample(lines: usize) -> OrderSlip {
        OrderSlip {
            store_name: "The store".into(),
            order_number: "ORD-1001".into(),
            placed_at: Utc::now(),
            timezone: chrono_tz::Europe::Brussels,
            mode: SlipMode::Pickup {
                location: "North branch".into(),
                ready_at: Some(Utc::now()),
            },
            paid: true,
            test: false,
            customer: vec!["A. Builder".into(), "a@example.test".into()],
            address: Vec::new(),
            notes: Some("Call on arrival".into()),
            lines: vec![
                SlipLine {
                    quantity: 2,
                    name: "Cement 25 kg".into(),
                    variant: None,
                    sku: Some("CEM25".into()),
                };
                lines
            ],
            total: Decimal::new(1798, 2),
            currency: "EUR".into(),
        }
    }

    #[test]
    fn a_slip_renders_on_a4_and_on_a_ticket() {
        for format in [SlipFormat::A4, SlipFormat::Ticket80] {
            let pdf = order_slip_pdf(&sample(3), format, "fr").unwrap();
            assert!(pdf.starts_with(b"%PDF"), "{format:?}");
        }
    }

    #[test]
    fn a_long_order_takes_more_pages_or_a_longer_ticket() {
        for format in [SlipFormat::A4, SlipFormat::Ticket80] {
            let short = order_slip_pdf(&sample(2), format, "en").unwrap();
            let long = order_slip_pdf(&sample(80), format, "en").unwrap();
            assert!(long.len() > short.len(), "{format:?}");
        }
    }

    #[test]
    fn the_ticket_is_as_long_as_its_rows() {
        let slip = sample(10);
        let rows = rows(&slip, "fr", 80.0 * MM - 8.0 * MM);
        assert!(
            rows.iter().any(|r| r.right.is_some()),
            "the total is aligned right"
        );
        assert!(rows.iter().filter(|r| r.rule).count() >= 2);
    }

    #[test]
    fn every_slip_text_exists_in_every_language() {
        let catalogues = [
            ("fr", include_str!("../notification/locales/fr.toml")),
            ("nl", include_str!("../notification/locales/nl.toml")),
            ("en", include_str!("../notification/locales/en.toml")),
            ("de", include_str!("../notification/locales/de.toml")),
            ("it", include_str!("../notification/locales/it.toml")),
            ("es", include_str!("../notification/locales/es.toml")),
            ("pt", include_str!("../notification/locales/pt.toml")),
            ("tr", include_str!("../notification/locales/tr.toml")),
            ("ro", include_str!("../notification/locales/ro.toml")),
            ("pl", include_str!("../notification/locales/pl.toml")),
        ];
        let keys = [
            "title", "placed", "test", "pickup", "ready", "delivery", "paid", "to_pay", "customer",
            "address", "notes", "sku", "total", "units",
        ];
        for (locale, source) in catalogues {
            let catalogue: toml::Table = source.parse().unwrap();
            let section = catalogue["order_slip"].as_table().unwrap();
            for key in keys {
                assert!(
                    section
                        .get(key)
                        .and_then(|v| v.as_str())
                        .is_some_and(|t| !t.is_empty()),
                    "{locale}.{key}"
                );
            }
        }
        assert!(label("fr", "pickup").contains("{location}"));
        assert!(label("nl", "units").contains("{count}"));
    }

    #[test]
    fn formats_parse_and_print_back() {
        for format in [SlipFormat::A4, SlipFormat::Ticket80] {
            assert_eq!(SlipFormat::parse(format.code()), Some(format));
        }
        assert_eq!(SlipFormat::parse("letter"), None);
    }
}
