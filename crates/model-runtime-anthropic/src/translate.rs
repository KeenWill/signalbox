//! Operation-to-wire translation.

use std::collections::BTreeSet;

use signalbox_model_runtime::{
    ConversationMessage, ConversationRole, DeliveryMode, MessagePart, ModelOperation,
    PreparationFailure, ToolChoice,
};

use crate::wire::{MessagesRequest, WireMessage, WireRequestBlock, WireTool, WireToolChoice};

/// Builds the wire request for one operation.
///
/// Pure translation: any failure is a trustworthy [`PreparationFailure`]
/// returned before a one-shot capability exists. Nothing has touched the
/// network.
///
/// A structured-output contract is realized as a forced tool call: the
/// contract joins the declared tools and `tool_choice` forces it with
/// parallel tool use disabled, so exactly one contract value returns.
/// [`ModelOperation::validate`] reserves the contract name from ordinary
/// tools before anything is sent.
pub(crate) fn build_request<C>(
    operation: &ModelOperation<C>,
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
    // serde_json serializes a non-finite f64 as null, and the provider only
    // accepts sampling controls in the inclusive unit interval. Reject both
    // cases during preparation rather than relying on a post-send 4xx.
    for (name, value) in [
        ("temperature", operation.settings.temperature),
        ("top_p", operation.settings.top_p),
    ] {
        if let Some(value) = value
            && !(0.0..=1.0).contains(&value)
        {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: format!("{name} must be a finite number from 0 through 1"),
            });
        }
    }
    validate_tool_history(&operation.messages)?;
    let (tools, tool_choice) = tools_and_choice(operation)?;
    Ok(MessagesRequest {
        model: operation.resolved_target.as_str().to_string(),
        max_tokens: operation.settings.max_output_tokens,
        messages: operation
            .messages
            .iter()
            .map(wire_message)
            .collect::<Result<Vec<_>, _>>()?,
        system: operation.system.clone(),
        stop_sequences: operation.settings.stop_sequences.clone(),
        temperature: operation.settings.temperature,
        top_p: operation.settings.top_p,
        tools,
        tool_choice,
        stream: operation.delivery == DeliveryMode::Streamed,
    })
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
                             the immediately following user message"
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

fn tools_and_choice<C>(
    operation: &ModelOperation<C>,
) -> Result<(Option<Vec<WireTool>>, Option<WireToolChoice>), PreparationFailure> {
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
        tools.push(WireTool {
            name: contract.name.as_str().to_string(),
            description: contract.description.clone(),
            input_schema: contract.schema.clone(),
        });
        return Ok((
            Some(tools),
            // The contract promises exactly one value; parallel tool use
            // could return several proposals for the forced tool.
            Some(WireToolChoice::Tool {
                name: contract.name.as_str().to_string(),
                disable_parallel_tool_use: Some(true),
            }),
        ));
    }
    if tools.is_empty() {
        return Ok((None, None));
    }
    let choice = match &operation.tool_choice {
        ToolChoice::Automatic => WireToolChoice::Auto,
        ToolChoice::AnyTool => WireToolChoice::Any,
        ToolChoice::Named(name) => WireToolChoice::Tool {
            name: name.as_str().to_string(),
            disable_parallel_tool_use: None,
        },
    };
    Ok((Some(tools), Some(choice)))
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
                detail: "Anthropic requires tool results in user messages and tool calls or \
                         thinking blocks in assistant messages"
                    .to_string(),
            });
        }
        if message.role == ConversationRole::User {
            match part {
                MessagePart::Text(_) => user_text_seen = true,
                MessagePart::ToolResult(_) if user_text_seen => {
                    return Err(PreparationFailure::UnsupportedOperation {
                        detail: "Anthropic requires every tool result in a user message to \
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
                if !serde_json::from_str::<serde_json::Value>(input.get())
                    .is_ok_and(|value| value.is_object())
                {
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
            CredentialReference::new("anthropic-primary"),
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
              "system": "Answer briefly.",
              "temperature": 0.5,
              "tool_choice": {
                "name": "lookup",
                "type": "tool"
              },
              "tools": [
                {
                  "description": "Looks up a city.",
                  "input_schema": {
                    "type": "object"
                  },
                  "name": "lookup"
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
    fn an_empty_conversation_is_rejected_before_any_send() {
        let mut operation = operation("call-empty-conversation");
        operation.messages.clear();

        assert!(matches!(
            build_request(&operation),
            Err(PreparationFailure::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn output_contract_becomes_the_forced_only_tool() {
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
            serde_json::json!({
                "type": "tool",
                "name": "verdict",
                "disable_parallel_tool_use": true
            })
        );
        assert_eq!(value["tools"][0]["name"], serde_json::json!("verdict"));
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

        assert_eq!(value["tools"][0]["name"], serde_json::json!("lookup"));
        assert_eq!(value["tools"][1]["name"], serde_json::json!("verdict"));
        assert_eq!(
            value["tool_choice"],
            serde_json::json!({
                "type": "tool",
                "name": "verdict",
                "disable_parallel_tool_use": true
            })
        );
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
    fn non_finite_temperature_is_rejected_not_silently_nulled() {
        let mut operation = operation("call-10");
        operation.settings.temperature = Some(f64::NAN);

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
        assert_temperature_is_rejected(1.1);
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
            .expect_err("Anthropic accepts tool_result only in a user message");

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
