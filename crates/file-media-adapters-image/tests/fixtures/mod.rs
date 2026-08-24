use std::{error::Error, io::Cursor};

use image::{DynamicImage, ImageBuffer, ImageError, ImageFormat, Rgba};

const VALID_WIDTH: u32 = 3;
const VALID_HEIGHT: u32 = 2;

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
    encode(format, VALID_WIDTH, VALID_HEIGHT)
}

pub(crate) const fn valid_dimensions() -> (u32, u32) {
    (VALID_WIDTH, VALID_HEIGHT)
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

pub(crate) fn pixel_bomb(format: FixtureFormat) -> Result<Vec<u8>, Box<dyn Error>> {
    const BOMB_AXIS: u32 = 4_097;
    match format {
        FixtureFormat::Png => {
            let mut bytes = valid(format)?;
            bytes[16..20].copy_from_slice(&BOMB_AXIS.to_be_bytes());
            bytes[20..24].copy_from_slice(&BOMB_AXIS.to_be_bytes());
            let checksum = crc32fast::hash(&bytes[12..29]);
            bytes[29..33].copy_from_slice(&checksum.to_be_bytes());
            Ok(bytes)
        }
        FixtureFormat::Jpeg => {
            let mut bytes = valid(format)?;
            let marker = bytes
                .windows(2)
                .position(|window| window == [0xff, 0xc0])
                .ok_or("synthetic JPEG has no baseline frame header")?;
            bytes[marker + 5..marker + 7].copy_from_slice(&BOMB_AXIS.to_be_bytes()[2..]);
            bytes[marker + 7..marker + 9].copy_from_slice(&BOMB_AXIS.to_be_bytes()[2..]);
            Ok(bytes)
        }
        FixtureFormat::WebP => {
            let mut bytes = valid(format)?;
            let chunk = bytes
                .windows(4)
                .position(|window| window == b"VP8L")
                .ok_or("synthetic WebP is not lossless")?;
            let packed = (BOMB_AXIS - 1) | ((BOMB_AXIS - 1) << 14);
            let packed_bytes = packed.to_le_bytes();
            bytes[chunk + 9..chunk + 13].copy_from_slice(&packed_bytes);
            Ok(bytes)
        }
        FixtureFormat::Gif => {
            let mut bytes = valid(format)?;
            bytes[6..8].copy_from_slice(&(BOMB_AXIS as u16).to_le_bytes());
            bytes[8..10].copy_from_slice(&(BOMB_AXIS as u16).to_le_bytes());
            Ok(bytes)
        }
    }
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
