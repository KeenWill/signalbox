//! Operation-to-wire translation.

use std::collections::BTreeSet;

use signalbox_model_runtime::{
    ConversationMessage, ConversationRole, DeliveryMode, MessagePart, ModelOperation,
    PreparationFailure, ToolChoice,
};

use crate::wire::{
    ChatRequest, StreamOptions, WireChatMessage, WireFunctionDefinition, WireFunctionTool,
    WireRequestFunction, WireRequestToolCall,
};

/// Builds the wire request for one operation.
///
/// Pure translation: any failure is a trustworthy [`PreparationFailure`]
/// returned before a one-shot capability exists. Nothing has touched the
/// network.
///
/// A structured-output contract is realized as a forced function call — the
/// same mechanism the Anthropic adapter uses — so the provider-independent
/// decode in the core crate applies unchanged. (The provider's native
/// `response_format` mechanism would return the value as content text and
/// require strict-mode schema transformation; a forced function keeps the
/// contract uniform across adapters.) The contract joins the declared tools
/// under its reserved name — [`ModelOperation::validate`] rejects
/// collisions — and is forced with parallel tool calling disabled.
///
/// Streamed delivery always requests `stream_options.include_usage`, so the
/// stream carries a usage record before its terminal marker.
pub(crate) fn build_request<C>(
    operation: &ModelOperation<C>,
) -> Result<ChatRequest, PreparationFailure> {
    if let Err(error) = operation.validate() {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: error.to_string(),
        });
    }
    validate_function_names(operation)?;
    if operation.settings.max_output_tokens == 0 {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: "max_output_tokens must be at least 1".to_string(),
        });
    }
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
    if operation.settings.stop_sequences.len() > 4 {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: "Chat Completions accepts at most four stop sequences".to_string(),
        });
    }
    let (tools, tool_choice, parallel_tool_calls) = tools_and_choice(operation)?;
    let mut messages = Vec::new();
    if let Some(system) = &operation.system {
        messages.push(WireChatMessage {
            role: "system",
            content: Some(system.clone()),
            tool_calls: None,
            tool_call_id: None,
        });
    }
    for message in &operation.messages {
        wire_messages(message, &mut messages)?;
    }
    if messages.is_empty() {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: "Chat Completions requires at least one message".to_string(),
        });
    }
    validate_tool_history(&operation.messages)?;
    let streamed = operation.delivery == DeliveryMode::Streamed;
    Ok(ChatRequest {
        model: operation.resolved_target.as_str().to_string(),
        messages,
        max_completion_tokens: operation.settings.max_output_tokens,
        temperature: operation.settings.temperature,
        top_p: operation.settings.top_p,
        stop: operation.settings.stop_sequences.clone(),
        tools,
        tool_choice,
        parallel_tool_calls,
        stream: streamed,
        stream_options: streamed.then_some(StreamOptions {
            include_usage: true,
        }),
    })
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
                "OpenAI {subject} name must contain 1 through 64 ASCII letters, digits, underscores, or hyphens"
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
                    detail: "Chat Completions requires one matching tool result for every tool \
                             call in the immediately following user message"
                        .to_string(),
                });
            }
        } else if !results.is_empty() {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: "Chat Completions tool results must answer calls from the immediately \
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
            detail: "Chat Completions requires tool calls to be followed by matching results"
                .to_string(),
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
    let mut tools: Vec<WireFunctionTool> = operation
        .tools
        .iter()
        .map(|tool| WireFunctionTool {
            kind: "function",
            function: WireFunctionDefinition {
                name: tool.name.as_str().to_string(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
            },
        })
        .collect();
    if let Some(contract) = &operation.output_contract {
        tools.push(WireFunctionTool {
            kind: "function",
            function: WireFunctionDefinition {
                name: contract.name.as_str().to_string(),
                description: contract.description.clone(),
                parameters: contract.schema.clone(),
            },
        });
        return Ok((
            Some(tools),
            Some(serde_json::json!({
                "type": "function",
                "function": { "name": contract.name.as_str() }
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
            "function": { "name": name.as_str() }
        }),
    };
    Ok((Some(tools), Some(choice), None))
}

/// Translates one conversation message into wire messages, in part order.
///
/// Chat Completions carries tool results as separate `tool`-role messages
/// rather than content parts, so one conversation message can produce
/// several wire messages: consecutive text parts group into one message,
/// assistant tool calls attach to the preceding assistant text (or form
/// their own message), and each tool result becomes its own message.
fn wire_messages(
    message: &ConversationMessage,
    out: &mut Vec<WireChatMessage>,
) -> Result<(), PreparationFailure> {
    if message.parts.is_empty() {
        return Err(PreparationFailure::UnsupportedOperation {
            detail: "the Chat Completions wire contract cannot preserve an empty conversation \
                     message"
                .to_string(),
        });
    }
    let role = match message.role {
        ConversationRole::User => "user",
        ConversationRole::Assistant => "assistant",
    };
    let mut pending_text: Option<String> = None;
    let mut pending_tool_calls: Vec<WireRequestToolCall> = Vec::new();
    let mut user_text_seen = false;
    for part in &message.parts {
        match part {
            MessagePart::Text(text) => {
                if !pending_tool_calls.is_empty() {
                    // One wire message orders content before tool_calls, and
                    // a split would separate the tool-call message from the
                    // tool result that must follow it; the ordering is not
                    // representable on this wire.
                    return Err(PreparationFailure::UnsupportedOperation {
                        detail: "the Chat Completions wire contract cannot represent \
                                 assistant text after a tool call in one message"
                            .to_string(),
                    });
                }
                match &mut pending_text {
                    Some(pending) => pending.push_str(text),
                    None => pending_text = Some(text.clone()),
                }
                if role == "user" {
                    user_text_seen = true;
                }
            }
            MessagePart::ToolCall(proposal) => {
                if role != "assistant" {
                    // Chat Completions permits tool_calls only on assistant
                    // messages; sending them elsewhere is a locally knowable
                    // declaration error.
                    return Err(PreparationFailure::UnsupportedOperation {
                        detail: "the Chat Completions wire contract permits tool calls only \
                                 in assistant history"
                            .to_string(),
                    });
                }
                if let Err(error) =
                    serde_json::from_str::<serde_json::Value>(&proposal.arguments_json)
                {
                    return Err(PreparationFailure::UnsupportedOperation {
                        detail: format!(
                            "replayed tool call {} carries arguments that are not valid JSON: \
                             {error}",
                            proposal.id.as_str()
                        ),
                    });
                }
                pending_tool_calls.push(WireRequestToolCall {
                    id: proposal.id.as_str().to_string(),
                    kind: "function",
                    function: WireRequestFunction {
                        name: proposal.name.as_str().to_string(),
                        arguments: proposal.arguments_json.clone(),
                    },
                })
            }
            MessagePart::ToolResult(result) => {
                if role != "user" {
                    return Err(PreparationFailure::UnsupportedOperation {
                        detail: "the Chat Completions wire contract permits tool results only \
                                 in user history"
                            .to_string(),
                    });
                }
                if result.is_error {
                    // The tool message has no native failure flag; encoding
                    // one would alter the caller's payload, and dropping the
                    // fact would misstate the conversation. The caller
                    // states the failure in the result content explicitly
                    // for this provider.
                    return Err(PreparationFailure::UnsupportedOperation {
                        detail: "the Chat Completions wire contract cannot mark a replayed \
                                 tool result as failed; state the failure in the result \
                                 content"
                            .to_string(),
                    });
                }
                if user_text_seen {
                    return Err(PreparationFailure::UnsupportedOperation {
                        detail: "Chat Completions requires replayed tool results before user text"
                            .to_string(),
                    });
                }
                flush(role, &mut pending_text, &mut pending_tool_calls, out);
                out.push(WireChatMessage {
                    role: "tool",
                    content: Some(result.content.clone()),
                    tool_calls: None,
                    tool_call_id: Some(result.tool_call_id.as_str().to_string()),
                });
            }
            // Chat Completions has no representation for replayed reasoning;
            // dropping caller-stated history silently would misstate the
            // conversation, so it is a preparation failure the caller can
            // act on (strip the parts or route to a reasoning-capable
            // provider).
            MessagePart::Thinking { .. } | MessagePart::RedactedThinking { .. } => {
                return Err(PreparationFailure::UnsupportedOperation {
                    detail: "the Chat Completions wire contract cannot represent replayed \
                             reasoning history"
                        .to_string(),
                });
            }
        }
    }
    flush(role, &mut pending_text, &mut pending_tool_calls, out);
    Ok(())
}

fn flush(
    role: &'static str,
    pending_text: &mut Option<String>,
    pending_tool_calls: &mut Vec<WireRequestToolCall>,
    out: &mut Vec<WireChatMessage>,
) {
    let content = pending_text.take();
    let tool_calls = std::mem::take(pending_tool_calls);
    if content.is_none() && tool_calls.is_empty() {
        return;
    }
    out.push(WireChatMessage {
        role,
        content,
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        tool_call_id: None,
    });
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use signalbox_model_runtime::CredentialReference;
    use signalbox_model_runtime::{
        ConversationMessage, ConversationRole, DeliveryMode, MessagePart, ModelOperation,
        ModelSettings, PreparationFailure, RequestedTarget, ResolvedTarget,
        StructuredOutputContract, ToolCallId, ToolCallProposal, ToolChoice, ToolDefinition,
        ToolName, ToolResultRecord,
    };

    use super::build_request;

    /// An operation whose correlation seed is the one knob; targets, one
    /// user message, and a 64-token ceiling are canonical.
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
        let value = serde_json::to_value(&request).expect("wire request serializes");
        format!("{value:#}")
    }

    #[test]
    fn full_operation_serializes_every_stated_fact() {
        let mut operation = operation("call-1");
        operation.system = Some("Answer briefly.".to_string());
        operation.settings.temperature = Some(0.5);
        operation.settings.top_p = Some(0.9);
        operation.settings.stop_sequences = vec!["END".to_string()];
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
              "max_completion_tokens": 64,
              "messages": [
                {
                  "content": "Answer briefly.",
                  "role": "system"
                },
                {
                  "content": "look up Oslo",
                  "role": "user"
                },
                {
                  "content": "Looking it up.",
                  "role": "assistant",
                  "tool_calls": [
                    {
                      "function": {
                        "arguments": "{\"city\":\"Oslo\"}",
                        "name": "lookup"
                      },
                      "id": "call_a1",
                      "type": "function"
                    }
                  ]
                },
                {
                  "content": "population 700000",
                  "role": "tool",
                  "tool_call_id": "call_a1"
                },
                {
                  "content": "thanks",
                  "role": "user"
                }
              ],
              "model": "model-exact-1",
              "stop": [
                "END"
              ],
              "stream": false,
              "temperature": 0.5,
              "tool_choice": {
                "function": {
                  "name": "lookup"
                },
                "type": "function"
              },
              "tools": [
                {
                  "function": {
                    "description": "Looks up a city.",
                    "name": "lookup",
                    "parameters": {
                      "type": "object"
                    }
                  },
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
    fn streamed_delivery_sets_the_stream_flag_and_requests_usage() {
        let mut operation = operation("call-3");
        operation.delivery = DeliveryMode::Streamed;

        let request = build_request(&operation).expect("translatable operation builds");
        let value = serde_json::to_value(&request).expect("wire request serializes");

        assert_eq!(value["stream"], serde_json::json!(true));
        assert_eq!(
            value["stream_options"],
            serde_json::json!({"include_usage": true})
        );
    }

    #[test]
    fn minimal_operation_omits_every_unset_optional_field() {
        expect![[r#"
            {
              "max_completion_tokens": 64,
              "messages": [
                {
                  "content": "hello",
                  "role": "user"
                }
              ],
              "model": "model-exact-1",
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
        assert_eq!(request.messages.len(), 1);
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
            schema: serde_json::json!({"type": "object"}),
        });

        let request = build_request(&operation).expect("contract-bearing operation builds");
        let value = serde_json::to_value(&request).expect("wire request serializes");

        assert_eq!(
            value["tool_choice"],
            serde_json::json!({"type": "function", "function": {"name": "verdict"}})
        );
        assert_eq!(
            value["tools"][0]["function"]["name"],
            serde_json::json!("verdict")
        );
    }

    #[test]
    fn contract_combined_with_caller_tools_declares_both_and_forces_the_contract() {
        let mut operation = operation("call-6");
        operation.output_contract = Some(StructuredOutputContract {
            name: ToolName::new("verdict"),
            description: "The verdict.".to_string(),
            schema: serde_json::json!({"type": "object"}),
        });
        operation.tools = vec![ToolDefinition::with_schema(
            "lookup",
            "Looks up a city.",
            serde_json::json!({"type": "object"}),
        )];

        let request = build_request(&operation).expect("distinct names translate");
        let value = serde_json::to_value(&request).expect("wire request serializes");

        assert_eq!(
            value["tools"][0]["function"]["name"],
            serde_json::json!("lookup")
        );
        assert_eq!(
            value["tools"][1]["function"]["name"],
            serde_json::json!("verdict")
        );
        assert_eq!(
            value["tool_choice"],
            serde_json::json!({"type": "function", "function": {"name": "verdict"}})
        );
        assert_eq!(value["parallel_tool_calls"], serde_json::json!(false));
    }

    #[test]
    fn contract_name_colliding_with_a_tool_is_rejected_before_any_send() {
        let mut operation = operation("call-11");
        operation.output_contract = Some(StructuredOutputContract {
            name: ToolName::new("verdict"),
            description: "The verdict.".to_string(),
            schema: serde_json::json!({"type": "object"}),
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
            schema: serde_json::json!({"type": "object"}),
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
    fn failed_tool_result_replay_is_rejected_not_silently_flattened() {
        let mut operation = operation("call-8");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::User,
            parts: vec![MessagePart::ToolResult(ToolResultRecord {
                tool_call_id: ToolCallId::new("call_a1"),
                content: "disk full".to_string(),
                is_error: true,
            })],
        }];

        let failure = build_request(&operation)
            .expect_err("a failed-tool fact this wire contract cannot carry must not vanish");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
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
    fn text_after_a_tool_call_is_rejected_as_unrepresentable() {
        let mut operation = operation("call-9");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![
                MessagePart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("call_a1"),
                    name: ToolName::new("lookup"),
                    arguments_json: "{}".to_string(),
                }),
                MessagePart::Text("after the call".to_string()),
            ],
        }];

        let failure = build_request(&operation)
            .expect_err("splitting would separate the tool call from its required result");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
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
    fn zero_output_token_limit_is_rejected_before_send() {
        let mut candidate = operation("call-zero-tokens");
        candidate.settings.max_output_tokens = 0;
        assert!(matches!(
            build_request(&candidate),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
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
    fn more_than_four_stop_sequences_are_rejected_before_any_send() {
        let mut operation = operation("call-too-many-stops");
        operation.settings.stop_sequences = (0..5).map(|index| format!("stop-{index}")).collect();

        let failure = build_request(&operation)
            .expect_err("the provider accepts no more than four stop sequences");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
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
