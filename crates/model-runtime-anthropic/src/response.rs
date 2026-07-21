//! Buffered-response decoding and shared response-fact mapping.

use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CompletionEvidence, ExchangeFacts, FinishReason,
    LossCause, Observation, ObservationFact, ObservationSink, ProviderMessageId,
    ProviderReportedModel, RefusalEvidence, TerminalEvidence, TokenUsage, ToolCallId,
    ToolCallProposal, ToolName,
};

use crate::wire::{MessagesResponse, WireResponseBlock, WireUsage, parse_response_block};

/// Maps the provider's `stop_reason` token to the normalized vocabulary.
///
/// `pause_turn` is deliberately left in the unrecognized branch: it arises
/// only on server-tool turns, which this adapter never requests, and mapping
/// it to a recognized finish would claim semantics this adapter cannot
/// honor.
pub(crate) fn map_finish(token: &str, stop_sequence: Option<String>) -> FinishReason {
    match token {
        "end_turn" => FinishReason::EndTurn,
        "max_tokens" => FinishReason::MaxOutputTokens,
        "stop_sequence" => FinishReason::StopSequence {
            sequence: stop_sequence,
        },
        "tool_use" => FinishReason::ToolUse,
        "refusal" => FinishReason::Refusal,
        other => FinishReason::Unrecognized {
            provider_token: other.to_string(),
        },
    }
}

/// Converts wire usage to the neutral usage record.
pub(crate) fn convert_usage(wire: &WireUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: wire.input_tokens,
        output_tokens: wire.output_tokens,
        cache_creation_input_tokens: wire.cache_creation_input_tokens,
        cache_read_input_tokens: wire.cache_read_input_tokens,
    }
}

/// A recognized response block converted to a neutral part, or the fact
/// that the block type is unrecognized.
pub(crate) fn convert_block(block: WireResponseBlock) -> Option<AssistantPart> {
    match block {
        WireResponseBlock::Text { text } => Some(AssistantPart::Text(text)),
        WireResponseBlock::ToolUse { id, name, input } => {
            Some(AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new(id),
                name: ToolName::new(name),
                // The provider's raw JSON slice, verbatim — never
                // re-serialized, so key order and lexemes survive.
                arguments_json: input.get().to_string(),
            }))
        }
        WireResponseBlock::Thinking {
            thinking,
            signature,
        } => Some(AssistantPart::Thinking {
            text: thinking,
            signature,
        }),
        WireResponseBlock::RedactedThinking { data } => {
            Some(AssistantPart::RedactedThinking { data })
        }
        WireResponseBlock::Unrecognized => None,
    }
}

/// Decodes a complete success-status response body into terminal evidence,
/// emitting the facts it learns as observations along the way.
///
/// A body that is not the documented completion material — unparseable,
/// missing the envelope's required fields (`type: "message"`,
/// `role: "assistant"`, `id`, `model`, `usage`), carrying an unrecognized
/// content-block type, or missing its stop reason — is boundary-loss
/// evidence (ADR-0043: a success status without valid completion material
/// is not definitive), with the facts observed before the defect retained.
pub(crate) fn decode_buffered_response<C: Clone>(
    body: &[u8],
    exchange: ExchangeFacts,
    correlation: &C,
    sink: &mut (dyn ObservationSink<C> + Send),
) -> TerminalEvidence {
    let response: MessagesResponse = match serde_json::from_slice(body) {
        Ok(response) => response,
        Err(error) => {
            return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                cause: LossCause::ResponseUnintelligible {
                    detail: format!("success response body is not a message: {error}"),
                },
                exchange,
                reported_model: None,
                finish_reported: None,
                usage: TokenUsage::unreported(),
            });
        }
    };
    if response.response_type.as_deref() != Some("message")
        || response.role.as_deref() != Some("assistant")
    {
        return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::ResponseUnintelligible {
                detail: "success response is missing its message/assistant envelope \
                         discriminators"
                    .to_string(),
            },
            exchange,
            reported_model: None,
            finish_reported: None,
            usage: TokenUsage::unreported(),
        });
    }
    let reported_model = response.model.map(ProviderReportedModel::new);
    if let Some(model) = &reported_model {
        sink.observe(Observation {
            correlation: correlation.clone(),
            fact: ObservationFact::ProviderModelReported(model.clone()),
        });
    }
    let usage = response
        .usage
        .as_ref()
        .map(convert_usage)
        .unwrap_or_default();
    let message_id = response.id.map(ProviderMessageId::new);
    if reported_model.is_none() || message_id.is_none() || response.usage.is_none() {
        // The documented completion envelope always carries id, model, and
        // usage; their absence means this is not valid completion material.
        return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::ResponseUnintelligible {
                detail: "success response is missing required completion fields \
                         (id, model, usage)"
                    .to_string(),
            },
            exchange,
            reported_model,
            finish_reported: None,
            usage,
        });
    }
    let mut content = Vec::new();
    for raw_block in response.content {
        let block = match parse_response_block(&raw_block) {
            Ok(block) => block,
            Err(error) => {
                return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                    cause: LossCause::ResponseUnintelligible {
                        detail: format!(
                            "success response carries a malformed content block: {error}"
                        ),
                    },
                    exchange,
                    reported_model,
                    finish_reported: None,
                    usage,
                });
            }
        };
        match convert_block(block) {
            Some(part) => {
                if let AssistantPart::ToolCall(proposal) = &part {
                    sink.observe(Observation {
                        correlation: correlation.clone(),
                        fact: ObservationFact::ToolCallProposed(proposal.clone()),
                    });
                }
                content.push(part);
            }
            None => {
                return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                    cause: LossCause::ResponseUnintelligible {
                        detail: "success response carries an unrecognized content-block type"
                            .to_string(),
                    },
                    exchange,
                    reported_model,
                    finish_reported: None,
                    usage,
                });
            }
        }
    }
    sink.observe(Observation {
        correlation: correlation.clone(),
        fact: ObservationFact::UsageReported(usage),
    });
    let Some(stop_reason) = response.stop_reason else {
        return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::ResponseUnintelligible {
                detail: "success response carries no stop_reason".to_string(),
            },
            exchange,
            reported_model,
            finish_reported: None,
            usage,
        });
    };
    let finish = map_finish(&stop_reason, response.stop_sequence);
    sink.observe(Observation {
        correlation: correlation.clone(),
        fact: ObservationFact::FinishReported(finish.clone()),
    });
    match finish.completion_finish() {
        None => TerminalEvidence::Refused(RefusalEvidence {
            exchange,
            message_id,
            reported_model,
            content,
            usage,
        }),
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

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use signalbox_expect_table::table;
    use signalbox_model_runtime::{
        AssistantPart, CompletionFinish, ExchangeFacts, FinishReason, LossCause, Observation,
        ObservationFact, ProviderMessageId, ProviderReportedModel, ProviderRequestId,
        TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal, ToolName,
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
        let (evidence, _) = decode(
            r#"{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [
                    {"type": "text", "text": "Oslo has"},
                    {"type": "thinking", "thinking": "checking", "signature": "sig_1"},
                    {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"city": "Oslo"}}
                ],
                "stop_reason": "tool_use",
                "stop_sequence": null,
                "usage": {"input_tokens": 12, "output_tokens": 34,
                          "cache_creation_input_tokens": 5, "cache_read_input_tokens": 6}
            }"#,
        );

        let TerminalEvidence::Completed(completion) = evidence else {
            panic!("a complete success message must decode as completion evidence");
        };
        assert_eq!(completion.exchange, exchange());
        assert_eq!(completion.message_id, Some(ProviderMessageId::new("msg_1")));
        assert_eq!(
            completion.reported_model,
            Some(ProviderReportedModel::new("model-exact-1"))
        );
        assert_eq!(completion.finish, CompletionFinish::ToolUse);
        assert_eq!(
            completion.content,
            vec![
                AssistantPart::Text("Oslo has".to_string()),
                AssistantPart::Thinking {
                    text: "checking".to_string(),
                    signature: Some("sig_1".to_string()),
                },
                AssistantPart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("toolu_1"),
                    name: ToolName::new("lookup"),
                    // The provider's raw slice verbatim — the fixture's
                    // interior space survives, proving no re-serialization.
                    arguments_json: r#"{"city": "Oslo"}"#.to_string(),
                }),
            ]
        );
        assert_eq!(
            completion.usage,
            TokenUsage {
                input_tokens: Some(12),
                output_tokens: Some(34),
                cache_creation_input_tokens: Some(5),
                cache_read_input_tokens: Some(6),
            }
        );
    }

    #[test]
    fn buffered_decode_emits_model_proposal_usage_then_finish() {
        let (_, observations) = decode(
            r#"{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [{"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}}],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 1, "output_tokens": 2}
            }"#,
        );

        assert_eq!(
            observations,
            vec![
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new(
                        "model-exact-1"
                    )),
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::ToolCallProposed(ToolCallProposal {
                        id: ToolCallId::new("toolu_1"),
                        name: ToolName::new("lookup"),
                        arguments_json: "{}".to_string(),
                    }),
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::UsageReported(TokenUsage {
                        input_tokens: Some(1),
                        output_tokens: Some(2),
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    }),
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::FinishReported(FinishReason::ToolUse),
                },
            ]
        );
    }

    #[test]
    fn refusal_stop_reason_is_refusal_evidence_not_completion() {
        let (evidence, _) = decode(
            r#"{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [{"type": "text", "text": "I cannot help with that."}],
                "stop_reason": "refusal",
                "usage": {"input_tokens": 9, "output_tokens": 8}
            }"#,
        );

        let TerminalEvidence::Refused(refusal) = evidence else {
            panic!("a refusal stop reason must decode as refusal evidence, never completion");
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
    fn missing_stop_reason_is_boundary_loss_with_retained_facts() {
        let (evidence, _) = decode(
            r#"{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [{"type": "text", "text": "partial"}],
                "usage": {"input_tokens": 3}
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a success body without stop_reason is not definitive completion material");
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
    fn unrecognized_content_block_type_is_boundary_loss_not_silent_drop() {
        let (evidence, _) = decode(
            r#"{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [{"type": "text", "text": "ok"},
                            {"type": "server_tool_use", "id": "srvtoolu_1"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("an unrecognized content-block type must surface as evidence, never drop");
        };
        assert!(matches!(
            loss.cause,
            LossCause::ResponseUnintelligible { .. }
        ));
        assert_eq!(
            loss.reported_model,
            Some(ProviderReportedModel::new("model-exact-1"))
        );
    }

    #[test]
    fn bare_envelope_without_required_fields_is_boundary_loss_not_completion() {
        let (evidence, _) =
            decode(r#"{"type": "message", "role": "assistant", "stop_reason": "end_turn"}"#);

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("an envelope missing id, model, and usage is not valid completion material");
        };
        assert!(matches!(
            loss.cause,
            LossCause::ResponseUnintelligible { .. }
        ));
    }

    #[test]
    fn envelope_without_discriminators_is_boundary_loss_not_completion() {
        let (evidence, _) = decode(
            r#"{"id": "msg_1", "model": "model-exact-1", "content": [],
                "stop_reason": "end_turn", "usage": {"input_tokens": 1}}"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("an envelope without message/assistant discriminators must not complete");
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

    /// Renders one mapping row per stop-reason token, in the given order,
    /// with the canonical reported stop sequence `"END"`.
    fn finish_rows(tokens: &[&'static str]) -> Vec<FinishRow> {
        tokens
            .iter()
            .map(|token| FinishRow {
                token,
                finish: format!("{:?}", map_finish(token, Some("END".to_string()))),
            })
            .collect()
    }

    #[test]
    fn every_documented_stop_reason_maps_and_unknown_is_retained_verbatim() {
        let rows = finish_rows(&[
            "end_turn",
            "max_tokens",
            "stop_sequence",
            "tool_use",
            "refusal",
            "pause_turn",
        ]);

        expect![[r#"
            ┌───────────────┬─────────────────────────────────────────────────┐
            │ token         │ finish                                          │
            ├───────────────┼─────────────────────────────────────────────────┤
            │ end_turn      │ EndTurn                                         │
            │ max_tokens    │ MaxOutputTokens                                 │
            │ stop_sequence │ StopSequence { sequence: Some(\"END\") }        │
            │ tool_use      │ ToolUse                                         │
            │ refusal       │ Refusal                                         │
            │ pause_turn    │ Unrecognized { provider_token: \"pause_turn\" } │
            └───────────────┴─────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table(rows));
    }
}
