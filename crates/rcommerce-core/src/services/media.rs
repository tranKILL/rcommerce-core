//! Pictures staff upload for the site (article covers and illustrations, shop
//! photos, the default share image). An upload (PNG, JPEG or WebP, at most
//! 8 MB and 6000 pixels a side) is decoded and encoded again, which drops its
//! metadata (camera, location) and anything that is not a picture: as JPEG
//! (quality 85) when it has no transparency, else as PNG. Pictures wider or
//! taller than 2400 pixels are shrunk to fit.
//!
//! Each picture is served at `/media/<id>.jpg` (or `.png`), the address
//! articles and shops refer to. The API decodes at most two uploads at a time.

use std::io::Cursor;

use chrono::{DateTime, Utc};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder, ImageFormat, ImageReader, Limits};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{Error, Result};

/// Largest upload accepted
pub const MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024;
/// Largest side of an upload, in pixels (bigger pictures are refused unread)
const MAX_SIDE: u32 = 6_000;
/// Most memory one decode may take (an 8-bit RGBA picture of the largest
/// size fits; a 16-bit one of that size is refused)
const MAX_DECODE_BYTES: u64 = 192 * 1024 * 1024;
/// Largest side of a stored picture
const STORED_SIDE: u32 = 2_400;

/// A picture ready to store
#[derive(Debug, Clone)]
pub struct Processed {
    pub bytes: Vec<u8>,
    /// `image/jpeg` or `image/png`
    pub content_type: &'static str,
    pub width: u32,
    pub height: u32,
}

/// A stored picture, as the API lists it
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MediaInfo {
    pub id: Uuid,
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub size: i32,
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl MediaInfo {
    /// Where the picture is served: `/media/<id>.jpg` or `.png`
    pub fn url(&self) -> String {
        format!("/media/{}.{}", self.id, extension(&self.content_type))
    }
}

fn extension(content_type: &str) -> &'static str {
    if content_type == "image/png" {
        "png"
    } else {
        "jpg"
    }
}

fn unsupported() -> Error {
    Error::validation("The file must be a PNG, JPEG or WebP picture")
}

/// Decode an upload, shrink it to fit and encode it again
pub fn process(upload: &[u8]) -> Result<Processed> {
    if upload.len() > MAX_UPLOAD_BYTES {
        return Err(Error::validation("The picture must be at most 8 MB"));
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
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|_| Error::validation("The file could not be read as a picture"))?;
    // 8 bits a channel from here on: half the memory of a 16-bit picture
    let decoded = DynamicImage::ImageRgba8(decoded.into_rgba8());
    encode(&fit(decoded))
}

/// Shrink to fit `STORED_SIDE`: a cheap shrink to twice the size first, so
/// the fine (Lanczos) pass works on a small picture
fn fit(picture: DynamicImage) -> DynamicImage {
    if picture.width() <= STORED_SIDE && picture.height() <= STORED_SIDE {
        return picture;
    }
    let near = if picture.width() > 2 * STORED_SIDE || picture.height() > 2 * STORED_SIDE {
        picture.thumbnail(2 * STORED_SIDE, 2 * STORED_SIDE)
    } else {
        picture
    };
    near.resize(STORED_SIDE, STORED_SIDE, FilterType::Lanczos3)
}

/// JPEG for opaque pictures, PNG when some pixel is transparent
fn encode(picture: &DynamicImage) -> Result<Processed> {
    let rgba = picture.to_rgba8();
    let (width, height) = rgba.dimensions();
    let transparent = rgba.pixels().any(|p| p[3] < 255);
    let mut bytes = Vec::new();
    let failed = |e: image::ImageError| Error::internal(format!("Picture encoding failed: {e}"));
    let content_type = if transparent {
        PngEncoder::new_with_quality(&mut bytes, CompressionType::Best, PngFilter::Adaptive)
            .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
            .map_err(failed)?;
        "image/png"
    } else {
        let rgb = DynamicImage::ImageRgba8(rgba).to_rgb8();
        JpegEncoder::new_with_quality(&mut bytes, 85)
            .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
            .map_err(failed)?;
        "image/jpeg"
    };
    Ok(Processed {
        bytes,
        content_type,
        width,
        height,
    })
}

/// `<uuid>.jpg` or `<uuid>.png` -> the id; anything else is `None`
pub fn parse_file_name(file: &str) -> Option<(Uuid, &'static str)> {
    let (id, ext) = file.rsplit_once('.')?;
    let content_type = match ext {
        "jpg" => "image/jpeg",
        "png" => "image/png",
        _ => return None,
    };
    Some((id.parse().ok()?, content_type))
}

/// Uploaded pictures
#[derive(Clone)]
pub struct MediaLibrary {
    pool: PgPool,
}

impl MediaLibrary {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Store a processed upload under the uploader's `name` for it
    pub async fn store(
        &self,
        picture: &Processed,
        name: Option<&str>,
        staff: Uuid,
    ) -> Result<MediaInfo> {
        let name = name
            .map(|n| n.trim().chars().take(200).collect::<String>())
            .filter(|n| !n.is_empty());
        let info = sqlx::query_as::<_, MediaInfo>(
            "INSERT INTO media_files (content_type, width, height, size, image, name, created_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             RETURNING id, content_type, width, height, size, name, created_at",
        )
        .bind(picture.content_type)
        .bind(picture.width as i32)
        .bind(picture.height as i32)
        .bind(picture.bytes.len() as i32)
        .bind(&picture.bytes)
        .bind(name)
        .bind(staff)
        .fetch_one(&self.pool)
        .await?;
        Ok(info)
    }

    /// The newest pictures first
    pub async fn list(&self, limit: i64, offset: i64) -> Result<Vec<MediaInfo>> {
        Ok(sqlx::query_as::<_, MediaInfo>(
            "SELECT id, content_type, width, height, size, name, created_at FROM media_files \
             ORDER BY created_at DESC LIMIT $1 OFFSET $2",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?)
    }

    /// A picture's bytes, when `id` has that content type
    pub async fn bytes(&self, id: Uuid, content_type: &str) -> Result<Option<Vec<u8>>> {
        Ok(
            sqlx::query_scalar("SELECT image FROM media_files WHERE id = $1 AND content_type = $2")
                .bind(id)
                .bind(content_type)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Remove a picture; `false` when there was none
    pub async fn remove(&self, id: Uuid) -> Result<bool> {
        let done = sqlx::query("DELETE FROM media_files WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb, Rgba};

    fn png(width: u32, height: u32, alpha: u8) -> Vec<u8> {
        let img = ImageBuffer::from_pixel(width, height, Rgba([200u8, 80, 40, alpha]));
        let mut out = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn opaque_pictures_become_jpeg_transparent_ones_png() {
        let photo = process(&png(40, 30, 255)).unwrap();
        assert_eq!(
            (photo.content_type, photo.width, photo.height),
            ("image/jpeg", 40, 30)
        );
        assert!(photo.bytes.starts_with(&[0xFF, 0xD8]), "a JPEG");
        let drawing = process(&png(10, 10, 100)).unwrap();
        assert_eq!(drawing.content_type, "image/png");
    }

    #[test]
    fn big_pictures_shrink_and_non_pictures_are_refused() {
        let img = ImageBuffer::from_pixel(3000, 1500, Rgb([1u8, 2, 3]));
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        let shrunk = process(&out).unwrap();
        assert_eq!((shrunk.width, shrunk.height), (2400, 1200));
        assert!(process(b"<svg onload=alert(1)>").is_err());
        assert!(process(b"GIF89a....").is_err());
    }

    #[test]
    fn file_names_are_an_id_and_a_known_extension() {
        let id = Uuid::new_v4();
        assert_eq!(
            parse_file_name(&format!("{id}.jpg")),
            Some((id, "image/jpeg"))
        );
        assert!(parse_file_name(&format!("{id}.svg")).is_none());
        assert!(parse_file_name("../etc.png").is_none());
    }
}
