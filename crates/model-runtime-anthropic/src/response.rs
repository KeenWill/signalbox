//! Buffered-response decoding and shared response-fact mapping.

use std::collections::BTreeSet;

use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CompletionEvidence, ExchangeFacts, FinishReason,
    LossCause, Observation, ObservationFact, ObservationSink, ProviderMessageId,
    ProviderReportedModel, RefusalEvidence, TerminalEvidence, TokenUsage, ToolCallId,
    ToolCallProposal, ToolCallsAtLoss, ToolName, validate_provider_json_nesting,
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
        "model_context_window_exceeded" => FinishReason::ContextWindowExceeded,
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
    fn aggregate(
        iterations: &[crate::wire::WireIterationUsage],
        field: impl Fn(&crate::wire::WireIterationUsage) -> Option<u64>,
    ) -> Option<u64> {
        iterations.iter().try_fold(0_u64, |total, iteration| {
            total.checked_add(field(iteration)?)
        })
    }
    fn aggregate_optional(
        iterations: &[crate::wire::WireIterationUsage],
        field: impl Fn(&crate::wire::WireIterationUsage) -> Option<u64>,
    ) -> Option<u64> {
        let mut reported = false;
        let total = iterations.iter().try_fold(0_u64, |total, iteration| {
            let value = field(iteration);
            reported |= value.is_some();
            total.checked_add(value.unwrap_or(0))
        })?;
        reported.then_some(total)
    }
    if let Some(iterations) = wire.iterations.as_deref().filter(|items| !items.is_empty()) {
        return TokenUsage {
            input_tokens: aggregate(iterations, |item| item.input_tokens),
            output_tokens: aggregate(iterations, |item| item.output_tokens),
            cache_creation_input_tokens: aggregate_optional(iterations, |item| {
                item.cache_creation_input_tokens
            }),
            cache_read_input_tokens: aggregate_optional(iterations, |item| {
                item.cache_read_input_tokens
            }),
        };
    }
    TokenUsage {
        input_tokens: wire.input_tokens,
        output_tokens: wire.output_tokens,
        cache_creation_input_tokens: wire.cache_creation_input_tokens,
        cache_read_input_tokens: wire.cache_read_input_tokens,
    }
}

/// Whether every provider iteration carries both required token axes and all
/// four aggregate axes fit the durable unsigned representation.
pub(crate) fn iteration_usage_is_complete(wire: &WireUsage) -> bool {
    let Some(iterations) = wire
        .iterations
        .as_deref()
        .filter(|iterations| !iterations.is_empty())
    else {
        return true;
    };
    let mut input = 0_u64;
    let mut output = 0_u64;
    let mut cache_creation = 0_u64;
    let mut cache_read = 0_u64;
    iterations.iter().all(|iteration| {
        let (Some(next_input), Some(next_output)) =
            (iteration.input_tokens, iteration.output_tokens)
        else {
            return false;
        };
        let Some(next_input) = input.checked_add(next_input) else {
            return false;
        };
        let Some(next_output) = output.checked_add(next_output) else {
            return false;
        };
        let Some(next_cache_creation) = iteration
            .cache_creation_input_tokens
            .map_or(Some(cache_creation), |value| {
                cache_creation.checked_add(value)
            })
        else {
            return false;
        };
        let Some(next_cache_read) = iteration
            .cache_read_input_tokens
            .map_or(Some(cache_read), |value| cache_read.checked_add(value))
        else {
            return false;
        };
        input = next_input;
        output = next_output;
        cache_creation = next_cache_creation;
        cache_read = next_cache_read;
        true
    })
}

/// A recognized response block converted to a neutral part, or the fact
/// that the block type is unrecognized.
pub(crate) fn convert_block(block: WireResponseBlock) -> Option<AssistantPart> {
    match block {
        WireResponseBlock::Text { text } => Some(AssistantPart::Text(text)),
        WireResponseBlock::ToolUse { id, name, input } => {
            if !crate::wire::raw_json_is_object(&input) {
                return None;
            }
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
        WireResponseBlock::Compaction { raw } => Some(AssistantPart::ProviderCompaction {
            block_json: raw.get().to_owned(),
        }),
        // A fallback marker is a routing fact, never assistant material; the
        // buffered decoder handles it before reaching this conversion.
        WireResponseBlock::Fallback { .. } | WireResponseBlock::Unrecognized => None,
    }
}

/// Whether a `tool_use` block had been reached in the content blocks the decode
/// classified before it stopped.
///
/// Sticky across the block loop: once a call has opened, no later block can
/// unopen it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolCallsOpened {
    /// A `tool_use` block was reached.
    Yes,
    /// No `tool_use` block was reached in the blocks classified so far.
    No,
}

/// Whether content blocks the loop never classified sit after the block that
/// stopped the decode.
///
/// This is the axis that separates a withheld answer from a stated negative: a
/// rejection on the final block leaves nothing unexamined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaterBlocks {
    /// Blocks after the stopping one were never classified.
    Unexamined,
    /// The stopping block was the last, so every block was classified.
    AllClassified,
}

/// The tool fact where the decode stopped on a block whose own content never
/// parsed.
///
/// The provider sends content blocks as opaque JSON values that this adapter
/// classifies one at a time, so a block that failed to parse could itself have
/// been a `tool_use` — its own material is unexamined whatever its position in
/// the list.
fn unparsed_block_tool_calls(opened: ToolCallsOpened) -> ToolCallsAtLoss {
    match opened {
        ToolCallsOpened::Yes => ToolCallsAtLoss::Opened,
        ToolCallsOpened::No => ToolCallsAtLoss::Unobserved,
    }
}

/// The tool fact where the decode stopped on a block it had already classified.
///
/// Rejecting a block the adapter understood — a fallback marker, an unsigned
/// thinking block, an unrecognized block type — leaves that block examined and
/// known not to be a tool call, so only the blocks the loop never reached can
/// withhold the answer. When the rejected block is the last one, nothing is
/// unexamined and the negative is a fact.
fn examined_block_tool_calls(opened: ToolCallsOpened, later: LaterBlocks) -> ToolCallsAtLoss {
    match (opened, later) {
        (ToolCallsOpened::Yes, LaterBlocks::Unexamined | LaterBlocks::AllClassified) => {
            ToolCallsAtLoss::Opened
        }
        (ToolCallsOpened::No, LaterBlocks::Unexamined) => ToolCallsAtLoss::Unobserved,
        (ToolCallsOpened::No, LaterBlocks::AllClassified) => ToolCallsAtLoss::NoneOpened,
    }
}

/// The tool fact for a buffered decode that classified every content block.
fn classified_tool_calls(opened: ToolCallsOpened) -> ToolCallsAtLoss {
    examined_block_tool_calls(opened, LaterBlocks::AllClassified)
}

/// The tool fact where the envelope decoded but its content was never walked.
///
/// An empty content list is a decoded fact: the provider sent no blocks, so no
/// tool call opened and the negative is established without classifying
/// anything. A non-empty list is opaque until the loop reaches it.
fn unwalked_content_tool_calls(content: &[Box<serde_json::value::RawValue>]) -> ToolCallsAtLoss {
    if content.is_empty() {
        ToolCallsAtLoss::NoneOpened
    } else {
        ToolCallsAtLoss::Unobserved
    }
}

/// Decodes a complete success-status response body into terminal evidence,
/// emitting the facts it learns as observations along the way.
///
/// A body that is not the documented completion material — unparseable,
/// missing the envelope's required fields (`type: "message"`,
/// `role: "assistant"`, `id`, `model`, `usage`), carrying an unrecognized
/// content-block type, or missing its stop reason — is boundary-loss
/// evidence (per `docs/spec/runtime-substrate.md`, a success status without
/// valid completion material is not definitive), with the facts observed
/// before the defect retained.
pub(crate) fn decode_buffered_response<C: Clone>(
    body: &[u8],
    exchange: ExchangeFacts,
    declared_stop_sequences: &[String],
    provider_compaction_enabled: bool,
    correlation: &C,
    sink: &mut (dyn ObservationSink<C> + Send),
) -> TerminalEvidence {
    if let Err(error) = validate_provider_json_nesting(body) {
        return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::ResponseUnintelligible {
                detail: format!("success response body exceeds the provider JSON bound: {error}"),
            },
            exchange,
            reported_model: None,
            finish_reported: None,
            tool_calls: ToolCallsAtLoss::Unobserved,
            usage: TokenUsage::unreported(),
        });
    }
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
                tool_calls: ToolCallsAtLoss::Unobserved,
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
            tool_calls: unwalked_content_tool_calls(&response.content),
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
    if reported_model.is_none()
        || message_id.is_none()
        || response.usage.is_none()
        || response.usage.as_ref().is_some_and(|usage| {
            usage.input_tokens.is_none()
                || usage.output_tokens.is_none()
                || !iteration_usage_is_complete(usage)
        })
    {
        // The documented completion envelope always carries id, model, and
        // usage; their absence means this is not valid completion material.
        return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::ResponseUnintelligible {
                detail: "success response is missing required completion fields \
                         (id, model, usage with input/output token counts)"
                    .to_string(),
            },
            exchange,
            reported_model,
            finish_reported: None,
            tool_calls: unwalked_content_tool_calls(&response.content),
            usage,
        });
    }
    let mut content = Vec::new();
    let mut tool_call_ids = BTreeSet::new();
    // Sticky, and deliberately not derived from `tool_call_ids`: that set holds
    // only the proposals that survived validation, but `convert_block` rejects a
    // `tool_use` block with a non-object `input` after its identity and name are
    // already decoded. Reading the set would report "none opened" for exactly
    // the malformed proposals this fact exists to distinguish.
    let mut opened_tool_calls = ToolCallsOpened::No;
    let block_count = response.content.len();
    for (block_index, raw_block) in response.content.into_iter().enumerate() {
        // Whether the loop still has blocks it has not classified. A rejection
        // on the final block leaves nothing unexamined, so the negative is a
        // fact rather than a gap.
        let later_blocks = if block_index + 1 < block_count {
            LaterBlocks::Unexamined
        } else {
            LaterBlocks::AllClassified
        };
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
                    tool_calls: unparsed_block_tool_calls(opened_tool_calls),
                    usage,
                });
            }
        };
        if matches!(block, WireResponseBlock::Compaction { .. }) && !provider_compaction_enabled {
            return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                cause: LossCause::ResponseUnintelligible {
                    detail: "success response carries a compaction block, but this operation did \
                             not enable provider compaction"
                        .to_string(),
                },
                exchange,
                reported_model,
                finish_reported: None,
                tool_calls: examined_block_tool_calls(opened_tool_calls, later_blocks),
                usage,
            });
        }
        if let WireResponseBlock::Fallback { to_model } = block {
            // This adapter never enables server-side fallback, so the marker
            // proves the response was served by a model other than the
            // resolved target. The substituting identity is surfaced as a
            // reported-model fact — the caller's provider-target rule
            // (docs/spec/model-call-execution.md) classifies it — and the
            // response itself is not completion material.
            if let Some(model) = to_model {
                sink.observe(Observation {
                    correlation: correlation.clone(),
                    fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new(model)),
                });
            }
            return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                cause: LossCause::ResponseUnintelligible {
                    detail: "success response carries a server-side fallback block, but this \
                             operation never enabled provider fallback"
                        .to_string(),
                },
                exchange,
                reported_model,
                finish_reported: None,
                tool_calls: examined_block_tool_calls(opened_tool_calls, later_blocks),
                usage,
            });
        }
        if matches!(block, WireResponseBlock::ToolUse { .. }) {
            // Set before the conversion below, which can reject this block for a
            // non-object `input` and return `None` indistinguishably from an
            // unrecognized block type. The call demonstrably opened either way.
            opened_tool_calls = ToolCallsOpened::Yes;
        }
        match convert_block(block) {
            Some(part)
                if matches!(
                    &part,
                    AssistantPart::Thinking { signature, .. }
                        if signature.as_deref().is_none_or(str::is_empty)
                ) =>
            {
                // The provider requires the integrity signature for any
                // replay; completion material without it is not usable.
                return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                    cause: LossCause::ResponseUnintelligible {
                        detail: "success response carries a thinking block without its \
                                 integrity signature"
                            .to_string(),
                    },
                    exchange,
                    reported_model,
                    finish_reported: None,
                    tool_calls: examined_block_tool_calls(opened_tool_calls, later_blocks),
                    usage,
                });
            }
            Some(part) => {
                if let AssistantPart::ToolCall(proposal) = &part {
                    if !tool_call_ids.insert(proposal.id.as_str().to_string()) {
                        return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                            cause: LossCause::ResponseUnintelligible {
                                detail: format!(
                                    "success response repeats tool-call identifier {:?}",
                                    proposal.id.as_str()
                                ),
                            },
                            exchange,
                            reported_model,
                            finish_reported: None,
                            tool_calls: examined_block_tool_calls(opened_tool_calls, later_blocks),
                            usage,
                        });
                    }
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
                    tool_calls: examined_block_tool_calls(opened_tool_calls, later_blocks),
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
            tool_calls: classified_tool_calls(opened_tool_calls),
            usage,
        });
    };
    if (stop_reason == "stop_sequence") != response.stop_sequence.is_some() {
        return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::ResponseUnintelligible {
                detail: "success response stop_reason contradicts its stop_sequence metadata"
                    .to_string(),
            },
            exchange,
            reported_model,
            finish_reported: None,
            tool_calls: classified_tool_calls(opened_tool_calls),
            usage,
        });
    }
    if let Some(sequence) = response.stop_sequence.as_deref()
        && !declared_stop_sequences
            .iter()
            .any(|declared| declared == sequence)
    {
        return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::ResponseUnintelligible {
                detail: "success response reports a stop sequence not declared by the request"
                    .to_string(),
            },
            exchange,
            reported_model,
            finish_reported: None,
            tool_calls: classified_tool_calls(opened_tool_calls),
            usage,
        });
    }
    let finish = map_finish(&stop_reason, response.stop_sequence);
    if matches!(finish, FinishReason::Unrecognized { .. }) {
        return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::ResponseUnintelligible {
                detail: "success response carries an unrecognized stop_reason".to_string(),
            },
            exchange,
            reported_model,
            finish_reported: Some(finish),
            tool_calls: classified_tool_calls(opened_tool_calls),
            usage,
        });
    }
    sink.observe(Observation {
        correlation: correlation.clone(),
        fact: ObservationFact::FinishReported(finish.clone()),
    });
    let has_tool_calls = content
        .iter()
        .any(|part| matches!(part, AssistantPart::ToolCall(_)));
    if matches!(finish, FinishReason::ToolUse) && !has_tool_calls {
        return TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::ResponseUnintelligible {
                detail: "success response content contradicts its stop_reason".to_string(),
            },
            exchange,
            reported_model,
            finish_reported: Some(finish),
            tool_calls: classified_tool_calls(opened_tool_calls),
            usage,
        });
    }
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
        ObservationFact, PROVIDER_JSON_NESTING_LIMIT, ProviderMessageId, ProviderReportedModel,
        ProviderRequestId, TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal,
        ToolCallsAtLoss, ToolName,
    };

    use super::{decode_buffered_response, map_finish};

    fn exchange() -> ExchangeFacts {
        ExchangeFacts {
            provider_request_id: Some(ProviderRequestId::new("req_1")),
            http_status: Some(200),
            retry_after: None,
        }
    }

    /// The provider identities the decode reported, in observation order.
    fn reported_models(observations: &[Observation<String>]) -> Vec<&str> {
        observations
            .iter()
            .filter_map(|observation| match &observation.fact {
                ObservationFact::ProviderModelReported(reported) => Some(reported.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Decodes the body against canonical exchange facts, collecting
    /// observations correlated to `"call-1"`.
    fn decode(body: &str) -> (TerminalEvidence, Vec<Observation<String>>) {
        decode_with_provider_compaction(body, true)
    }

    fn decode_with_provider_compaction(
        body: &str,
        provider_compaction_enabled: bool,
    ) -> (TerminalEvidence, Vec<Observation<String>>) {
        let mut observations: Vec<Observation<String>> = Vec::new();
        let evidence = decode_buffered_response(
            body.as_bytes(),
            exchange(),
            &["END".to_string()],
            provider_compaction_enabled,
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
    fn buffered_compaction_block_is_verbatim_and_iteration_usage_is_aggregated() {
        let raw_block =
            r#"{"type":"compaction", "content":"summary", "encrypted_content":"opaque=="}"#;
        let body = format!(
            r#"{{
                "id":"msg_compact","type":"message","role":"assistant","model":"model-exact-1",
                "content":[{raw_block},{{"type":"text","text":"done"}}],
                "stop_reason":"end_turn","usage":{{
                    "input_tokens":3,"output_tokens":4,
                    "iterations":[
                        {{"input_tokens":10,"output_tokens":2,"cache_creation_input_tokens":1,"cache_read_input_tokens":3}},
                        {{"input_tokens":5,"output_tokens":4,"cache_creation_input_tokens":0,"cache_read_input_tokens":2}}
                    ]
                }}
            }}"#
        );
        let (evidence, _) = decode(&body);
        let TerminalEvidence::Completed(completion) = evidence else {
            panic!("compaction response must be completion evidence");
        };
        assert_eq!(
            completion.content,
            vec![
                AssistantPart::ProviderCompaction {
                    block_json: raw_block.to_string(),
                },
                AssistantPart::Text("done".to_string()),
            ]
        );
        assert_eq!(
            completion.usage,
            TokenUsage {
                input_tokens: Some(15),
                output_tokens: Some(6),
                cache_creation_input_tokens: Some(1),
                cache_read_input_tokens: Some(5),
            }
        );
    }

    #[test]
    fn compaction_block_is_rejected_when_the_request_disabled_it() {
        let body = r#"{
            "id":"msg_compact","type":"message","role":"assistant","model":"model-exact-1",
            "content":[{"type":"compaction","content":"summary"}],
            "stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":4}
        }"#;

        assert!(matches!(
            decode_with_provider_compaction(body, false).0,
            TerminalEvidence::BoundaryLoss(_)
        ));
    }

    #[test]
    fn iteration_usage_preserves_unreported_cache_axes() {
        let body = r#"{
            "id":"msg_usage","type":"message","role":"assistant","model":"model-exact-1",
            "content":[{"type":"text","text":"done"}],
            "stop_reason":"end_turn","usage":{
                "input_tokens":15,"output_tokens":6,
                "iterations":[
                    {"input_tokens":10,"output_tokens":2},
                    {"input_tokens":5,"output_tokens":4,"cache_read_input_tokens":3}
                ]
            }
        }"#;
        let TerminalEvidence::Completed(completion) = decode(body).0 else {
            panic!("complete response must remain completion evidence");
        };

        assert_eq!(completion.usage.cache_creation_input_tokens, None);
        assert_eq!(completion.usage.cache_read_input_tokens, Some(3));
    }

    #[test]
    fn incomplete_required_iteration_usage_is_boundary_loss() {
        let body = r#"{
            "id":"msg_usage","type":"message","role":"assistant","model":"model-exact-1",
            "content":[{"type":"text","text":"done"}],
            "stop_reason":"end_turn","usage":{
                "input_tokens":15,"output_tokens":6,
                "iterations":[{"input_tokens":10,"output_tokens":2},{"input_tokens":5}]
            }
        }"#;

        assert!(matches!(decode(body).0, TerminalEvidence::BoundaryLoss(_)));
    }

    #[test]
    fn overflowing_iteration_usage_is_boundary_loss() {
        let body = format!(
            r#"{{
                "id":"msg_usage","type":"message","role":"assistant","model":"model-exact-1",
                "content":[{{"type":"text","text":"done"}}],
                "stop_reason":"end_turn","usage":{{
                    "input_tokens":1,"output_tokens":1,
                    "iterations":[
                        {{"input_tokens":{},"output_tokens":1}},
                        {{"input_tokens":1,"output_tokens":1}}
                    ]
                }}
            }}"#,
            u64::MAX
        );

        assert!(matches!(decode(&body).0, TerminalEvidence::BoundaryLoss(_)));
    }

    #[test]
    fn malformed_buffered_compaction_blocks_are_boundary_loss() {
        for block in [
            r#"{"type":"compaction","encrypted_content":"opaque"}"#,
            r#"{"type":"compaction","content":""}"#,
            r#"{"type":"compaction","content":null,"encrypted_content":1}"#,
        ] {
            let body = format!(
                r#"{{
                    "id":"msg_compact","type":"message","role":"assistant","model":"model-exact-1",
                    "content":[{block}],"stop_reason":"end_turn",
                    "usage":{{"input_tokens":1,"output_tokens":1}}
                }}"#
            );
            assert!(matches!(decode(&body).0, TerminalEvidence::BoundaryLoss(_)));
        }
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
                "usage": {"input_tokens": 3, "output_tokens": 2}
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
        assert_eq!(loss.usage.output_tokens, Some(2));
    }

    #[test]
    fn missing_output_tokens_is_boundary_loss_with_retained_input_usage() {
        let (evidence, _) = decode(
            r#"{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [{"type": "text", "text": "partial"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3}
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a success body without output_tokens is not completion material");
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
        assert_eq!(loss.usage.output_tokens, None);
    }

    /// a decode that stopped part way through the content blocks does
    /// not report "no tool call opened".
    ///
    /// The provider sends content blocks as opaque values this adapter
    /// classifies one at a time. A block it cannot classify ends the decode
    /// with the following blocks unexamined, so their tool material is
    /// unobserved — reporting it as absent would be a claim the decode never
    /// established.
    #[test]
    fn a_decode_abandoned_mid_content_withholds_the_tool_fact() {
        let (evidence, _) = decode(
            r#"{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [
                    {"type": "quasar"},
                    {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 3, "output_tokens": 1}
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("an unclassifiable content block is not definitive completion material");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
    }

    /// A `tool_use` block whose `input` is not an object is rejected by
    /// `convert_block` *after* its identity and name are decoded, and never
    /// reaches `tool_call_ids`. Reading the surviving-proposal set would report
    /// the malformed proposal as no tool call at all — the exact distinction
    /// this fact exists to carry.
    #[test]
    fn a_tool_call_rejected_for_a_non_object_input_still_reports_as_opened() {
        let (evidence, observations) = decode(
            r#"{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": "not-an-object"}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 3, "output_tokens": 1}
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a tool_use block with a non-object input is not completion material");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Opened);
        // The rejected proposal reaches no observation, so the evidence field is
        // the only channel carrying the fact that a call opened.
        assert!(
            !observations.iter().any(|observation| matches!(
                observation.fact,
                ObservationFact::ToolCallProposed(_)
            )),
            "a rejected proposal is never emitted as an observation"
        );
    }

    /// Decodes a body whose only content block is `content`, asserting the tool
    /// fact its rejection carries.
    ///
    /// Plumbing only: the body shape is irrelevant to what each case states, and
    /// the block under test stays at its own call site.
    #[track_caller]
    fn assert_sole_block_tool_fact(content: &str, expected: ToolCallsAtLoss) {
        let (evidence, _) = decode(&format!(
            r#"{{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [{content}],
                "stop_reason": "end_turn",
                "usage": {{"input_tokens": 3, "output_tokens": 1}}
            }}"#
        ));

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a rejected content block is not definitive completion material");
        };
        assert_eq!(loss.tool_calls, expected);
    }

    /// An envelope that decoded with no content blocks establishes the negative
    /// without walking anything: the provider sent nothing that could open a
    /// call.
    #[test]
    fn an_empty_decoded_content_list_states_the_negative() {
        let (evidence, _) = decode(
            r#"{
                "id": "msg_1",
                "type": "quasar",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 1}
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a bad envelope discriminator is not completion material");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::NoneOpened);
    }

    /// The same exit with blocks present withholds: they are opaque until the
    /// loop reaches them, and it never runs.
    #[test]
    fn an_unwalked_non_empty_content_list_withholds() {
        let (evidence, _) = decode(
            r#"{
                "id": "msg_1",
                "type": "quasar",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [{"type": "text", "text": "hi"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 1}
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a bad envelope discriminator is not completion material");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
    }

    /// A sole fallback marker is classified and known not to be a tool call,
    /// and nothing follows it, so the negative is a fact.
    #[test]
    fn a_sole_fallback_block_states_the_negative() {
        assert_sole_block_tool_fact(
            r#"{"type": "fallback", "to_model": "other-model"}"#,
            ToolCallsAtLoss::NoneOpened,
        );
    }

    /// The same for a sole thinking block rejected for its missing signature.
    #[test]
    fn a_sole_unsigned_thinking_block_states_the_negative() {
        assert_sole_block_tool_fact(
            r#"{"type": "thinking", "thinking": "hm", "signature": ""}"#,
            ToolCallsAtLoss::NoneOpened,
        );
    }

    /// An unrecognized block type is still classified — serde matching no known
    /// variant proves it is not `tool_use` — so it too states the negative.
    #[test]
    fn a_sole_unrecognized_block_states_the_negative() {
        assert_sole_block_tool_fact(r#"{"type": "quasar"}"#, ToolCallsAtLoss::NoneOpened);
    }

    /// A block whose own bytes never parsed withholds even as the sole block:
    /// its content is unexamined whatever its position.
    #[test]
    fn a_sole_unparsed_block_withholds() {
        assert_sole_block_tool_fact(r#"{"type": 7}"#, ToolCallsAtLoss::Unobserved);
    }

    /// The same rejections with a block still behind them withhold, which is
    /// what makes the test above a statement about position rather than cause.
    #[test]
    fn the_same_rejection_with_a_block_behind_it_withholds() {
        let (evidence, _) = decode(
            r#"{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [
                    {"type": "fallback", "to_model": "other-model"},
                    {"type": "text", "text": "hi"}
                ],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 1}
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a fallback block is not definitive completion material");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
    }

    /// A tool call already classified is a fact the same abandoned decode can
    /// state, so the withholding above is not a blanket refusal to answer.
    #[test]
    fn a_decode_abandoned_after_a_tool_call_still_reports_it() {
        let (evidence, _) = decode(
            r#"{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}},
                    {"type": "quasar"}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 3, "output_tokens": 1}
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("an unclassifiable content block is not definitive completion material");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Opened);
    }

    /// Decodes a response whose sole content block is `content`, under a stop
    /// reason that is not definitive completion material, and asserts the tool
    /// fact its boundary loss carries.
    #[track_caller]
    fn assert_classified_tool_fact(content: &str, expected: ToolCallsAtLoss) {
        let (evidence, _) = decode(&format!(
            r#"{{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [{content}],
                "stop_reason": "quasar",
                "usage": {{"input_tokens": 3, "output_tokens": 1}}
            }}"#
        ));

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("an unrecognized stop_reason is not definitive completion material");
        };
        assert_eq!(loss.tool_calls, expected);
    }

    /// Every block was classified and none was a tool call, so the decode
    /// examined the material the question is about and states the negative.
    #[test]
    fn a_fully_classified_decode_without_a_tool_call_states_the_negative() {
        assert_classified_tool_fact(
            r#"{"type": "text", "text": "hi"}"#,
            ToolCallsAtLoss::NoneOpened,
        );
    }

    /// The same decode carrying a `tool_use` block reports it, so a caller reads
    /// one vocabulary from a complete and a partial decode.
    #[test]
    fn a_fully_classified_decode_with_a_tool_call_reports_it() {
        assert_classified_tool_fact(
            r#"{"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}}"#,
            ToolCallsAtLoss::Opened,
        );
    }

    #[test]
    fn stop_sequence_reason_without_sequence_is_boundary_loss() {
        let body = r#"{
            "id":"msg_1","type":"message","role":"assistant",
            "model":"model-exact-1","content":[],
            "stop_reason":"stop_sequence","stop_sequence":null,
            "usage":{"input_tokens":1,"output_tokens":1}
        }"#;

        assert!(matches!(decode(body).0, TerminalEvidence::BoundaryLoss(_)));
    }

    #[test]
    fn sequence_metadata_with_a_different_reason_is_boundary_loss() {
        let body = r#"{
            "id":"msg_1","type":"message","role":"assistant",
            "model":"model-exact-1","content":[],
            "stop_reason":"end_turn","stop_sequence":"END",
            "usage":{"input_tokens":1,"output_tokens":1}
        }"#;

        assert!(matches!(decode(body).0, TerminalEvidence::BoundaryLoss(_)));
    }

    #[test]
    fn undeclared_stop_sequence_is_boundary_loss() {
        let (evidence, _) = decode(
            r#"{"id":"msg_1","type":"message","role":"assistant",
                "model":"model-exact-1","content":[],
                "stop_reason":"stop_sequence","stop_sequence":"OTHER",
                "usage":{"input_tokens":1,"output_tokens":1}}"#,
        );

        assert!(matches!(evidence, TerminalEvidence::BoundaryLoss(_)));
    }

    #[test]
    fn empty_thinking_signature_is_boundary_loss() {
        let (evidence, _) = decode(
            r#"{"id":"msg_1","type":"message","role":"assistant",
                "model":"model-exact-1",
                "content":[{"type":"thinking","thinking":"step","signature":""}],
                "stop_reason":"end_turn","stop_sequence":null,
                "usage":{"input_tokens":1,"output_tokens":1}}"#,
        );

        assert!(matches!(evidence, TerminalEvidence::BoundaryLoss(_)));
    }

    #[test]
    fn non_object_tool_arguments_are_boundary_loss() {
        let (evidence, _) = decode(
            r#"{"id":"msg_1","type":"message","role":"assistant",
                "model":"model-exact-1",
                "content":[{"type":"tool_use","id":"toolu_1","name":"lookup","input":[]}],
                "stop_reason":"tool_use","stop_sequence":null,
                "usage":{"input_tokens":1,"output_tokens":1}}"#,
        );

        assert!(matches!(evidence, TerminalEvidence::BoundaryLoss(_)));
    }

    /// S20: the provider's server-side fallback marker is the distinct
    /// substitution signal. This adapter never enables fallback, so the
    /// response is not completion material, and the substituting identity is
    /// surfaced as a reported-model fact for the caller's provider-target
    /// rule rather than being lost in a generic unknown-block failure.
    #[test]
    fn s20_server_side_fallback_block_reports_the_substituting_model() {
        let (evidence, observations) = decode(
            r#"{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [
                    {"type": "fallback",
                     "from": {"model": "model-exact-1"},
                     "to": {"model": "substitute-model-2"}},
                    {"type": "text", "text": "served by the other model"}
                ],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }"#,
        );

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a fallback-served response is not the resolved target's completion material");
        };
        let LossCause::ResponseUnintelligible { detail } = loss.cause else {
            panic!("a fallback marker is response-unintelligible evidence");
        };
        assert!(detail.contains("server-side fallback block"));
        assert_eq!(
            reported_models(&observations),
            vec!["model-exact-1", "substitute-model-2"],
            "both the envelope identity and the substituting identity reach the caller"
        );
    }

    /// A fallback marker without a named continuing model still refuses to
    /// complete; nothing is fabricated about which model served.
    #[test]
    fn s20_fallback_block_without_a_named_model_still_refuses_to_complete() {
        let (evidence, observations) = decode(
            r#"{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-exact-1",
                "content": [{"type": "fallback"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }"#,
        );

        assert!(matches!(evidence, TerminalEvidence::BoundaryLoss(_)));
        assert_eq!(
            reported_models(&observations),
            vec!["model-exact-1"],
            "only the envelope identity is reported"
        );
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
    fn envelope_without_content_is_boundary_loss_not_empty_completion() {
        let (evidence, _) = decode(
            r#"{"id":"msg_1","type":"message","role":"assistant",
                "model":"model-exact-1","stop_reason":"end_turn",
                "usage":{"input_tokens":1,"output_tokens":1}}"#,
        );

        assert!(matches!(evidence, TerminalEvidence::BoundaryLoss(_)));
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

    #[test]
    fn overdeep_raw_content_material_is_response_unintelligible() {
        let nested = format!(
            "{}null{}",
            "[".repeat(PROVIDER_JSON_NESTING_LIMIT + 1),
            "]".repeat(PROVIDER_JSON_NESTING_LIMIT + 1)
        );
        let body = format!(
            r#"{{
                "id":"msg_1","type":"message","role":"assistant",
                "model":"model-exact-1",
                "content":[{{"type":"text","text":"ok","future":{nested}}}],
                "stop_reason":"end_turn",
                "usage":{{"input_tokens":1,"output_tokens":1}}
            }}"#
        );

        let TerminalEvidence::BoundaryLoss(loss) = decode(&body).0 else {
            panic!("overdeep RawValue content must be rejected before typed parsing");
        };
        let LossCause::ResponseUnintelligible { detail } = loss.cause else {
            panic!("deep success JSON must be response-unintelligible evidence");
        };
        let expected = format!("{PROVIDER_JSON_NESTING_LIMIT}-container nesting limit");
        assert!(detail.contains(&expected));
    }

    #[test]
    fn shallow_additive_fields_remain_tolerated() {
        let (evidence, _) = decode(
            r#"{
                "id":"msg_1","type":"message","role":"assistant",
                "model":"model-exact-1","future_envelope":{"enabled":true},
                "content":[{"type":"text","text":"ok","future_block":[1,2]}],
                "stop_reason":"end_turn",
                "usage":{"input_tokens":1,"output_tokens":1,"future_usage":"kept-compatible"}
            }"#,
        );

        assert!(matches!(evidence, TerminalEvidence::Completed(_)));
    }

    #[test]
    fn tool_use_stop_without_a_tool_call_is_boundary_loss() {
        let (evidence, _) = decode(
            r#"{"id":"msg_1","type":"message","role":"assistant",
                "model":"model-exact-1","content":[{"type":"text","text":"partial"}],
                "stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":1}}"#,
        );

        assert!(matches!(evidence, TerminalEvidence::BoundaryLoss(_)));
    }

    #[test]
    fn duplicate_tool_call_ids_are_boundary_loss() {
        let (evidence, observations) = decode(
            r#"{"id":"msg_1","type":"message","role":"assistant",
                "model":"model-exact-1","content":[
                {"type":"tool_use","id":"toolu_1","name":"first","input":{}},
                {"type":"tool_use","id":"toolu_1","name":"second","input":{}}],
                "stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":1}}"#,
        );

        assert!(matches!(evidence, TerminalEvidence::BoundaryLoss(_)));
        assert_eq!(
            observations
                .iter()
                .filter(|observation| {
                    matches!(observation.fact, ObservationFact::ToolCallProposed(_))
                })
                .count(),
            1,
            "the duplicate proposal is rejected before observation"
        );
    }

    #[test]
    fn max_token_completion_retains_a_partial_tool_call() {
        let (evidence, _) = decode(
            r#"{"id":"msg_1","type":"message","role":"assistant",
                "model":"model-exact-1","content":[{"type":"tool_use",
                "id":"toolu_1","name":"lookup","input":{}}],
                "stop_reason":"max_tokens",
                "usage":{"input_tokens":1,"output_tokens":1}}"#,
        );

        let TerminalEvidence::Completed(completion) = evidence else {
            panic!("token exhaustion with partial tool material is definitive completion");
        };
        assert_eq!(completion.finish, CompletionFinish::MaxOutputTokens);
        assert!(matches!(
            completion.content.as_slice(),
            [AssistantPart::ToolCall(_)]
        ));
    }

    #[test]
    fn context_window_stop_is_definitive_completion() {
        let (evidence, _) = decode(
            r#"{"id":"msg_1","type":"message","role":"assistant",
                "model":"model-exact-1","content":[{"type":"text","text":"complete"}],
                "stop_reason":"model_context_window_exceeded",
                "usage":{"input_tokens":1,"output_tokens":1}}"#,
        );

        let TerminalEvidence::Completed(completion) = evidence else {
            panic!("the documented context-window stop is definitive completion");
        };
        assert_eq!(completion.finish, CompletionFinish::ContextWindowExceeded);
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
            "model_context_window_exceeded",
            "stop_sequence",
            "tool_use",
            "refusal",
            "pause_turn",
        ]);

        expect![[r#"
            ┌───────────────────────────────┬─────────────────────────────────────────────────┐
            │ token                         │ finish                                          │
            ├───────────────────────────────┼─────────────────────────────────────────────────┤
            │ end_turn                      │ EndTurn                                         │
            │ max_tokens                    │ MaxOutputTokens                                 │
            │ model_context_window_exceeded │ ContextWindowExceeded                           │
            │ stop_sequence                 │ StopSequence { sequence: Some(\"END\") }        │
            │ tool_use                      │ ToolUse                                         │
            │ refusal                       │ Refusal                                         │
            │ pause_turn                    │ Unrecognized { provider_token: \"pause_turn\" } │
            └───────────────────────────────┴─────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table(rows));
    }
}
