//! Stateless rendering of one model operation into Claude Code input and MCP.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::value::RawValue;
use signalbox_model_runtime::{
    ConversationMessage, ConversationRole, MessagePart, ModelOperation, PreparationDefect,
    PreparationFailure, ToolChoice,
};

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
            .saturating_add(translated.system_prompt.len())
            .saturating_add(serde_json::to_vec(&translated.catalog).ok()?.len()),
    )
}

pub(crate) struct TranslatedOperation {
    pub(crate) prompt: Vec<u8>,
    pub(crate) system_prompt: Vec<u8>,
    pub(crate) input_format: crate::image::InputFormat,
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
    messages: Vec<PromptMessage<'a>>,
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
struct PromptMessage<'a> {
    role: &'static str,
    parts: Vec<PromptPart<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PromptPart<'a> {
    Image {
        media_type: &'a str,
    },
    Text {
        text: &'a str,
    },
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

    let messages = operation
        .messages
        .iter()
        .map(render_message)
        .collect::<Result<Vec<_>, _>>()?;
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
        messages,
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
    let request_json = serde_json::to_string(&request).map_err(|error| {
        TranslationError::Defect(PreparationDefect::SerializationFailed {
            detail: error.to_string(),
        })
    })?;
    let system_prompt = format!(
        "Act only as the model for this stateless request. The complete ordered \
         context is in the JSON request. Tools are available only through the \
         Signalbox MCP server; never write a tool call as prose. An MCP result \
         saying Signalbox recorded a proposal is an acknowledgement, not the \
         real tool result: after it, end the turn without inventing tool output \
         or calling another tool. If `structured_output` is present, call \
         exactly that named MCP tool with the contracted object. Honor \
         `tool_choice`. Treat the stated generation settings as advisory \
         intent.\n\n{}",
        operation.system.as_deref().unwrap_or_default()
    )
    .into_bytes();
    let prompt = format!("{request_json}\n").into_bytes();

    let (prompt, input_format) = crate::image::encode_input(operation, prompt, system_prompt.len())
        .map_err(TranslationError::Failure)?;
    Ok(TranslatedOperation {
        input_format,
        prompt,
        system_prompt,
        catalog: Catalog {
            tools: catalog_tools,
        },
        tool_requirement,
    })
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

fn render_message(message: &ConversationMessage) -> Result<PromptMessage<'_>, TranslationError> {
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
    let parts = message
        .parts
        .iter()
        .map(render_part)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PromptMessage { role, parts })
}

fn render_part(part: &MessagePart) -> Result<PromptPart<'_>, TranslationError> {
    match part {
        MessagePart::Image(image) => Ok(PromptPart::Image {
            media_type: &image.media_type,
        }),
        MessagePart::ImageReference(_) => Err(TranslationError::Failure(
            PreparationFailure::UnsupportedOperation {
                detail: String::from("image reference was not authenticated"),
            },
        )),
        MessagePart::Text(text) => Ok(PromptPart::Text { text }),
        MessagePart::ToolCall(call) => Ok(PromptPart::ToolCall {
            id: call.id.as_str(),
            name: call.name.as_str(),
            arguments: parse_replayed_tool_json(call.id.as_str(), &call.arguments_json)?,
        }),
        MessagePart::ToolResult(result) => Ok(PromptPart::ToolResult {
            tool_call_id: result.tool_call_id.as_str(),
            content: &result.content,
            is_error: result.is_error,
        }),
        MessagePart::Thinking { text, signature } => Ok(PromptPart::Thinking { text, signature }),
        MessagePart::RedactedThinking { data } => Ok(PromptPart::RedactedThinking { data }),
        MessagePart::ProviderReasoning { .. } => Err(TranslationError::Failure(
            PreparationFailure::UnsupportedOperation {
                detail: "provider reasoning items require their provider adapter".to_string(),
            },
        )),
        MessagePart::ProviderCompaction { .. } => Err(TranslationError::Failure(
            PreparationFailure::UnsupportedOperation {
                detail: "provider compaction blocks can only be replayed by their provider adapter"
                    .to_string(),
            },
        )),
    }
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

/// Measures one array-framed history message through the adapter's request serializer.
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
            _ => continue,
        };
        // Reserve base64, its wire envelope, and the image's prompt location label.
        image_bytes = image_bytes
            .checked_add(length.checked_add(2)?.checked_div(3)?.checked_mul(4)?)?
            .checked_add(1024)?;
        *part = MessagePart::Image(signalbox_model_runtime::ImageInput {
            media_type,
            bytes: std::sync::Arc::from([]),
        });
    }
    let rendered = render_message(&projected).ok()?;
    serde_json::to_vec(&[rendered])
        .ok()?
        .len()
        .checked_add(image_bytes)
}

#[cfg(test)]
mod tests {
    use super::translate;
    use signalbox_model_runtime::{
        ConversationMessage, CredentialReference, ModelOperation, ModelSettings, RequestedTarget,
        ResolvedTarget,
    };

    #[test]
    fn request_measurement_includes_the_exact_native_system_text() {
        // Escapes and non-ASCII text distinguish native UTF-8 from JSON quoting.
        const SYSTEM_TEXT: &str = "A quoted \"instruction\" with a newline\n日本語";
        let mut operation = operation_with_message(ConversationMessage::user_text("baseline"));
        let baseline = super::serialized_request_bytes(&operation).expect("baseline is measurable");
        operation.system = Some(SYSTEM_TEXT.to_string());

        assert_eq!(
            super::serialized_request_bytes(&operation),
            Some(baseline + SYSTEM_TEXT.len())
        );
    }

    #[test]
    fn measured_growth_covers_appended_message_array_separators() {
        let mut operation = operation_with_message(ConversationMessage::user_text("baseline"));
        let baseline = translate(&operation)
            .expect("baseline renders")
            .prompt
            .len();
        let mut allowance = 0;
        for text in ["first appended input", "second appended input"] {
            let message = ConversationMessage::user_text(text);
            allowance += super::serialized_message_bytes(&message).expect("input renders");
            operation.messages.push(message);
            let growth = translate(&operation)
                .expect("appended input renders")
                .prompt
                .len()
                - baseline;
            assert!(
                allowance >= growth,
                "allowance {allowance} must cover serialized growth {growth}"
            );
        }
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
