//! Stateless rendering of one model operation into Codex stdin.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::value::RawValue;
use signalbox_model_runtime::{
    ConversationMessage, ConversationRole, MessagePart, ModelOperation, PreparationDefect,
    PreparationFailure, ToolChoice,
};

pub(crate) struct TranslatedOperation {
    pub(crate) prompt: Vec<u8>,
    pub(crate) declared_tools: Vec<String>,
    pub(crate) output_contract_name: Option<String>,
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
    messages: Vec<PromptMessage<'a>>,
    settings: PromptSettings<'a>,
    tools: Vec<PromptTool<'a>>,
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
struct PromptTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a RawValue,
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
    schema: &'a RawValue,
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
    let tools = operation
        .tools
        .iter()
        .map(|tool| {
            let input_schema = parse_object_schema(
                &tool.input_schema,
                &format!("tool `{}` input schema", tool.name.as_str()),
            )?;
            Ok(PromptTool {
                name: tool.name.as_str(),
                description: &tool.description,
                input_schema,
            })
        })
        .collect::<Result<Vec<_>, TranslationError>>()?;
    let output_contract = operation
        .output_contract
        .as_ref()
        .map(|contract| {
            let schema = parse_object_schema(
                &contract.schema,
                &format!("structured output `{}` schema", contract.name.as_str()),
            )?;
            Ok(PromptStructuredOutput {
                name: contract.name.as_str(),
                description: &contract.description,
                schema,
            })
        })
        .transpose()?;

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
        messages,
        settings: PromptSettings {
            max_output_tokens: operation.settings.max_output_tokens,
            temperature: operation.settings.temperature,
            top_p: operation.settings.top_p,
            stop_sequences: &operation.settings.stop_sequences,
        },
        tools,
        tool_choice,
        structured_output: output_contract,
    };
    let request_json = serde_json::to_string(&request).map_err(|error| {
        TranslationError::Defect(PreparationDefect::SerializationFailed {
            detail: error.to_string(),
        })
    })?;
    // The preamble states tool authority exactly once, affirmatively, and
    // before anything else it says about tools. An earlier revision opened
    // with a categorical prohibition ("Do not use shell, file, web, MCP, or
    // collaboration tools") aimed at the wrapped CLI's native facilities;
    // because the prohibited categories are exactly the categories a caller's
    // tool catalog populates, a model could read that sentence as the
    // controlling authority over the declared tools and refuse work they
    // authorize. The native facilities need no prompt-level prohibition — the
    // invocation disables them mechanically, and prompt text is never a
    // capability boundary — so the preamble names the serialized `tools`
    // array as the single authority, carves out `tool_choice` and
    // `structured_output` as the only statements that narrow or add to what
    // may be proposed, and never states a tool prohibition.
    let prompt = format!(
        "Act only as the model for the following stateless request. The `tools` array \
         in the JSON below is the single authority on which tools exist for this \
         request; the `tool_choice` and `structured_output` members below are the \
         only statements that narrow or add to what you may propose, and nothing \
         from this harness changes either. This harness's own facilities are \
         disabled and are not the declared tools; a tool runs only when you propose \
         it in the response envelope and the caller executes it. The complete \
         ordered context is in the JSON below. Return exactly the response envelope \
         required by the supplied output schema. `outcome` is `refused` only for a \
         safety refusal; declared-tool availability is never grounds for refusal. \
         For ordinary completion put response text in `text`. Propose declared tools \
         only in `tool_calls`; each `arguments` value is a string carrying exactly \
         the tool's JSON argument object. If `structured_output` is present, return \
         exactly one tool call bearing its name and the contracted value \
         JSON-encoded in its `arguments` string. Honor `tool_choice`. Treat the \
         stated generation settings as advisory intent.\n\n{request_json}\n"
    )
    .into_bytes();

    Ok(TranslatedOperation {
        prompt,
        declared_tools: operation
            .tools
            .iter()
            .map(|tool| tool.name.as_str().to_string())
            .collect(),
        output_contract_name: operation
            .output_contract
            .as_ref()
            .map(|contract| contract.name.as_str().to_string()),
        tool_requirement,
    })
}

fn render_message(message: &ConversationMessage) -> Result<PromptMessage<'_>, TranslationError> {
    let role = match message.role {
        ConversationRole::User => "user",
        ConversationRole::Assistant => "assistant",
    };
    // Reject locally knowable role/part contradictions before the operation
    // crosses the spawn boundary, matching the sibling adapters: a tool call is
    // replayed assistant output and a tool result is caller-produced, so
    // neither belongs under the opposite role, and thinking is assistant-only.
    for part in &message.parts {
        let valid = matches!(part, MessagePart::Text(_))
            || matches!(
                (message.role, part),
                (ConversationRole::User, MessagePart::ToolResult(_))
                    | (
                        ConversationRole::Assistant,
                        MessagePart::ToolCall(_)
                            | MessagePart::Thinking { .. }
                            | MessagePart::RedactedThinking { .. }
                    )
            );
        if !valid {
            return Err(TranslationError::Failure(
                PreparationFailure::UnsupportedOperation {
                    detail: "Codex requires tool results in user-role messages and tool calls or \
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
    // The declared tool interface (`ToolDefinition::input_schema`) describes
    // an arguments object, so scalar or array replay history could never have
    // satisfied it; mirror the sibling adapters and fail preparation instead
    // of rendering an impossible call into the prompt.
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
        && !(0.0..=2.0).contains(&value)
    {
        return Err(TranslationError::Failure(
            PreparationFailure::UnsupportedOperation {
                detail: "temperature must be a finite number from 0 through 2".to_string(),
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

#[cfg(test)]
mod tests {
    use serde_json::value::RawValue;
    use signalbox_model_runtime::{
        ConversationMessage, ConversationRole, CredentialReference, MessagePart, ModelOperation,
        ModelSettings, RequestedTarget, ResolvedTarget, ToolCallId, ToolCallProposal,
        ToolDefinition, ToolName, ToolResultRecord,
    };

    use super::translate;

    fn operation_with_message(message: ConversationMessage) -> ModelOperation<()> {
        ModelOperation::new(
            (),
            CredentialReference::new("codex-subscription"),
            RequestedTarget::new("requested"),
            ResolvedTarget::new("resolved"),
            vec![message],
            ModelSettings::new(64),
        )
    }

    #[test]
    fn tool_call_in_a_user_message_is_unsupported() {
        let operation = operation_with_message(ConversationMessage {
            role: ConversationRole::User,
            parts: vec![MessagePart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("call_role"),
                name: ToolName::new("lookup"),
                arguments_json: "{}".to_string(),
            })],
        });

        assert!(translate(&operation).is_err());
    }

    #[test]
    fn tool_result_in_an_assistant_message_is_unsupported() {
        let operation = operation_with_message(ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::ToolResult(ToolResultRecord {
                tool_call_id: ToolCallId::new("call_role"),
                content: "done".to_string(),
                is_error: false,
            })],
        });

        assert!(translate(&operation).is_err());
    }

    /// Builds an assistant message replaying one tool call with the given
    /// raw arguments and asserts preparation rejects it for not being a JSON
    /// object, keeping each named case a straight-line invocation.
    #[track_caller]
    fn assert_replayed_arguments_rejected(arguments: &str) {
        let operation = operation_with_message(ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("call_shape"),
                name: ToolName::new("lookup"),
                arguments_json: arguments.to_string(),
            })],
        });

        assert!(
            translate(&operation).is_err(),
            "`{arguments}` must fail preparation: the declared tool interface \
             describes an arguments object"
        );
    }

    /// Replayed tool-call arguments must be a JSON object — scalar or array
    /// history could never have satisfied `ToolDefinition::input_schema`, and
    /// the sibling adapters reject the same shapes during preparation.
    #[test]
    fn replayed_array_tool_arguments_are_rejected() {
        assert_replayed_arguments_rejected("[1, 2]");
    }

    #[test]
    fn replayed_number_tool_arguments_are_rejected() {
        assert_replayed_arguments_rejected("3");
    }

    #[test]
    fn replayed_string_tool_arguments_are_rejected() {
        assert_replayed_arguments_rejected(r#""Oslo""#);
    }

    #[test]
    fn replayed_null_tool_arguments_are_rejected() {
        assert_replayed_arguments_rejected("null");
    }

    #[test]
    fn valid_role_part_pairs_translate() {
        let user = operation_with_message(ConversationMessage {
            role: ConversationRole::User,
            parts: vec![MessagePart::ToolResult(ToolResultRecord {
                tool_call_id: ToolCallId::new("call_role"),
                content: "done".to_string(),
                is_error: false,
            })],
        });
        let assistant = operation_with_message(ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("call_role"),
                name: ToolName::new("lookup"),
                arguments_json: "{}".to_string(),
            })],
        });

        assert!(translate(&user).is_ok());
        assert!(translate(&assistant).is_ok());
    }

    fn deeply_nested_operation() -> ModelOperation<()> {
        let depth = 512;
        let nested = format!(
            "{}\"leaf\"{}",
            r#"{"nested":"#.repeat(depth),
            "}".repeat(depth)
        );
        let schema = format!(r#"{{"type":"object","deep":{nested}}}"#);
        let arguments = format!(r#"{{"deep":{nested}}}"#);
        let mut operation = ModelOperation::new(
            (),
            CredentialReference::new("codex-subscription"),
            RequestedTarget::new("requested"),
            ResolvedTarget::new("resolved"),
            vec![ConversationMessage::user_text("hello")],
            ModelSettings::new(64),
        );
        operation.tools.push(ToolDefinition::with_raw_schema(
            "deep",
            "Deep stack-safety fixture.",
            RawValue::from_string(schema).expect("deep schema is valid raw JSON"),
        ));
        operation.messages.push(ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("call_deep"),
                name: ToolName::new("deep"),
                arguments_json: arguments,
            })],
        });
        operation
    }

    #[test]
    fn deep_schema_and_replay_arguments_remain_raw_through_prompt_rendering() {
        let operation = deeply_nested_operation();
        let translated = translate(&operation).expect("deep raw JSON translates");
        let prompt = String::from_utf8(translated.prompt).expect("prompt is UTF-8");

        assert!(prompt.contains(r#""call_deep""#));
        assert!(prompt.contains(r#""leaf""#));
        drop(operation);
    }

    /// The phantom-prohibition regression: an earlier preamble opened with
    /// "Do not use shell, file, web, MCP, or collaboration tools" — aimed at
    /// the wrapped CLI's native facilities — and because the prohibited
    /// categories are exactly the categories a caller's catalog populates, a
    /// model could quote that sentence as the controlling authority over the
    /// declared tools and refuse work they authorize. The rendered prompt
    /// must carry exactly one tool-authority statement, positioned before
    /// the serialized request it governs, and no categorical tool
    /// prohibition a model could mistake for one governing declared tools.
    #[test]
    fn prompt_states_tool_authority_once_and_never_prohibits_declared_tools() {
        let mut operation = operation_with_message(ConversationMessage::user_text("hello"));
        operation.tools.push(ToolDefinition::with_raw_schema(
            "shell",
            "Run one command on the caller's side.",
            RawValue::from_string(r#"{"type":"object"}"#.to_string())
                .expect("tool schema is valid raw JSON"),
        ));

        let translated = translate(&operation).expect("declared tools translate");
        let prompt = String::from_utf8(translated.prompt).expect("prompt is UTF-8");

        assert!(
            !prompt.contains("Do not use"),
            "the preamble must not restate a categorical tool prohibition"
        );
        let authority = "single authority on which tools exist";
        assert_eq!(
            prompt.matches(authority).count(),
            1,
            "the tool-authority statement must appear exactly once"
        );
        let authority_position = prompt.find(authority).expect("authority statement present");
        let request_position = prompt.find("\n\n{").expect("serialized request present");
        assert!(
            authority_position < request_position,
            "the tool-authority statement must precede the serialized request"
        );
    }
}
