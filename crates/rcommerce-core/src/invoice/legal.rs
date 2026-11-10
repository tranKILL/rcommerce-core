//! The PDF of a legal invoice: the store's logo, seller and buyer side by side
//! with their VAT and company numbers, the invoice's references, the lines,
//! the VAT per rate, the totals and how to pay, in the invoice's language.

use printpdf::{Mm, Op, ParsedFont, PdfDocument, PdfFontHandle, PdfPage, PdfSaveOptions};
use rust_decimal::Decimal;

use super::{
    black, draw_line, gray, light_gray, wrap_text, write_text, PdfLogo, HELVETICA_BOLD,
    HELVETICA_REGULAR,
};
use super::{MARGIN, PAGE_HEIGHT_PT, RIGHT_EDGE};
use crate::invoicing::{Invoice, Party};
use crate::notification::i18n;
use crate::{Error, Result};

/// Column positions of the lines table (right edges for numbers)
const QTY_RIGHT: f32 = 300.0;
const PRICE_RIGHT: f32 = 410.0;
const RATE_RIGHT: f32 = 455.0;
/// Below this height a new page starts
const BOTTOM: f32 = 90.0;
/// Left edge of the buyer's block (the window of a right-window envelope)
const BUYER_X: f32 = 330.0;
/// Box the logo is fitted in, in points
const LOGO_BOX: (f32, f32) = (180.0, 64.0);

/// Width of `text` in points, from the font's glyph advances
fn measure(font: &ParsedFont, size: f32, text: &str) -> f32 {
    let units = f32::from(font.pdf_font_metrics.units_per_em.max(1));
    let advance: f32 = text
        .chars()
        .map(|c| {
            font.lookup_glyph_index(c as u32)
                .map(|glyph| f32::from(font.get_horizontal_advance(glyph)))
                .unwrap_or(units / 2.0)
        })
        .sum();
    advance * size / units
}

/// Pages being written, top to bottom
struct Canvas {
    pages: Vec<Vec<Op>>,
    ops: Vec<Op>,
    y: f32,
    regular: PdfFontHandle,
    bold: PdfFontHandle,
    /// The same fonts, to measure text
    metrics: (ParsedFont, ParsedFont),
}

impl Canvas {
    fn width(&self, bold: bool, size: f32, text: &str) -> f32 {
        let font = if bold {
            &self.metrics.1
        } else {
            &self.metrics.0
        };
        measure(font, size, text)
    }

    fn text(&mut self, bold: bool, size: f32, x: f32, text: &str) {
        let font = if bold {
            self.bold.clone()
        } else {
            self.regular.clone()
        };
        write_text(&mut self.ops, font, size, x, self.y, text);
    }

    /// Text ending at `right`
    fn right(&mut self, bold: bool, size: f32, right: f32, text: &str) {
        let x = right - self.width(bold, size, text);
        self.text(bold, size, x, text);
    }

    fn rule(&mut self, light: bool) {
        let color = if light { light_gray() } else { black() };
        draw_line(
            &mut self.ops,
            MARGIN,
            self.y,
            RIGHT_EDGE,
            self.y,
            0.6,
            color,
        );
    }

    fn down(&mut self, by: f32) {
        self.y -= by;
        if self.y < BOTTOM {
            self.pages.push(std::mem::take(&mut self.ops));
            self.y = PAGE_HEIGHT_PT - 60.0;
        }
    }
}

/// The lines of a party block: name, address, VAT and company numbers
fn party_lines(party: &Party, locale: &str) -> Vec<String> {
    let mut lines = vec![
        party.street.clone(),
        format!("{} {}", party.postcode, party.city)
            .trim()
            .to_string(),
        party.country.clone(),
    ];
    if let Some(vat) = &party.vat_number {
        lines.push(format!(
            "{} {vat}",
            i18n::text(locale, "invoice_document", "vat_number")
        ));
    }
    if let Some(number) = &party.company_number {
        lines.push(format!(
            "{} {number}",
            i18n::text(locale, "invoice_document", "company_number")
        ));
    }
    if let Some(email) = &party.email {
        lines.push(email.clone());
    }
    lines.retain(|l| !l.is_empty());
    lines
}

/// A party's block at `x` from the current height: an optional heading, the
/// name and its lines; returns the height below the last line
fn party_block(
    canvas: &mut Canvas,
    x: f32,
    heading: Option<&str>,
    party: &Party,
    locale: &str,
) -> f32 {
    if let Some(heading) = heading {
        canvas.text(true, 8.5, x, &heading.to_uppercase());
        canvas.y -= 14.0;
    }
    canvas.text(
        true,
        if heading.is_some() { 11.0 } else { 13.0 },
        x,
        &party.name,
    );
    for line in party_lines(party, locale) {
        canvas.y -= 12.0;
        canvas.text(false, 9.0, x, &line);
    }
    canvas.y
}

/// The invoice's number, dates and order: labels, colons and values each in
/// their own column
fn references(canvas: &mut Canvas, invoice: &Invoice) {
    let locale = invoice.locale.as_str();
    let t = |key: &str| i18n::text(locale, "invoice_document", key);
    let rows = [
        (t("number"), invoice.number.clone(), true),
        (
            t("issue_date"),
            i18n::date(locale, invoice.issue_date),
            false,
        ),
        (t("due_date"), i18n::date(locale, invoice.due_date), false),
        (t("order"), invoice.order_number.clone(), false),
    ];
    let widest = rows
        .iter()
        .map(|(label, _, _)| canvas.width(false, 9.5, label))
        .fold(0.0, f32::max);
    let colon = MARGIN + widest + 5.0;
    for (label, value, strong) in rows {
        canvas.text(false, 9.5, MARGIN, &label);
        canvas.text(false, 9.5, colon, ":");
        canvas.text(strong, 9.5, colon + 9.0, &value);
        canvas.y -= 14.0;
    }
}

/// Top of the first page: logo (already drawn, `logo_height` tall) and title,
/// the seller on the left and the buyer on the right at the same height, then
/// the references
fn header(canvas: &mut Canvas, invoice: &Invoice, logo_height: f32) {
    let locale = invoice.locale.as_str();
    let t = |key: &str| i18n::text(locale, "invoice_document", key);
    let top = canvas.y;
    let title = if invoice.kind == "credit_note" {
        t("credit_note")
    } else {
        t("title")
    };
    canvas.y = top - 18.0;
    canvas.right(true, 20.0, RIGHT_EDGE, &title.to_uppercase());
    let blocks = if logo_height > 0.0 {
        top - logo_height - 24.0
    } else {
        top - 12.0
    };
    canvas.y = blocks;
    let seller_end = party_block(canvas, MARGIN, None, &invoice.seller, locale);
    canvas.y = blocks.min(top - 50.0);
    let buyer_end = party_block(canvas, BUYER_X, Some(&t("buyer")), &invoice.buyer, locale);
    canvas.y = seller_end.min(buyer_end) - 30.0;
    references(canvas, invoice);
    canvas.y -= 20.0;
}

fn lines_table(canvas: &mut Canvas, invoice: &Invoice) {
    let locale = invoice.locale.as_str();
    let t = |key: &str| i18n::text(locale, "invoice_document", key);
    let money = |amount: Decimal| i18n::money(locale, amount, &invoice.currency);
    canvas.text(true, 8.5, MARGIN, &t("description"));
    canvas.right(true, 8.5, QTY_RIGHT, &t("quantity"));
    canvas.right(true, 8.5, PRICE_RIGHT, &t("unit_price"));
    canvas.right(true, 8.5, RATE_RIGHT, &t("vat_rate"));
    canvas.right(true, 8.5, RIGHT_EDGE, &t("amount"));
    canvas.down(6.0);
    canvas.rule(false);
    for line in invoice.lines.iter() {
        canvas.down(14.0);
        let mut label = line.description.clone();
        if let Some(sku) = &line.sku {
            label = format!("{label} ({sku})");
        }
        let wrapped = wrap_text(&label, 9.0, QTY_RIGHT - MARGIN - 40.0);
        canvas.text(
            false,
            9.0,
            MARGIN,
            wrapped.first().map(String::as_str).unwrap_or_default(),
        );
        canvas.right(false, 9.0, QTY_RIGHT, &line.quantity.to_string());
        canvas.right(false, 9.0, PRICE_RIGHT, &money(line.unit_price));
        canvas.right(
            false,
            9.0,
            RATE_RIGHT,
            &format!("{} %", line.vat_rate.normalize()),
        );
        canvas.right(false, 9.0, RIGHT_EDGE, &money(line.net));
        for more in wrapped.iter().skip(1) {
            canvas.down(11.0);
            canvas.text(false, 9.0, MARGIN, more);
        }
        canvas.down(5.0);
        canvas.rule(true);
    }
    canvas.down(22.0);
}

fn totals(canvas: &mut Canvas, invoice: &Invoice) {
    let locale = invoice.locale.as_str();
    let t = |key: &str| i18n::text(locale, "invoice_document", key);
    let money = |amount: Decimal| i18n::money(locale, amount, &invoice.currency);
    let top = canvas.y;
    canvas.text(true, 8.5, MARGIN, &t("vat_rate"));
    canvas.right(true, 8.5, 190.0, &t("taxable"));
    canvas.right(true, 8.5, 260.0, &t("vat_amount"));
    for rate in invoice.vat.iter() {
        canvas.y -= 13.0;
        canvas.text(false, 9.0, MARGIN, &format!("{} %", rate.rate.normalize()));
        canvas.right(false, 9.0, 190.0, &money(rate.taxable));
        canvas.right(false, 9.0, 260.0, &money(rate.tax));
    }
    let after_vat = canvas.y;
    canvas.y = top;
    let mut rows = vec![
        (t("subtotal"), invoice.subtotal, false),
        (t("vat_amount"), invoice.tax_total, false),
        (t("total"), invoice.total, true),
    ];
    if !invoice.paid_amount.is_zero() {
        rows.push((t("paid"), -invoice.paid_amount, false));
        rows.push((t("amount_due"), invoice.amount_due(), true));
    }
    for (label, value, strong) in rows {
        canvas.text(strong, 9.5, 350.0, &label);
        canvas.right(strong, 9.5, RIGHT_EDGE, &money(value));
        canvas.y -= 15.0;
    }
    canvas.y = canvas.y.min(after_vat) - 20.0;
}

fn payment(canvas: &mut Canvas, invoice: &Invoice) {
    let locale = invoice.locale.as_str();
    let t = |key: &str| i18n::text(locale, "invoice_document", key);
    if invoice.vat.iter().any(|v| v.rate.is_zero())
        && invoice.buyer.country != invoice.seller.country
    {
        canvas.text(false, 9.0, MARGIN, &t("reverse_charge"));
        canvas.down(16.0);
    }
    if invoice.amount_due().is_zero() {
        return;
    }
    if let Some(iban) = &invoice.seller.iban {
        let bic = invoice
            .seller
            .bic
            .as_deref()
            .map(|b| format!(" (BIC {b})"))
            .unwrap_or_default();
        canvas.text(false, 9.5, MARGIN, &format!("{} {iban}{bic}", t("pay_to")));
        canvas.down(14.0);
    }
    canvas.text(
        true,
        9.5,
        MARGIN,
        &format!("{} : {}", t("reference"), invoice.payment_reference),
    );
    canvas.down(14.0);
    canvas.text(
        false,
        9.5,
        MARGIN,
        &format!(
            "{} : {}",
            t("due_date"),
            i18n::date(locale, invoice.due_date)
        ),
    );
}

/// The invoice's PDF (A4, as many pages as its lines need), with the store's
/// logo when it has one
pub fn invoice_pdf(invoice: &Invoice, logo: Option<&PdfLogo>) -> Result<Vec<u8>> {
    let mut warnings = Vec::new();
    let regular = ParsedFont::from_bytes(HELVETICA_REGULAR, 0, &mut warnings)
        .ok_or_else(|| Error::internal("Failed to parse embedded regular font"))?;
    let bold = ParsedFont::from_bytes(HELVETICA_BOLD, 0, &mut warnings)
        .ok_or_else(|| Error::internal("Failed to parse embedded bold font"))?;
    let mut doc = PdfDocument::new(&invoice.number);
    let mut canvas = Canvas {
        pages: Vec::new(),
        ops: Vec::new(),
        y: PAGE_HEIGHT_PT - 50.0,
        regular: PdfFontHandle::External(doc.add_font(&regular)),
        bold: PdfFontHandle::External(doc.add_font(&bold)),
        metrics: (regular, bold),
    };
    let logo_height = logo.map_or(0.0, |logo| {
        logo.draw(&mut doc, &mut canvas.ops, (MARGIN, canvas.y), LOGO_BOX)
    });
    header(&mut canvas, invoice, logo_height);
    lines_table(&mut canvas, invoice);
    totals(&mut canvas, invoice);
    payment(&mut canvas, invoice);
    let footer = format!("{} · {}", invoice.seller.name, invoice.number);
    canvas.pages.push(std::mem::take(&mut canvas.ops));
    for ops in canvas.pages.iter_mut() {
        write_text(ops, canvas.regular.clone(), 7.5, MARGIN, 40.0, &footer);
        draw_line(ops, MARGIN, 52.0, RIGHT_EDGE, 52.0, 0.4, gray());
    }
    for ops in canvas.pages {
        doc.pages.push(PdfPage::new(Mm(210.0), Mm(297.0), ops));
    }
    let mut save_warnings = Vec::new();
    Ok(doc.save(&PdfSaveOptions::default(), &mut save_warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invoice_renders_as_a_pdf_of_one_or_more_pages() {
        let mut invoice = crate::peppol::ubl::tests::sample();
        let pdf = invoice_pdf(&invoice, None).unwrap();
        assert!(pdf.starts_with(b"%PDF"));
        let line = invoice.lines.0[0].clone();
        invoice.lines.0 = vec![line; 60];
        let long = invoice_pdf(&invoice, None).unwrap();
        assert!(long.len() > pdf.len());
    }

    #[test]
    fn a_store_logo_is_embedded() {
        let invoice = crate::peppol::ubl::tests::sample();
        let logo = PdfLogo::from_png(super::super::LOGO_PNG).unwrap();
        let plain = invoice_pdf(&invoice, None).unwrap();
        let with_logo = invoice_pdf(&invoice, Some(&logo)).unwrap();
        assert!(with_logo.len() > plain.len() + 1000);
    }

    #[test]
    fn text_is_measured_with_the_font() {
        let mut warnings = Vec::new();
        let font = ParsedFont::from_bytes(HELVETICA_REGULAR, 0, &mut warnings).unwrap();
        // Helvetica: "i" is narrow, "M" wide; 10 pt "MM" is about 16.7 pt
        assert!(measure(&font, 10.0, "iiii") < measure(&font, 10.0, "MM"));
        assert!((measure(&font, 10.0, "MM") - 16.7).abs() < 0.5);
    }
}
