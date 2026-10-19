//! Shared preparation for images returned by native and MCP tools.

use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};

use super::ImageData;

// Bound encoded input/output, decoder allocations and raw pixel storage before
// decoding. Larger accepted images retain the native Read tool's 2000px resize.
pub(super) const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_SOURCE_DIMENSION: u32 = 8192;
const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_OUTPUT_DIMENSION: u32 = 2000;

pub(super) fn is_supported_image(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

fn decode_error(error: image::ImageError) -> &'static str {
    if matches!(error, image::ImageError::Limits(_)) {
        "image exceeds decoding limits (8192px per side, 64 MiB decoded)"
    } else {
        "could not decode image"
    }
}

pub(super) fn prepare_image(bytes: &[u8], mime: &str) -> Result<ImageData, &'static str> {
    if !is_supported_image(mime) {
        return Err("unsupported image type; expected PNG, JPEG, GIF or WebP");
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("image exceeds the 10 MiB size limit");
    }
    let format = ImageFormat::from_mime_type(mime).unwrap();
    let detected = image::guess_format(bytes).map_err(|_| "could not decode image")?;
    if detected != format {
        return Err("image MIME type does not match its encoded format");
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(decode_error)?;
    if decoder.total_bytes() > MAX_DECODED_BYTES {
        return Err("image exceeds the 64 MiB decoded size limit");
    }
    let image = DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    let bytes = if image.width() <= MAX_OUTPUT_DIMENSION && image.height() <= MAX_OUTPUT_DIMENSION {
        bytes.to_vec()
    } else {
        let resized = image.resize(
            MAX_OUTPUT_DIMENSION,
            MAX_OUTPUT_DIMENSION,
            image::imageops::FilterType::Triangle,
        );
        let mut buffer = Cursor::new(Vec::new());
        resized
            .write_to(&mut buffer, format)
            .map_err(|_| "could not re-encode image")?;
        buffer.into_inner()
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("prepared image exceeds the 10 MiB size limit");
    }
    Ok(ImageData {
        bytes,
        media_type: mime.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(format: ImageFormat, width: u32) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(width, 1)
            .write_to(&mut buffer, format)
            .unwrap();
        buffer.into_inner()
    }

    #[test]
    fn supported_formats_validate_and_resize_with_truthful_mime() {
        for (format, mime) in [
            (ImageFormat::Png, "image/png"),
            (ImageFormat::Jpeg, "image/jpeg"),
            (ImageFormat::Gif, "image/gif"),
            (ImageFormat::WebP, "image/webp"),
        ] {
            let bytes = encoded(format, 1);
            let prepared = prepare_image(&bytes, mime).unwrap();
            assert_eq!(prepared.bytes, bytes);
            assert_eq!(prepared.media_type, mime);
            let prepared = prepare_image(&encoded(format, 2001), mime).unwrap();
            let image = image::load_from_memory(&prepared.bytes).unwrap();
            assert_eq!(image.width(), 2000);
            assert_eq!(image.height(), 1);
            assert_eq!(image::guess_format(&prepared.bytes).unwrap(), format);
            assert_eq!(prepared.media_type, mime);
        }
    }

    #[test]
    fn malformed_and_mislabeled_images_are_rejected() {
        let png = encoded(ImageFormat::Png, 1);
        assert!(
            prepare_image(&png, "image/svg+xml")
                .err()
                .unwrap()
                .contains("unsupported")
        );
        assert!(
            prepare_image(&png, "image/jpeg")
                .err()
                .unwrap()
                .contains("does not match")
        );
        assert_eq!(
            prepare_image(b"not an image", "image/png").err(),
            Some("could not decode image")
        );
        assert_eq!(
            prepare_image(&png[..8], "image/png").err(),
            Some("could not decode image")
        );
    }

    #[test]
    fn input_dimensions_and_decoded_storage_are_bounded_before_loading_pixels() {
        assert!(
            prepare_image(&vec![0; MAX_IMAGE_BYTES + 1], "image/png")
                .err()
                .unwrap()
                .contains("10 MiB")
        );
        assert!(
            prepare_image(
                &encoded(ImageFormat::Png, MAX_SOURCE_DIMENSION + 1),
                "image/png"
            )
            .err()
            .unwrap()
            .contains("decoding limits")
        );
        // A small GIF can declare a huge canvas. Inspect decoder dimensions
        // and storage before allocating that canvas, not after resizing it.
        let mut gif = encoded(ImageFormat::Gif, 1);
        gif[6..8].copy_from_slice(&8192u16.to_le_bytes());
        gif[8..10].copy_from_slice(&8192u16.to_le_bytes());
        assert!(
            prepare_image(&gif, "image/gif")
                .err()
                .unwrap()
                .contains("64 MiB")
        );
    }
}
