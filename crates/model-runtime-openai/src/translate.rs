//! Operation-to-wire translation.

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
/// Pure translation: any failure is a [`PreparationFailure`] the runtime
/// reports as proven-unsent evidence — nothing has touched the network.
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
    // serde_json serializes a non-finite f64 as null, which would silently
    // drop the caller's stated setting; reject during preparation instead.
    for (name, value) in [
        ("temperature", operation.settings.temperature),
        ("top_p", operation.settings.top_p),
    ] {
        if let Some(value) = value
            && !value.is_finite()
        {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: format!("{name} must be a finite number"),
            });
        }
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
