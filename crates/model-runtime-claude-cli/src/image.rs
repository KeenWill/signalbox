use std::num::{NonZeroU64, NonZeroUsize};

use base64::Engine as _;
use serde::Serialize;
use signalbox_model_runtime::{
    ImagePresentationCapability, MessagePart, ModelOperation, PreparationFailure,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputFormat {
    Text,
    StreamJson,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InputPart {
    Text { text: String },
    Image { source: ImageSource },
}

#[derive(Serialize)]
struct ImageSource {
    r#type: &'static str,
    media_type: String,
    data: String,
}

#[derive(Serialize)]
struct InputMessage {
    role: &'static str,
    content: Vec<InputPart>,
}

#[derive(Serialize)]
struct InputEvent {
    r#type: &'static str,
    message: InputMessage,
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

pub(crate) fn encode_input<C>(
    operation: &ModelOperation<C>,
    prompt: Vec<u8>,
) -> Result<(Vec<u8>, InputFormat), PreparationFailure> {
    let Some(limit) = signalbox_model_runtime::image_request_byte_limit(
        operation,
        &image_presentation_capability(),
    )?
    else {
        return Ok((prompt, InputFormat::Text));
    };
    let unsupported = || PreparationFailure::UnsupportedOperation {
        detail: String::from("encoded Claude image request exceeds its presentation bound"),
    };
    let mut content = vec![InputPart::Text {
        text: String::from_utf8(prompt).map_err(|_| unsupported())?,
    }];
    for (message_index, message) in operation.messages.iter().enumerate() {
        for (part_index, part) in message.parts.iter().enumerate() {
            if let MessagePart::Image(image) = part {
                content.push(InputPart::Text { text: format!("Image for messages[{message_index}].parts[{part_index}] in the stateless request:") });
                content.push(InputPart::Image {
                    source: ImageSource {
                        r#type: "base64",
                        media_type: image.media_type.clone(),
                        data: base64::engine::general_purpose::STANDARD.encode(&image.bytes),
                    },
                });
            }
        }
    }
    let mut bytes = serde_json::to_vec(&InputEvent {
        r#type: "user",
        message: InputMessage {
            role: "user",
            content,
        },
    })
    .map_err(|_| unsupported())?;
    bytes.push(b'\n');
    if bytes.len() > limit {
        return Err(unsupported());
    }
    Ok((bytes, InputFormat::StreamJson))
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_model_runtime::*;
    #[test]
    fn image_input_uses_stream_json_and_counts_the_complete_request() {
        let mut operation = ModelOperation::new(
            (),
            CredentialReference::new("fixture"),
            RequestedTarget::new("fixture"),
            ResolvedTarget::new("fixture"),
            vec![ConversationMessage {
                role: ConversationRole::User,
                parts: vec![MessagePart::Image(ImageInput {
                    media_type: "image/png".into(),
                    bytes: std::sync::Arc::from([1_u8, 2, 3]),
                })],
            }],
            ModelSettings::new(64),
        );
        operation.image_presentation = Some(image_presentation_capability());
        let (bytes, format) = encode_input(&operation, b"request".to_vec()).unwrap();
        assert_eq!(format, InputFormat::StreamJson);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "user");
        assert_eq!(
            value["message"]["content"][2],
            serde_json::json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"AQID"}})
        );
        operation.image_presentation =
            Some(image_presentation_capability().limited_by(u64::MAX, bytes.len() - 1));
        assert!(encode_input(&operation, b"request".to_vec()).is_err());
        operation.image_presentation =
            Some(image_presentation_capability().limited_by(2, usize::MAX));
        assert!(encode_input(&operation, b"request".to_vec()).is_err());
    }
}
