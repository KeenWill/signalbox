//! Native image content and the declared Claude presentation bounds.

use std::num::{NonZeroU64, NonZeroUsize};

use base64::Engine as _;
use serde::Serialize;
use signalbox_model_runtime::{ImageInput, ImagePresentationCapability};

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum InputPart {
    Text { text: String },
    Image { source: ImageSource },
}

#[derive(Serialize)]
pub(crate) struct ImageSource {
    r#type: &'static str,
    media_type: String,
    data: String,
}

/// Claude Code image support with the CLI's complete stdin payload limit.
pub fn image_presentation_capability() -> ImagePresentationCapability {
    // Claude Code caps piped stdin at 10 MB:
    // https://code.claude.com/docs/en/headless#pipe-data-through-claude
    // Base64 consumes four encoded bytes for every three source bytes.
    const REQUEST_BYTES: NonZeroUsize = NonZeroUsize::new(10_000_000).unwrap();
    const IMAGE_BYTES: NonZeroU64 = NonZeroU64::new(7_500_000).unwrap();
    let types = ["image/png", "image/jpeg", "image/webp"];
    let envelope = types
        .iter()
        .map(|media_type| {
            serde_json::to_string(&InputPart::Image {
                source: ImageSource {
                    r#type: "base64",
                    media_type: String::from(*media_type),
                    data: String::new(),
                },
            })
            .map(|text| text.len())
            .unwrap_or(usize::MAX)
        })
        .max()
        .unwrap_or_default();
    ImagePresentationCapability::new(
        types.into_iter().map(String::from).collect(),
        IMAGE_BYTES,
        REQUEST_BYTES,
        envelope,
    )
}

pub(crate) fn image_content(image: &ImageInput) -> InputPart {
    InputPart::Image {
        source: ImageSource {
            r#type: "base64",
            media_type: image.media_type.clone(),
            data: base64::engine::general_purpose::STANDARD.encode(&image.bytes),
        },
    }
}
