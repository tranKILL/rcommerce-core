//! Invoice data types and PDF generation for order invoices.

pub mod legal;

use std::sync::Arc;

use crate::notification::i18n;
use crate::{Error, Result};
use printpdf::{
    Color, Line, LinePoint, Mm, Op, ParsedFont, PdfDocument, PdfFontHandle, PdfPage,
    PdfSaveOptions, Point, Pt, RawImage, RawImageData, RawImageFormat, Rgb, TextItem,
    XObjectTransform,
};

const HELVETICA_REGULAR: &[u8] = include_bytes!("../../assets/fonts/Helvetica-Regular.ttf");
const HELVETICA_BOLD: &[u8] = include_bytes!("../../assets/fonts/Helvetica-Bold.ttf");
const LOGO_PNG: &[u8] = include_bytes!("../../assets/88-logo.png");

#[derive(Debug, Clone)]
pub struct InvoiceData {
    pub order_number: String,
    pub order_date: String,
    pub status: String,
    pub currency: String,
    pub customer_email: String,
    pub billing: Option<InvoiceAddress>,
    pub shipping: Option<InvoiceAddress>,
    pub items: Vec<InvoiceItem>,
    pub subtotal: String,
    /// Discount amount formatted as money ("12.75"); None = no discount line
    pub discount_total: Option<String>,
    /// Coupon code that produced the discount ("SUMMER20"); display only
    pub coupon_code: Option<String>,
    pub shipping_total: String,
    pub tax_total: String,
    pub total: String,
    /// How the order is paid, ready to print; empty leaves the row out
    pub payment_method: String,
    pub shipping_method: Option<String>,
    /// Language of the texts, dates and amounts (`fr`, `nl-BE`, `en`…)
    pub locale: String,
    /// The store issues legal invoices of its own: this document is then an
    /// order summary, titled as such and saying it is not an invoice
    pub summary_only: bool,
    /// How the order is handed over, ready to print ("Collect at: South depot")
    pub fulfilment: Option<String>,
    /// Store brand name (invoice header); empty falls back to the default
    pub brand_name: String,
    /// Store support email (invoice footer); empty falls back to the default
    pub brand_email: String,
    /// Store logo (top left); None prints the built-in one
    pub logo: Option<PdfLogo>,
    /// Store address under its name (top right); empty prints the built-in one
    pub company_lines: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct InvoiceAddress {
    pub first_name: String,
    pub last_name: String,
    pub address_1: String,
    pub address_2: Option<String>,
    pub city: String,
    pub state: Option<String>,
    pub postcode: String,
    pub country: String,
    pub phone: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InvoiceItem {
    pub name: String,
    /// Variant/option selection, e.g. "Degree: 50" (None = no variant line)
    pub variant_title: Option<String>,
    pub sku: Option<String>,
    pub quantity: i32,
    pub unit_price: String,
    pub total: String,
    /// Pre-formatted weight, e.g. "0.49kg" (None = no weight line)
    pub weight: Option<String>,
}

pub fn format_address(addr: &InvoiceAddress) -> Vec<String> {
    let mut lines = vec![format!("{} {}", addr.first_name, addr.last_name)];
    if let Some(ref addr2) = addr.address_2 {
        if !addr2.is_empty() {
            lines.push(addr2.clone());
        }
    }
    lines.push(addr.address_1.clone());
    // "City, ST 12345" with a state; "1000 City" (European order) without
    let city_line = match addr.state.as_deref().filter(|s| !s.is_empty()) {
        Some(state) => format!("{}, {} {}", addr.city, state, addr.postcode),
        None => format!("{} {}", addr.postcode, addr.city),
    };
    lines.push(city_line);
    lines.push(addr.country.clone());
    if let Some(ref phone) = addr.phone {
        if !phone.is_empty() {
            lines.push(phone.clone());
        }
    }
    lines
}

impl InvoiceData {
    /// A text of the order document in the document's language
    pub fn text(&self, key: &str) -> String {
        i18n::text(&self.locale, "order_document", key)
    }

    /// An amount ("1234.5") as written in the document's language
    pub fn money(&self, amount: &str) -> String {
        match amount.parse::<rust_decimal::Decimal>() {
            Ok(value) => i18n::money(&self.locale, value, &self.currency),
            Err(_) => format!("{}{}", currency_symbol(&self.currency), amount),
        }
    }

    /// The document's title: an invoice, or an order summary when the store
    /// issues legal invoices separately
    pub fn title(&self) -> String {
        self.text(if self.summary_only {
            "title_summary"
        } else {
            "title_invoice"
        })
    }

    /// File name of the PDF, in the document's language
    pub fn file_name(&self) -> String {
        let stem = self.text(if self.summary_only {
            "file_summary"
        } else {
            "file_invoice"
        });
        format!("{stem}-{}.pdf", self.order_number)
    }

    /// The reference rows under the address: label and value
    fn reference_rows(&self) -> Vec<(String, String)> {
        let today = i18n::date(&self.locale, chrono::Utc::now().date_naive());
        let mut rows = vec![
            (self.text("document_date"), today),
            (self.text("order_number"), self.order_number.clone()),
            (self.text("order_date"), self.order_date.clone()),
        ];
        if !self.payment_method.is_empty() {
            rows.push((self.text("payment"), self.payment_method.clone()));
        }
        rows
    }
}

const PAGE_WIDTH_PT: f32 = 595.28;
const PAGE_HEIGHT_PT: f32 = 841.89;
const MARGIN: f32 = 50.0;
const RIGHT_EDGE: f32 = PAGE_WIDTH_PT - MARGIN;
/// Shared grid: Quantity column x (also Ship To / totals labels)
const QTY_X: f32 = 350.0;
/// Shared grid: Price column x (also totals values)
const PRICE_X: f32 = 450.0;
/// Right-hand blocks (company, meta) sit left of the Quantity grid to leave
/// room for long order numbers.
const COMPANY_X: f32 = 332.0;
/// All money amounts right-align at this x, a few pt in from the page edge.
const MONEY_RIGHT: f32 = RIGHT_EDGE - 6.0;

fn rgb(r: f32, g: f32, b: f32) -> Color {
    Color::Rgb(Rgb::new(r, g, b, None))
}

fn black() -> Color {
    rgb(0.1, 0.1, 0.1)
}

fn gray() -> Color {
    rgb(0.45, 0.45, 0.45)
}

fn light_gray() -> Color {
    rgb(0.82, 0.82, 0.82)
}

/// Rough estimate of rendered text width (points) for wrapping/alignment.
fn estimate_width(text: &str, size: f32, bold: bool) -> f32 {
    let factor = if bold { 0.56 } else { 0.52 };
    text.chars().count() as f32 * size * factor
}

/// Currency symbol for a currency code (falls back to the code itself).
fn currency_symbol(code: &str) -> &str {
    match code {
        "USD" | "AUD" | "CAD" | "NZD" | "SGD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" | "CNY" => "¥",
        "HKD" => "HK$",
        other => other,
    }
}

/// Humanize a shipping method slug: "standard_us" -> "Standard Us".
pub fn display_shipping_method(method: &str) -> String {
    method
        .replace('_', " ")
        .split_whitespace()
        .map(|word| {
            // All-caps short region words: "us" -> "US"
            if word.len() <= 2 && word.chars().all(|c| c.is_ascii_alphabetic()) {
                return word.to_uppercase();
            }
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn wrap_text(text: &str, size: f32, max_width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{} {}", current, word)
        };
        if estimate_width(&candidate, size, false) <= max_width || current.is_empty() {
            current = candidate;
        } else {
            lines.push(current);
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn write_text(ops: &mut Vec<Op>, font: PdfFontHandle, size: f32, x: f32, y: f32, text: &str) {
    ops.push(Op::StartTextSection);
    ops.push(Op::SetFont {
        font,
        size: Pt(size),
    });
    ops.push(Op::SetTextCursor {
        pos: Point { x: Pt(x), y: Pt(y) },
    });
    ops.push(Op::ShowText {
        items: vec![TextItem::Text(text.to_string())],
    });
    ops.push(Op::EndTextSection);
}

fn draw_line(ops: &mut Vec<Op>, x1: f32, y1: f32, x2: f32, y2: f32, thickness: f32, color: Color) {
    ops.push(Op::SaveGraphicsState);
    ops.push(Op::SetOutlineColor { col: color });
    ops.push(Op::SetOutlineThickness { pt: Pt(thickness) });
    ops.push(Op::DrawLine {
        line: Line {
            points: vec![
                LinePoint {
                    p: Point {
                        x: Pt(x1),
                        y: Pt(y1),
                    },
                    bezier: false,
                },
                LinePoint {
                    p: Point {
                        x: Pt(x2),
                        y: Pt(y2),
                    },
                    bezier: false,
                },
            ],
            is_closed: false,
        },
    });
    ops.push(Op::RestoreGraphicsState);
}

/// A logo printed at the top of PDFs, decoded once from a PNG and shared
#[derive(Clone)]
pub struct PdfLogo(Arc<RawImage>);

impl std::fmt::Debug for PdfLogo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PdfLogo({}x{})", self.0.width, self.0.height)
    }
}

impl PartialEq for PdfLogo {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for PdfLogo {}

/// 8-bit pixels of a decoded PNG as RGBA, whatever its colour type
fn rgba_pixels(buf: Vec<u8>, color: png::ColorType) -> Result<Vec<u8>> {
    let pixel = |px: &[u8]| -> [u8; 4] {
        match px.len() {
            1 => [px[0], px[0], px[0], 255],
            2 => [px[0], px[0], px[0], px[1]],
            3 => [px[0], px[1], px[2], 255],
            _ => [px[0], px[1], px[2], px[3]],
        }
    };
    let size = match color {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => return Ok(buf),
        other => {
            return Err(Error::internal(format!(
                "Unsupported logo color type: {other:?}"
            )))
        }
    };
    Ok(buf.chunks_exact(size).flat_map(pixel).collect())
}

/// Largest logo kept, in pixels: the logo box (180 x 64 pt) at about 300 dpi.
/// Bigger pictures are shrunk so every PDF stays small.
const MAX_LOGO_PIXELS: (usize, usize) = (750, 270);

/// Shrink RGBA pixels by the smallest whole factor that fits `max`, each new
/// pixel the average of a block; returns the pixels and their size
fn shrink(
    pixels: Vec<u8>,
    (width, height): (usize, usize),
    max: (usize, usize),
) -> (Vec<u8>, usize, usize) {
    let factor = width.div_ceil(max.0).max(height.div_ceil(max.1)).max(1);
    if factor == 1 {
        return (pixels, width, height);
    }
    let (new_width, new_height) = (width / factor, height / factor);
    let mut out = Vec::with_capacity(new_width * new_height * 4);
    for y in 0..new_height {
        for x in 0..new_width {
            let mut sum = [0u32; 4];
            for dy in 0..factor {
                let row = ((y * factor + dy) * width + x * factor) * 4;
                for px in pixels[row..row + factor * 4].chunks_exact(4) {
                    for (total, value) in sum.iter_mut().zip(px) {
                        *total += u32::from(*value);
                    }
                }
            }
            let count = (factor * factor) as u32;
            out.extend(sum.iter().map(|total| (total / count) as u8));
        }
    }
    (out, new_width, new_height)
}

impl PdfLogo {
    /// Decode a PNG (any colour type and bit depth; palettes are expanded),
    /// shrunk to at most 750 x 270 pixels
    pub fn from_png(bytes: &[u8]) -> Result<Self> {
        let mut decoder = png::Decoder::new(bytes);
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder
            .read_info()
            .map_err(|e| Error::internal(format!("Failed to read logo PNG: {e}")))?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader
            .next_frame(&mut buf)
            .map_err(|e| Error::internal(format!("Failed to decode logo PNG: {e}")))?;
        buf.truncate(info.buffer_size());
        let size = (info.width as usize, info.height as usize);
        let (pixels, width, height) =
            shrink(rgba_pixels(buf, info.color_type)?, size, MAX_LOGO_PIXELS);
        Ok(Self(Arc::new(RawImage {
            pixels: RawImageData::U8(pixels),
            width,
            height,
            data_format: RawImageFormat::RGBA8,
            tag: b"store-logo".to_vec(),
        })))
    }

    /// Read and decode a PNG file
    pub fn load(path: &str) -> Result<Self> {
        let bytes = std::fs::read(path)
            .map_err(|e| Error::internal(format!("Cannot read the logo {path}: {e}")))?;
        Self::from_png(&bytes)
    }

    /// rCommerce's own logo, printed when a store has none
    fn built_in() -> Result<Self> {
        Self::from_png(LOGO_PNG)
    }

    /// Width and height in points once fitted inside `max_width` x
    /// `max_height` (proportions kept), and the DPI that gives that size
    fn fit(&self, max_width: f32, max_height: f32) -> (f32, f32, f32) {
        let (width, height) = (self.0.width as f32, self.0.height as f32);
        let points_per_pixel = (max_width / width).min(max_height / height);
        (
            width * points_per_pixel,
            height * points_per_pixel,
            72.0 / points_per_pixel,
        )
    }

    /// Place the logo with its top left corner at (`x`, `top`), fitted inside
    /// the box; returns its height in points
    pub(crate) fn draw(
        &self,
        doc: &mut PdfDocument,
        ops: &mut Vec<Op>,
        (x, top): (f32, f32),
        (max_width, max_height): (f32, f32),
    ) -> f32 {
        let (_, height, dpi) = self.fit(max_width, max_height);
        let id = doc.add_image(&self.0);
        ops.push(Op::UseXobject {
            id,
            transform: XObjectTransform {
                translate_x: Some(Pt(x)),
                translate_y: Some(Pt(top - height)),
                rotate: None,
                scale_x: None,
                scale_y: None,
                dpi: Some(dpi),
            },
        });
        height
    }
}

/// Generate a single-page A4 PDF invoice (mimics the WooCommerce reference).
pub fn generate_invoice_pdf(data: &InvoiceData) -> Result<Vec<u8>> {
    let mut font_warnings = Vec::new();
    let regular = ParsedFont::from_bytes(HELVETICA_REGULAR, 0, &mut font_warnings)
        .ok_or_else(|| Error::internal("Failed to parse embedded regular font"))?;
    let bold = ParsedFont::from_bytes(HELVETICA_BOLD, 0, &mut font_warnings)
        .ok_or_else(|| Error::internal("Failed to parse embedded bold font"))?;

    let mut doc = PdfDocument::new(&format!("{} {}", data.title(), data.order_number));
    let regular_id = doc.add_font(&regular);
    let bold_id = doc.add_font(&bold);
    let regular = PdfFontHandle::External(regular_id);
    let bold = PdfFontHandle::External(bold_id);

    let mut ops: Vec<Op> = Vec::new();
    let mut y = PAGE_HEIGHT_PT - 55.0;

    // Row 1: logo (top left, sole brand element) | company block (top right)
    let logo_height_pt = match &data.logo {
        Some(logo) => logo.draw(&mut doc, &mut ops, (MARGIN, y), (180.0, 60.0)),
        None => PdfLogo::built_in()?.draw(&mut doc, &mut ops, (MARGIN, y), (46.0, 46.0)),
    };
    let brand_name = if data.brand_name.is_empty() {
        "rCommerce"
    } else {
        &data.brand_name
    };
    write_text(
        &mut ops,
        bold.clone(),
        10.5,
        COMPANY_X,
        y - 11.0,
        brand_name,
    );
    let built_in_lines = [
        "2301 Bayfield Building,",
        "99 Hennessy Road,",
        "Wanchai, Hong Kong",
    ];
    let company_lines: Vec<&str> = if data.company_lines.is_empty() {
        built_in_lines.to_vec()
    } else {
        data.company_lines.iter().map(String::as_str).collect()
    };
    for (i, line) in company_lines.iter().enumerate() {
        write_text(
            &mut ops,
            regular.clone(),
            9.5,
            COMPANY_X,
            y - 25.0 - i as f32 * 13.0,
            line,
        );
    }

    // Row 2: INVOICE heading (left)
    let invoice_y = y - logo_height_pt - 42.0;
    let title = data.title().to_uppercase();
    write_text(&mut ops, bold.clone(), 21.0, MARGIN, invoice_y, &title);

    // Row 3: single shipping address directly under INVOICE (plain lines, no
    // label); the meta block sits to the right at the same vertical level.
    let address_top = invoice_y - 40.0;
    let address_lines = data
        .shipping
        .clone()
        .or_else(|| data.billing.clone())
        .map(|a| format_address(&a))
        .unwrap_or_else(|| vec![data.text("no_address")]);
    let address_lines = match &data.fulfilment {
        Some(line) => [address_lines, vec![String::new(), line.clone()]].concat(),
        None => address_lines,
    };
    for (i, line) in address_lines.iter().enumerate() {
        write_text(
            &mut ops,
            regular.clone(),
            9.5,
            MARGIN,
            address_top - i as f32 * 13.0,
            line,
        );
    }
    // Labels, colons and values each in their own column
    let meta_rows = data.reference_rows();
    let widest = meta_rows
        .iter()
        .map(|(label, _)| estimate_width(label, 9.5, false))
        .fold(0.0, f32::max);
    let colon_x = COMPANY_X + widest + 4.0;
    for (i, (label, value)) in meta_rows.iter().enumerate() {
        let row_y = address_top - i as f32 * 14.0;
        write_text(&mut ops, regular.clone(), 9.5, COMPANY_X, row_y, label);
        write_text(&mut ops, regular.clone(), 9.5, colon_x, row_y, ":");
        write_text(&mut ops, regular.clone(), 9.5, colon_x + 7.0, row_y, value);
    }

    // Items table with a solid black header bar (white bold column titles)
    let block_height = (address_lines.len() as f32 * 13.0).max(meta_rows.len() as f32 * 14.0);
    let bar_top = address_top - block_height - 22.0;
    let bar_height = 24.0;
    ops.push(Op::SetFillColor { col: black() });
    ops.push(Op::DrawRectangle {
        rectangle: printpdf::Rect {
            x: Pt(MARGIN),
            y: Pt(bar_top - bar_height),
            width: Pt(RIGHT_EDGE - MARGIN),
            height: Pt(bar_height),
            mode: Some(printpdf::PaintMode::Fill),
            winding_order: None,
        },
    });
    ops.push(Op::SetFillColor {
        col: Color::Rgb(Rgb::new(1.0, 1.0, 1.0, None)),
    });
    write_text(
        &mut ops,
        bold.clone(),
        9.5,
        MARGIN + 6.0,
        bar_top - 16.5,
        &data.text("product"),
    );
    write_text(
        &mut ops,
        bold.clone(),
        9.5,
        QTY_X,
        bar_top - 16.5,
        &data.text("quantity"),
    );
    let price_header = data.text("price");
    write_text(
        &mut ops,
        bold.clone(),
        9.5,
        MONEY_RIGHT - estimate_width(&price_header, 9.5, true),
        bar_top - 16.5,
        &price_header,
    );
    ops.push(Op::SetFillColor { col: black() });

    // Item row content sits slightly inset from the table edge (like the
    // padding inside the black header bar); Quantity stays on its grid column
    // and Price amounts right-align at the page edge.
    let item_x = MARGIN + 6.0;
    y = bar_top - bar_height - 18.0;
    for item in &data.items {
        if y < 200.0 {
            break;
        }
        let name_lines = wrap_text(&item.name, 9.5, QTY_X - item_x - 10.0);
        for (i, line) in name_lines.iter().enumerate() {
            write_text(
                &mut ops,
                regular.clone(),
                9.5,
                item_x,
                y - i as f32 * 12.5,
                line,
            );
        }
        let qty = format!("{}", item.quantity);
        write_text(&mut ops, regular.clone(), 9.5, QTY_X, y, &qty);
        let price = data.money(&item.total);
        write_text(
            &mut ops,
            regular.clone(),
            9.5,
            MONEY_RIGHT - estimate_width(&price, 9.5, false),
            y,
            &price,
        );

        // Fixed sub-line grid: the variant selection sits 11pt below the name
        // (shifting the SKU/weight slots down), the SKU slot is always
        // reserved, and the weight slot 10.5pt below that (also always
        // reserved, so every row has the same height whether or not the
        // individual lines are drawn).
        let name_bottom = y - (name_lines.len() - 1) as f32 * 12.5;
        let mut sub_y = name_bottom - 11.0;
        if let Some(ref variant) = item.variant_title {
            if !variant.is_empty() {
                write_text(&mut ops, regular.clone(), 8.0, item_x, sub_y, variant);
                sub_y -= 10.5;
            }
        }
        let sku_y = sub_y;
        if let Some(ref sku) = item.sku {
            if !sku.is_empty() {
                write_text(
                    &mut ops,
                    regular.clone(),
                    8.0,
                    item_x,
                    sku_y,
                    &data.text("sku_line").replace("{value}", sku),
                );
            }
        }
        let weight_y = sku_y - 10.5;
        if let Some(ref weight) = item.weight {
            if !weight.is_empty() {
                write_text(
                    &mut ops,
                    regular.clone(),
                    8.0,
                    item_x,
                    weight_y,
                    &data.text("weight_line").replace("{value}", weight),
                );
            }
        }
        y = weight_y - 12.0;
        draw_line(&mut ops, MARGIN, y, RIGHT_EDGE, y, 0.4, light_gray());
        y -= 12.0;
    }

    // Totals block under the Quantity/Price columns: bold labels left at the
    // Quantity column x, amounts right-aligned at the (slightly inset) money
    // column, one uniform row rhythm, thin rule under Subtotal, thick rules
    // snug above and below Total.
    let row_pitch = 17.0;
    y -= 8.0;

    write_text(
        &mut ops,
        bold.clone(),
        9.5,
        QTY_X,
        y,
        &data.text("subtotal"),
    );
    let subtotal_value = data.money(&data.subtotal);
    write_text(
        &mut ops,
        regular.clone(),
        9.5,
        MONEY_RIGHT - estimate_width(&subtotal_value, 9.5, false),
        y,
        &subtotal_value,
    );
    draw_line(
        &mut ops,
        QTY_X,
        y - 6.0,
        MONEY_RIGHT,
        y - 6.0,
        0.5,
        light_gray(),
    );
    y -= row_pitch;

    // Discount row: only when a discount was applied, shown as a negative
    // amount in green, labelled with the coupon code when known.
    let discount_amount = data
        .discount_total
        .as_deref()
        .and_then(|d| d.parse::<rust_decimal::Decimal>().ok())
        .filter(|d| *d > rust_decimal::Decimal::ZERO);
    if let Some(discount) = discount_amount {
        let label = match data.coupon_code.as_deref() {
            Some(code) if !code.is_empty() => data.text("discount_code").replace("{code}", code),
            _ => data.text("discount"),
        };
        write_text(&mut ops, bold.clone(), 9.5, QTY_X, y, &label);
        let discount_value = data.money(&(-discount).to_string());
        ops.push(Op::SetFillColor {
            col: rgb(0.02, 0.59, 0.41), // #059669
        });
        write_text(
            &mut ops,
            regular.clone(),
            9.5,
            MONEY_RIGHT - estimate_width(&discount_value, 9.5, false),
            y,
            &discount_value,
        );
        ops.push(Op::SetFillColor { col: black() });
        y -= row_pitch;
    }

    write_text(
        &mut ops,
        bold.clone(),
        9.5,
        QTY_X,
        y,
        &data.text("shipping"),
    );
    let shipping_value = data.money(&data.shipping_total);
    write_text(
        &mut ops,
        regular.clone(),
        9.5,
        MONEY_RIGHT - estimate_width(&shipping_value, 9.5, false),
        y,
        &shipping_value,
    );
    y -= row_pitch;

    if data.tax_total != "0.00" {
        write_text(&mut ops, bold.clone(), 9.5, QTY_X, y, &data.text("tax"));
        let tax_value = data.money(&data.tax_total);
        write_text(
            &mut ops,
            regular.clone(),
            9.5,
            MONEY_RIGHT - estimate_width(&tax_value, 9.5, false),
            y,
            &tax_value,
        );
        y -= row_pitch;
    }

    y -= 6.0;
    draw_line(&mut ops, QTY_X, y, MONEY_RIGHT, y, 2.0, black());
    y -= 12.0;
    let total_value = data.money(&data.total);
    write_text(&mut ops, bold.clone(), 10.5, QTY_X, y, &data.text("total"));
    write_text(
        &mut ops,
        bold.clone(),
        10.5,
        MONEY_RIGHT - estimate_width(&total_value, 10.5, true),
        y,
        &total_value,
    );
    y -= 12.0;
    draw_line(&mut ops, QTY_X, y, MONEY_RIGHT, y, 2.0, black());

    // Footer pinned to the bottom of the page, centered.
    let footer_rule_y = 100.0;
    draw_line(
        &mut ops,
        MARGIN,
        footer_rule_y,
        RIGHT_EDGE,
        footer_rule_y,
        0.4,
        light_gray(),
    );
    let brand_email = if data.brand_email.is_empty() {
        "support@example.com"
    } else {
        &data.brand_email
    };
    let contact = data.text("thanks").replace("{email}", brand_email);
    let footer = if data.summary_only {
        data.text("not_an_invoice")
    } else {
        String::new()
    };
    let footer = footer.as_str();
    ops.push(Op::SetFillColor { col: gray() });
    write_text(
        &mut ops,
        regular.clone(),
        9.0,
        (PAGE_WIDTH_PT - estimate_width(&contact, 9.0, false)) / 2.0,
        footer_rule_y - 15.0,
        &contact,
    );
    write_text(
        &mut ops,
        regular.clone(),
        9.0,
        (PAGE_WIDTH_PT - estimate_width(footer, 9.0, false)) / 2.0,
        footer_rule_y - 29.0,
        footer,
    );
    ops.push(Op::SetFillColor { col: black() });

    doc.pages.push(PdfPage::new(Mm(210.0), Mm(297.0), ops));

    let mut save_warnings = Vec::new();
    Ok(doc.save(&PdfSaveOptions::default(), &mut save_warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_invoice() -> InvoiceData {
        InvoiceData {
            order_number: "ORD123456".to_string(),
            order_date: "October 01, 2026".to_string(),
            status: "completed".to_string(),
            currency: "USD".to_string(),
            customer_email: "customer@example.com".to_string(),
            billing: Some(InvoiceAddress {
                first_name: "John".to_string(),
                last_name: "Doe".to_string(),
                address_1: "123 Main Street".to_string(),
                address_2: None,
                city: "Los Angeles".to_string(),
                state: Some("CA".to_string()),
                postcode: "90001".to_string(),
                country: "United States".to_string(),
                phone: None,
            }),
            shipping: Some(InvoiceAddress {
                first_name: "John".to_string(),
                last_name: "Doe".to_string(),
                address_1: "123 Main Street".to_string(),
                address_2: Some("Apt 4B".to_string()),
                city: "Los Angeles".to_string(),
                state: Some("CA".to_string()),
                postcode: "90001".to_string(),
                country: "United States".to_string(),
                phone: Some("+1 555 123 4567".to_string()),
            }),
            items: vec![
                InvoiceItem {
                    name: "Example Premium Longboard Wheels 70mm 78a Orange".to_string(),
                    variant_title: None,
                    sku: Some("WHEEL-70-ORG".to_string()),
                    quantity: 2,
                    unit_price: "45.00".to_string(),
                    total: "90.00".to_string(),
                    weight: Some("0.49kg".to_string()),
                },
                InvoiceItem {
                    name: "Caliber 3 Precision Baseplate".to_string(),
                    variant_title: Some("Degree: 50".to_string()),
                    sku: Some("CALIBER3-50".to_string()),
                    quantity: 1,
                    unit_price: "75.00".to_string(),
                    total: "75.00".to_string(),
                    weight: None,
                },
                InvoiceItem {
                    name: "Skate Tool".to_string(),
                    variant_title: None,
                    sku: Some("TOOL-01".to_string()),
                    quantity: 1,
                    unit_price: "25.00".to_string(),
                    total: "25.00".to_string(),
                    weight: None,
                },
            ],
            subtotal: "115.00".to_string(),
            discount_total: None,
            coupon_code: None,
            shipping_total: "10.00".to_string(),
            tax_total: "0.00".to_string(),
            total: "125.00".to_string(),
            payment_method: "Credit Card".to_string(),
            shipping_method: Some("Standard US Shipping".to_string()),
            brand_name: String::new(),
            brand_email: String::new(),
            logo: None,
            company_lines: Vec::new(),
            locale: "en".to_string(),
            summary_only: false,
            fulfilment: None,
        }
    }

    #[test]
    fn an_order_summary_speaks_the_order_language() {
        let data = InvoiceData {
            locale: "fr".to_string(),
            currency: "EUR".to_string(),
            summary_only: true,
            fulfilment: Some("Retrait : Dépôt Sud".to_string()),
            ..sample_invoice()
        };
        assert_eq!(data.title(), "Bon de commande");
        assert_eq!(data.file_name(), "bon-de-commande-ORD123456.pdf");
        let amount = data.money("1234.5");
        assert!(
            amount.contains("234,50") && amount.ends_with('€'),
            "{amount}"
        );
        assert_eq!(data.text("subtotal"), "Sous-total");
        let rows = data.reference_rows();
        assert_eq!(
            rows[1],
            ("Numéro de commande".to_string(), "ORD123456".to_string())
        );
        assert_eq!(&generate_invoice_pdf(&data).unwrap()[..4], b"%PDF");

        let english = sample_invoice();
        assert_eq!(english.title(), "Invoice");
        assert_eq!(english.file_name(), "invoice-ORD123456.pdf");
    }

    #[test]
    fn test_generate_invoice_pdf() {
        let pdf = generate_invoice_pdf(&sample_invoice()).expect("pdf generation failed");
        assert!(pdf.len() > 2000, "pdf too small: {}", pdf.len());
        assert_eq!(&pdf[..4], b"%PDF");
    }

    #[test]
    fn test_generate_invoice_pdf_with_variant_item() {
        let data = sample_invoice();
        let pdf = generate_invoice_pdf(&data).expect("pdf generation failed");
        assert_eq!(&pdf[..4], b"%PDF");
        // Variant item rendered without disturbing the rows around it; the
        // text is font-encoded, so we assert structural success only here.
        assert!(pdf.len() > 2000);
    }

    #[test]
    fn test_generate_invoice_pdf_with_discount() {
        let mut data = sample_invoice();
        data.discount_total = Some("12.75".to_string());
        data.coupon_code = Some("1V97NIBP40".to_string());
        data.total = "112.25".to_string();
        let pdf = generate_invoice_pdf(&data).expect("pdf generation failed");
        assert_eq!(&pdf[..4], b"%PDF");
        assert!(pdf.len() > 2000);
    }

    #[test]
    fn test_generate_invoice_pdf_zero_discount_no_line() {
        let mut data = sample_invoice();
        data.discount_total = Some("0.00".to_string());
        let pdf = generate_invoice_pdf(&data).expect("pdf generation failed");
        assert_eq!(&pdf[..4], b"%PDF");
        assert!(pdf.len() > 2000);
    }

    #[test]
    fn test_generate_invoice_pdf_minimal() {
        let mut data = sample_invoice();
        data.billing = None;
        data.shipping = None;
        data.shipping_method = None;
        data.items.clear();
        let pdf = generate_invoice_pdf(&data).expect("pdf generation failed");
        assert!(pdf.len() > 2000);
        assert_eq!(&pdf[..4], b"%PDF");
    }

    #[test]
    fn big_logos_are_shrunk_by_averaging_blocks() {
        // 4 x 2 pixels: grey levels 0, 100, 200, 50 in the top row, 0s below
        let mut pixels = Vec::new();
        for level in [0u8, 100, 200, 50, 0, 0, 0, 0] {
            pixels.extend([level, level, level, 255]);
        }
        let (small, width, height) = shrink(pixels.clone(), (4, 2), (2, 1));
        assert_eq!((width, height), (2, 1));
        assert_eq!(small, vec![25, 25, 25, 255, 62, 62, 62, 255]);
        let (same, width, height) = shrink(pixels.clone(), (4, 2), (4, 2));
        assert_eq!((same, width, height), (pixels, 4, 2));
        let logo = PdfLogo::built_in().unwrap();
        assert!(logo.0.width <= MAX_LOGO_PIXELS.0 && logo.0.height <= MAX_LOGO_PIXELS.1);
    }

    #[test]
    fn test_format_address() {
        let addr = InvoiceAddress {
            first_name: "Jane".to_string(),
            last_name: "Smith".to_string(),
            address_1: "1 Road".to_string(),
            address_2: Some("Floor 2".to_string()),
            city: "Hong Kong".to_string(),
            state: None,
            postcode: "0000".to_string(),
            country: "Hong Kong".to_string(),
            phone: None,
        };
        let lines = format_address(&addr);
        assert_eq!(lines[0], "Jane Smith");
        assert_eq!(lines[1], "Floor 2");
        assert_eq!(lines[2], "1 Road");
        assert_eq!(lines[3], "0000 Hong Kong", "postcode first without a state");
    }

    #[test]
    fn test_wrap_text() {
        let long = "A very long product name that will definitely need to wrap onto multiple lines"
            .to_string();
        let lines = wrap_text(&long, 9.5, 200.0);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(estimate_width(line, 9.5, false) <= 200.0 + 20.0);
        }
    }

    #[test]
    fn test_currency_symbol() {
        assert_eq!(currency_symbol("USD"), "$");
        assert_eq!(currency_symbol("EUR"), "€");
        assert_eq!(currency_symbol("GBP"), "£");
        assert_eq!(currency_symbol("JPY"), "¥");
        assert_eq!(currency_symbol("HKD"), "HK$");
        assert_eq!(currency_symbol("CHF"), "CHF");
    }

    #[test]
    fn test_display_shipping_method() {
        assert_eq!(display_shipping_method("standard"), "Standard");
        assert_eq!(
            display_shipping_method("standard_us_shipping"),
            "Standard US Shipping"
        );
        assert_eq!(display_shipping_method("Express"), "Express");
    }
}
