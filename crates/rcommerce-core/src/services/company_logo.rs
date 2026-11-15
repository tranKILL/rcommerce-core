//! Company logos: a business customer's logo, printed beside their name on
//! the documents addressed to them (order summaries, invoices, credit notes).
//!
//! An upload (PNG, JPEG or WebP, at most 5 MB and 8000 pixels a side) has its
//! empty margins trimmed and is shrunk to fit 600 × 240 pixels: the buyer box
//! of a document (150 × 60 pt) at about 300 dpi. It is stored compressed: as
//! PNG (best compression) for drawings, or as JPEG (quality 85, on white) when
//! a photo-like logo would weigh more than 120 KB as PNG.
//!
//! Each upload is a new, immutable row; the company points to its current one
//! and an invoice to the one it was issued with, so issued documents never
//! change. Rows that nothing points to any more are deleted.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder, ImageFormat, ImageReader, Limits, RgbaImage};
use serde::Serialize;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::invoice::PdfLogo;
use crate::{Error, Result};

/// Largest upload accepted
pub const MAX_UPLOAD_BYTES: usize = 5 * 1024 * 1024;
/// Largest side of an upload, in pixels (bigger pictures are refused unread)
const MAX_SIDE: u32 = 8_000;
/// The box a stored logo fits in, in pixels
pub const LOGO_BOX: (u32, u32) = (600, 240);
/// Above this, a logo without transparency is stored as JPEG when smaller
const PNG_BUDGET: usize = 120 * 1024;
/// Decoded logos kept for PDFs (rows never change, so they never go stale)
const CACHE_SIZE: usize = 64;

/// A logo ready to store
#[derive(Debug, Clone)]
pub struct ProcessedLogo {
    pub bytes: Vec<u8>,
    /// `image/png` or `image/jpeg`
    pub content_type: &'static str,
    pub width: u32,
    pub height: u32,
}

/// A stored logo, as the API describes it
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct LogoInfo {
    pub id: Uuid,
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    /// Stored size in bytes
    pub size: i32,
}

/// A stored logo's picture
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LogoImage {
    pub id: Uuid,
    pub image: Vec<u8>,
    pub content_type: String,
}

fn unsupported() -> Error {
    Error::validation("The logo must be a PNG, JPEG or WebP picture")
}

/// Decode, trim, shrink and compress an upload
pub fn process(upload: &[u8]) -> Result<ProcessedLogo> {
    if upload.len() > MAX_UPLOAD_BYTES {
        return Err(Error::validation("The logo must be at most 5 MB"));
    }
    let format = image::guess_format(upload).map_err(|_| unsupported())?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        return Err(unsupported());
    }
    let mut reader = ImageReader::with_format(Cursor::new(upload), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|_| Error::validation("The logo could not be read as a picture"))?;
    let trimmed = trim(decoded.to_rgba8());
    let (width, height) = trimmed.dimensions();
    let fitted = if width > LOGO_BOX.0 || height > LOGO_BOX.1 {
        DynamicImage::ImageRgba8(trimmed)
            .resize(LOGO_BOX.0, LOGO_BOX.1, FilterType::Lanczos3)
            .to_rgba8()
    } else {
        trimmed
    };
    encode(&fitted)
}

/// Whether a pixel is background: transparent, or near white when the
/// picture has no transparency at all
fn background(pixel: &image::Rgba<u8>, transparent: bool) -> bool {
    if transparent {
        pixel[3] < 8
    } else {
        pixel[0] >= 245 && pixel[1] >= 245 && pixel[2] >= 245
    }
}

/// Cut the empty margins around the logo, keeping 2 % of a margin
fn trim(image: RgbaImage) -> RgbaImage {
    let transparent = image.pixels().any(|p| p[3] < 255);
    let (width, height) = image.dimensions();
    let (mut left, mut top, mut right, mut bottom) = (width, height, 0, 0);
    for (x, y, pixel) in image.enumerate_pixels() {
        if !background(pixel, transparent) {
            left = left.min(x);
            top = top.min(y);
            right = right.max(x);
            bottom = bottom.max(y);
        }
    }
    if left > right || top > bottom {
        // all background: keep it as it is
        return image;
    }
    let pad = (width.max(height) / 50).max(1);
    let (x0, y0) = (left.saturating_sub(pad), top.saturating_sub(pad));
    let (x1, y1) = ((right + pad).min(width - 1), (bottom + pad).min(height - 1));
    image::imageops::crop_imm(&image, x0, y0, x1 - x0 + 1, y1 - y0 + 1).to_image()
}

/// Compress: PNG, or JPEG on white for a heavy logo without transparency
fn encode(image: &RgbaImage) -> Result<ProcessedLogo> {
    let (width, height) = image.dimensions();
    let opaque = image.pixels().all(|p| p[3] == 255);
    let failed = |e: image::ImageError| Error::internal(format!("Logo encoding failed: {e}"));
    let mut png = Vec::new();
    let encoder =
        PngEncoder::new_with_quality(&mut png, CompressionType::Best, PngFilter::Adaptive);
    if opaque {
        let rgb = DynamicImage::ImageRgba8(image.clone()).to_rgb8();
        encoder
            .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
            .map_err(failed)?;
    } else {
        encoder
            .write_image(image, width, height, image::ExtendedColorType::Rgba8)
            .map_err(failed)?;
    }
    if png.len() > PNG_BUDGET && opaque {
        let rgb = DynamicImage::ImageRgba8(image.clone()).to_rgb8();
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 85)
            .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
            .map_err(failed)?;
        if jpeg.len() < png.len() {
            return Ok(ProcessedLogo {
                bytes: jpeg,
                content_type: "image/jpeg",
                width,
                height,
            });
        }
    }
    Ok(ProcessedLogo {
        bytes: png,
        content_type: "image/png",
        width,
        height,
    })
}

/// Company logos: upload, read, remove, and decoded for PDFs
#[derive(Clone)]
pub struct CompanyLogos {
    pool: PgPool,
    cache: Arc<Mutex<HashMap<Uuid, PdfLogo>>>,
}

impl CompanyLogos {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Store a new logo for `company` (processed off the async threads) and
    /// make it the current one
    pub async fn set(&self, company: Uuid, upload: Vec<u8>, by: Option<Uuid>) -> Result<LogoInfo> {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM companies WHERE id = $1)")
                .bind(company)
                .fetch_one(&self.pool)
                .await?;
        if !exists {
            return Err(Error::not_found("Company not found"));
        }
        let logo = tokio::task::spawn_blocking(move || process(&upload))
            .await
            .map_err(|e| Error::internal(format!("Logo processing stopped: {e}")))??;
        let mut tx = self.pool.begin().await?;
        let info: LogoInfo = sqlx::query_as(
            "INSERT INTO company_logos (company_id, image, content_type, width, height, uploaded_by) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             RETURNING id, content_type, width, height, octet_length(image) AS size",
        )
        .bind(company)
        .bind(&logo.bytes)
        .bind(logo.content_type)
        .bind(logo.width as i32)
        .bind(logo.height as i32)
        .bind(by)
        .fetch_one(&mut *tx)
        .await?;
        let updated =
            sqlx::query("UPDATE companies SET logo_id = $2, updated_at = NOW() WHERE id = $1")
                .bind(company)
                .bind(info.id)
                .execute(&mut *tx)
                .await?;
        if updated.rows_affected() == 0 {
            return Err(Error::not_found("Company not found"));
        }
        Self::prune(&mut tx, company).await?;
        tx.commit().await?;
        Ok(info)
    }

    /// Remove the company's logo (documents already issued keep theirs)
    pub async fn remove(&self, company: Uuid) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE companies SET logo_id = NULL, updated_at = NOW() WHERE id = $1")
            .bind(company)
            .execute(&mut *tx)
            .await?;
        Self::prune(&mut tx, company).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Delete the company's logos that neither it nor an invoice points to
    async fn prune(conn: &mut PgConnection, company: Uuid) -> Result<()> {
        sqlx::query(
            "DELETE FROM company_logos l WHERE l.company_id = $1 \
             AND NOT EXISTS (SELECT 1 FROM companies c WHERE c.logo_id = l.id) \
             AND NOT EXISTS (SELECT 1 FROM invoices i WHERE i.buyer_logo_id = l.id)",
        )
        .bind(company)
        .execute(conn)
        .await?;
        Ok(())
    }

    /// The company's current logo, if any
    pub async fn image(&self, company: Uuid) -> Result<Option<LogoImage>> {
        Ok(sqlx::query_as(
            "SELECT l.id, l.image, l.content_type FROM companies c \
             JOIN company_logos l ON l.id = c.logo_id WHERE c.id = $1",
        )
        .bind(company)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The logo of the company an order was placed for, ready for its PDF
    pub async fn for_order(&self, order: Uuid) -> Result<Option<PdfLogo>> {
        let id: Option<Uuid> = sqlx::query_scalar(
            "SELECT c.logo_id FROM orders o JOIN companies c ON c.id = o.company_id WHERE o.id = $1",
        )
        .bind(order)
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        match id {
            Some(id) => self.pdf_logo(id).await,
            None => Ok(None),
        }
    }

    /// One logo decoded for PDFs (kept for the next documents)
    pub async fn pdf_logo(&self, id: Uuid) -> Result<Option<PdfLogo>> {
        if let Some(logo) = self.cache.lock().ok().and_then(|c| c.get(&id).cloned()) {
            return Ok(Some(logo));
        }
        let bytes: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT image FROM company_logos WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let logo = PdfLogo::from_bytes(&bytes, b"buyer-logo")?;
        if let Ok(mut cache) = self.cache.lock() {
            if cache.len() >= CACHE_SIZE {
                cache.clear();
            }
            cache.insert(id, logo.clone());
        }
        Ok(Some(logo))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// A red bar on a white page, as a PNG
    fn picture(width: u32, height: u32, transparent: bool) -> Vec<u8> {
        let page = if transparent {
            Rgba([0, 0, 0, 0])
        } else {
            Rgba([255, 255, 255, 255])
        };
        let mut img = RgbaImage::from_pixel(width, height, page);
        for x in width / 4..width * 3 / 4 {
            for y in height / 3..height * 2 / 3 {
                img.put_pixel(x, y, Rgba([236, 26, 37, 255]));
            }
        }
        let mut out = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn a_big_logo_is_trimmed_shrunk_and_small() {
        let upload = picture(3000, 3000, false);
        let logo = process(&upload).unwrap();
        // the white margins are cut: the red bar is 1500 x 1000, then fitted
        assert!(logo.width <= LOGO_BOX.0 && logo.height <= LOGO_BOX.1);
        assert_eq!(logo.height, LOGO_BOX.1);
        assert!(logo.width > logo.height, "{}x{}", logo.width, logo.height);
        assert_eq!(logo.content_type, "image/png");
        assert!(logo.bytes.len() < 20_000, "{} bytes", logo.bytes.len());
        assert!(PdfLogo::from_bytes(&logo.bytes, b"t").is_ok());
    }

    #[test]
    fn transparency_is_kept_and_only_transparent_margins_are_cut() {
        let logo = process(&picture(400, 300, true)).unwrap();
        let decoded = image::load_from_memory(&logo.bytes).unwrap().to_rgba8();
        assert!(
            decoded.pixels().any(|p| p[3] == 0),
            "transparent margin kept"
        );
        assert!(logo.width < 400 && logo.height < 300);
    }

    #[test]
    fn a_noisy_photo_is_stored_as_jpeg() {
        let mut img = RgbaImage::new(600, 240);
        let mut seed = 7_u32;
        for pixel in img.pixels_mut() {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let [a, b, c, _] = seed.to_le_bytes();
            *pixel = Rgba([a, b, c, 255]);
        }
        let mut upload = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut upload), ImageFormat::Png)
            .unwrap();
        let logo = process(&upload).unwrap();
        assert_eq!(logo.content_type, "image/jpeg");
        assert!(logo.bytes.len() < upload.len());
    }

    #[test]
    fn other_files_are_refused() {
        assert!(process(b"<svg xmlns='http://www.w3.org/2000/svg'/>").is_err());
        assert!(process(b"GIF89a....").is_err());
        assert!(process(&[0_u8; 16]).is_err());
        let mut broken = picture(50, 50, false);
        broken.truncate(60);
        assert!(process(&broken).is_err());
        assert!(process(&vec![0_u8; MAX_UPLOAD_BYTES + 1]).is_err());
    }
}
