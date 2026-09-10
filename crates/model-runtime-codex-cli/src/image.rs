use std::num::{NonZeroU64, NonZeroUsize};

use signalbox_model_runtime::ImagePresentationCapability;

/// Image types and encoded request bounds supported by the Codex image RPC path.
pub fn image_presentation_capability() -> ImagePresentationCapability {
    // Responses image inputs accept a 512 MB request payload:
    // https://developers.openai.com/api/docs/guides/images-vision#image-input-requirements
    // A base64 image consumes four encoded bytes for every three source bytes.
    const REQUEST_BYTES: NonZeroUsize = NonZeroUsize::new(512_000_000).unwrap();
    const IMAGE_BYTES: NonZeroU64 = NonZeroU64::new(384_000_000).unwrap();
    let types = ["image/png", "image/jpeg", "image/webp"];
    let envelope = types
        .iter()
        .map(|media_type| {
            serde_json::json!({"type":"image","url":format!("data:{media_type};base64,")})
                .to_string()
                .len()
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
