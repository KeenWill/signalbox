//! Buffered-response decoding and shared response-fact mapping.

use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CompletionEvidence, ExchangeFacts, FinishReason,
    LossCause, Observation, ObservationFact, ObservationSink, ProviderMessageId,
    ProviderReportedModel, RefusalEvidence, TerminalEvidence, TokenUsage, ToolCallId,
    ToolCallProposal, ToolName,
};

use crate::wire::{ChatCompletion, WireResponseToolCall, WireUsage};

/// Maps the provider's `finish_reason` token to the normalized vocabulary.
///
/// `content_filter` maps to [`FinishReason::Refusal`]: the provider filtered
/// the output, which is its refusal outcome, and the response's `refusal`
/// payload (when present) is carried as refusal evidence. The provider does
/// not distinguish a natural stop from a caller stop-sequence hit — both
/// arrive as `stop` — so [`FinishReason::StopSequence`] is never produced
/// here. The legacy `function_call` token is deliberately left in the
/// unrecognized branch: this adapter never requests legacy functions.
pub(crate) fn map_finish(token: &str) -> FinishReason {
    match token {
        "stop" => FinishReason::EndTurn,
        "length" => FinishReason::MaxOutputTokens,
        "tool_calls" => FinishReason::ToolUse,
        "content_filter" => FinishReason::Refusal,
        other => FinishReason::Unrecognized {
            provider_token: other.to_string(),
        },
    }
}

/// Converts wire usage to the neutral usage record.
///
/// `prompt_tokens_details.cached_tokens` is the provider's cache-read count;
/// Chat Completions reports no cache-creation count, so that fact stays
/// unreported rather than being fabricated as zero.
pub(crate) fn convert_usage(wire: &WireUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: wire.prompt_tokens,
        output_tokens: wire.completion_tokens,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: wire
            .prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens),
    }
}

/// Converts one response tool call into a typed proposal, or reports why it
/// is not recognizable completion material.
pub(crate) fn convert_tool_call(call: &WireResponseToolCall) -> Result<ToolCallProposal, String> {
    if call.kind.as_deref() != Some("function") {
        return Err(format!(
            "tool call carries unrecognized type {:?}",
            call.kind.as_deref().unwrap_or("<absent>")
        ));
    }
    let (Some(id), Some(function)) = (&call.id, &call.function) else {
        return Err("tool call is missing its id or function".to_string());
    };
    let Some(name) = &function.name else {
        return Err("tool call is missing its function name".to_string());
    };
    Ok(ToolCallProposal {
        id: ToolCallId::new(id.clone()),
        name: ToolName::new(name.clone()),
        arguments_json: function
            .arguments
            .clone()
            .unwrap_or_else(|| "{}".to_string()),
    })
}

/// Decodes a complete success-status response body into terminal evidence,
/// emitting the facts it learns as observations along the way.
///
/// A body that is not the documented completion material — unparseable, not
/// exactly one choice, missing its finish reason, or carrying an
/// unrecognizable tool call — is boundary-loss evidence (ADR-0043: a success
/// status without valid completion material is not definitive), with the
/// facts observed before the defect retained. A non-empty `refusal` payload
/// or a `content_filter` finish is refusal evidence, never completion.
pub(crate) fn decode_buffered_response<C: Clone>(
    body: &[u8],
    exchange: ExchangeFacts,
    correlation: &C,
    sink: &mut (dyn ObservationSink<C> + Send),
) -> TerminalEvidence {
    let completion: ChatCompletion = match serde_json::from_slice(body) {
        Ok(completion) => completion,
        Err(error) => {
            return unintelligible(
                format!("success response body is not a chat completion: {error}"),
                exchange,
                None,
                TokenUsage::unreported(),
            );
        }
    };
    let reported_model = completion.model.map(ProviderReportedModel::new);
    if let Some(model) = &reported_model {
        sink.observe(Observation {
            correlation: correlation.clone(),
            fact: ObservationFact::ProviderModelReported(model.clone()),
        });
    }
    let usage = completion
        .usage
        .as_ref()
        .map(convert_usage)
        .unwrap_or_default();
    let message_id = completion.id.map(ProviderMessageId::new);
    let [choice] = completion.choices.as_slice() else {
        return unintelligible(
            format!(
                "success response carries {} choices; exactly one is requested",
                completion.choices.len()
            ),
            exchange,
            reported_model,
            usage,
        );
    };
    let Some(message) = &choice.message else {
        return unintelligible(
            "success response choice carries no message".to_string(),
            exchange,
            reported_model,
            usage,
        );
    };
    let mut content = Vec::new();
    if let Some(text) = &message.content
        && !text.is_empty()
    {
        content.push(AssistantPart::Text(text.clone()));
    }
    for call in &message.tool_calls {
        match convert_tool_call(call) {
            Ok(proposal) => {
                sink.observe(Observation {
                    correlation: correlation.clone(),
                    fact: ObservationFact::ToolCallProposed(proposal.clone()),
                });
                content.push(AssistantPart::ToolCall(proposal));
            }
            Err(detail) => return unintelligible(detail, exchange, reported_model, usage),
        }
    }
    sink.observe(Observation {
        correlation: correlation.clone(),
        fact: ObservationFact::UsageReported(usage),
    });
    let Some(finish_token) = &choice.finish_reason else {
        return unintelligible(
            "success response carries no finish_reason".to_string(),
            exchange,
            reported_model,
            usage,
        );
    };
    let mut finish = map_finish(finish_token);
    let refusal_payload = message
        .refusal
        .clone()
        .filter(|refusal| !refusal.is_empty());
    if refusal_payload.is_some() {
        finish = FinishReason::Refusal;
    }
    sink.observe(Observation {
        correlation: correlation.clone(),
        fact: ObservationFact::FinishReported(finish.clone()),
    });
    match finish.completion_finish() {
        None => {
            if let Some(refusal) = refusal_payload {
                content.push(AssistantPart::Text(refusal));
            }
            TerminalEvidence::Refused(RefusalEvidence {
                exchange,
                message_id,
                reported_model,
                content,
                usage,
            })
        }
        Some(finish) => TerminalEvidence::Completed(CompletionEvidence {
            exchange,
            message_id,
            reported_model,
            finish,
            content,
            usage,
        }),
    }
}

fn unintelligible(
    detail: String,
    exchange: ExchangeFacts,
    reported_model: Option<ProviderReportedModel>,
    usage: TokenUsage,
) -> TerminalEvidence {
    TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
        cause: LossCause::ResponseUnintelligible { detail },
        exchange,
        reported_model,
        finish_reported: None,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use signalbox_expect_table::table;
    use signalbox_model_runtime::{
        AssistantPart, CompletionFinish, ExchangeFacts, LossCause, Observation, ObservationFact,
        ProviderMessageId, ProviderReportedModel, ProviderRequestId, TerminalEvidence, TokenUsage,
        ToolCallId, ToolCallProposal, ToolName,
    };

    use super::{decode_buffered_response, map_finish};

    fn exchange() -> ExchangeFacts {
        ExchangeFacts {
            provider_request_id: Some(ProviderRequestId::new("req_1")),
            http_status: Some(200),
        }
    }

    /// Decodes the body against canonical exchange facts, collecting
    /// observations correlated to `"call-1"`.
    fn decode(body: &str) -> (TerminalEvidence, Vec<Observation<String>>) {
        let mut observations: Vec<Observation<String>> = Vec::new();
        let evidence = decode_buffered_response(
            body.as_bytes(),
            exchange(),
            &"call-1".to_string(),
            &mut observations,
        );
        (evidence, observations)
    }

    #[test]
    fn completed_response_decodes_every_reported_fact() {
        let (evidence, observations) = decode(
            r#"{
                "id": "chatcmpl_1",
                "model": "model-exact-1",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Checking.",
                        "refusal": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "lookup", "arguments": "{\"city\":\"Oslo\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {
                    "prompt_tokens": 12,
                    "completion_tokens": 34,
                    "prompt_tokens_details": {"cached_tokens": 6}
                }
            }"#,
        );

        let TerminalEvidence::Completed(completion) = evidence else {
            panic!("a complete success chat completion must decode as completion evidence");
        };
        assert_eq!(completion.exchange, exchange());
        assert_eq!(
            completion.message_id,
            Some(ProviderMessageId::new("chatcmpl_1"))
        );
        assert_eq!(
            completion.reported_model,
            Some(ProviderReportedModel::new("model-exact-1"))
        );
        assert_eq!(completion.finish, CompletionFinish::ToolUse);
        assert_eq!(
            completion.content,
            vec![
                AssistantPart::Text("Checking.".to_string()),
                AssistantPart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("call_1"),
                    name: ToolName::new("lookup"),
                    arguments_json: r#"{"city":"Oslo"}"#.to_string(),
                }),
            ]
        );
        assert_eq!(
            completion.usage,
            TokenUsage {
                input_tokens: Some(12),
                output_tokens: Some(34),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: Some(6),
            }
        );
        assert_eq!(
            observations.first(),
            Some(&Observation {
                correlation: "call-1".to_string(),
                fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new(
                    "model-exact-1"
                )),
            })
        );
    }

    #[test]
    fn refusal_payload_is_refusal_evidence_carrying_the_refusal_text() {
        let (evidence, _) = decode(
            r#"{
                "id": "chatcmpl_1",
                "model": "model-exact-1",
                "choices": [{
                    "message": {"role": "assistant", "content": null,
                                "refusal": "I cannot help with that."},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 9, "completion_tokens": 8}
            }"#,
        );

        let TerminalEvidence::Refused(refusal) = evidence else {
            panic!("a refusal payload must decode as refusal evidence, never completion");
        };
        assert_eq!(
            refusal.content,
            vec![AssistantPart::Text("I cannot help with that.".to_string())]
        );
        assert_eq!(
            refusal.reported_model,
            Some(ProviderReportedModel::new("model-exact-1"))
        );
    }

    #[test]
    fn content_filter_finish_is_refusal_evidence() {
        let (evidence, _) = decode(
            r#"{
                "id": "chatcmpl_1",
                "model": "model-exact-1",
                "choices": [{
                    "message": {"role": "assistant", "content": "partial"},
                    "finish_reason": "content_filter"
                }]
            }"#,
        );

        let TerminalEvidence::Refused(refusal) = evidence else {
            panic!("a content_filter finish is the provider's refusal outcome");
        };
        assert_eq!(
            refusal.content,
            vec![AssistantPart::Text("partial".to_string())]
        );
    }

    #[test]
    fn missing_finish_reason_is_boundary_loss_with_retained_facts() {
        let (evidence, _) = decode(
            r#"{
                "id": "chatcmpl_1",
                "model": "model-exact-1",
                "choices": [{"message": {"role": "assistant", "content": "partial"}}],
                "usage": {"prompt_tokens": 3}
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a success body without finish_reason is not definitive completion material");
        };
        assert!(matches!(
            loss.cause,
            LossCause::ResponseUnintelligible { .. }
        ));
        assert_eq!(
            loss.reported_model,
            Some(ProviderReportedModel::new("model-exact-1"))
        );
        assert_eq!(loss.usage.input_tokens, Some(3));
    }

    #[test]
    fn zero_choices_is_boundary_loss() {
        let (evidence, _) =
            decode(r#"{"id": "chatcmpl_1", "model": "model-exact-1", "choices": []}"#);

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a response without the one requested choice is not definitive");
        };
        assert!(matches!(
            loss.cause,
            LossCause::ResponseUnintelligible { .. }
        ));
    }

    #[test]
    fn unrecognized_tool_call_type_is_boundary_loss_not_silent_drop() {
        let (evidence, _) = decode(
            r#"{
                "id": "chatcmpl_1",
                "model": "model-exact-1",
                "choices": [{
                    "message": {"role": "assistant",
                                "tool_calls": [{"id": "call_1", "type": "custom",
                                                "custom": {"name": "x", "input": "y"}}]},
                    "finish_reason": "tool_calls"
                }]
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("an unrecognized tool-call type must surface as evidence, never drop");
        };
        assert!(matches!(
            loss.cause,
            LossCause::ResponseUnintelligible { .. }
        ));
    }

    #[test]
    fn unparseable_success_body_is_boundary_loss() {
        let (evidence, observations) = decode("<html>gateway</html>");

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("an unparseable success body is not definitive completion material");
        };
        assert!(matches!(
            loss.cause,
            LossCause::ResponseUnintelligible { .. }
        ));
        assert_eq!(loss.exchange, exchange());
        assert_eq!(observations, vec![]);
    }

    #[derive(Debug)]
    #[allow(
        dead_code,
        reason = "the table renderer reads every field through the Debug derive"
    )]
    struct FinishRow {
        token: &'static str,
        finish: String,
    }

    /// Renders one mapping row per finish-reason token, in the given order.
    fn finish_rows(tokens: &[&'static str]) -> Vec<FinishRow> {
        tokens
            .iter()
            .map(|token| FinishRow {
                token,
                finish: format!("{:?}", map_finish(token)),
            })
            .collect()
    }

    #[test]
    fn every_documented_finish_reason_maps_and_unknown_is_retained_verbatim() {
        let rows = finish_rows(&[
            "stop",
            "length",
            "tool_calls",
            "content_filter",
            "function_call",
        ]);

        expect![[r#"
            ┌────────────────┬────────────────────────────────────────────────────┐
            │ token          │ finish                                             │
            ├────────────────┼────────────────────────────────────────────────────┤
            │ stop           │ EndTurn                                            │
            │ length         │ MaxOutputTokens                                    │
            │ tool_calls     │ ToolUse                                            │
            │ content_filter │ Refusal                                            │
            │ function_call  │ Unrecognized { provider_token: \"function_call\" } │
            └────────────────┴────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table(rows));
    }
}
