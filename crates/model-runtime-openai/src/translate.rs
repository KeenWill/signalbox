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
/// contract uniform across adapters.) Combining a contract with caller
/// tools is rejected as unsupported rather than silently overriding the
/// caller's tool choice.
///
/// Streamed delivery always requests `stream_options.include_usage`, so the
/// stream carries a usage record before its terminal marker.
pub(crate) fn build_request<C>(
    operation: &ModelOperation<C>,
) -> Result<ChatRequest, PreparationFailure> {
    let (tools, tool_choice) = tools_and_choice(operation)?;
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
        wire_messages(message, &mut messages);
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
        stream: streamed,
        stream_options: streamed.then_some(StreamOptions {
            include_usage: true,
        }),
    })
}

fn tools_and_choice<C>(
    operation: &ModelOperation<C>,
) -> Result<(Option<Vec<WireFunctionTool>>, Option<serde_json::Value>), PreparationFailure> {
    if let Some(contract) = &operation.output_contract {
        if !operation.tools.is_empty() {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: "an operation cannot combine caller tools with a structured-output \
                         contract: the contract is realized as a forced function call, which \
                         would suppress the caller's tools"
                    .to_string(),
            });
        }
        return Ok((
            Some(vec![WireFunctionTool {
                kind: "function",
                function: WireFunctionDefinition {
                    name: contract.name.as_str().to_string(),
                    description: contract.description.clone(),
                    parameters: contract.schema.clone(),
                },
            }]),
            Some(serde_json::json!({
                "type": "function",
                "function": { "name": contract.name.as_str() }
            })),
        ));
    }
    if operation.tools.is_empty() {
        return Ok((None, None));
    }
    let tools = operation
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
    let choice = match &operation.tool_choice {
        ToolChoice::Automatic => serde_json::json!("auto"),
        ToolChoice::AnyTool => serde_json::json!("required"),
        ToolChoice::Named(name) => serde_json::json!({
            "type": "function",
            "function": { "name": name.as_str() }
        }),
    };
    Ok((Some(tools), Some(choice)))
}

/// Translates one conversation message into wire messages, in part order.
///
/// Chat Completions carries tool results as separate `tool`-role messages
/// rather than content parts, so one conversation message can produce
/// several wire messages: consecutive text parts group into one message,
/// assistant tool calls attach to the preceding assistant text (or form
/// their own message), and each tool result becomes its own message.
fn wire_messages(message: &ConversationMessage, out: &mut Vec<WireChatMessage>) {
    let role = match message.role {
        ConversationRole::User => "user",
        ConversationRole::Assistant => "assistant",
    };
    let mut pending_text: Option<String> = None;
    let mut pending_tool_calls: Vec<WireRequestToolCall> = Vec::new();
    for part in &message.parts {
        match part {
            MessagePart::Text(text) => match &mut pending_text {
                Some(pending) => pending.push_str(text),
                None => pending_text = Some(text.clone()),
            },
            MessagePart::ToolCall(proposal) => pending_tool_calls.push(WireRequestToolCall {
                id: proposal.id.as_str().to_string(),
                kind: "function",
                function: WireRequestFunction {
                    name: proposal.name.as_str().to_string(),
                    arguments: proposal.arguments_json.clone(),
                },
            }),
            MessagePart::ToolResult(result) => {
                flush(role, &mut pending_text, &mut pending_tool_calls, out);
                out.push(WireChatMessage {
                    role: "tool",
                    content: Some(result.content.clone()),
                    tool_calls: None,
                    tool_call_id: Some(result.tool_call_id.as_str().to_string()),
                });
            }
        }
    }
    flush(role, &mut pending_text, &mut pending_tool_calls, out);
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
}
