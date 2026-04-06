// ===========================================================================
// Image processing — resize and base64-encode images for vision models.
//
// Anthropic recommends images be at most 1568px on their longest side
// and under 5MB base64.  This module enforces both limits:
//
//   1. Decode from any supported format (JPEG, PNG, WebP)
//   2. Resize if either dimension exceeds MAX_DIMENSION (Lanczos3)
//   3. Re-encode as JPEG (smaller than PNG, universally supported)
//   4. Base64-encode
//   5. Reject if the result exceeds MAX_BASE64_BYTES
// ===========================================================================

use base64::Engine;
use image::ImageReader;
use image::imageops::FilterType;
use std::io::Cursor;

use crate::message::ContentBlock;

/// Maximum dimension (width or height) before resizing.
const MAX_DIMENSION: u32 = 1568;

/// Maximum base64 size in bytes (4MB — safety margin under Anthropic's 5MB limit).
const MAX_BASE64_BYTES: usize = 4 * 1024 * 1024;

/// Process raw image bytes into a ContentBlock::Image.
///
/// Decodes the image, resizes if necessary, re-encodes as JPEG, and
/// base64-encodes the result.
pub fn process_image(data: &[u8]) -> crate::Result<ContentBlock> {
    // Decode from any supported format.
    let img = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(crate::DysonError::Io)?
        .decode()
        .map_err(|e| crate::DysonError::Config(format!("failed to decode image: {e}")))?;

    // Resize if either dimension exceeds the limit.
    let img = if img.width() > MAX_DIMENSION || img.height() > MAX_DIMENSION {
        tracing::info!(
            width = img.width(),
            height = img.height(),
            max = MAX_DIMENSION,
            "resizing image to fit dimension limit"
        );
        img.resize(MAX_DIMENSION, MAX_DIMENSION, FilterType::Lanczos3)
    } else {
        img
    };

    // Re-encode as JPEG.
    let mut jpeg_buf = Vec::new();
    let mut cursor = Cursor::new(&mut jpeg_buf);
    img.write_to(&mut cursor, image::ImageFormat::Jpeg)
        .map_err(|e| crate::DysonError::Config(format!("failed to encode image as JPEG: {e}")))?;

    // Base64-encode.
    let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg_buf);

    if b64.len() > MAX_BASE64_BYTES {
        return Err(crate::DysonError::Config(format!(
            "image too large after processing: {} bytes base64 (max {})",
            b64.len(),
            MAX_BASE64_BYTES
        )));
    }

    tracing::debug!(
        original_bytes = data.len(),
        jpeg_bytes = jpeg_buf.len(),
        base64_bytes = b64.len(),
        width = img.width(),
        height = img.height(),
        "image processed successfully"
    );

    Ok(ContentBlock::Image {
        data: b64,
        media_type: "image/jpeg".to_string(),
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a minimal valid JPEG for testing.
    fn tiny_jpeg() -> Vec<u8> {
        let img = image::RgbImage::new(10, 10);
        let mut buf = Vec::new();
        let mut cursor = Cursor::new(&mut buf);
        img.write_to(&mut cursor, image::ImageFormat::Jpeg).unwrap();
        buf
    }

    #[test]
    fn process_small_image() {
        let data = tiny_jpeg();
        let block = process_image(&data).unwrap();
        match block {
            ContentBlock::Image { data, media_type } => {
                assert_eq!(media_type, "image/jpeg");
                assert!(!data.is_empty());
                // Verify it's valid base64.
                base64::engine::general_purpose::STANDARD
                    .decode(&data)
                    .expect("should be valid base64");
            }
            other => panic!("expected Image block, got: {other:?}"),
        }
    }

    #[test]
    fn oversized_image_is_resized() {
        // Create a 2000x2000 image — exceeds MAX_DIMENSION.
        let img = image::RgbImage::new(2000, 2000);
        let mut buf = Vec::new();
        let mut cursor = Cursor::new(&mut buf);
        img.write_to(&mut cursor, image::ImageFormat::Jpeg).unwrap();

        let block = process_image(&buf).unwrap();
        match block {
            ContentBlock::Image { data, .. } => {
                // Decode the base64 back and check dimensions.
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&data)
                    .unwrap();
                let img = ImageReader::new(Cursor::new(&decoded))
                    .with_guessed_format()
                    .unwrap()
                    .decode()
                    .unwrap();
                assert!(
                    img.width() <= MAX_DIMENSION && img.height() <= MAX_DIMENSION,
                    "resized image should fit within {MAX_DIMENSION}px, got {}x{}",
                    img.width(),
                    img.height()
                );
            }
            other => panic!("expected Image block, got: {other:?}"),
        }
    }

    #[test]
    fn invalid_data_returns_error() {
        let result = process_image(b"not an image");
        assert!(result.is_err());
    }
}
