//! Chat Completions stream decoding with terminal-integrity evidence.
//!
//! The stream's terminal marker is the literal `[DONE]` data record. The
//! decoder accumulates content, refusal, and per-index tool-call fragments,
//! and only a `[DONE]` preceded by a reported finish reason yields terminal
//! success or refusal evidence. A stream that ends any other way is explicit
//! incomplete-stream or protocol-violation evidence with the partial facts
//! retained — never silent success (ADR-0043's ambiguous branch).
//!
//! Because `stream_options.include_usage` is always requested (see the
//! request translation), a conforming stream reports usage before `[DONE]`;
//! a usage-only chunk carries empty `choices` and is absorbed as a usage
//! observation.

use std::collections::BTreeMap;

use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CompletionEvidence, ExchangeFacts, FinishReason,
    LossCause, Observation, ObservationFact, ObservationSink, ProviderErrorEvidence,
    ProviderMessageId, ProviderReportedModel, RefusalEvidence, SseRecord, StreamInterruption,
    TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal, ToolName,
};

use crate::response::{convert_usage, map_finish};
use crate::status::classify_error;
use crate::wire::ChatChunk;

/// The decoder's verdict on one record.
pub(crate) enum StreamStep {
    /// Keep reading.
    Continue,
    /// The stream reached typed terminal evidence; stop reading.
    Terminal(TerminalEvidence),
}

#[derive(Default)]
struct ToolBuilder {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// Incremental decoder for one chat-completion stream.
pub(crate) struct StreamDecoder {
    exchange: ExchangeFacts,
    message_id: Option<ProviderMessageId>,
    reported_model: Option<ProviderReportedModel>,
    usage: TokenUsage,
    finish: Option<FinishReason>,
    content_text: String,
    refusal_text: String,
    tool_builders: BTreeMap<u32, ToolBuilder>,
    completed_tools: Vec<ToolCallProposal>,
}

impl StreamDecoder {
    pub(crate) fn new(exchange: ExchangeFacts) -> Self {
        Self {
            exchange,
            message_id: None,
            reported_model: None,
            usage: TokenUsage::unreported(),
            finish: None,
            content_text: String::new(),
            refusal_text: String::new(),
            tool_builders: BTreeMap::new(),
            completed_tools: Vec::new(),
        }
    }

    /// Applies one framed record.
    pub(crate) fn apply<C: Clone>(
        &mut self,
        record: &SseRecord,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> StreamStep {
        if record.data.trim() == "[DONE]" {
            return self.apply_done();
        }
        let chunk: ChatChunk = match serde_json::from_str(&record.data) {
            Ok(chunk) => chunk,
            Err(error) => {
                return self.violation(format!("malformed stream chunk payload: {error}"));
            }
        };
        if let Some(error) = chunk.error {
            // A mid-stream error record is a definitive provider error;
            // with no HTTP status of its own it classifies by native code.
            let kind = classify_error(0, error.code_text().as_deref());
            return StreamStep::Terminal(TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: self.exchange.clone(),
                kind,
                native: error.into_native_facts(),
            }));
        }
        if self.message_id.is_none() {
            self.message_id = chunk.id.map(ProviderMessageId::new);
        }
        if let Some(model) = chunk.model {
            let model = ProviderReportedModel::new(model);
            match &self.reported_model {
                None => {
                    self.reported_model = Some(model.clone());
                    Self::emit(
                        correlation,
                        sink,
                        ObservationFact::ProviderModelReported(model),
                    );
                }
                Some(existing) if *existing != model => {
                    // A spliced or corrupted stream reporting a second
                    // identity must not complete under the first one
                    // (ADR-0005's mismatch evidence relies on this fact).
                    return self.violation("stream chunks report conflicting model identities");
                }
                Some(_) => {}
            }
        }
        if let Some(usage) = chunk.usage.as_ref() {
            let usage = convert_usage(usage);
            self.usage.absorb(usage);
            Self::emit(correlation, sink, ObservationFact::UsageReported(usage));
        }
        for choice in chunk.choices {
            if self.finish.is_some() {
                // After the finish reason, the only valid remaining records
                // are the usage-only chunk (empty choices) and [DONE];
                // further choice material could alter the completion after
                // FinishReported was emitted.
                return self.violation("choice material after the reported finish_reason");
            }
            if choice.index != 0 {
                return self.violation(format!(
                    "stream chunk carries choice index {}; exactly one choice is requested",
                    choice.index
                ));
            }
            if let Some(delta) = choice.delta {
                if let Some(text) = delta.content
                    && !text.is_empty()
                {
                    if !self.tool_builders.is_empty() {
                        // The protocol streams content before tool calls;
                        // content arriving afterwards would shift the part
                        // positions already reported on tool fragments.
                        return self.violation("content delta after tool-call fragments began");
                    }
                    self.content_text.push_str(&text);
                    Self::emit(
                        correlation,
                        sink,
                        ObservationFact::TextDelta { index: 0, text },
                    );
                }
                if let Some(refusal) = delta.refusal {
                    self.refusal_text.push_str(&refusal);
                }
                for call in delta.tool_calls {
                    if let Some(kind) = &call.kind
                        && kind != "function"
                    {
                        // The buffered decoder rejects non-function tool
                        // material; the streamed path must not assemble it
                        // into an ordinary proposal either.
                        return self.violation(format!(
                            "tool call at index {} carries unrecognized type {kind:?}",
                            call.index
                        ));
                    }
                    let builder = self.tool_builders.entry(call.index).or_default();
                    if let Some(id) = call.id {
                        match &builder.id {
                            None => builder.id = Some(id),
                            Some(existing) if *existing != id => {
                                return self.violation(format!(
                                    "tool call at index {} reports conflicting ids",
                                    call.index
                                ));
                            }
                            Some(_) => {}
                        }
                    }
                    if let Some(function) = call.function {
                        if let Some(name) = function.name {
                            match &builder.name {
                                None => builder.name = Some(name),
                                Some(existing) if *existing != name => {
                                    return self.violation(format!(
                                        "tool call at index {} reports conflicting names",
                                        call.index
                                    ));
                                }
                                Some(_) => {}
                            }
                        }
                        if let Some(fragment) = function.arguments
                            && !fragment.is_empty()
                        {
                            let builder = self.tool_builders.entry(call.index).or_default();
                            builder.arguments.push_str(&fragment);
                            let text_parts = u32::from(!self.content_text.is_empty());
                            Self::emit(
                                correlation,
                                sink,
                                ObservationFact::ToolArgumentsDelta {
                                    // Part order: the text part (when one
                                    // exists) at 0, then tool call k. Stable
                                    // because content cannot arrive after
                                    // tool fragments (violation above).
                                    index: text_parts + call.index,
                                    fragment,
                                },
                            );
                        }
                    }
                }
            }
            if let Some(token) = choice.finish_reason {
                let mut finish = map_finish(&token);
                if !self.refusal_text.is_empty() {
                    // Accumulated refusal material is the provider's refusal
                    // outcome; the observation must match the terminal
                    // evidence (the buffered path normalizes identically).
                    finish = FinishReason::Refusal;
                }
                // The choice is complete here, so its proposals are final:
                // emit them before announcing the finish, in index order.
                if let Some(step) = self.finalize_tools(correlation, sink) {
                    return step;
                }
                self.finish = Some(finish.clone());
                Self::emit(correlation, sink, ObservationFact::FinishReported(finish));
            }
        }
        StreamStep::Continue
    }

    /// Evidence for a stream that ended without `[DONE]`.
    pub(crate) fn lost(self, interruption: StreamInterruption) -> TerminalEvidence {
        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::StreamEndedWithoutTerminalMarker { interruption },
            exchange: self.exchange,
            reported_model: self.reported_model,
            finish_reported: self.finish,
            usage: self.usage,
        })
    }

    /// Evidence for a caller cancellation observed mid-stream.
    pub(crate) fn cancelled(self) -> TerminalEvidence {
        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::CancellationRequested,
            exchange: self.exchange,
            reported_model: self.reported_model,
            finish_reported: self.finish,
            usage: self.usage,
        })
    }

    /// Protocol-violation evidence retaining the facts observed so far.
    pub(crate) fn violation_evidence(&self, detail: impl Into<String>) -> TerminalEvidence {
        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::StreamProtocolViolation {
                detail: detail.into(),
            },
            exchange: self.exchange.clone(),
            reported_model: self.reported_model.clone(),
            finish_reported: self.finish.clone(),
            usage: self.usage,
        })
    }

    fn violation(&self, detail: impl Into<String>) -> StreamStep {
        StreamStep::Terminal(self.violation_evidence(detail))
    }

    fn emit<C: Clone>(
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
        fact: ObservationFact,
    ) {
        sink.observe(Observation {
            correlation: correlation.clone(),
            fact,
        });
    }

    /// Finalizes accumulated tool builders into proposals when the choice
    /// closes, emitting each in index order.
    ///
    /// The provider's raw argument bytes are preserved exactly — empty or
    /// even malformed accumulations are the provider's own value, exposed
    /// verbatim for typed decoding to judge (`decode_tool_arguments` owns
    /// the JsonSyntax classification).
    fn finalize_tools<C: Clone>(
        &mut self,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> Option<StreamStep> {
        let builders = std::mem::take(&mut self.tool_builders);
        for (index, builder) in builders {
            let (Some(id), Some(name)) = (builder.id, builder.name) else {
                return Some(self.violation(format!(
                    "tool call at index {index} terminated without an id and name"
                )));
            };
            let proposal = ToolCallProposal {
                id: ToolCallId::new(id),
                name: ToolName::new(name),
                arguments_json: builder.arguments,
            };
            Self::emit(
                correlation,
                sink,
                ObservationFact::ToolCallProposed(proposal.clone()),
            );
            self.completed_tools.push(proposal);
        }
        None
    }

    fn apply_done(&mut self) -> StreamStep {
        let Some(mut finish) = self.finish.clone() else {
            return self.violation("stream terminated without a reported finish_reason");
        };
        let mut content = Vec::new();
        if !self.content_text.is_empty() {
            content.push(AssistantPart::Text(std::mem::take(&mut self.content_text)));
        }
        for proposal in std::mem::take(&mut self.completed_tools) {
            content.push(AssistantPart::ToolCall(proposal));
        }
        let refusal_payload =
            (!self.refusal_text.is_empty()).then(|| std::mem::take(&mut self.refusal_text));
        if refusal_payload.is_some() {
            finish = FinishReason::Refusal;
        }
        let evidence = match finish.completion_finish() {
            None => {
                if let Some(refusal) = refusal_payload {
                    content.push(AssistantPart::Text(refusal));
                }
                TerminalEvidence::Refused(RefusalEvidence {
                    exchange: self.exchange.clone(),
                    message_id: self.message_id.clone(),
                    reported_model: self.reported_model.clone(),
                    content,
                    usage: self.usage,
                })
            }
            Some(finish) => TerminalEvidence::Completed(CompletionEvidence {
                exchange: self.exchange.clone(),
                message_id: self.message_id.clone(),
                reported_model: self.reported_model.clone(),
                finish,
                content,
                usage: self.usage,
            }),
        };
        StreamStep::Terminal(evidence)
    }
}

#[cfg(test)]
mod tests {
    use signalbox_model_runtime::{
        AssistantPart, CompletionFinish, ExchangeFacts, FinishReason, LossCause, Observation,
        ObservationFact, ProviderErrorKind, ProviderMessageId, ProviderReportedModel,
        ProviderRequestId, SseFraming, SseRecord, StreamInterruption, TerminalEvidence, TokenUsage,
        ToolCallId, ToolCallProposal, ToolName,
    };

    use super::{StreamDecoder, StreamStep};

    /// Pushes one chunk that must frame without a failure and returns its
    /// completed records.
    #[track_caller]
    fn push_ok(framing: &mut SseFraming, chunk: &[u8]) -> Vec<SseRecord> {
        let outcome = framing.push(chunk);
        assert_eq!(outcome.error, None, "test fixtures frame cleanly");
        outcome.records
    }

    fn exchange() -> ExchangeFacts {
        ExchangeFacts {
            provider_request_id: Some(ProviderRequestId::new("req_1")),
            http_status: Some(200),
        }
    }

    /// Runs byte chunks through real SSE framing and the decoder, exactly as
    /// the runtime does, correlating to `"call-1"`.
    fn drive(chunks: &[&[u8]]) -> (Option<TerminalEvidence>, Vec<Observation<String>>) {
        let mut framing = SseFraming::new(1024 * 1024);
        let mut decoder = StreamDecoder::new(exchange());
        let mut observations: Vec<Observation<String>> = Vec::new();
        let correlation = "call-1".to_string();
        let mut terminal = None;
        for chunk in chunks {
            let records = push_ok(&mut framing, chunk);
            for record in records {
                assert!(
                    terminal.is_none(),
                    "fixture continues past its terminal record"
                );
                match decoder.apply(&record, &correlation, &mut observations) {
                    StreamStep::Continue => {}
                    StreamStep::Terminal(evidence) => terminal = Some(evidence),
                }
            }
        }
        (terminal, observations)
    }

    /// Drives chunks that must not terminate, then reports the end-of-stream
    /// loss evidence for the resulting decoder state.
    fn drive_to_eof(chunks: &[&[u8]]) -> (TerminalEvidence, Vec<Observation<String>>) {
        let mut framing = SseFraming::new(1024 * 1024);
        let mut decoder = StreamDecoder::new(exchange());
        let mut observations: Vec<Observation<String>> = Vec::new();
        let correlation = "call-1".to_string();
        for chunk in chunks {
            let records = push_ok(&mut framing, chunk);
            for record in records {
                match decoder.apply(&record, &correlation, &mut observations) {
                    StreamStep::Continue => {}
                    StreamStep::Terminal(_) => {
                        panic!("fixture expected to end without a terminal record")
                    }
                }
            }
        }
        (decoder.lost(StreamInterruption::EndOfStream), observations)
    }

    fn first_chunk() -> &'static [u8] {
        b"data: {\"id\":\"chatcmpl_1\",\"model\":\"model-exact-1\",\
          \"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n"
    }

    #[test]
    fn content_stream_gated_on_done_completes_with_assembled_content() {
        let (terminal, observations) = drive(&[
            first_chunk(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":25,\"completion_tokens\":7}}\n\n",
            b"data: [DONE]\n\n",
        ]);

        let Some(TerminalEvidence::Completed(completion)) = terminal else {
            panic!("a [DONE]-gated stream must complete");
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
        assert_eq!(completion.finish, CompletionFinish::EndTurn);
        assert_eq!(
            completion.content,
            vec![AssistantPart::Text("Hello".to_string())]
        );
        assert_eq!(
            completion.usage,
            TokenUsage {
                input_tokens: Some(25),
                output_tokens: Some(7),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }
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
                    fact: ObservationFact::TextDelta {
                        index: 0,
                        text: "Hel".to_string()
                    },
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::TextDelta {
                        index: 0,
                        text: "lo".to_string()
                    },
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::FinishReported(FinishReason::EndTurn),
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::UsageReported(TokenUsage {
                        input_tokens: Some(25),
                        output_tokens: Some(7),
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    }),
                },
            ]
        );
    }

    #[test]
    fn tool_arguments_accumulate_across_chunks_into_one_proposal() {
        let (terminal, observations) = drive(&[
            first_chunk(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"type\":\"function\",\
              \"function\":{\"name\":\"lookup\",\"arguments\":\"\"}}]}}]}\n\n",
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"function\":{\"arguments\":\"{\\\"city\\\":\"}}]}}]}\n\n",
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"function\":{\"arguments\":\"\\\"Oslo\\\"}\"}}]}}]}\n\n",
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\
              \"finish_reason\":\"tool_calls\"}]}\n\n",
            b"data: [DONE]\n\n",
        ]);

        let proposal = ToolCallProposal {
            id: ToolCallId::new("call_1"),
            name: ToolName::new("lookup"),
            arguments_json: r#"{"city":"Oslo"}"#.to_string(),
        };
        let Some(TerminalEvidence::Completed(completion)) = terminal else {
            panic!("a tool-call stream gated on [DONE] must complete");
        };
        assert_eq!(
            completion.content,
            vec![AssistantPart::ToolCall(proposal.clone())]
        );
        assert_eq!(completion.finish, CompletionFinish::ToolUse);
        assert!(observations.contains(&Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolCallProposed(proposal),
        }));
        assert!(observations.contains(&Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: "{\"city\":".to_string(),
            },
        }));
    }

    #[test]
    fn eof_without_done_is_explicit_incomplete_stream_evidence_with_partials() {
        let (evidence, _) = drive_to_eof(&[
            first_chunk(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
        ]);

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("EOF before [DONE] must never read as success");
        };
        assert_eq!(
            loss.cause,
            LossCause::StreamEndedWithoutTerminalMarker {
                interruption: StreamInterruption::EndOfStream
            }
        );
        assert_eq!(
            loss.reported_model,
            Some(ProviderReportedModel::new("model-exact-1"))
        );
        assert_eq!(loss.finish_reported, None);
    }

    #[test]
    fn eof_after_finish_reason_but_without_done_is_still_incomplete() {
        // The audited upstream gap: a cut stream that already carried a
        // finish reason must still classify as incomplete, not success.
        let (evidence, _) = drive_to_eof(&[
            first_chunk(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done-ish\"},\
              \"finish_reason\":\"stop\"}]}\n\n",
        ]);

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("a stream without [DONE] must never read as success");
        };
        assert_eq!(loss.finish_reported, Some(FinishReason::EndTurn));
        assert_eq!(
            loss.cause,
            LossCause::StreamEndedWithoutTerminalMarker {
                interruption: StreamInterruption::EndOfStream
            }
        );
    }

    #[test]
    fn done_without_finish_reason_is_a_protocol_violation() {
        let (terminal, _) = drive(&[first_chunk(), b"data: [DONE]\n\n"]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("[DONE] without a finish reason must not read as success");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn refusal_deltas_accumulate_into_refusal_evidence_at_done() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"refusal\":\"I cannot \"}}]}\n\n",
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"refusal\":\"help with that.\"},\
              \"finish_reason\":\"stop\"}]}\n\n",
            b"data: [DONE]\n\n",
        ]);

        let Some(TerminalEvidence::Refused(refusal)) = terminal else {
            panic!("accumulated refusal material is refusal evidence, never completion");
        };
        assert_eq!(
            refusal.content,
            vec![AssistantPart::Text("I cannot help with that.".to_string())]
        );
    }

    #[test]
    fn content_filter_finish_is_refusal_evidence_at_done() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\
              \"finish_reason\":\"content_filter\"}]}\n\n",
            b"data: [DONE]\n\n",
        ]);

        let Some(TerminalEvidence::Refused(refusal)) = terminal else {
            panic!("a content_filter finish is the provider's refusal outcome");
        };
        assert_eq!(
            refusal.content,
            vec![AssistantPart::Text("partial".to_string())]
        );
    }

    #[test]
    fn malformed_chunk_payload_is_a_protocol_violation() {
        let (terminal, _) = drive(&[first_chunk(), b"data: {not json\n\n"]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a malformed chunk must surface as a protocol violation");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn mid_stream_error_record_is_definitive_provider_error_evidence() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"error\":{\"message\":\"quota exhausted\",\
              \"type\":\"insufficient_quota\",\"code\":\"insufficient_quota\"}}\n\n",
        ]);

        let Some(TerminalEvidence::ProviderError(error)) = terminal else {
            panic!("a mid-stream error record is a definitive provider error");
        };
        assert_eq!(error.kind, ProviderErrorKind::QuotaExhausted);
        assert_eq!(
            error.native.error_code,
            Some("insufficient_quota".to_string())
        );
    }

    #[test]
    fn a_second_choice_index_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"choices\":[{\"index\":1,\"delta\":{\"content\":\"ghost\"}}]}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("an unrequested second choice must surface as a protocol violation");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn malformed_streamed_tool_arguments_are_preserved_for_typed_decoding() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"function\":{\"name\":\"lookup\",\
              \"arguments\":\"{\\\"city\\\":\"}}]}}]}\n\n",
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\
              \"finish_reason\":\"tool_calls\"}]}\n\n",
            b"data: [DONE]\n\n",
        ]);

        let proposal = ToolCallProposal {
            id: ToolCallId::new("call_1"),
            name: ToolName::new("lookup"),
            arguments_json: "{\"city\":".to_string(),
        };
        let Some(TerminalEvidence::Completed(completion)) = terminal else {
            panic!("the provider's authoritative proposal must be exposed, not suppressed");
        };
        assert_eq!(
            completion.content,
            vec![AssistantPart::ToolCall(proposal.clone())]
        );
        let failure =
            signalbox_model_runtime::decode_tool_arguments::<serde_json::Value>(&proposal)
                .expect_err("typed decoding owns the JsonSyntax classification");
        assert!(matches!(
            failure,
            signalbox_model_runtime::ToolDecodeFailure::JsonSyntax { .. }
        ));
    }

    #[test]
    fn a_streamed_tool_call_with_an_unrecognized_type_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"type\":\"custom\",\
              \"function\":{\"name\":\"lookup\",\"arguments\":\"{}\"}}]}}]}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("non-function tool material must not assemble into an ordinary proposal");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn streamed_refusal_finish_observation_matches_the_terminal_outcome() {
        let (terminal, observations) = drive(&[
            first_chunk(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"refusal\":\"No.\"}}]}\n\n",
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            b"data: [DONE]\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::Refused(_))));
        assert!(observations.contains(&Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::FinishReported(FinishReason::Refusal),
        }));
        assert!(!observations.contains(&Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::FinishReported(FinishReason::EndTurn),
        }));
    }

    #[test]
    fn tool_proposals_are_observed_before_the_finish_fact() {
        let (_, observations) = drive(&[
            first_chunk(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"function\":{\"name\":\"ping\",\"arguments\":\"{}\"}}]}}]}\n\n",
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\
              \"finish_reason\":\"tool_calls\"}]}\n\n",
            b"data: [DONE]\n\n",
        ]);

        let proposal_at = observations
            .iter()
            .position(|observation| {
                matches!(observation.fact, ObservationFact::ToolCallProposed(_))
            })
            .expect("the completed proposal is observed");
        let finish_at = observations
            .iter()
            .position(|observation| matches!(observation.fact, ObservationFact::FinishReported(_)))
            .expect("the finish is observed");
        assert!(
            proposal_at < finish_at,
            "a finished-generation fact must never precede its proposals"
        );
    }

    #[test]
    fn conflicting_streamed_model_identities_are_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"model\":\"other-model\",\"choices\":[]}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a second model identity must not complete under the first");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn choice_material_after_the_finish_reason_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late\"}}]}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("material after the finish reason must not alter the completion");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn content_after_tool_fragments_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{}\"}}]}}]}\n\n",
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late text\"}}]}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("content after tool fragments would shift already-reported part positions");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn empty_streamed_tool_arguments_are_preserved_raw_for_typed_decoding() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"function\":{\"name\":\"ping\",\"arguments\":\"\"}}]}}]}\n\n",
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\
              \"finish_reason\":\"tool_calls\"}]}\n\n",
            b"data: [DONE]\n\n",
        ]);

        let Some(TerminalEvidence::Completed(completion)) = terminal else {
            panic!("an empty argument accumulation is the provider's value, not corruption");
        };
        assert_eq!(
            completion.content,
            vec![AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("call_1"),
                name: ToolName::new("ping"),
                arguments_json: String::new(),
            })]
        );
    }

    #[test]
    fn usage_only_chunk_reports_usage_and_keeps_streaming() {
        let (terminal, observations) = drive(&[
            first_chunk(),
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":1,\
              \"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            b"data: [DONE]\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::Completed(_))));
        assert!(observations.contains(&Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::UsageReported(TokenUsage {
                input_tokens: Some(9),
                output_tokens: Some(1),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: Some(4),
            }),
        }));
    }
}
