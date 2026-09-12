use image::{DynamicImage, ImageEncoder as _};

pub(crate) const DOWNSCALE_SCHEMA: &str = r#"{"additionalProperties":false,"properties":{"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1}},"required":["width","height"],"type":"object"}"#;
pub(crate) const CROP_SCHEMA: &str = r#"{"additionalProperties":false,"properties":{"x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1},"scale":{"type":"number","exclusiveMinimum":0,"maximum":1}},"required":["x","y","width","height"],"type":"object"}"#;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Size {
    width: u32,
    height: u32,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Crop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    scale: Option<f64>,
}

pub(crate) enum Transform {
    Downscale(Size),
    Crop(Crop),
}

impl Transform {
    pub(crate) fn parse(
        view: &str,
        options: &serde_json::Value,
        source: (u32, u32),
    ) -> Option<Self> {
        match view {
            "downscale" => {
                let size: Size = serde_json::from_value(options.clone()).ok()?;
                (size.width > 0
                    && size.height > 0
                    && size.width <= crate::MAX_IMAGE_AXIS
                    && size.height <= crate::MAX_IMAGE_AXIS)
                    .then_some(Self::Downscale(size))
            }
            "crop" => {
                let crop: Crop = serde_json::from_value(options.clone()).ok()?;
                (crop.width > 0
                    && crop.height > 0
                    && crop
                        .x
                        .checked_add(crop.width)
                        .is_some_and(|end| end <= source.0)
                    && crop
                        .y
                        .checked_add(crop.height)
                        .is_some_and(|end| end <= source.1)
                    && crop
                        .scale
                        .is_none_or(|scale| scale.is_finite() && scale > 0.0 && scale <= 1.0))
                .then_some(Self::Crop(crop))
            }
            _ => None,
        }
    }

    pub(crate) fn encode(self, image: DynamicImage) -> Option<Vec<u8>> {
        let view = match self {
            Self::Downscale(size) => image.resize(
                size.width.min(image.width()),
                size.height.min(image.height()),
                image::imageops::FilterType::Lanczos3,
            ),
            Self::Crop(crop) => {
                let image = image.crop_imm(crop.x, crop.y, crop.width, crop.height);
                match crop.scale {
                    Some(scale) => image.resize_exact(
                        (f64::from(crop.width) * scale).floor().max(1.0) as u32,
                        (f64::from(crop.height) * scale).floor().max(1.0) as u32,
                        image::imageops::FilterType::Lanczos3,
                    ),
                    None => image,
                }
            }
        }
        .to_rgba8();
        encode_png(&view)
    }
}

pub(crate) fn encode_png(view: &image::RgbaImage) -> Option<Vec<u8>> {
    let mut output = BoundedOutput(Vec::new());
    image::codecs::png::PngEncoder::new_with_quality(
        &mut output,
        image::codecs::png::CompressionType::Best,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(
        view.as_raw(),
        view.width(),
        view.height(),
        image::ExtendedColorType::Rgba8,
    )
    .ok()?;
    Some(output.0)
}

struct BoundedOutput(Vec<u8>);

impl std::io::Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().checked_add(bytes.len()).is_none_or(|length| {
            length as u64 > signalbox_file_media_runtime::MAX_PRESENTED_IMAGE_BYTES
        }) {
            return Err(std::io::Error::other(
                "derived image exceeds its view bound",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, Rgba};

    #[test]
    fn crop_coordinates_address_full_resolution_and_optional_scale_is_bounded() {
        let image = DynamicImage::ImageRgba8(image::ImageBuffer::from_fn(8, 6, |x, y| {
            Rgba([x as u8, y as u8, 0, 255])
        }));
        let options = serde_json::json!({"x":3,"y":2,"width":4,"height":2});
        let bytes = Transform::parse("crop", &options, (8, 6))
            .unwrap()
            .encode(image.clone())
            .unwrap();
        let cropped = image::load_from_memory(&bytes).unwrap();
        assert_eq!(cropped.dimensions(), (4, 2));
        assert_eq!(cropped.get_pixel(0, 0), image.get_pixel(3, 2));
        let scaled = Transform::parse(
            "crop",
            &serde_json::json!({"x":3,"y":2,"width":4,"height":2,"scale":0.5}),
            (8, 6),
        )
        .unwrap()
        .encode(image)
        .unwrap();
        assert_eq!(
            image::load_from_memory(&scaled).unwrap().dimensions(),
            (2, 1)
        );
        assert!(
            Transform::parse(
                "crop",
                &serde_json::json!({"x":7,"y":2,"width":4,"height":2}),
                (8, 6)
            )
            .is_none()
        );
    }
    #[test]
    fn downscale_preserves_aspect_without_upscaling_and_compression_is_deterministic() {
        let image = DynamicImage::new_rgba8(8, 4);
        let options = serde_json::json!({"width":3,"height":3});
        let first = Transform::parse("downscale", &options, (8, 4))
            .unwrap()
            .encode(image.clone())
            .unwrap();
        let second = Transform::parse("downscale", &options, (8, 4))
            .unwrap()
            .encode(image)
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            image::load_from_memory(&first).unwrap().dimensions(),
            (3, 2)
        );
    }
}
