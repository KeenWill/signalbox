//! Operation-to-wire translation.

use std::collections::BTreeSet;

use signalbox_model_runtime::{
    AnthropicServiceTier, CodexCliServiceTier, ConversationMessage, ConversationRole, DeliveryMode,
    FastMode, MessagePart, ModelOperation, ModelSettings, OpenAiServiceTier, PreparationFailure,
    ReasoningLevel, ServiceTier, ToolChoice,
};

use crate::wire::{CreateResponse, WireFunctionTool, WireInputItem, WireReasoning};

/// Measures the complete adapter request, including instructions and tool declarations.
pub fn serialized_request_bytes<C>(operation: &ModelOperation<C>) -> Option<usize> {
    let request = build_request_with_fast_mode(operation, operation.settings.fast_mode).ok()?;
    Some(serde_json::to_vec(&request).ok()?.len())
}

/// Measures one standalone history message through the adapter's request serializer.
///
/// Returns `None` for a message the adapter cannot render. Independent message
/// envelopes conservatively retain framing that adjacent messages may share.
pub fn serialized_message_bytes(message: &ConversationMessage) -> Option<usize> {
    let mut rendered = Vec::new();
    wire_messages(message, &mut rendered).ok()?;
    serde_json::to_vec(&rendered).ok().map(|bytes| bytes.len())
}

/// Builds the wire request for one operation.
///
/// Pure translation: any failure is a trustworthy [`PreparationFailure`]
/// returned before a one-shot capability exists. Nothing has touched the
/// network.
///
/// A structured-output contract is realized as a forced function call — the
/// same mechanism the Anthropic adapter uses — so the provider-independent
/// decode in the core crate applies unchanged. (The provider's native
/// `text.format` mechanism would return the value as content text and
/// require strict-mode schema transformation; a forced function keeps the
/// contract uniform across adapters.) The contract joins the declared tools
/// under its reserved name — [`ModelOperation::validate`] rejects
/// collisions — and is forced with parallel tool calling disabled.
///
/// Streamed delivery reports usage in the terminal response event.
#[cfg(test)]
pub(crate) fn build_request<C>(
    operation: &ModelOperation<C>,
) -> Result<CreateResponse, PreparationFailure> {
    build_request_with_fast_mode(operation, operation.settings.fast_mode)
}

pub(crate) fn build_request_with_fast_mode<C>(
    operation: &ModelOperation<C>,
    request_fast_mode: FastMode,
) -> Result<CreateResponse, PreparationFailure> {
    if let Err(error) = operation.validate() {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: error.to_string(),
        });
    }
    validate_function_names(operation)?;
    validate_output_ceiling(&operation.settings)?;
    if let Some(value) = operation.settings.temperature
        && !(0.0..=2.0).contains(&value)
    {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: "temperature must be a finite number from 0 through 2".to_string(),
        });
    }
    if let Some(value) = operation.settings.top_p
        && !(0.0..=1.0).contains(&value)
    {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: "top_p must be a finite number from 0 through 1".to_string(),
        });
    }
    validate_stop_sequences(&operation.settings)?;
    let (tools, tool_choice, parallel_tool_calls) = tools_and_choice(operation)?;
    let mut messages = Vec::new();
    if let Some(system) = &operation.system {
        messages.push(WireInputItem::Message {
            role: "system",
            content: system.clone(),
        });
    }
    for message in &operation.messages {
        wire_messages(message, &mut messages)?;
    }
    if messages.is_empty() {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: "Responses requires at least one message".to_string(),
        });
    }
    validate_tool_history(&operation.messages)?;
    let streamed = operation.delivery == DeliveryMode::Streamed;
    let reasoning_effort = operation
        .settings
        .reasoning_level
        .map(openai_reasoning_effort)
        .transpose()?;
    let service_tier = openai_service_tier(&operation.settings, request_fast_mode)?;
    Ok(CreateResponse {
        model: operation.resolved_target.as_str().to_string(),
        input: messages,
        max_output_tokens: operation.settings.max_output_tokens,
        reasoning: reasoning_effort.map(|effort| WireReasoning { effort }),
        store: false,
        include: &["reasoning.encrypted_content"],
        service_tier,
        temperature: operation.settings.temperature,
        top_p: operation.settings.top_p,
        tools,
        tool_choice,
        parallel_tool_calls,
        stream: streamed,
    })
}

/// Validates the complete settings combination enforced by this adapter.
///
/// Capability-set validation remains the caller's responsibility. This check
/// owns cross-knob constraints that independent capability sets cannot state.
pub fn validate_model_settings(settings: &ModelSettings) -> Result<(), PreparationFailure> {
    validate_output_ceiling(settings)?;
    validate_stop_sequences(settings)?;
    settings
        .reasoning_level
        .map(openai_reasoning_effort)
        .transpose()?;
    openai_service_tier(settings, settings.fast_mode)?;
    Ok(())
}

fn validate_stop_sequences(settings: &ModelSettings) -> Result<(), PreparationFailure> {
    if !settings.stop_sequences.is_empty() {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: "Responses does not support stop sequences".to_string(),
        });
    }
    Ok(())
}

fn validate_output_ceiling(settings: &ModelSettings) -> Result<(), PreparationFailure> {
    if settings.max_output_tokens < 16 {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: "max_output_tokens must be at least 16".to_string(),
        });
    }
    Ok(())
}

fn openai_reasoning_effort(level: ReasoningLevel) -> Result<&'static str, PreparationFailure> {
    match level {
        ReasoningLevel::None => Ok("none"),
        ReasoningLevel::Minimal => Ok("minimal"),
        ReasoningLevel::Low => Ok("low"),
        ReasoningLevel::Medium => Ok("medium"),
        ReasoningLevel::High => Ok("high"),
        ReasoningLevel::XHigh => Ok("xhigh"),
        ReasoningLevel::Max => Ok("max"),
        ReasoningLevel::Ultra => Err(PreparationFailure::UnsupportedOperation {
            detail: "OpenAI Responses cannot enforce ultra reasoning".to_string(),
        }),
    }
}

fn openai_service_tier(
    settings: &ModelSettings,
    request_fast_mode: FastMode,
) -> Result<Option<&'static str>, PreparationFailure> {
    let tier = match settings.service_tier {
        Some(ServiceTier::OpenAi(OpenAiServiceTier::Auto)) => Some("auto"),
        Some(ServiceTier::OpenAi(OpenAiServiceTier::Default)) => Some("default"),
        Some(ServiceTier::OpenAi(OpenAiServiceTier::Flex)) => Some("flex"),
        Some(ServiceTier::OpenAi(OpenAiServiceTier::Scale)) => Some("scale"),
        Some(ServiceTier::OpenAi(OpenAiServiceTier::Priority)) => Some("priority"),
        Some(ServiceTier::OpenAi(OpenAiServiceTier::Fast)) => Some("fast"),
        Some(ServiceTier::Anthropic(
            AnthropicServiceTier::Auto | AnthropicServiceTier::StandardOnly,
        ))
        | Some(ServiceTier::CodexCli(
            CodexCliServiceTier::Default
            | CodexCliServiceTier::Priority
            | CodexCliServiceTier::Flex,
        )) => {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: "OpenAI cannot enforce another provider's service tier".to_string(),
            });
        }
        None => None,
    };
    match (settings.fast_mode, tier) {
        (FastMode::Enabled, None | Some("fast")) => Ok(match (request_fast_mode, tier) {
            (FastMode::Enabled, _) => Some("fast"),
            (FastMode::Disabled, tier) => tier,
        }),
        (FastMode::Enabled, Some(_)) => Err(PreparationFailure::UnsupportedOperation {
            detail: "OpenAI fast mode is incompatible with a non-fast service tier".to_string(),
        }),
        (FastMode::Disabled, _) => Ok(tier),
    }
}

fn validate_function_names<C>(operation: &ModelOperation<C>) -> Result<(), PreparationFailure> {
    for tool in &operation.tools {
        validate_function_name(tool.name.as_str(), "tool")?;
    }
    if let Some(contract) = &operation.output_contract {
        validate_function_name(contract.name.as_str(), "structured-output contract")?;
    }
    for message in &operation.messages {
        for part in &message.parts {
            if let MessagePart::ToolCall(call) = part {
                validate_function_name(call.name.as_str(), "replayed tool call")?;
            }
        }
    }
    Ok(())
}

fn validate_function_name(name: &str, subject: &str) -> Result<(), PreparationFailure> {
    if is_valid_function_name(name) {
        Ok(())
    } else {
        Err(PreparationFailure::UnsupportedOperation {
            detail: format!(
                "OpenAI {subject} name must contain 1 through 64 ASCII letters, digits, underscores, or hyphens"
            ),
        })
    }
}

/// Whether a function name satisfies the grammar shared by request and
/// response tool material.
pub(crate) fn is_valid_function_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn validate_tool_history(messages: &[ConversationMessage]) -> Result<(), PreparationFailure> {
    let mut pending_calls: Option<BTreeSet<&str>> = None;
    for message in messages {
        let mut results = BTreeSet::new();
        for part in &message.parts {
            if let MessagePart::ToolResult(result) = part
                && !results.insert(result.tool_call_id.as_str())
            {
                return Err(PreparationFailure::UnsupportedOperation {
                    detail: format!(
                        "tool result {} appears more than once",
                        result.tool_call_id.as_str()
                    ),
                });
            }
        }

        if let Some(mut expected) = pending_calls.take() {
            if message.role != ConversationRole::User || results.is_empty() {
                return Err(PreparationFailure::UnsupportedOperation {
                    detail: "Responses requires consecutive user tool-result messages \
                             until every pending tool call is answered"
                        .to_string(),
                });
            }
            if let Some(unexpected) = results.iter().find(|id| !expected.contains(**id)) {
                return Err(PreparationFailure::UnsupportedOperation {
                    detail: format!(
                        "tool result {unexpected} does not answer a pending Responses \
                         tool call"
                    ),
                });
            }
            for part in &message.parts {
                match part {
                    MessagePart::Image(_) | MessagePart::ImageReference(_) => {
                        return Err(PreparationFailure::UnsupportedOperation {
                            detail: String::from("this adapter does not present images"),
                        });
                    }
                    MessagePart::ToolResult(result) => {
                        expected.remove(result.tool_call_id.as_str());
                    }
                    _ if !expected.is_empty() => {
                        return Err(PreparationFailure::UnsupportedOperation {
                            detail: "Responses requires every pending tool result before \
                                     intervening user content"
                                .to_string(),
                        });
                    }
                    _ => {}
                }
            }
            if !expected.is_empty() {
                pending_calls = Some(expected);
                continue;
            }
        } else if !results.is_empty() {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: "Responses tool results must answer calls from the immediately \
                         preceding assistant message"
                    .to_string(),
            });
        }

        if message.role == ConversationRole::Assistant {
            let mut calls = BTreeSet::new();
            for part in &message.parts {
                if let MessagePart::ToolCall(call) = part
                    && !calls.insert(call.id.as_str())
                {
                    return Err(PreparationFailure::UnsupportedOperation {
                        detail: format!("tool call {} appears more than once", call.id.as_str()),
                    });
                }
            }
            if !calls.is_empty() {
                pending_calls = Some(calls);
            }
        }
    }
    if pending_calls.is_some() {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: "Responses requires tool calls to be followed by matching results".to_string(),
        });
    }
    Ok(())
}

type ToolsAndChoice = (
    Option<Vec<WireFunctionTool>>,
    Option<serde_json::Value>,
    Option<bool>,
);

fn tools_and_choice<C>(
    operation: &ModelOperation<C>,
) -> Result<ToolsAndChoice, PreparationFailure> {
    for tool in &operation.tools {
        if !raw_json_is_object(&tool.input_schema) {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: format!(
                    "Responses requires function {} to carry a JSON Schema object",
                    tool.name.as_str()
                ),
            });
        }
    }
    if let Some(contract) = &operation.output_contract
        && !raw_json_is_object(&contract.schema)
    {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: format!(
                "Responses requires output contract {} to carry a JSON Schema object",
                contract.name.as_str()
            ),
        });
    }
    let function_count = operation.tools.len() + usize::from(operation.output_contract.is_some());
    if function_count > 128 {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: "Responses accepts at most 128 functions in one request".to_string(),
        });
    }
    let mut tools: Vec<WireFunctionTool> = operation
        .tools
        .iter()
        .map(|tool| WireFunctionTool {
            kind: "function",

            name: tool.name.as_str().to_string(),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
            strict: false,
        })
        .collect();
    if let Some(contract) = &operation.output_contract {
        tools.push(WireFunctionTool {
            kind: "function",

            name: contract.name.as_str().to_string(),
            description: contract.description.clone(),
            parameters: contract.schema.clone(),
            strict: false,
        });
        return Ok((
            Some(tools),
            Some(serde_json::json!({
                "type": "function",
                "name": contract.name.as_str()
            })),
            // The contract promises exactly one value; parallel tool
            // calling could return several calls to the forced function.
            Some(false),
        ));
    }
    if tools.is_empty() {
        return Ok((None, None, None));
    }
    let choice = match &operation.tool_choice {
        ToolChoice::Automatic => serde_json::json!("auto"),
        ToolChoice::AnyTool => serde_json::json!("required"),
        ToolChoice::Named(name) => serde_json::json!({
            "type": "function",
            "name": name.as_str()
        }),
    };
    Ok((Some(tools), Some(choice), None))
}

/// Translates parts in their original order into Responses input items.
fn wire_messages(
    message: &ConversationMessage,
    out: &mut Vec<WireInputItem>,
) -> Result<(), PreparationFailure> {
    let unsupported = |detail: &str| PreparationFailure::UnsupportedOperation {
        detail: detail.to_string(),
    };
    if message.parts.is_empty() {
        return Err(unsupported(
            "Responses cannot preserve an empty conversation message",
        ));
    }
    let role = match message.role {
        ConversationRole::User => "user",
        ConversationRole::Assistant => "assistant",
    };
    let mut parts = message.parts.iter().peekable();
    while let Some(part) = parts.next() {
        match part {
            MessagePart::Image(_) | MessagePart::ImageReference(_) => {
                return Err(unsupported("this adapter does not present images"));
            }
            MessagePart::Text(text) => {
                let mut content = text.clone();
                while let Some(MessagePart::Text(text)) = parts.peek() {
                    content.push_str(text);
                    parts.next();
                }
                out.push(WireInputItem::Message { role, content });
            }
            MessagePart::ToolCall(call) => {
                if message.role != ConversationRole::Assistant {
                    return Err(unsupported("tool calls require assistant history"));
                }
                let arguments =
                    serde_json::value::RawValue::from_string(call.arguments_json.clone())
                        .map_err(|_| unsupported("replayed tool arguments must be JSON"))?;
                if !raw_json_is_object(&arguments) {
                    return Err(unsupported("replayed tool arguments must be an object"));
                }
                out.push(WireInputItem::FunctionCall {
                    call_id: call.id.as_str().to_string(),
                    name: call.name.as_str().to_string(),
                    arguments: call.arguments_json.clone(),
                });
            }
            MessagePart::ToolResult(result) => {
                if message.role != ConversationRole::User {
                    return Err(unsupported("tool results require user history"));
                }
                out.push(WireInputItem::FunctionCallOutput {
                    call_id: result.tool_call_id.as_str().to_string(),
                    output: result.content.clone(),
                });
            }
            MessagePart::Thinking { .. } | MessagePart::RedactedThinking { .. } => {
                return Err(unsupported(
                    "OpenAI cannot replay another provider's thinking",
                ));
            }
            MessagePart::ProviderReasoning { item_json, .. } => {
                if message.role != ConversationRole::Assistant {
                    return Err(unsupported("provider reasoning requires assistant history"));
                }
                signalbox_model_runtime::validate_provider_json_nesting(item_json.as_bytes())
                    .map_err(|_| {
                        unsupported("replayed reasoning exceeds the JSON nesting bound")
                    })?;
                let item: crate::wire::WireOutputItem =
                    serde_json::from_str(item_json).map_err(|_| {
                        unsupported("replayed reasoning must be a complete reasoning item")
                    })?;
                if item.kind != "reasoning"
                    || item.id.as_deref().is_none_or(str::is_empty)
                    || item.encrypted_content.is_none()
                {
                    return Err(unsupported(
                        "replayed reasoning lacks its type, id or encrypted content",
                    ));
                }
                let raw = serde_json::value::RawValue::from_string(item_json.clone())
                    .map_err(|_| unsupported("replayed reasoning must be JSON"))?;
                out.push(WireInputItem::ProviderReasoning(raw));
            }
            MessagePart::ProviderCompaction { .. } => {
                return Err(unsupported("OpenAI provider compaction is unsupported"));
            }
        }
    }
    Ok(())
}

fn raw_json_is_object(raw: &serde_json::value::RawValue) -> bool {
    raw.get().bytes().find(|byte| !byte.is_ascii_whitespace()) == Some(b'{')
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use signalbox_model_runtime::CredentialReference;
    use signalbox_model_runtime::{
        ConversationMessage, ConversationRole, DeliveryMode, FastMode, MessagePart, ModelOperation,
        ModelSettings, OpenAiServiceTier, PreparationFailure, ReasoningLevel, RequestedTarget,
        ResolvedTarget, ServiceTier, StructuredOutputContract, ToolCallId, ToolCallProposal,
        ToolChoice, ToolDefinition, ToolName, ToolResultRecord,
    };

    use super::{build_request, build_request_with_fast_mode, validate_model_settings};

    /// An operation whose correlation seed is the one knob; targets, one
    /// user-role message, and a 64-token ceiling are canonical.
    fn operation(correlation: &str) -> ModelOperation<String> {
        ModelOperation::new(
            correlation.to_string(),
            CredentialReference::new("openai-primary"),
            RequestedTarget::new("fast-alias"),
            ResolvedTarget::new("model-exact-1"),
            vec![ConversationMessage::user_text("hello")],
            ModelSettings::new(64),
        )
    }

    #[test]
    fn reasoning_uses_the_openai_effort_control() {
        let mut operation = operation("call-settings");
        operation.settings.reasoning_level = Some(ReasoningLevel::Minimal);

        let request = build_request(&operation).expect("supported reasoning translates");
        let value = serde_json::to_value(request).expect("wire request serializes");

        assert_eq!(value["reasoning"]["effort"], "minimal");
    }

    #[test]
    fn fast_mode_and_tier_use_the_openai_wire_control() {
        let mut operation = operation("call-settings");
        operation.settings.fast_mode = FastMode::Enabled;
        operation.settings.service_tier = Some(ServiceTier::OpenAi(OpenAiServiceTier::Fast));

        let request = build_request(&operation).expect("supported controls translate");
        let value = serde_json::to_value(request).expect("wire request serializes");

        assert_eq!(value["service_tier"], "fast");
    }

    #[test]
    fn fast_mode_rejects_openai_flex_tier_before_send() {
        let mut operation = operation("call-incompatible-settings");
        operation.settings.fast_mode = FastMode::Enabled;
        operation.settings.service_tier = Some(ServiceTier::OpenAi(OpenAiServiceTier::Flex));

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn mapped_fast_mode_still_rejects_openai_flex_tier() {
        let mut operation = operation("call-mapped-incompatible-settings");
        operation.settings.fast_mode = FastMode::Enabled;
        operation.settings.service_tier = Some(ServiceTier::OpenAi(OpenAiServiceTier::Flex));

        assert!(matches!(
            build_request_with_fast_mode(&operation, FastMode::Disabled),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[track_caller]
    fn assert_temperature_is_rejected(value: f64) {
        let mut candidate = operation("call-temperature");
        candidate.settings.temperature = Some(value);
        assert!(matches!(
            build_request(&candidate),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[track_caller]
    fn assert_top_p_is_rejected(value: f64) {
        let mut candidate = operation("call-top-p");
        candidate.settings.top_p = Some(value);
        assert!(matches!(
            build_request(&candidate),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    fn request_json(operation: &ModelOperation<String>) -> String {
        let request = build_request(operation).expect("translatable operation builds");
        let mut value = serde_json::to_value(&request).expect("wire request serializes");
        value.sort_all_objects();
        format!("{value:#}")
    }

    #[test]
    fn deep_schema_and_replay_arguments_remain_stack_safe_through_wire_lifetime() {
        let depth = 512;
        let nested = format!(
            "{}\"leaf\"{}",
            r#"{"nested":"#.repeat(depth),
            "}".repeat(depth)
        );
        let schema = format!(r#"{{"type":"object","deep":{nested}}}"#);
        let arguments = format!(r#"{{"deep":{nested}}}"#);
        let mut operation = operation("call-deep-json");
        operation.tools.push(ToolDefinition::with_raw_schema(
            "deep",
            "Deep stack-safety fixture.",
            serde_json::value::RawValue::from_string(schema)
                .expect("deep schema is valid raw JSON"),
        ));
        operation.messages.push(ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("call_deep"),
                name: ToolName::new("deep"),
                arguments_json: arguments,
            })],
        });
        operation.messages.push(ConversationMessage {
            role: ConversationRole::User,
            parts: vec![MessagePart::ToolResult(ToolResultRecord {
                tool_call_id: ToolCallId::new("call_deep"),
                content: "done".to_owned(),
                is_error: false,
            })],
        });

        let request = build_request(&operation).expect("deep raw JSON translates");
        let encoded = serde_json::to_string(&request).expect("deep raw JSON serializes");
        assert!(encoded.contains(r#""leaf""#));
        drop(request);
        drop(operation);
    }

    #[test]
    fn full_operation_serializes_every_stated_fact() {
        let mut operation = operation("call-1");
        operation.system = Some("Answer briefly.".to_string());
        operation.settings.temperature = Some(0.5);
        operation.settings.top_p = Some(0.9);
        operation.messages = vec![
            ConversationMessage::user_text("look up Oslo"),
            ConversationMessage {
                role: ConversationRole::Assistant,
                parts: vec![
                    MessagePart::Text("Looking it up.".to_string()),
                    MessagePart::ToolCall(ToolCallProposal {
                        id: ToolCallId::new("call_a1"),
                        name: ToolName::new("lookup"),
                        arguments_json: r#"{"city":"Oslo"}"#.to_string(),
                    }),
                ],
            },
            ConversationMessage {
                role: ConversationRole::User,
                parts: vec![
                    MessagePart::ToolResult(ToolResultRecord {
                        tool_call_id: ToolCallId::new("call_a1"),
                        content: "population 700000".to_string(),
                        is_error: false,
                    }),
                    MessagePart::Text("thanks".to_string()),
                ],
            },
        ];
        operation.tools = vec![ToolDefinition::with_schema(
            "lookup",
            "Looks up a city.",
            serde_json::json!({"type": "object"}),
        )];
        operation.tool_choice = ToolChoice::Named(ToolName::new("lookup"));

        expect![[r#"
            {
              "include": [
                "reasoning.encrypted_content"
              ],
              "input": [
                {
                  "content": "Answer briefly.",
                  "role": "system",
                  "type": "message"
                },
                {
                  "content": "look up Oslo",
                  "role": "user",
                  "type": "message"
                },
                {
                  "content": "Looking it up.",
                  "role": "assistant",
                  "type": "message"
                },
                {
                  "arguments": "{\"city\":\"Oslo\"}",
                  "call_id": "call_a1",
                  "name": "lookup",
                  "type": "function_call"
                },
                {
                  "call_id": "call_a1",
                  "output": "population 700000",
                  "type": "function_call_output"
                },
                {
                  "content": "thanks",
                  "role": "user",
                  "type": "message"
                }
              ],
              "max_output_tokens": 64,
              "model": "model-exact-1",
              "store": false,
              "stream": false,
              "temperature": 0.5,
              "tool_choice": {
                "name": "lookup",
                "type": "function"
              },
              "tools": [
                {
                  "description": "Looks up a city.",
                  "name": "lookup",
                  "parameters": {
                    "type": "object"
                  },
                  "strict": false,
                  "type": "function"
                }
              ],
              "top_p": 0.9
            }"#]]
        .assert_eq(&request_json(&operation));
    }

    #[test]
    fn the_wire_model_is_the_resolved_target_never_the_requested_selection() {
        let operation = operation("call-2");

        let request = build_request(&operation).expect("translatable operation builds");

        assert_eq!(request.model, operation.resolved_target.as_str());
    }

    #[test]
    fn streamed_delivery_sets_stream_and_disables_storage() {
        let mut operation = operation("call-3");
        operation.delivery = DeliveryMode::Streamed;

        let request = build_request(&operation).expect("translatable operation builds");
        let value = serde_json::to_value(&request).expect("wire request serializes");

        assert_eq!(value["stream"], serde_json::json!(true));
        assert_eq!(value["store"], serde_json::json!(false));
    }

    #[test]
    fn adjacent_text_parts_share_a_message_without_crossing_calls_or_message_boundaries() {
        let mut candidate = operation("call-adjacent-text");
        candidate.messages = vec![
            ConversationMessage {
                role: ConversationRole::User,
                parts: vec![
                    MessagePart::Text("hello".to_string()),
                    MessagePart::Text(" there".to_string()),
                ],
            },
            ConversationMessage {
                role: ConversationRole::Assistant,
                parts: vec![
                    MessagePart::Text("first".to_string()),
                    MessagePart::Text(" second".to_string()),
                    MessagePart::ToolCall(ToolCallProposal {
                        id: ToolCallId::new("call_fixture"),
                        name: ToolName::new("lookup"),
                        arguments_json: "{}".to_string(),
                    }),
                    MessagePart::Text("after".to_string()),
                    MessagePart::Text(" call".to_string()),
                ],
            },
            ConversationMessage {
                role: ConversationRole::User,
                parts: vec![
                    MessagePart::ToolResult(ToolResultRecord {
                        tool_call_id: ToolCallId::new("call_fixture"),
                        content: "result".to_string(),
                        is_error: false,
                    }),
                    MessagePart::Text("done".to_string()),
                    MessagePart::Text(" now".to_string()),
                ],
            },
            ConversationMessage::user_text("next boundary"),
        ];
        let request = serde_json::to_value(build_request(&candidate).unwrap()).unwrap();
        assert_eq!(
            request["input"],
            serde_json::json!([
                {"type":"message","role":"user","content":"hello there"},
                {"type":"message","role":"assistant","content":"first second"},
                {"type":"function_call","call_id":"call_fixture","name":"lookup","arguments":"{}"},
                {"type":"message","role":"assistant","content":"after call"},
                {"type":"function_call_output","call_id":"call_fixture","output":"result"},
                {"type":"message","role":"user","content":"done now"},
                {"type":"message","role":"user","content":"next boundary"}
            ])
        );
    }

    #[test]
    fn minimal_operation_omits_every_unset_optional_field() {
        expect![[r#"
            {
              "include": [
                "reasoning.encrypted_content"
              ],
              "input": [
                {
                  "content": "hello",
                  "role": "user",
                  "type": "message"
                }
              ],
              "max_output_tokens": 64,
              "model": "model-exact-1",
              "store": false,
              "stream": false
            }"#]]
        .assert_eq(&request_json(&operation("call-4")));
    }

    #[test]
    fn an_empty_message_list_is_rejected_before_any_send() {
        let mut operation = operation("call-empty-messages");
        operation.messages.clear();

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn a_system_message_satisfies_the_wire_message_cardinality() {
        let mut operation = operation("call-system-only");
        operation.messages.clear();
        operation.system = Some("System only.".to_string());

        let request = build_request(&operation).expect("one system message is representable");
        assert_eq!(request.input.len(), 1);
    }

    #[test]
    fn an_empty_conversation_message_is_rejected_before_any_send() {
        let mut operation = operation("call-empty-message");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::User,
            parts: Vec::new(),
        }];

        let error = build_request(&operation)
            .expect_err("silently dropping a caller-stated role boundary changes history");
        assert!(matches!(
            error,
            PreparationFailure::UnsupportedOperation { .. }
        ));
    }

    #[test]
    fn output_contract_becomes_the_forced_only_function() {
        let mut operation = operation("call-5");
        operation.output_contract = Some(StructuredOutputContract {
            name: ToolName::new("verdict"),
            description: "The verdict.".to_string(),
            schema: serde_json::value::to_raw_value(&serde_json::json!({"type": "object"}))
                .expect("fixture schema serializes"),
        });

        let request = build_request(&operation).expect("contract-bearing operation builds");
        let value = serde_json::to_value(&request).expect("wire request serializes");

        assert_eq!(
            value["tool_choice"],
            serde_json::json!({"type": "function", "name": "verdict"})
        );
        assert_eq!(value["tools"][0]["name"], serde_json::json!("verdict"));
    }

    #[test]
    fn contract_combined_with_caller_tools_declares_both_and_forces_the_contract() {
        let mut operation = operation("call-6");
        operation.output_contract = Some(StructuredOutputContract {
            name: ToolName::new("verdict"),
            description: "The verdict.".to_string(),
            schema: serde_json::value::to_raw_value(&serde_json::json!({"type": "object"}))
                .expect("fixture schema serializes"),
        });
        operation.tools = vec![ToolDefinition::with_schema(
            "lookup",
            "Looks up a city.",
            serde_json::json!({"type": "object"}),
        )];

        let request = build_request(&operation).expect("distinct names translate");
        let value = serde_json::to_value(&request).expect("wire request serializes");

        assert_eq!(value["tools"][0]["name"], serde_json::json!("lookup"));
        assert_eq!(value["tools"][1]["name"], serde_json::json!("verdict"));
        assert_eq!(
            value["tool_choice"],
            serde_json::json!({"type": "function", "name": "verdict"})
        );
        assert_eq!(value["parallel_tool_calls"], serde_json::json!(false));
    }

    #[test]
    fn contract_name_colliding_with_a_tool_is_rejected_before_any_send() {
        let mut operation = operation("call-11");
        operation.output_contract = Some(StructuredOutputContract {
            name: ToolName::new("verdict"),
            description: "The verdict.".to_string(),
            schema: serde_json::value::to_raw_value(&serde_json::json!({"type": "object"}))
                .expect("fixture schema serializes"),
        });
        operation.tools = vec![ToolDefinition::with_schema(
            "verdict",
            "An ordinary tool under the reserved name.",
            serde_json::json!({"type": "object"}),
        )];

        let failure = build_request(&operation)
            .expect_err("a proposal under a colliding name would be indistinguishable");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
    }

    #[test]
    fn an_empty_tool_name_is_rejected_before_any_send() {
        let mut operation = operation("call-empty-tool-name");
        operation.tools = vec![ToolDefinition::with_schema(
            "",
            "Invalid empty name.",
            serde_json::json!({"type": "object"}),
        )];

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn an_overlong_contract_name_is_rejected_before_any_send() {
        let mut operation = operation("call-long-contract-name");
        operation.output_contract = Some(StructuredOutputContract {
            name: ToolName::new("a".repeat(65)),
            description: "Invalid overlong name.".to_string(),
            schema: serde_json::value::to_raw_value(&serde_json::json!({"type": "object"}))
                .expect("fixture schema serializes"),
        });

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn invalid_replayed_function_name_characters_are_rejected_before_any_send() {
        let mut operation = operation("call-invalid-replayed-name");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("call_a1"),
                name: ToolName::new("not/a/function"),
                arguments_json: "{}".to_string(),
            })],
        }];

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn more_than_128_ordinary_tools_are_rejected_before_any_send() {
        let mut operation = operation("call-too-many-tools");
        operation.tools = (0..129)
            .map(|index| {
                ToolDefinition::with_schema(
                    format!("tool_{index}"),
                    "A tool.",
                    serde_json::json!({"type": "object"}),
                )
            })
            .collect();

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn a_non_object_function_schema_is_rejected_before_any_send() {
        let mut operation = operation("call-non-object-function-schema");
        operation.tools = vec![ToolDefinition::with_schema(
            "lookup",
            "Looks up a city.",
            serde_json::Value::Null,
        )];

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn a_non_object_contract_schema_is_rejected_before_any_send() {
        let mut operation = operation("call-non-object-contract-schema");
        operation.output_contract = Some(StructuredOutputContract {
            name: ToolName::new("contract"),
            description: "The contract.".to_string(),
            schema: serde_json::value::to_raw_value(&serde_json::json!([]))
                .expect("fixture schema serializes"),
        });

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn a_contract_counts_toward_the_128_function_limit() {
        let mut operation = operation("call-contract-over-limit");
        operation.tools = (0..128)
            .map(|index| {
                ToolDefinition::with_schema(
                    format!("tool_{index}"),
                    "A tool.",
                    serde_json::json!({"type": "object"}),
                )
            })
            .collect();
        operation.output_contract = Some(StructuredOutputContract {
            name: ToolName::new("contract"),
            description: "The contract.".to_string(),
            schema: serde_json::value::to_raw_value(&serde_json::json!({"type": "object"}))
                .expect("fixture schema serializes"),
        });

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn failed_tool_result_replay_uses_explicit_error_content() {
        let mut operation = operation("call-8");
        operation.messages = vec![
            ConversationMessage {
                role: ConversationRole::Assistant,
                parts: vec![MessagePart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("call_a1"),
                    name: ToolName::new("lookup"),
                    arguments_json: "{}".to_string(),
                })],
            },
            ConversationMessage {
                role: ConversationRole::User,
                parts: vec![MessagePart::ToolResult(ToolResultRecord {
                    tool_call_id: ToolCallId::new("call_a1"),
                    content: r#"{"error":{"kind":"execution_failed"}}"#.to_string(),
                    is_error: true,
                })],
            },
        ];

        let request = build_request(&operation)
            .expect("explicit error content is valid Responses tool history");
        assert_eq!(
            serde_json::to_value(&request).unwrap()["input"][1]["output"],
            serde_json::json!(r#"{"error":{"kind":"execution_failed"}}"#)
        );
    }

    #[test]
    fn an_assistant_role_tool_result_is_rejected_before_any_send() {
        let mut operation = operation("call-assistant-result");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::ToolResult(ToolResultRecord {
                tool_call_id: ToolCallId::new("call_a1"),
                content: "result".to_string(),
                is_error: false,
            })],
        }];

        let failure = build_request(&operation)
            .expect_err("rewriting assistant-authored material as a tool role changes history");
        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
    }

    #[test]
    fn text_after_a_tool_call_keeps_its_position() {
        let mut operation = operation("call-9");
        operation.messages = vec![
            ConversationMessage {
                role: ConversationRole::Assistant,
                parts: vec![
                    MessagePart::ToolCall(ToolCallProposal {
                        id: ToolCallId::new("call_a1"),
                        name: ToolName::new("lookup"),
                        arguments_json: "{}".to_string(),
                    }),
                    MessagePart::Text("after the call".to_string()),
                ],
            },
            ConversationMessage {
                role: ConversationRole::User,
                parts: vec![MessagePart::ToolResult(ToolResultRecord {
                    tool_call_id: ToolCallId::new("call_a1"),
                    content: "done".to_string(),
                    is_error: false,
                })],
            },
        ];

        let value = serde_json::to_value(build_request(&operation).unwrap()).unwrap();
        assert_eq!(
            value["input"][operation.messages[0].parts.len() - 1]["content"],
            "after the call"
        );
    }

    #[test]
    fn text_segments_separated_by_a_tool_call_keep_their_positions() {
        let mut operation = operation("call-separated-text");
        operation.messages = vec![
            ConversationMessage {
                role: ConversationRole::Assistant,
                parts: vec![
                    MessagePart::Text("before the call".to_string()),
                    MessagePart::ToolCall(ToolCallProposal {
                        id: ToolCallId::new("call_a1"),
                        name: ToolName::new("lookup"),
                        arguments_json: "{}".to_string(),
                    }),
                    MessagePart::Text("after the call".to_string()),
                ],
            },
            ConversationMessage {
                role: ConversationRole::User,
                parts: vec![MessagePart::ToolResult(ToolResultRecord {
                    tool_call_id: ToolCallId::new("call_a1"),
                    content: "done".to_string(),
                    is_error: false,
                })],
            },
        ];

        let value = serde_json::to_value(build_request(&operation).unwrap()).unwrap();
        assert_eq!(
            value["input"][operation.messages[0].parts.len() - 1]["content"],
            "after the call"
        );
    }

    #[test]
    fn non_finite_temperature_is_rejected_not_silently_nulled() {
        let mut operation = operation("call-10");
        operation.settings.temperature = Some(f64::INFINITY);

        let failure = build_request(&operation)
            .expect_err("serde_json would serialize a non-finite setting as null");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
    }

    #[test]
    fn temperature_outside_the_provider_domain_is_rejected_before_send() {
        assert_temperature_is_rejected(-0.1);
        assert_temperature_is_rejected(2.1);
        assert_temperature_is_rejected(f64::INFINITY);
    }

    #[test]
    fn top_p_outside_the_provider_domain_is_rejected_before_send() {
        assert_top_p_is_rejected(-0.1);
        assert_top_p_is_rejected(1.1);
        assert_top_p_is_rejected(f64::INFINITY);
    }

    #[test]
    fn output_token_limits_below_sixteen_fail_configuration_and_preparation() {
        let mut candidate = operation("call-output-ceiling");
        for limit in 1..16 {
            candidate.settings.max_output_tokens = limit;
            assert!(matches!(
                validate_model_settings(&candidate.settings),
                Err(PreparationFailure::UnsupportedOperation { .. })
            ));
            assert!(matches!(
                build_request(&candidate),
                Err(PreparationFailure::UnsupportedOperation { .. })
            ));
        }
        candidate.settings.max_output_tokens = 16;
        assert!(validate_model_settings(&candidate.settings).is_ok());
        assert_eq!(build_request(&candidate).unwrap().max_output_tokens, 16);
    }

    #[test]
    fn tool_results_must_match_the_immediately_preceding_tool_calls() {
        let result = |id: &str| {
            MessagePart::ToolResult(ToolResultRecord {
                tool_call_id: ToolCallId::new(id),
                content: "done".to_string(),
                is_error: false,
            })
        };
        let call = MessagePart::ToolCall(ToolCallProposal {
            id: ToolCallId::new("call_a1"),
            name: ToolName::new("lookup"),
            arguments_json: "{}".to_string(),
        });

        let mut orphan = operation("call-orphan");
        orphan.messages = vec![ConversationMessage {
            role: ConversationRole::User,
            parts: vec![result("call_a1")],
        }];
        assert!(matches!(
            build_request(&orphan),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));

        let mut missing = operation("call-missing");
        missing.messages = vec![ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![call.clone()],
        }];
        assert!(matches!(
            build_request(&missing),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));

        let mut mismatched = operation("call-mismatch");
        mismatched.messages = vec![
            ConversationMessage {
                role: ConversationRole::Assistant,
                parts: vec![call],
            },
            ConversationMessage {
                role: ConversationRole::User,
                parts: vec![result("call_other")],
            },
        ];
        assert!(matches!(
            build_request(&mismatched),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn parallel_tool_results_can_span_consecutive_user_messages() {
        let call = |id: &str, name: &str| {
            MessagePart::ToolCall(ToolCallProposal {
                id: ToolCallId::new(id),
                name: ToolName::new(name),
                arguments_json: "{}".to_string(),
            })
        };
        let result = |id: &str, content: &str| {
            MessagePart::ToolResult(ToolResultRecord {
                tool_call_id: ToolCallId::new(id),
                content: content.to_string(),
                is_error: false,
            })
        };
        let mut operation = operation("call-parallel-results");
        operation.messages = vec![
            ConversationMessage {
                role: ConversationRole::Assistant,
                parts: vec![call("call_a", "lookup_a"), call("call_b", "lookup_b")],
            },
            ConversationMessage {
                role: ConversationRole::User,
                parts: vec![result("call_a", "first")],
            },
            ConversationMessage {
                role: ConversationRole::User,
                parts: vec![result("call_b", "second")],
            },
        ];

        let request = build_request(&operation)
            .expect("consecutive tool messages preserve representable parallel results");
        let value = serde_json::to_value(request).expect("wire request serializes");

        assert_eq!(value["input"][2]["type"], "function_call_output");
        assert_eq!(value["input"][2]["call_id"], "call_a");
        assert_eq!(value["input"][3]["type"], "function_call_output");
        assert_eq!(value["input"][3]["call_id"], "call_b");
    }

    #[test]
    fn user_content_cannot_intervene_between_parallel_tool_results() {
        let call = |id: &str| {
            MessagePart::ToolCall(ToolCallProposal {
                id: ToolCallId::new(id),
                name: ToolName::new("lookup"),
                arguments_json: "{}".to_string(),
            })
        };
        let result = MessagePart::ToolResult(ToolResultRecord {
            tool_call_id: ToolCallId::new("call_a"),
            content: "first".to_string(),
            is_error: false,
        });
        let mut operation = operation("call-intervening-content");
        operation.messages = vec![
            ConversationMessage {
                role: ConversationRole::Assistant,
                parts: vec![call("call_a"), call("call_b")],
            },
            ConversationMessage {
                role: ConversationRole::User,
                parts: vec![result, MessagePart::Text("continue".to_string())],
            },
        ];

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn the_final_tool_result_must_precede_user_text() {
        for text_first in [true, false] {
            let mut candidate = operation("call-final-result-order");
            let result = MessagePart::ToolResult(ToolResultRecord {
                tool_call_id: ToolCallId::new("call_pending"),
                content: "done".to_string(),
                is_error: false,
            });
            let mut parts = vec![result, MessagePart::Text("continue".to_string())];
            if text_first {
                parts.swap(0, 1);
            }
            candidate.messages = vec![
                ConversationMessage {
                    role: ConversationRole::Assistant,
                    parts: vec![MessagePart::ToolCall(ToolCallProposal {
                        id: ToolCallId::new("call_pending"),
                        name: ToolName::new("lookup"),
                        arguments_json: "{}".to_string(),
                    })],
                },
                ConversationMessage {
                    role: ConversationRole::User,
                    parts,
                },
            ];
            let request = build_request(&candidate);
            if text_first {
                assert!(matches!(
                    request,
                    Err(PreparationFailure::UnsupportedOperation { .. })
                ));
            } else {
                let wire = serde_json::to_value(request.unwrap()).unwrap();
                assert_eq!(wire["input"][1]["type"], "function_call_output");
                assert_eq!(wire["input"][2]["type"], "message");
            }
        }
    }

    #[test]
    fn replayed_tool_arguments_must_be_valid_json() {
        let mut operation = operation("call-invalid-json");
        operation.messages = vec![
            ConversationMessage {
                role: ConversationRole::Assistant,
                parts: vec![MessagePart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("call_a1"),
                    name: ToolName::new("lookup"),
                    arguments_json: "{not json".to_string(),
                })],
            },
            ConversationMessage {
                role: ConversationRole::User,
                parts: vec![MessagePart::ToolResult(ToolResultRecord {
                    tool_call_id: ToolCallId::new("call_a1"),
                    content: "done".to_string(),
                    is_error: false,
                })],
            },
        ];

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn nonempty_stop_sequences_fail_configuration_and_preparation() {
        let mut candidate = operation("call-stop-sequences");
        candidate.settings.stop_sequences = vec!["END".to_string()];

        let validation = validate_model_settings(&candidate.settings)
            .expect_err("Responses cannot enforce a stop sequence");
        let preparation =
            build_request(&candidate).expect_err("unsupported stops must fail before send");

        assert_eq!(
            validation,
            PreparationFailure::UnsupportedOperation {
                detail: "Responses does not support stop sequences".to_string(),
            }
        );
        assert_eq!(preparation, validation);
    }

    #[test]
    fn replayed_tool_result_after_user_text_is_rejected_before_any_send() {
        let mut operation = operation("call-text-before-result");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::User,
            parts: vec![
                MessagePart::Text("first".to_string()),
                MessagePart::ToolResult(ToolResultRecord {
                    tool_call_id: ToolCallId::new("call_a1"),
                    content: "result".to_string(),
                    is_error: false,
                }),
            ],
        }];

        let failure = build_request(&operation)
            .expect_err("splitting the user turn would reorder its stated parts");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
    }

    #[test]
    fn a_user_role_tool_call_is_rejected_before_any_send() {
        let mut operation = operation("call-12");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::User,
            parts: vec![MessagePart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("call_a1"),
                name: ToolName::new("lookup"),
                arguments_json: "{}".to_string(),
            })],
        }];

        let failure =
            build_request(&operation).expect_err("tool calls are assistant material on this wire");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
    }

    #[test]
    fn replayed_reasoning_history_is_rejected_not_silently_dropped() {
        let mut operation = operation("call-7");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::Thinking {
                text: "step one".to_string(),
                signature: Some("sig_1".to_string()),
            }],
        }];

        let failure = build_request(&operation)
            .expect_err("reasoning history this wire contract cannot represent must not vanish");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
    }
}
