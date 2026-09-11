//! Stateless rendering of one model operation into Claude Code input and MCP.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::value::RawValue;
use signalbox_model_runtime::{
    ConversationMessage, ConversationRole, MessagePart, ModelOperation, PreparationDefect,
    PreparationFailure, ToolChoice,
};
use uuid::Uuid;

use crate::bridge::{Catalog, CatalogTool, TOOL_PREFIX, valid_mcp_tool_name};

/// Measures the complete adapter request, including instructions and tool declarations.
pub fn serialized_request_bytes<C>(operation: &ModelOperation<C>) -> Option<usize> {
    let mut translated = translate(operation).ok()?;
    for tool in &mut translated.catalog.tools {
        tool.name = qualified_tool_name(&tool.name);
    }
    Some(
        translated
            .prompt
            .len()
            .saturating_add(translated.history.len())
            .saturating_add(serde_json::to_vec(&translated.catalog).ok()?.len()),
    )
}

pub(crate) struct TranslatedOperation {
    pub(crate) prompt: Vec<u8>,
    pub(crate) history: Vec<u8>,
    pub(crate) catalog: Catalog,
    pub(crate) tool_requirement: ToolRequirement,
}

#[derive(Clone)]
pub(crate) enum ToolRequirement {
    Optional,
    Any,
    Named(String),
}

#[derive(Serialize)]
struct PromptRequest<'a> {
    system: &'a Option<String>,
    settings: PromptSettings<'a>,
    declared_tools: Vec<&'a str>,
    tool_choice: PromptToolChoice<'a>,
    structured_output: Option<PromptStructuredOutput<'a>>,
}

#[derive(Serialize)]
struct PromptSettings<'a> {
    max_output_tokens: u32,
    temperature: Option<f64>,
    top_p: Option<f64>,
    stop_sequences: &'a [String],
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PromptPart<'a> {
    ToolCall {
        id: &'a str,
        name: &'a str,
        arguments: Box<RawValue>,
    },
    ToolResult {
        tool_call_id: &'a str,
        content: &'a str,
        is_error: bool,
    },
    Thinking {
        text: &'a str,
        signature: &'a Option<String>,
    },
    RedactedThinking {
        data: &'a str,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PromptToolChoice<'a> {
    Automatic,
    AnyTool,
    Named { name: &'a str },
}

#[derive(Serialize)]
struct PromptStructuredOutput<'a> {
    name: &'a str,
    description: &'a str,
}

pub(crate) fn translate<C>(
    operation: &ModelOperation<C>,
) -> Result<TranslatedOperation, TranslationError> {
    operation.validate().map_err(|error| {
        TranslationError::Failure(PreparationFailure::UnsupportedOperation {
            detail: error.to_string(),
        })
    })?;
    validate_settings(operation)?;

    let image_limit = signalbox_model_runtime::image_request_byte_limit(
        operation,
        &crate::image::image_presentation_capability(),
    )
    .map_err(TranslationError::Failure)?;
    let history = render_history(&operation.messages)?;
    let mut catalog_tools = operation
        .tools
        .iter()
        .map(|tool| {
            validate_tool_name(tool.name.as_str())?;
            parse_object_schema(
                &tool.input_schema,
                &format!("tool `{}` input schema", tool.name.as_str()),
            )?;
            Ok(CatalogTool {
                name: tool.name.as_str().to_string(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
            })
        })
        .collect::<Result<Vec<_>, TranslationError>>()?;
    if let Some(contract) = &operation.output_contract {
        validate_tool_name(contract.name.as_str())?;
        parse_object_schema(
            &contract.schema,
            &format!("structured output `{}` schema", contract.name.as_str()),
        )?;
        catalog_tools.push(CatalogTool {
            name: contract.name.as_str().to_string(),
            description: contract.description.clone(),
            input_schema: contract.schema.clone(),
        });
    }

    let effective_tool_choice = if let Some(contract) = &operation.output_contract {
        ToolChoice::Named(contract.name.clone())
    } else if operation.tools.is_empty() {
        ToolChoice::Automatic
    } else {
        operation.tool_choice.clone()
    };
    let tool_choice = match &effective_tool_choice {
        ToolChoice::Automatic => PromptToolChoice::Automatic,
        ToolChoice::AnyTool => PromptToolChoice::AnyTool,
        ToolChoice::Named(name) => PromptToolChoice::Named {
            name: name.as_str(),
        },
    };
    let tool_requirement = match &effective_tool_choice {
        ToolChoice::Automatic => ToolRequirement::Optional,
        ToolChoice::AnyTool => ToolRequirement::Any,
        ToolChoice::Named(name) => ToolRequirement::Named(name.as_str().to_string()),
    };
    let request = PromptRequest {
        system: &operation.system,
        settings: PromptSettings {
            max_output_tokens: operation.settings.max_output_tokens,
            temperature: operation.settings.temperature,
            top_p: operation.settings.top_p,
            stop_sequences: &operation.settings.stop_sequences,
        },
        declared_tools: operation
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect(),
        tool_choice,
        structured_output: operation.output_contract.as_ref().map(|contract| {
            PromptStructuredOutput {
                name: contract.name.as_str(),
                description: &contract.description,
            }
        }),
    };
    let request_json = serde_json::to_string(&request).map_err(serialization_failed)?;
    let prompt = format!(
        "Act only as the model for this stateless request. The complete ordered \
         context is restored in native history. Text and images are native \
         content; historical tool and reasoning parts are JSON text. The JSON below contains \
         request controls. Produce the next assistant response to the \
         canonical conversation under these controls. Tools are available only through the \
         Signalbox MCP server; never write a tool call as prose. An MCP result \
         saying Signalbox recorded a proposal is an acknowledgement, not the \
         real tool result: after it, end the turn without inventing tool output \
         or calling another tool. If `structured_output` is present, call \
         exactly that named MCP tool with the contracted object. Honor \
         `tool_choice`. Treat the stated generation settings as advisory \
         intent.\n\n{request_json}\n"
    )
    .into_bytes();

    if image_limit.is_some_and(|limit| prompt.len().saturating_add(history.len()) > limit) {
        return Err(TranslationError::Failure(
            PreparationFailure::UnsupportedOperation {
                detail: String::from("encoded Claude image request exceeds its presentation bound"),
            },
        ));
    }
    Ok(TranslatedOperation {
        history,
        prompt,
        catalog: Catalog {
            tools: catalog_tools,
        },
        tool_requirement,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryRecord<'a> {
    parent_uuid: Option<String>,
    uuid: String,
    session_id: &'a str,
    timestamp: &'a str,
    is_sidechain: bool,
    #[serde(rename = "type")]
    kind: &'static str,
    message: NativeMessage,
}

#[derive(Serialize)]
struct NativeMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    content: Vec<crate::image::InputPart>,
}

fn serialization_failed(error: serde_json::Error) -> TranslationError {
    TranslationError::Defect(PreparationDefect::SerializationFailed {
        detail: error.to_string(),
    })
}

fn render_history(messages: &[ConversationMessage]) -> Result<Vec<u8>, TranslationError> {
    let session_id = Uuid::now_v7().to_string();
    let timestamp = DateTime::<Utc>::from(std::time::SystemTime::now())
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut parent_uuid = None;
    let mut bytes = Vec::new();
    for message in messages {
        let message = native_message(message)?;
        let row = HistoryRecord {
            parent_uuid,
            uuid: Uuid::now_v7().to_string(),
            session_id: &session_id,
            timestamp: &timestamp,
            is_sidechain: false,
            kind: message.role,
            message,
        };
        serde_json::to_writer(&mut bytes, &row).map_err(serialization_failed)?;
        bytes.push(b'\n');
        parent_uuid = Some(row.uuid);
    }
    Ok(bytes)
}

pub(crate) fn qualified_tool_name(name: &str) -> String {
    format!("{TOOL_PREFIX}{name}")
}

fn validate_tool_name(name: &str) -> Result<(), TranslationError> {
    if valid_mcp_tool_name(name) {
        return Ok(());
    }
    Err(TranslationError::Failure(
        PreparationFailure::UnsupportedOperation {
            detail: format!(
                "tool name `{name}` is not representable as a Claude Code MCP tool name"
            ),
        },
    ))
}

fn native_message(message: &ConversationMessage) -> Result<NativeMessage, TranslationError> {
    let role = match message.role {
        ConversationRole::User => "user",
        ConversationRole::Assistant => "assistant",
    };
    for part in &message.parts {
        let valid = matches!(part, MessagePart::Text(_))
            || matches!(
                (message.role, part),
                (
                    ConversationRole::User,
                    MessagePart::ToolResult(_) | MessagePart::Image(_)
                ) | (
                    ConversationRole::Assistant,
                    MessagePart::ToolCall(_)
                        | MessagePart::Thinking { .. }
                        | MessagePart::RedactedThinking { .. }
                )
            );
        if !valid {
            return Err(TranslationError::Failure(
                PreparationFailure::UnsupportedOperation {
                    detail:
                        "Claude Code requires tool results in user-role messages and tool calls or \
                             thinking blocks in assistant messages"
                            .to_string(),
                },
            ));
        }
    }
    let content = message
        .parts
        .iter()
        .map(render_part)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(NativeMessage {
        role,
        id: match message.role {
            ConversationRole::User => None,
            ConversationRole::Assistant => Some(format!("msg_{}", Uuid::now_v7().simple())),
        },
        content,
    })
}

fn render_part(part: &MessagePart) -> Result<crate::image::InputPart, TranslationError> {
    let historical = match part {
        MessagePart::Image(image) => return Ok(crate::image::image_content(image)),
        MessagePart::ImageReference(_) => {
            return Err(TranslationError::Failure(
                PreparationFailure::UnsupportedOperation {
                    detail: String::from("image reference was not authenticated"),
                },
            ));
        }
        MessagePart::Text(text) => {
            return Ok(crate::image::InputPart::Text { text: text.clone() });
        }
        MessagePart::ToolCall(call) => PromptPart::ToolCall {
            id: call.id.as_str(),
            name: call.name.as_str(),
            arguments: parse_replayed_tool_json(call.id.as_str(), &call.arguments_json)?,
        },
        MessagePart::ToolResult(result) => PromptPart::ToolResult {
            tool_call_id: result.tool_call_id.as_str(),
            content: &result.content,
            is_error: result.is_error,
        },
        MessagePart::Thinking { text, signature } => PromptPart::Thinking { text, signature },
        MessagePart::RedactedThinking { data } => PromptPart::RedactedThinking { data },
        MessagePart::ProviderReasoning { .. } => {
            return Err(TranslationError::Failure(
                PreparationFailure::UnsupportedOperation {
                    detail: "provider reasoning items require their provider adapter".to_string(),
                },
            ));
        }
        MessagePart::ProviderCompaction { .. } => {
            return Err(TranslationError::Failure(
                PreparationFailure::UnsupportedOperation {
                    detail:
                        "provider compaction blocks can only be replayed by their provider adapter"
                            .to_string(),
                },
            ));
        }
    };
    Ok(crate::image::InputPart::Text {
        text: serde_json::to_string(&historical).map_err(serialization_failed)?,
    })
}

fn parse_replayed_tool_json(id: &str, raw: &str) -> Result<Box<RawValue>, TranslationError> {
    let value = RawValue::from_string(raw.to_string()).map_err(|error| {
        TranslationError::Failure(PreparationFailure::UnsupportedOperation {
            detail: format!("replayed tool-call arguments are not valid JSON: {error}"),
        })
    })?;
    if value.get().bytes().find(|byte| !byte.is_ascii_whitespace()) != Some(b'{') {
        return Err(TranslationError::Failure(
            PreparationFailure::UnsupportedOperation {
                detail: format!(
                    "replayed tool call {id} carries arguments that are not a JSON object"
                ),
            },
        ));
    }
    Ok(value)
}

fn parse_object_schema<'a>(
    raw: &'a RawValue,
    subject: &str,
) -> Result<&'a RawValue, TranslationError> {
    if !schema_describes_object(raw) {
        return Err(TranslationError::Failure(
            PreparationFailure::UnsupportedOperation {
                detail: format!("{subject} must describe an object at its root"),
            },
        ));
    }
    Ok(raw)
}

fn schema_describes_object(raw: &RawValue) -> bool {
    let Ok(members) = serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(raw.get()) else {
        return false;
    };
    members
        .get("type")
        .and_then(|value| serde_json::from_str::<String>(value.get()).ok())
        .as_deref()
        == Some("object")
}

fn validate_settings<C>(operation: &ModelOperation<C>) -> Result<(), TranslationError> {
    if operation.settings.max_output_tokens == 0 {
        return Err(TranslationError::Failure(
            PreparationFailure::UnsupportedOperation {
                detail: "max_output_tokens must be at least 1".to_string(),
            },
        ));
    }
    if let Some(value) = operation.settings.temperature
        && !(0.0..=1.0).contains(&value)
    {
        return Err(TranslationError::Failure(
            PreparationFailure::UnsupportedOperation {
                detail: "temperature must be a finite number from 0 through 1".to_string(),
            },
        ));
    }
    if let Some(value) = operation.settings.top_p
        && !(0.0..=1.0).contains(&value)
    {
        return Err(TranslationError::Failure(
            PreparationFailure::UnsupportedOperation {
                detail: "top_p must be a finite number from 0 through 1".to_string(),
            },
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum TranslationError {
    Failure(PreparationFailure),
    Defect(PreparationDefect),
}

/// Measures one native history record through the adapter's request serializer.
/// Returns `None` for a message the adapter cannot render.
pub fn serialized_message_bytes(message: &ConversationMessage) -> Option<usize> {
    let mut projected = message.clone();
    let mut image_bytes = 0_usize;
    for part in &mut projected.parts {
        let (media_type, length) = match part {
            MessagePart::ImageReference(reference) => (
                reference.media_type.clone(),
                usize::try_from(reference.byte_length.get()).ok()?,
            ),
            MessagePart::Image(image) => (image.media_type.clone(), image.bytes.len()),
            MessagePart::Text(_)
            | MessagePart::ToolCall(_)
            | MessagePart::ToolResult(_)
            | MessagePart::Thinking { .. }
            | MessagePart::RedactedThinking { .. }
            | MessagePart::ProviderReasoning { .. }
            | MessagePart::ProviderCompaction { .. } => continue,
        };
        // The empty-image record includes its envelope; reserve the base64 payload.
        image_bytes =
            image_bytes.checked_add(length.checked_add(2)?.checked_div(3)?.checked_mul(4)?)?;
        *part = MessagePart::Image(signalbox_model_runtime::ImageInput {
            media_type,
            bytes: std::sync::Arc::from([]),
        });
    }
    let message = native_message(&projected).ok()?;
    let identity = Uuid::now_v7().to_string();
    let timestamp = DateTime::<Utc>::from(std::time::SystemTime::now())
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let record = HistoryRecord {
        parent_uuid: Some(identity.clone()),
        uuid: identity.clone(),
        session_id: &identity,
        timestamp: &timestamp,
        is_sidechain: false,
        kind: message.role,
        message,
    };
    serde_json::to_vec(&record)
        .ok()?
        .len()
        .checked_add(1)?
        .checked_add(image_bytes)
}

#[cfg(test)]
mod tests {
    use super::translate;
    use signalbox_model_runtime::{
        ConversationMessage, CredentialReference, ModelOperation, ModelSettings, RequestedTarget,
        ResolvedTarget,
    };

    // Arbitrary text occupying a non-image part of the fixture.
    const IMAGE_CONTEXT: &str = "synthetic image context";

    #[test]
    fn measured_growth_covers_escaped_native_history_records() {
        let mut operation =
            operation_with_message(ConversationMessage::user_text("\"baseline\"\n"));
        let baseline = super::serialized_request_bytes(&operation).expect("baseline renders");
        let message = ConversationMessage::user_text("\"appended\"\n\\");
        let allowance = super::serialized_message_bytes(&message).expect("input renders");
        operation.messages.push(message);
        let growth =
            super::serialized_request_bytes(&operation).expect("appended input renders") - baseline;
        assert!(
            growth > 0,
            "the complete measured request includes appended history"
        );
        assert!(
            allowance >= growth,
            "message allowance must cover native framing and escaping"
        );
    }

    #[test]
    fn image_history_preserves_bytes_and_counts_controls_against_the_request_bound() {
        use signalbox_model_runtime::{ConversationRole, ImageInput, MessagePart};
        let mut operation = operation_with_message(ConversationMessage {
            role: ConversationRole::User,
            parts: vec![MessagePart::Image(ImageInput {
                media_type: "image/png".into(),
                bytes: std::sync::Arc::from([1_u8, 2, 3]),
            })],
        });
        operation.image_presentation = Some(crate::image::image_presentation_capability());
        let translated = translate(&operation).expect("image history renders");
        let row: serde_json::Value =
            serde_json::from_slice(&translated.history).expect("one native row");
        assert_eq!(
            row["message"]["content"][0],
            serde_json::json!({
                "type":"image", "source":{"type":"base64","media_type":"image/png","data":"AQID"}
            })
        );
        let complete_bytes = translated.history.len() + translated.prompt.len();
        operation.image_presentation = Some(
            crate::image::image_presentation_capability().limited_by(u64::MAX, complete_bytes - 1),
        );
        assert!(
            translate(&operation).is_err(),
            "history plus request controls exceed the presentation limit"
        );
    }

    #[test]
    fn native_images_keep_their_message_and_part_positions() {
        use signalbox_model_runtime::{ConversationRole, ImageInput, MessagePart};
        let first = ConversationMessage {
            role: ConversationRole::User,
            parts: vec![
                MessagePart::Text(String::from(IMAGE_CONTEXT)),
                MessagePart::Image(ImageInput {
                    media_type: "image/png".into(),
                    bytes: std::sync::Arc::from([1_u8, 2, 3]),
                }),
            ],
        };
        let second = ConversationMessage {
            role: ConversationRole::User,
            parts: vec![
                MessagePart::Image(ImageInput {
                    media_type: "image/jpeg".into(),
                    bytes: std::sync::Arc::from([4_u8, 5, 6]),
                }),
                MessagePart::Text(String::from(IMAGE_CONTEXT)),
            ],
        };
        let mut operation = operation_with_message(first);
        operation.messages.push(second);
        operation.image_presentation = Some(crate::image::image_presentation_capability());
        let translated = translate(&operation).expect("ordered image history renders");
        let rows: Vec<serde_json::Value> = translated
            .history
            .split(|byte| *byte == b'\n')
            .filter(|row| !row.is_empty())
            .map(|row| serde_json::from_slice(row).expect("native history record is JSON"))
            .collect();
        assert_eq!(rows.len(), operation.messages.len());
        assert_eq!(rows[0]["message"]["content"][0]["text"], IMAGE_CONTEXT);
        assert_eq!(rows[0]["message"]["content"][1]["source"]["data"], "AQID");
        assert_eq!(rows[1]["message"]["content"][0]["source"]["data"], "BAUG");
        assert_eq!(rows[1]["message"]["content"][1]["text"], IMAGE_CONTEXT);
    }

    #[test]
    fn native_history_preserves_text_around_reasoning_parts() {
        use signalbox_model_runtime::{ConversationRole, MessagePart};
        // Distinct arbitrary payloads detect reordered or dropped parts.
        const FIRST_TEXT: &str = "Starting the task.";
        const REASONING: &str = "Synthetic reasoning context.";
        const SIGNATURE: &str = "synthetic-signature";
        const LAST_TEXT: &str = "Task completed.";
        const REDACTED: &str = "synthetic-redacted-data";
        let message = ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![
                MessagePart::Text(FIRST_TEXT.into()),
                MessagePart::Thinking {
                    text: REASONING.into(),
                    signature: Some(SIGNATURE.into()),
                },
                MessagePart::Text(LAST_TEXT.into()),
                MessagePart::RedactedThinking {
                    data: REDACTED.into(),
                },
            ],
        };
        let rendered = super::native_message(&message).expect("native message renders");
        let actual = serde_json::to_value(rendered).expect("native message is JSON");
        assert_eq!(actual["content"][0]["text"], FIRST_TEXT);
        let reasoning: serde_json::Value = serde_json::from_str(
            actual["content"][1]["text"]
                .as_str()
                .expect("reasoning text"),
        )
        .expect("historical reasoning is JSON text");
        assert_eq!(
            reasoning,
            serde_json::json!({"type": "thinking", "text": REASONING, "signature": SIGNATURE})
        );
        assert_eq!(actual["content"][2]["text"], LAST_TEXT);
        let redacted: serde_json::Value = serde_json::from_str(
            actual["content"][3]["text"]
                .as_str()
                .expect("redacted text"),
        )
        .expect("historical redacted reasoning is JSON text");
        assert_eq!(
            redacted,
            serde_json::json!({"type": "redacted_thinking", "data": REDACTED})
        );
    }

    #[test]
    fn image_history_rejects_source_bytes_over_the_presentation_bound() {
        use signalbox_model_runtime::{ConversationRole, ImageInput, MessagePart};
        let mut operation = operation_with_message(ConversationMessage {
            role: ConversationRole::User,
            parts: vec![MessagePart::Image(ImageInput {
                media_type: "image/png".into(),
                bytes: std::sync::Arc::from([1_u8, 2, 3]),
            })],
        });
        operation.image_presentation =
            Some(crate::image::image_presentation_capability().limited_by(2, usize::MAX));
        assert!(
            translate(&operation).is_err(),
            "source image exceeds its three-byte fixture's two-byte bound"
        );
    }

    #[test]
    fn measured_growth_includes_appended_native_image_payload() {
        use signalbox_model_runtime::{ConversationRole, ImageInput, MessagePart};
        let mut operation = operation_with_message(ConversationMessage::user_text(IMAGE_CONTEXT));
        operation.image_presentation = Some(crate::image::image_presentation_capability());
        let baseline = super::serialized_request_bytes(&operation).expect("baseline renders");
        let message = ConversationMessage {
            role: ConversationRole::User,
            parts: vec![MessagePart::Image(ImageInput {
                media_type: "image/png".into(),
                bytes: std::sync::Arc::from([1_u8, 2, 3]),
            })],
        };
        let allowance =
            super::serialized_message_bytes(&message).expect("image measurement renders");
        operation.messages.push(message);
        let growth =
            super::serialized_request_bytes(&operation).expect("image history renders") - baseline;
        assert!(
            growth > 0,
            "the complete request includes native image bytes"
        );
        assert!(
            allowance >= growth,
            "the message allowance covers base64 and native history framing"
        );
    }

    fn operation_with_message(message: ConversationMessage) -> ModelOperation<()> {
        ModelOperation::new(
            (),
            CredentialReference::new("claude-subscription"),
            RequestedTarget::new("requested"),
            ResolvedTarget::new("resolved"),
            vec![message],
            ModelSettings::new(64),
        )
    }
}
