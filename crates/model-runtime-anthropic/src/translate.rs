//! Operation-to-wire translation.

use std::collections::BTreeSet;

use signalbox_model_runtime::{
    AnthropicServiceTier, CodexCliServiceTier, ConversationMessage, ConversationRole, DeliveryMode,
    FastMode, MessagePart, ModelOperation, ModelSettings, OpenAiServiceTier, PreparationFailure,
    ReasoningLevel, ServiceTier, ToolChoice,
};

use crate::wire::{
    MessagesRequest, OutputConfig, WireMessage, WireRequestBlock, WireTool, WireToolChoice,
};

/// Builds the wire request for one operation.
///
/// Pure translation: any failure is a trustworthy [`PreparationFailure`]
/// returned before a one-shot capability exists. Nothing has touched the
/// network.
///
/// A structured-output contract joins the declared tools, `tool_choice` is
/// `auto` with parallel tool use disabled, and the exactly-one demand travels
/// as a model-visible instruction. [`ModelOperation::validate`] reserves the
/// contract name from ordinary tools before anything is sent.
#[cfg(test)]
pub(crate) fn build_request<C>(
    operation: &ModelOperation<C>,
) -> Result<MessagesRequest, PreparationFailure> {
    build_request_with_fast_mode(operation, operation.settings.fast_mode)
}

pub(crate) fn build_request_with_fast_mode<C>(
    operation: &ModelOperation<C>,
    request_fast_mode: FastMode,
) -> Result<MessagesRequest, PreparationFailure> {
    if let Err(error) = operation.validate() {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: error.to_string(),
        });
    }
    if operation.messages.is_empty() {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: "Anthropic requires at least one conversation message".to_string(),
        });
    }
    if operation.settings.max_output_tokens == 0 {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: "max_output_tokens must be at least 1".to_string(),
        });
    }
    if operation.settings.stop_sequences.len() > 4 {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: "Anthropic accepts at most four stop sequences".to_string(),
        });
    }
    validate_sampling_controls(&operation.settings)?;
    validate_tool_names(operation)?;
    validate_tool_history(&operation.messages)?;
    let plan = tool_plan(operation)?;
    let effort = operation
        .settings
        .reasoning_level
        .map(anthropic_effort)
        .transpose()?;
    let service_tier = anthropic_service_tier(&operation.settings, request_fast_mode)?;
    Ok(MessagesRequest {
        model: operation.resolved_target.as_str().to_string(),
        max_tokens: operation.settings.max_output_tokens,
        messages: operation
            .messages
            .iter()
            .map(wire_message)
            .collect::<Result<Vec<_>, _>>()?,
        system: plan.system_text(operation.system.as_deref()),
        stop_sequences: operation.settings.stop_sequences.clone(),
        output_config: effort.map(|effort| OutputConfig { effort }),
        service_tier,
        speed: (request_fast_mode == FastMode::Enabled).then_some("fast"),
        tools: plan.tools,
        tool_choice: plan.tool_choice,
        stream: operation.delivery == DeliveryMode::Streamed,
    })
}

/// Refuses a caller-set sampling control before any request is built.
///
/// The Messages API answers `temperature`, `top_p`, or `top_k` with a 400.
/// Refusing keeps a dropped demand from reading as an honored one.
fn validate_sampling_controls(settings: &ModelSettings) -> Result<(), PreparationFailure> {
    for (name, value) in [
        ("temperature", settings.temperature),
        ("top_p", settings.top_p),
    ] {
        if value.is_some() {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: format!("Anthropic accepts no {name} sampling control"),
            });
        }
    }
    Ok(())
}

fn anthropic_effort(level: ReasoningLevel) -> Result<&'static str, PreparationFailure> {
    match level {
        ReasoningLevel::Low => Ok("low"),
        ReasoningLevel::Medium => Ok("medium"),
        ReasoningLevel::High => Ok("high"),
        ReasoningLevel::XHigh => Ok("xhigh"),
        ReasoningLevel::Max => Ok("max"),
        ReasoningLevel::None | ReasoningLevel::Minimal | ReasoningLevel::Ultra => {
            Err(PreparationFailure::UnsupportedOperation {
                detail: "Anthropic cannot enforce the requested reasoning level".to_string(),
            })
        }
    }
}

/// Validates the complete settings combination enforced by this adapter.
///
/// Capability-set validation remains the caller's responsibility. This check
/// owns the cross-knob constraints that independent capability sets cannot
/// state, and the sampling controls this adapter enforces for no model.
pub fn validate_model_settings(settings: &ModelSettings) -> Result<(), PreparationFailure> {
    settings.reasoning_level.map(anthropic_effort).transpose()?;
    anthropic_service_tier(settings, settings.fast_mode)?;
    validate_sampling_controls(settings)?;
    Ok(())
}

fn anthropic_service_tier(
    settings: &ModelSettings,
    request_fast_mode: FastMode,
) -> Result<Option<&'static str>, PreparationFailure> {
    match (settings.fast_mode, settings.service_tier) {
        (FastMode::Enabled, Some(ServiceTier::Anthropic(AnthropicServiceTier::Auto))) => {
            Err(PreparationFailure::UnsupportedOperation {
                detail: "Anthropic fast mode is incompatible with the auto service tier"
                    .to_string(),
            })
        }
        (FastMode::Enabled, None) => {
            Ok((request_fast_mode == FastMode::Enabled).then_some("standard_only"))
        }
        (FastMode::Enabled, Some(ServiceTier::Anthropic(AnthropicServiceTier::StandardOnly))) => {
            Ok(Some("standard_only"))
        }
        (FastMode::Disabled, Some(ServiceTier::Anthropic(AnthropicServiceTier::Auto))) => {
            Ok(Some("auto"))
        }
        (FastMode::Disabled, Some(ServiceTier::Anthropic(AnthropicServiceTier::StandardOnly))) => {
            Ok(Some("standard_only"))
        }
        (FastMode::Disabled, None) => Ok(None),
        (
            FastMode::Disabled | FastMode::Enabled,
            Some(
                ServiceTier::OpenAi(
                    OpenAiServiceTier::Auto
                    | OpenAiServiceTier::Default
                    | OpenAiServiceTier::Flex
                    | OpenAiServiceTier::Scale
                    | OpenAiServiceTier::Priority
                    | OpenAiServiceTier::Fast,
                )
                | ServiceTier::CodexCli(
                    CodexCliServiceTier::Default
                    | CodexCliServiceTier::Priority
                    | CodexCliServiceTier::Flex,
                ),
            ),
        ) => Err(PreparationFailure::UnsupportedOperation {
            detail: "Anthropic cannot enforce another provider's service tier".to_string(),
        }),
    }
}

fn validate_tool_names<C>(operation: &ModelOperation<C>) -> Result<(), PreparationFailure> {
    for tool in &operation.tools {
        validate_tool_name(tool.name.as_str(), "tool")?;
    }
    if let Some(contract) = &operation.output_contract {
        validate_tool_name(contract.name.as_str(), "structured-output contract")?;
    }
    for message in &operation.messages {
        for part in &message.parts {
            if let MessagePart::ToolCall(call) = part {
                validate_tool_name(call.name.as_str(), "replayed tool call")?;
            }
        }
    }
    Ok(())
}

fn validate_tool_name(name: &str, subject: &str) -> Result<(), PreparationFailure> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(PreparationFailure::UnsupportedOperation {
            detail: format!(
                "Anthropic {subject} name must contain 1 through 64 ASCII letters, digits, underscores, or hyphens"
            ),
        })
    }
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

        if let Some(expected) = pending_calls.take() {
            if message.role != ConversationRole::User || results != expected {
                return Err(PreparationFailure::UnsupportedOperation {
                    detail: "Anthropic requires one matching tool result for every tool use in \
                             the immediately following user-role message"
                        .to_string(),
                });
            }
        } else if !results.is_empty() {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: "Anthropic tool results must answer tool uses from the immediately \
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
            detail: "Anthropic requires tool uses to be followed by matching tool results"
                .to_string(),
        });
    }
    Ok(())
}

/// What one operation's tools, tool choice, and contract translate into.
struct ToolPlan {
    /// The declared tools, including the contract tool when one exists.
    tools: Option<Vec<WireTool>>,
    /// The emitted `tool_choice`, always the `auto` shape when present.
    tool_choice: Option<WireToolChoice>,
    /// The adapter-authored instruction carrying a tool demand the provider
    /// no longer accepts as a request control, when the operation makes one.
    instruction: Option<String>,
}

impl ToolPlan {
    /// Joins the caller's system text with this plan's instruction.
    ///
    /// The caller's text stays first and unmodified, the instruction follows
    /// after a blank line.
    fn system_text(&self, caller_system: Option<&str>) -> Option<String> {
        match (caller_system, self.instruction.as_deref()) {
            (Some(system), Some(instruction)) => Some(format!("{system}\n\n{instruction}")),
            (Some(system), None) => Some(system.to_string()),
            (None, Some(instruction)) => Some(instruction.to_string()),
            (None, None) => None,
        }
    }
}

/// The instruction carrying a tool demand the provider dropped as a control.
///
/// `tool_choice` `any` and `tool` answer with a 400 on `/v1/messages` and on
/// `/v1/messages/count_tokens`. The documented replacement is `auto` plus an
/// instruction naming the expected tool, which is this text: model-visible
/// prompt text, not a transport control.
fn tool_instruction(demand: &ToolDemand) -> String {
    match demand {
        ToolDemand::Contract { name } => format!(
            "Answer by calling the {name} tool exactly once, passing the answer as its \
             arguments. Call no other tool and add no other reply."
        ),
        ToolDemand::AnyTool => {
            "Answer by calling at least one of the declared tools rather than replying \
             with text."
                .to_string()
        }
        ToolDemand::Named { name } => format!(
            "Answer by calling the {name} tool rather than replying with text. Call no \
             other tool."
        ),
    }
}

/// The tool demand an operation makes that `tool_choice` can no longer carry.
enum ToolDemand {
    /// Exactly one proposal under the structured-output contract's name.
    Contract {
        /// The contract's reserved tool name.
        name: String,
    },
    /// At least one proposal under any declared tool.
    AnyTool,
    /// A proposal under one caller-selected tool.
    Named {
        /// The selected tool's name.
        name: String,
    },
}

fn tool_plan<C>(operation: &ModelOperation<C>) -> Result<ToolPlan, PreparationFailure> {
    for tool in &operation.tools {
        if !crate::wire::raw_json_is_object(&tool.input_schema) {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: format!(
                    "Anthropic requires tool {} to carry a JSON Schema object",
                    tool.name.as_str()
                ),
            });
        }
    }
    if let Some(contract) = &operation.output_contract
        && !crate::wire::raw_json_is_object(&contract.schema)
    {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: format!(
                "Anthropic requires output contract {} to carry a JSON Schema object",
                contract.name.as_str()
            ),
        });
    }
    let mut tools: Vec<WireTool> = operation
        .tools
        .iter()
        .map(|tool| WireTool {
            name: tool.name.as_str().to_string(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
        })
        .collect();
    if let Some(contract) = &operation.output_contract {
        let name = contract.name.as_str().to_string();
        tools.push(WireTool {
            name: name.clone(),
            description: contract.description.clone(),
            input_schema: contract.schema.clone(),
        });
        return Ok(ToolPlan {
            tools: Some(tools),
            // The contract promises exactly one value; parallel tool use
            // could return several proposals for the contract tool.
            tool_choice: Some(WireToolChoice::Auto {
                disable_parallel_tool_use: Some(true),
            }),
            instruction: Some(tool_instruction(&ToolDemand::Contract { name })),
        });
    }
    if tools.is_empty() {
        return Ok(ToolPlan {
            tools: None,
            tool_choice: None,
            instruction: None,
        });
    }
    // An ordinary tool choice admits several proposals — a named choice
    // requires every proposal to carry the selected name, not that there be
    // only one — so parallel tool use stays enabled for both. Only the
    // contract above promises exactly one value.
    let demand = match &operation.tool_choice {
        ToolChoice::Automatic => None,
        ToolChoice::AnyTool => Some(ToolDemand::AnyTool),
        ToolChoice::Named(name) => Some(ToolDemand::Named {
            name: name.as_str().to_string(),
        }),
    };
    Ok(ToolPlan {
        tools: Some(tools),
        tool_choice: Some(WireToolChoice::Auto {
            disable_parallel_tool_use: None,
        }),
        instruction: demand.as_ref().map(tool_instruction),
    })
}

fn wire_message(message: &ConversationMessage) -> Result<WireMessage, PreparationFailure> {
    let mut user_text_seen = false;
    for part in &message.parts {
        let valid_role = matches!(part, MessagePart::Text(_))
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
        if !valid_role {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: "Anthropic requires tool results in user-role messages and tool calls or \
                         thinking blocks in assistant messages"
                    .to_string(),
            });
        }
        if message.role == ConversationRole::User {
            match part {
                MessagePart::Text(_) => user_text_seen = true,
                MessagePart::ToolResult(_) if user_text_seen => {
                    return Err(PreparationFailure::UnsupportedOperation {
                        detail: "Anthropic requires every tool result in a user-role message to \
                                 precede text content"
                            .to_string(),
                    });
                }
                _ => {}
            }
        }
    }
    let role = match message.role {
        ConversationRole::User => "user",
        ConversationRole::Assistant => "assistant",
    };
    let content = message
        .parts
        .iter()
        .map(|part| match part {
            MessagePart::Text(text) => Ok(WireRequestBlock::Text { text: text.clone() }),
            MessagePart::ToolCall(proposal) => {
                let input =
                    serde_json::value::RawValue::from_string(proposal.arguments_json.clone())
                        .map_err(|error| PreparationFailure::UnsupportedOperation {
                            detail: format!(
                                "replayed tool call {} carries arguments that are not valid JSON: \
                             {error}",
                                proposal.id.as_str()
                            ),
                        })?;
                if !crate::wire::raw_json_is_object(&input) {
                    return Err(PreparationFailure::UnsupportedOperation {
                        detail: format!(
                            "replayed tool call {} carries arguments that are not a JSON object",
                            proposal.id.as_str()
                        ),
                    });
                }
                Ok(WireRequestBlock::ToolUse {
                    id: proposal.id.as_str().to_string(),
                    name: proposal.name.as_str().to_string(),
                    input,
                })
            }
            MessagePart::ToolResult(result) => Ok(WireRequestBlock::ToolResult {
                tool_use_id: result.tool_call_id.as_str().to_string(),
                content: result.content.clone(),
                is_error: result.is_error,
            }),
            MessagePart::Thinking { text, signature } => match signature {
                // The provider requires replayed thinking blocks to carry
                // their integrity signature; sending one without it would
                // only be rejected after the acceptance boundary.
                None => Err(PreparationFailure::UnsupportedOperation {
                    detail: "a replayed thinking block without its integrity signature \
                             cannot be sent"
                        .to_string(),
                }),
                Some(signature) if signature.is_empty() => {
                    Err(PreparationFailure::UnsupportedOperation {
                        detail: "a replayed thinking block with an empty integrity signature \
                                 cannot be sent"
                            .to_string(),
                    })
                }
                Some(signature) => Ok(WireRequestBlock::Thinking {
                    thinking: text.clone(),
                    signature: signature.clone(),
                }),
            },
            MessagePart::RedactedThinking { data } => {
                Ok(WireRequestBlock::RedactedThinking { data: data.clone() })
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(WireMessage { role, content })
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use signalbox_model_runtime::CredentialReference;
    use signalbox_model_runtime::{
        AnthropicServiceTier, ConversationMessage, ConversationRole, DeliveryMode, FastMode,
        MessagePart, ModelOperation, ModelSettings, PreparationFailure, ReasoningLevel,
        RequestedTarget, ResolvedTarget, ServiceTier, StructuredOutputContract, ToolCallId,
        ToolCallProposal, ToolChoice, ToolDefinition, ToolName, ToolResultRecord,
    };

    use super::{build_request, build_request_with_fast_mode, validate_model_settings};

    /// An operation whose correlation seed is the one knob; targets, one
    /// user-role message, and a 64-token ceiling are canonical.
    fn operation(correlation: &str) -> ModelOperation<String> {
        ModelOperation::new(
            correlation.to_string(),
            CredentialReference::new("anthropic-primary"),
            RequestedTarget::new("fast-alias"),
            ResolvedTarget::new("model-exact-1"),
            vec![ConversationMessage::user_text("hello")],
            ModelSettings::new(64),
        )
    }

    fn sort_json_object_keys(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => {
                object.sort_keys();
                for nested in object.values_mut() {
                    sort_json_object_keys(nested);
                }
            }
            serde_json::Value::Array(values) => {
                for nested in values {
                    sort_json_object_keys(nested);
                }
            }
            _ => {}
        }
    }

    fn request_json(operation: &ModelOperation<String>) -> String {
        let request = build_request(operation).expect("translatable operation builds");
        let mut value = serde_json::to_value(&request).expect("wire request serializes");
        sort_json_object_keys(&mut value);
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
        operation.settings.stop_sequences = vec!["END".to_string()];
        operation.messages = vec![
            ConversationMessage::user_text("look up Oslo"),
            ConversationMessage {
                role: ConversationRole::Assistant,
                parts: vec![
                    MessagePart::Text("Looking it up.".to_string()),
                    MessagePart::ToolCall(ToolCallProposal {
                        id: ToolCallId::new("toolu_1"),
                        name: ToolName::new("lookup"),
                        arguments_json: r#"{"city":"Oslo"}"#.to_string(),
                    }),
                ],
            },
            ConversationMessage {
                role: ConversationRole::User,
                parts: vec![MessagePart::ToolResult(ToolResultRecord {
                    tool_call_id: ToolCallId::new("toolu_1"),
                    content: "population 700000".to_string(),
                    is_error: false,
                })],
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
              "max_tokens": 64,
              "messages": [
                {
                  "content": [
                    {
                      "text": "look up Oslo",
                      "type": "text"
                    }
                  ],
                  "role": "user"
                },
                {
                  "content": [
                    {
                      "text": "Looking it up.",
                      "type": "text"
                    },
                    {
                      "id": "toolu_1",
                      "input": {
                        "city": "Oslo"
                      },
                      "name": "lookup",
                      "type": "tool_use"
                    }
                  ],
                  "role": "assistant"
                },
                {
                  "content": [
                    {
                      "content": "population 700000",
                      "is_error": false,
                      "tool_use_id": "toolu_1",
                      "type": "tool_result"
                    }
                  ],
                  "role": "user"
                }
              ],
              "model": "model-exact-1",
              "stop_sequences": [
                "END"
              ],
              "stream": false,
              "system": "Answer briefly.\n\nAnswer by calling the lookup tool rather than replying with text. Call no other tool.",
              "tool_choice": {
                "type": "auto"
              },
              "tools": [
                {
                  "description": "Looks up a city.",
                  "input_schema": {
                    "type": "object"
                  },
                  "name": "lookup"
                }
              ]
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
    fn streamed_delivery_sets_the_stream_flag() {
        let mut operation = operation("call-3");
        operation.delivery = DeliveryMode::Streamed;

        let request = build_request(&operation).expect("translatable operation builds");

        assert!(request.stream);
    }

    #[test]
    fn minimal_operation_omits_every_unset_optional_field() {
        expect![[r#"
            {
              "max_tokens": 64,
              "messages": [
                {
                  "content": [
                    {
                      "text": "hello",
                      "type": "text"
                    }
                  ],
                  "role": "user"
                }
              ],
              "model": "model-exact-1",
              "stream": false
            }"#]]
        .assert_eq(&request_json(&operation("call-4")));
    }

    #[test]
    fn reasoning_uses_the_anthropic_effort_control() {
        let mut operation = operation("call-settings");
        operation.settings.reasoning_level = Some(ReasoningLevel::XHigh);

        let request = build_request(&operation).expect("supported reasoning translates");
        let value = serde_json::to_value(request).expect("wire request serializes");

        assert_eq!(value["output_config"]["effort"], "xhigh");
    }

    #[test]
    fn fast_mode_and_tier_use_the_anthropic_wire_controls() {
        let mut operation = operation("call-settings");
        operation.settings.fast_mode = FastMode::Enabled;
        operation.settings.service_tier =
            Some(ServiceTier::Anthropic(AnthropicServiceTier::StandardOnly));

        let request = build_request(&operation).expect("supported controls translate");
        let value = serde_json::to_value(request).expect("wire request serializes");

        assert_eq!(value["speed"], "fast");
        assert_eq!(value["service_tier"], "standard_only");
    }

    #[test]
    fn fast_mode_rejects_anthropic_auto_tier_before_send() {
        let mut operation = operation("call-incompatible-settings");
        operation.settings.fast_mode = FastMode::Enabled;
        operation.settings.service_tier = Some(ServiceTier::Anthropic(AnthropicServiceTier::Auto));

        assert!(matches!(
            validate_model_settings(&operation.settings),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn mapped_fast_mode_still_rejects_anthropic_auto_tier() {
        let mut operation = operation("call-mapped-incompatible-settings");
        operation.settings.fast_mode = FastMode::Enabled;
        operation.settings.service_tier = Some(ServiceTier::Anthropic(AnthropicServiceTier::Auto));

        assert!(matches!(
            build_request_with_fast_mode(&operation, FastMode::Disabled),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn an_empty_conversation_is_rejected_before_any_send() {
        let mut operation = operation("call-empty-conversation");
        operation.messages.clear();

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn more_than_four_stop_sequences_are_rejected_before_any_send() {
        let mut operation = operation("call-too-many-stops");
        operation.settings.stop_sequences = vec![
            "one".to_string(),
            "two".to_string(),
            "three".to_string(),
            "four".to_string(),
            "five".to_string(),
        ];

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn an_empty_tool_name_is_rejected_before_any_send() {
        let mut operation = operation("call-empty-tool-name");
        operation.tools = vec![ToolDefinition::with_schema(
            "",
            "An invalid tool.",
            serde_json::json!({"type": "object"}),
        )];

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn an_overlong_contract_name_is_rejected_before_any_send() {
        let mut operation = operation("call-overlong-contract-name");
        operation.output_contract = Some(StructuredOutputContract {
            name: ToolName::new("a".repeat(65)),
            description: "An invalid contract.".to_string(),
            schema: serde_json::value::to_raw_value(&serde_json::json!({"type": "object"}))
                .expect("fixture schema serializes"),
        });

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn invalid_replayed_tool_name_characters_are_rejected_before_any_send() {
        let mut operation = operation("call-invalid-replayed-tool-name");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("toolu_1"),
                name: ToolName::new("not/a/tool"),
                arguments_json: "{}".to_string(),
            })],
        }];

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn output_contract_declares_its_tool_and_never_forces_the_choice() {
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
            serde_json::json!({
                "type": "auto",
                "disable_parallel_tool_use": true
            }),
            "a forced tool choice is answered with a 400 by the current Claude generation"
        );
        assert_eq!(value["tools"][0]["name"], serde_json::json!("verdict"));
    }

    #[test]
    fn output_contract_asks_for_its_one_value_by_instruction() {
        let mut operation = operation("call-contract-instruction");
        operation.system = Some("Answer briefly.".to_string());
        operation.output_contract = Some(StructuredOutputContract {
            name: ToolName::new("verdict"),
            description: "The verdict.".to_string(),
            schema: serde_json::value::to_raw_value(&serde_json::json!({"type": "object"}))
                .expect("fixture schema serializes"),
        });

        let request = build_request(&operation).expect("contract-bearing operation builds");

        expect![[r#"
            Answer briefly.

            Answer by calling the verdict tool exactly once, passing the answer as its arguments. Call no other tool and add no other reply."#]]
        .assert_eq(request.system.as_deref().expect("an instruction is stated"));
    }

    #[test]
    fn a_non_object_tool_schema_is_rejected_before_any_send() {
        let mut operation = operation("call-non-object-tool-schema");
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
            name: ToolName::new("verdict"),
            description: "The verdict.".to_string(),
            schema: serde_json::value::to_raw_value(&serde_json::json!([]))
                .expect("fixture schema serializes"),
        });

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn contract_combined_with_caller_tools_declares_both_and_asks_for_the_contract() {
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
            serde_json::json!({
                "type": "auto",
                "disable_parallel_tool_use": true
            })
        );
    }

    #[test]
    fn an_any_tool_choice_becomes_auto_and_states_the_demand_as_an_instruction() {
        let mut operation = operation("call-any-tool");
        operation.tools = vec![ToolDefinition::with_schema(
            "lookup",
            "Looks up a city.",
            serde_json::json!({"type": "object"}),
        )];
        operation.tool_choice = ToolChoice::AnyTool;

        let request = build_request(&operation).expect("an any-tool choice translates");
        let value = serde_json::to_value(&request).expect("wire request serializes");

        assert_eq!(value["tool_choice"], serde_json::json!({"type": "auto"}));
        expect![[
            "Answer by calling at least one of the declared tools rather than replying with text."
        ]]
        .assert_eq(request.system.as_deref().expect("an instruction is stated"));
    }

    #[test]
    fn a_named_tool_choice_becomes_auto_and_names_the_tool_in_the_instruction() {
        let mut operation = operation("call-named-tool");
        operation.tools = vec![ToolDefinition::with_schema(
            "lookup",
            "Looks up a city.",
            serde_json::json!({"type": "object"}),
        )];
        operation.tool_choice = ToolChoice::Named(ToolName::new("lookup"));

        let request = build_request(&operation).expect("a named choice translates");
        let value = serde_json::to_value(&request).expect("wire request serializes");

        assert_eq!(
            value["tool_choice"],
            serde_json::json!({"type": "auto"}),
            "a named choice admits several proposals, so parallel tool use stays enabled"
        );
        expect![[
            "Answer by calling the lookup tool rather than replying with text. Call no other tool."
        ]]
        .assert_eq(request.system.as_deref().expect("an instruction is stated"));
    }

    #[test]
    fn an_automatic_tool_choice_adds_no_instruction_to_the_caller_system_text() {
        let mut operation = operation("call-automatic-tool");
        operation.system = Some("Answer briefly.".to_string());
        operation.tools = vec![ToolDefinition::with_schema(
            "lookup",
            "Looks up a city.",
            serde_json::json!({"type": "object"}),
        )];
        operation.tool_choice = ToolChoice::Automatic;

        let request = build_request(&operation).expect("an automatic choice translates");
        let value = serde_json::to_value(&request).expect("wire request serializes");

        assert_eq!(value["tool_choice"], serde_json::json!({"type": "auto"}));
        assert_eq!(request.system.as_deref(), Some("Answer briefly."));
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
    fn replayed_tool_call_with_invalid_argument_json_fails_preparation() {
        let mut operation = operation("call-7");
        operation.messages = vec![
            ConversationMessage {
                role: ConversationRole::Assistant,
                parts: vec![MessagePart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("toolu_9"),
                    name: ToolName::new("lookup"),
                    arguments_json: "{not json".to_string(),
                })],
            },
            ConversationMessage {
                role: ConversationRole::User,
                parts: vec![MessagePart::ToolResult(ToolResultRecord {
                    tool_call_id: ToolCallId::new("toolu_9"),
                    content: "done".to_string(),
                    is_error: false,
                })],
            },
        ];

        let failure = build_request(&operation)
            .expect_err("invalid replayed tool arguments must fail before any send");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
    }

    #[test]
    fn replayed_tool_call_with_non_object_arguments_fails_preparation() {
        let mut operation = operation("call-non-object");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("toolu_1"),
                name: ToolName::new("lookup"),
                arguments_json: "[]".to_string(),
            })],
        }];

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn replayed_tool_arguments_preserve_raw_json_verbatim() {
        let mut operation = operation("call-raw");
        let raw = r#"{"identifier":184467440737095516160,"duplicate":1,"duplicate":2}"#;
        operation.messages = vec![
            ConversationMessage {
                role: ConversationRole::Assistant,
                parts: vec![MessagePart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("toolu_raw"),
                    name: ToolName::new("lookup"),
                    arguments_json: raw.to_string(),
                })],
            },
            ConversationMessage {
                role: ConversationRole::User,
                parts: vec![MessagePart::ToolResult(ToolResultRecord {
                    tool_call_id: ToolCallId::new("toolu_raw"),
                    content: "done".to_string(),
                    is_error: false,
                })],
            },
        ];

        let request = build_request(&operation).expect("raw arguments are valid JSON");
        let serialized = serde_json::to_string(&request).expect("request serializes");

        assert!(serialized.contains(raw));
    }

    #[test]
    fn unsigned_replayed_thinking_is_rejected_before_any_send() {
        let mut operation = operation("call-9");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::Thinking {
                text: "step one".to_string(),
                signature: None,
            }],
        }];

        let failure = build_request(&operation)
            .expect_err("an unsigned thinking block would only be rejected after the boundary");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));

        operation.messages[0].parts[0] = MessagePart::Thinking {
            text: "step one".to_string(),
            signature: Some(String::new()),
        };
        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn a_set_temperature_is_rejected_rather_than_silently_dropped() {
        let mut operation = operation("call-10");
        operation.settings.temperature = Some(0.5);

        let failure = build_request(&operation)
            .expect_err("a sampling demand the provider rejects must not travel as silence");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
    }

    #[test]
    fn a_set_top_p_is_rejected_rather_than_silently_dropped() {
        let mut operation = operation("call-top-p");
        operation.settings.top_p = Some(0.9);

        let failure = build_request(&operation)
            .expect_err("a sampling demand the provider rejects must not travel as silence");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
    }

    #[test]
    fn configured_settings_carrying_a_sampling_control_are_unsupported() {
        let mut settings = ModelSettings::new(64);
        settings.temperature = Some(0.5);

        assert!(matches!(
            validate_model_settings(&settings),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn zero_output_token_limit_is_rejected_before_send() {
        let mut operation = operation("call-zero-tokens");
        operation.settings.max_output_tokens = 0;
        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn tool_results_must_match_the_immediately_preceding_tool_uses() {
        let result = |id: &str| {
            MessagePart::ToolResult(ToolResultRecord {
                tool_call_id: ToolCallId::new(id),
                content: "done".to_string(),
                is_error: false,
            })
        };
        let call = MessagePart::ToolCall(ToolCallProposal {
            id: ToolCallId::new("toolu_1"),
            name: ToolName::new("lookup"),
            arguments_json: "{}".to_string(),
        });

        let mut orphan = operation("call-orphan");
        orphan.messages = vec![ConversationMessage {
            role: ConversationRole::User,
            parts: vec![result("toolu_1")],
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
                parts: vec![result("toolu_other")],
            },
        ];
        assert!(matches!(
            build_request(&mismatched),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn assistant_tool_result_is_rejected_before_any_send() {
        let mut operation = operation("call-12");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::ToolResult(ToolResultRecord {
                tool_call_id: ToolCallId::new("toolu_1"),
                content: "result".to_string(),
                is_error: false,
            })],
        }];

        let failure = build_request(&operation)
            .expect_err("Anthropic accepts tool_result only in a user-role message");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
    }

    #[test]
    fn user_tool_call_is_rejected_before_any_send() {
        let mut operation = operation("call-13");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::User,
            parts: vec![MessagePart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("toolu_1"),
                name: ToolName::new("lookup"),
                arguments_json: "{}".to_string(),
            })],
        }];

        let failure =
            build_request(&operation).expect_err("Anthropic accepts tool_use only from assistant");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
    }

    #[test]
    fn replayed_reasoning_parts_serialize_as_thinking_blocks() {
        let mut operation = operation("call-8");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![
                MessagePart::Thinking {
                    text: "step one".to_string(),
                    signature: Some("sig_1".to_string()),
                },
                MessagePart::RedactedThinking {
                    data: "opaque".to_string(),
                },
            ],
        }];

        let request = build_request(&operation).expect("reasoning history translates");
        let value = serde_json::to_value(&request).expect("wire request serializes");

        assert_eq!(
            value["messages"][0]["content"],
            serde_json::json!([
                {"type": "thinking", "thinking": "step one", "signature": "sig_1"},
                {"type": "redacted_thinking", "data": "opaque"}
            ])
        );
    }

    #[test]
    fn user_tool_results_after_text_are_rejected_before_send() {
        let mut operation = operation("call-14");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::User,
            parts: vec![
                MessagePart::Text("before".to_string()),
                MessagePart::ToolResult(ToolResultRecord {
                    tool_call_id: ToolCallId::new("toolu_1"),
                    content: "result".to_string(),
                    is_error: false,
                }),
            ],
        }];

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }
}
