use std::io::Cursor;

use image::{DynamicImage, ImageBuffer, ImageError, ImageFormat, Rgba};

#[derive(Clone, Copy)]
pub(crate) enum FixtureFormat {
    Png,
    Jpeg,
    WebP,
    Gif,
}

impl FixtureFormat {
    pub(crate) const fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::WebP => "image/webp",
            Self::Gif => "image/gif",
        }
    }

    const fn image_format(self) -> ImageFormat {
        match self {
            Self::Png => ImageFormat::Png,
            Self::Jpeg => ImageFormat::Jpeg,
            Self::WebP => ImageFormat::WebP,
            Self::Gif => ImageFormat::Gif,
        }
    }
}

pub(crate) fn valid(format: FixtureFormat) -> Result<Vec<u8>, ImageError> {
    encode(format, 3, 2)
}

pub(crate) fn expected_metadata(format: FixtureFormat) -> serde_json::Value {
    let channels = match format {
        FixtureFormat::Jpeg => 3,
        FixtureFormat::Png | FixtureFormat::WebP | FixtureFormat::Gif => 4,
    };
    serde_json::json!({"channels": channels, "height": 2, "width": 3})
}

pub(crate) fn truncated(format: FixtureFormat) -> Result<Vec<u8>, ImageError> {
    let mut bytes = valid(format)?;
    bytes.truncate(bytes.len() / 2);
    Ok(bytes)
}

pub(crate) fn malformed(format: FixtureFormat) -> Vec<u8> {
    match format {
        FixtureFormat::Png => b"\x89PNG\r\n\x1a\nmalformed".to_vec(),
        FixtureFormat::Jpeg => vec![0xff, 0xd8, 0xff, 0x00],
        FixtureFormat::WebP => b"RIFF\x08\x00\x00\x00WEBPbad!".to_vec(),
        FixtureFormat::Gif => b"GIF89a\x01\x00\x01\x00\x00\x00\x00\x01".to_vec(),
    }
}

pub(crate) fn oversized(format: FixtureFormat) -> Result<Vec<u8>, ImageError> {
    let mut bytes = valid(format)?;
    bytes.resize(
        signalbox_file_media_adapters_image::MAX_IMAGE_SOURCE_BYTES as usize + 1,
        0,
    );
    Ok(bytes)
}

pub(crate) fn dimension_bomb(format: FixtureFormat) -> Result<Vec<u8>, ImageError> {
    encode(
        format,
        signalbox_file_media_adapters_image::MAX_IMAGE_AXIS + 1,
        1,
    )
}

fn encode(format: FixtureFormat, width: u32, height: u32) -> Result<Vec<u8>, ImageError> {
    let pixels = ImageBuffer::from_fn(width, height, |x, y| {
        Rgba([
            x.to_le_bytes()[0],
            y.to_le_bytes()[0],
            x.wrapping_add(y).to_le_bytes()[0],
            255,
        ])
    });
    let image = DynamicImage::ImageRgba8(pixels);
    let mut encoded = Cursor::new(Vec::new());
    image.write_to(&mut encoded, format.image_format())?;
    Ok(encoded.into_inner())
}
