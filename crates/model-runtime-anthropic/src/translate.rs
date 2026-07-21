//! Operation-to-wire translation.

use signalbox_model_runtime::{
    ConversationMessage, ConversationRole, DeliveryMode, MessagePart, ModelOperation,
    PreparationFailure, ToolChoice,
};

use crate::wire::{MessagesRequest, WireMessage, WireRequestBlock, WireTool, WireToolChoice};

/// Builds the wire request for one operation.
///
/// Pure translation: any failure is a [`PreparationFailure`] the runtime
/// reports as proven-unsent evidence — nothing has touched the network.
///
/// A structured-output contract is realized as a forced tool call: the
/// contract becomes the request's only tool and `tool_choice` forces it.
/// Combining a contract with caller tools is rejected as unsupported rather
/// than silently overriding the caller's tool choice.
pub(crate) fn build_request<C>(
    operation: &ModelOperation<C>,
) -> Result<MessagesRequest, PreparationFailure> {
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

fn tools_and_choice<C>(
    operation: &ModelOperation<C>,
) -> Result<(Option<Vec<WireTool>>, Option<WireToolChoice>), PreparationFailure> {
    if let Some(contract) = &operation.output_contract {
        if !operation.tools.is_empty() {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: "an operation cannot combine caller tools with a structured-output \
                         contract: the contract is realized as a forced tool call, which would \
                         suppress the caller's tools"
                    .to_string(),
            });
        }
        return Ok((
            Some(vec![WireTool {
                name: contract.name.as_str().to_string(),
                description: contract.description.clone(),
                input_schema: contract.schema.clone(),
            }]),
            // The contract promises exactly one value; parallel tool use
            // could return several proposals for the forced tool.
            Some(WireToolChoice::Tool {
                name: contract.name.as_str().to_string(),
                disable_parallel_tool_use: Some(true),
            }),
        ));
    }
    if operation.tools.is_empty() {
        return Ok((None, None));
    }
    let tools = operation
        .tools
        .iter()
        .map(|tool| WireTool {
            name: tool.name.as_str().to_string(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
        })
        .collect();
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
                let input: serde_json::Value = serde_json::from_str(&proposal.arguments_json)
                    .map_err(|error| PreparationFailure::SerializationFailed {
                        detail: format!(
                            "replayed tool call {} carries arguments that are not valid JSON: \
                             {error}",
                            proposal.id.as_str()
                        ),
                    })?;
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
            MessagePart::Thinking { text, signature } => Ok(WireRequestBlock::Thinking {
                thinking: text.clone(),
                signature: signature.clone(),
            }),
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
    fn contract_combined_with_caller_tools_is_rejected_as_unsupported() {
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

        let failure = build_request(&operation)
            .expect_err("a contract combined with caller tools must not silently translate");

        assert!(matches!(
            failure,
            PreparationFailure::UnsupportedOperation { .. }
        ));
    }

    #[test]
    fn replayed_tool_call_with_invalid_argument_json_fails_preparation() {
        let mut operation = operation("call-7");
        operation.messages = vec![ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("toolu_9"),
                name: ToolName::new("lookup"),
                arguments_json: "{not json".to_string(),
            })],
        }];

        let failure = build_request(&operation)
            .expect_err("invalid replayed tool arguments must fail before any send");

        assert!(matches!(
            failure,
            PreparationFailure::SerializationFailed { .. }
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
}
