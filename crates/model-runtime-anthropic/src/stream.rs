//! SSE stream decoding with terminal-integrity evidence.
//!
//! The decoder consumes framed SSE records and enforces the Messages API
//! stream protocol: `message_start` first, block bookkeeping by index, a
//! stop reason before `message_stop`, and `message_stop` itself as the only
//! terminal marker. A stream that ends any other way is explicit
//! incomplete-stream or protocol-violation evidence with the partial facts
//! retained — never silent success (ADR-0043's ambiguous branch).
//!
//! Unknown SSE *event names* and unknown *delta types* are tolerated, as the
//! provider documents additive evolution of both. An unrecognized *content
//! block type* or any malformed known event ends interpretation with
//! protocol-violation evidence: later records about material this adapter
//! cannot interpret would not be trustworthy.

use std::collections::BTreeMap;

use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CompletionEvidence, ExchangeFacts, FinishReason,
    LossCause, Observation, ObservationFact, ObservationSink, ProviderErrorEvidence,
    ProviderMessageId, ProviderReportedModel, RefusalEvidence, SseRecord, StreamInterruption,
    TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal, ToolName,
};

use crate::response::{convert_usage, map_finish};
use crate::status::classify_error_token;
use crate::wire::{
    ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent, ErrorEnvelope,
    MessageDeltaEvent, MessageStartEvent, MessageStopEvent, WireDelta, WireResponseBlock,
    parse_response_block,
};

/// The decoder's verdict on one record.
pub(crate) enum StreamStep {
    /// Keep reading.
    Continue,
    /// The stream reached typed terminal evidence; stop reading.
    Terminal(TerminalEvidence),
}

enum BlockBuilder {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        start_input: String,
        accumulated: String,
    },
}

/// Incremental decoder for one message stream.
pub(crate) struct StreamDecoder {
    exchange: ExchangeFacts,
    started: bool,
    message_id: Option<ProviderMessageId>,
    reported_model: Option<ProviderReportedModel>,
    usage: TokenUsage,
    input_usage_reported: bool,
    final_output_usage_reported: bool,
    finish: Option<FinishReason>,
    open_blocks: BTreeMap<u32, BlockBuilder>,
    closed: BTreeMap<u32, AssistantPart>,
}

impl StreamDecoder {
    pub(crate) fn new(exchange: ExchangeFacts) -> Self {
        Self {
            exchange,
            started: false,
            message_id: None,
            reported_model: None,
            usage: TokenUsage::unreported(),
            input_usage_reported: false,
            final_output_usage_reported: false,
            finish: None,
            open_blocks: BTreeMap::new(),
            closed: BTreeMap::new(),
        }
    }

    /// Applies one framed record.
    pub(crate) fn apply<C: Clone>(
        &mut self,
        record: &SseRecord,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> StreamStep {
        let Some(event) = record.event.as_deref() else {
            return self.violation("SSE record without an event name");
        };
        match event {
            "ping" => StreamStep::Continue,
            "error" => self.apply_error(record),
            "message_start" => self.apply_message_start(record, correlation, sink),
            "content_block_start" => self.apply_block_start(record),
            "content_block_delta" => self.apply_block_delta(record, correlation, sink),
            "content_block_stop" => self.apply_block_stop(record, correlation, sink),
            "message_delta" => self.apply_message_delta(record, correlation, sink),
            "message_stop" => self.apply_message_stop(record),
            // The provider documents that new event types may be added and
            // must be tolerated.
            _ => StreamStep::Continue,
        }
    }

    /// Evidence for a stream that ended without `message_stop`.
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

    fn parse<'a, T: serde::Deserialize<'a>>(
        &self,
        record: &'a SseRecord,
        event: &str,
    ) -> Result<T, Box<StreamStep>> {
        serde_json::from_str(&record.data).map_err(|error| {
            Box::new(self.violation(format!("malformed {event} event payload: {error}")))
        })
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

    fn apply_error(&mut self, record: &SseRecord) -> StreamStep {
        let envelope: ErrorEnvelope = match self.parse(record, "error") {
            Ok(envelope) => envelope,
            Err(step) => return *step,
        };
        if envelope.envelope_type != "error" {
            return self.violation("error event payload has the wrong discriminator");
        }
        let Some(error) = envelope.error else {
            return self.violation("error event without an error payload");
        };
        let kind = error
            .error_type
            .as_deref()
            .map(classify_error_token)
            .unwrap_or(signalbox_model_runtime::ProviderErrorKind::Unrecognized);
        StreamStep::Terminal(TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange: self.exchange.clone(),
            reported_model: self.reported_model.clone(),
            kind,
            native: error.into_native_facts(),
        }))
    }

    fn apply_message_start<C: Clone>(
        &mut self,
        record: &SseRecord,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> StreamStep {
        if self.started {
            return self.violation("duplicate message_start");
        }
        let event: MessageStartEvent = match self.parse(record, "message_start") {
            Ok(event) => event,
            Err(step) => return *step,
        };
        if event.event_type != "message_start" {
            return self.violation("message_start payload has the wrong discriminator");
        }
        // The stream's opening envelope is held to the same documented
        // shape as a buffered success: discriminators, id, model, and
        // usage must all be present.
        if event.message.response_type.as_deref() != Some("message")
            || event.message.role.as_deref() != Some("assistant")
        {
            return self.violation(
                "message_start is missing its message/assistant envelope discriminators",
            );
        }
        if !event.message.content.is_empty() {
            return self.violation("message_start must not carry content blocks");
        }
        let (Some(id), Some(model), Some(usage)) = (
            event.message.id,
            event.message.model,
            event.message.usage.as_ref(),
        ) else {
            return self.violation("message_start is missing required fields (id, model, usage)");
        };
        if usage.input_tokens.is_none() {
            return self.violation("message_start usage is missing input_tokens");
        }
        self.started = true;
        self.input_usage_reported = true;
        self.message_id = Some(ProviderMessageId::new(id));
        let model = ProviderReportedModel::new(model);
        self.reported_model = Some(model.clone());
        Self::emit(
            correlation,
            sink,
            ObservationFact::ProviderModelReported(model),
        );
        let usage = convert_usage(usage);
        self.usage.absorb(usage);
        Self::emit(correlation, sink, ObservationFact::UsageReported(usage));
        StreamStep::Continue
    }

    fn apply_block_start(&mut self, record: &SseRecord) -> StreamStep {
        if !self.started {
            return self.violation("content_block_start before message_start");
        }
        let event: ContentBlockStartEvent = match self.parse(record, "content_block_start") {
            Ok(event) => event,
            Err(step) => return *step,
        };
        if event.event_type != "content_block_start" {
            return self.violation("content_block_start payload has the wrong discriminator");
        }
        if self.open_blocks.contains_key(&event.index) || self.closed.contains_key(&event.index) {
            return self.violation(format!("content_block_start reopens index {}", event.index));
        }
        let content_block = match parse_response_block(&event.content_block) {
            Ok(block) => block,
            Err(error) => {
                return self.violation(format!("malformed content_block_start payload: {error}"));
            }
        };
        let builder = match content_block {
            WireResponseBlock::Text { text } => BlockBuilder::Text(text),
            WireResponseBlock::ToolUse { id, name, input } => BlockBuilder::ToolUse {
                id,
                name,
                start_input: input.get().to_string(),
                accumulated: String::new(),
            },
            WireResponseBlock::Thinking {
                thinking,
                signature,
            } => BlockBuilder::Thinking {
                text: thinking,
                signature,
            },
            WireResponseBlock::RedactedThinking { data } => BlockBuilder::RedactedThinking { data },
            WireResponseBlock::Unrecognized => {
                return self.violation(format!(
                    "unrecognized content-block type opened at index {}",
                    event.index
                ));
            }
        };
        self.open_blocks.insert(event.index, builder);
        StreamStep::Continue
    }

    fn apply_block_delta<C: Clone>(
        &mut self,
        record: &SseRecord,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> StreamStep {
        if !self.started {
            return self.violation("content_block_delta before message_start");
        }
        let event: ContentBlockDeltaEvent = match self.parse(record, "content_block_delta") {
            Ok(event) => event,
            Err(step) => return *step,
        };
        if event.event_type != "content_block_delta" {
            return self.violation("content_block_delta payload has the wrong discriminator");
        }
        let index = event.index;
        let Some(builder) = self.open_blocks.get_mut(&index) else {
            return self.violation(format!("content_block_delta for unopened index {index}"));
        };
        match (builder, event.delta) {
            (BlockBuilder::Text(text), WireDelta::Text { text: fragment }) => {
                text.push_str(&fragment);
                Self::emit(
                    correlation,
                    sink,
                    ObservationFact::TextDelta {
                        index,
                        text: fragment,
                    },
                );
                StreamStep::Continue
            }
            (BlockBuilder::Thinking { text, .. }, WireDelta::Thinking { thinking }) => {
                text.push_str(&thinking);
                Self::emit(
                    correlation,
                    sink,
                    ObservationFact::ThinkingDelta {
                        index,
                        text: thinking,
                    },
                );
                StreamStep::Continue
            }
            (
                BlockBuilder::Thinking { signature, .. },
                WireDelta::Signature { signature: value },
            ) => {
                *signature = Some(value);
                StreamStep::Continue
            }
            (BlockBuilder::ToolUse { accumulated, .. }, WireDelta::InputJson { partial_json }) => {
                accumulated.push_str(&partial_json);
                Self::emit(
                    correlation,
                    sink,
                    ObservationFact::ToolArgumentsDelta {
                        index,
                        fragment: partial_json,
                    },
                );
                StreamStep::Continue
            }
            // Additive delta evolution is tolerated on any block type.
            (_, WireDelta::Unrecognized) => StreamStep::Continue,
            _ => self.violation(format!(
                "content_block_delta type does not match the open block at index {index}"
            )),
        }
    }

    fn apply_block_stop<C: Clone>(
        &mut self,
        record: &SseRecord,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> StreamStep {
        if !self.started {
            return self.violation("content_block_stop before message_start");
        }
        let event: ContentBlockStopEvent = match self.parse(record, "content_block_stop") {
            Ok(event) => event,
            Err(step) => return *step,
        };
        if event.event_type != "content_block_stop" {
            return self.violation("content_block_stop payload has the wrong discriminator");
        }
        let Some(builder) = self.open_blocks.remove(&event.index) else {
            return self.violation(format!(
                "content_block_stop for unopened index {}",
                event.index
            ));
        };
        let part = match builder {
            BlockBuilder::Text(text) => AssistantPart::Text(text),
            BlockBuilder::Thinking { text, signature } => {
                let Some(signature) = signature else {
                    // The provider requires the integrity signature for any
                    // replay; a thinking block closing without one is not
                    // trustworthy completion material.
                    return self.violation(format!(
                        "thinking block {} closed without its integrity signature",
                        event.index
                    ));
                };
                AssistantPart::Thinking {
                    text,
                    signature: Some(signature),
                }
            }
            BlockBuilder::RedactedThinking { data } => AssistantPart::RedactedThinking { data },
            BlockBuilder::ToolUse {
                id,
                name,
                start_input,
                accumulated,
            } => {
                let arguments_json = if accumulated.is_empty() {
                    start_input
                } else {
                    accumulated
                };
                if serde_json::from_str::<serde_json::Value>(&arguments_json).is_err() {
                    return self.violation(format!(
                        "tool_use block {} closed with invalid accumulated argument JSON",
                        event.index
                    ));
                }
                let proposal = ToolCallProposal {
                    id: ToolCallId::new(id),
                    name: ToolName::new(name),
                    arguments_json,
                };
                Self::emit(
                    correlation,
                    sink,
                    ObservationFact::ToolCallProposed(proposal.clone()),
                );
                AssistantPart::ToolCall(proposal)
            }
        };
        // Retained by index: the indices define part positions in the final
        // message, so assembly is by index order, not stop-event order.
        self.closed.insert(event.index, part);
        StreamStep::Continue
    }

    fn apply_message_delta<C: Clone>(
        &mut self,
        record: &SseRecord,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> StreamStep {
        if !self.started {
            return self.violation("message_delta before message_start");
        }
        let event: MessageDeltaEvent = match self.parse(record, "message_delta") {
            Ok(event) => event,
            Err(step) => return *step,
        };
        if event.event_type != "message_delta" {
            return self.violation("message_delta payload has the wrong discriminator");
        }
        if let Some(delta) = event.delta
            && let Some(stop_reason) = delta.stop_reason
        {
            if self.finish.is_some() {
                // The stop reason is terminal outcome metadata; a second
                // report must not silently replace the first disposition.
                return self.violation("message_delta reports a second stop_reason");
            }
            let finish = map_finish(&stop_reason, delta.stop_sequence);
            self.finish = Some(finish.clone());
            if matches!(finish, FinishReason::Unrecognized { .. }) {
                return self.violation("message_delta carries an unrecognized stop_reason");
            }
            Self::emit(correlation, sink, ObservationFact::FinishReported(finish));
        }
        if let Some(usage) = event.usage.as_ref() {
            if usage.output_tokens.is_some() {
                self.final_output_usage_reported = true;
            }
            let usage = convert_usage(usage);
            self.usage.absorb(usage);
            Self::emit(correlation, sink, ObservationFact::UsageReported(usage));
        }
        StreamStep::Continue
    }

    fn apply_message_stop(&mut self, record: &SseRecord) -> StreamStep {
        if !self.started {
            return self.violation("message_stop before message_start");
        }
        // The terminal record's payload is validated like every other known
        // event: a malformed terminal must not cross the integrity gate.
        if let Err(step) = self.parse::<MessageStopEvent>(record, "message_stop") {
            return *step;
        }
        if !self.open_blocks.is_empty() {
            return self.violation("message_stop with open content blocks");
        }
        if !self.input_usage_reported || !self.final_output_usage_reported {
            return self.violation("message_stop before required usage counts were reported");
        }
        let Some(finish) = self.finish.clone() else {
            return self.violation("message_stop without a reported stop_reason");
        };
        if self.closed.keys().copied().ne(0..self.closed.len() as u32) {
            return self.violation("message_stop with sparse content-block indices");
        }
        let has_tool_calls = self
            .closed
            .values()
            .any(|part| matches!(part, AssistantPart::ToolCall(_)));
        if has_tool_calls != matches!(finish, FinishReason::ToolUse) {
            return self.violation("stream content contradicts its stop_reason");
        }
        let evidence = match finish.completion_finish() {
            None => TerminalEvidence::Refused(RefusalEvidence {
                exchange: self.exchange.clone(),
                message_id: self.message_id.clone(),
                reported_model: self.reported_model.clone(),
                content: std::mem::take(&mut self.closed).into_values().collect(),
                usage: self.usage,
            }),
            Some(finish) => TerminalEvidence::Completed(CompletionEvidence {
                exchange: self.exchange.clone(),
                message_id: self.message_id.clone(),
                reported_model: self.reported_model.clone(),
                finish,
                content: std::mem::take(&mut self.closed).into_values().collect(),
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
    /// the runtime does, correlating to `"call-1"`. Returns the terminal
    /// evidence when one record produced it (later chunks are then
    /// rejected by the panic below, keeping fixtures honest) and the
    /// observation log.
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

    fn message_start() -> &'static [u8] {
        b"event: message_start\n\
          data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\
          \"role\":\"assistant\",\"id\":\"msg_1\",\
          \"model\":\"model-exact-1\",\"content\":[],\"usage\":{\"input_tokens\":25}}}\n\n"
    }

    #[test]
    fn text_stream_gated_on_message_stop_completes_with_assembled_content() {
        let (terminal, observations) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
              \"usage\":{\"output_tokens\":7}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        let Some(TerminalEvidence::Completed(completion)) = terminal else {
            panic!("a message_stop-gated stream must complete");
        };
        assert_eq!(completion.exchange, exchange());
        assert_eq!(completion.message_id, Some(ProviderMessageId::new("msg_1")));
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
                    fact: ObservationFact::UsageReported(TokenUsage {
                        input_tokens: Some(25),
                        output_tokens: None,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    }),
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
                        input_tokens: None,
                        output_tokens: Some(7),
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    }),
                },
            ]
        );
    }

    #[test]
    fn tool_arguments_accumulate_across_deltas_into_one_proposal() {
        let (terminal, observations) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\
              \"name\":\"lookup\",\"input\":{}}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Oslo\\\"}\"}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\
              \"usage\":{\"output_tokens\":7}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        let proposal = ToolCallProposal {
            id: ToolCallId::new("toolu_1"),
            name: ToolName::new("lookup"),
            arguments_json: r#"{"city":"Oslo"}"#.to_string(),
        };
        let Some(TerminalEvidence::Completed(completion)) = terminal else {
            panic!("a tool-use stream gated on message_stop must complete");
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
    }

    #[test]
    fn tool_block_without_argument_deltas_proposes_the_start_input() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\
              \"name\":\"ping\",\"input\":{}}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\
              \"usage\":{\"output_tokens\":1}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        let Some(TerminalEvidence::Completed(completion)) = terminal else {
            panic!("a delta-less tool block must still complete");
        };
        assert_eq!(
            completion.content,
            vec![AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("toolu_1"),
                name: ToolName::new("ping"),
                arguments_json: "{}".to_string(),
            })]
        );
    }

    #[test]
    fn premature_eof_is_explicit_incomplete_stream_evidence_with_partials() {
        let (evidence, _) = drive_to_eof(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
        ]);

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("EOF before message_stop must never read as success");
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
        assert_eq!(loss.usage.input_tokens, Some(25));
    }

    #[test]
    fn refusal_stop_reason_with_message_stop_is_refusal_evidence() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"},\
              \"usage\":{\"output_tokens\":2}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        let Some(TerminalEvidence::Refused(refusal)) = terminal else {
            panic!("a refusal stop reason gated on message_stop is refusal evidence");
        };
        assert_eq!(
            refusal.reported_model,
            Some(ProviderReportedModel::new("model-exact-1"))
        );
        assert_eq!(refusal.usage.output_tokens, Some(2));
    }

    #[test]
    fn refusal_reported_but_stream_cut_before_message_stop_is_not_refusal() {
        let (evidence, _) = drive_to_eof(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"}}\n\n",
        ]);

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("an incomplete exchange must not classify as refusal (ADR-0043 precondition)");
        };
        assert_eq!(loss.finish_reported, Some(FinishReason::Refusal));
        assert_eq!(
            loss.cause,
            LossCause::StreamEndedWithoutTerminalMarker {
                interruption: StreamInterruption::EndOfStream
            }
        );
    }

    #[test]
    fn usage_only_message_delta_reports_usage_and_keeps_streaming() {
        let (terminal, observations) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":11}}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
              \"usage\":{\"output_tokens\":12}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        let Some(TerminalEvidence::Completed(completion)) = terminal else {
            panic!("usage-only terminal metadata must not end the stream");
        };
        assert_eq!(completion.usage.output_tokens, Some(12));
        assert!(observations.contains(&Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::UsageReported(TokenUsage {
                input_tokens: None,
                output_tokens: Some(11),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
        }));
    }

    #[test]
    fn message_stop_without_stop_reason_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("message_stop without a stop_reason must not read as success");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn message_stop_without_final_output_usage_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("completion without final output usage must be rejected");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn tool_use_stop_without_a_tool_block_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\
              \"usage\":{\"output_tokens\":1}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("tool_use without a tool proposal must be rejected");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn malformed_known_event_payload_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\ndata: {not json\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a malformed known event must surface as a protocol violation");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn ping_and_unknown_event_names_are_tolerated() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: ping\ndata: {\"type\":\"ping\"}\n\n",
            b"event: content_block_heartbeat\ndata: {\"type\":\"content_block_heartbeat\"}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
              \"usage\":{\"output_tokens\":0}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::Completed(_))));
    }

    #[test]
    fn mid_stream_error_event_is_definitive_provider_error_evidence() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: error\n\
              data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\
              \"message\":\"Overloaded\"}}\n\n",
        ]);

        let Some(TerminalEvidence::ProviderError(error)) = terminal else {
            panic!("a mid-stream error event is a definitive provider error");
        };
        assert_eq!(error.kind, ProviderErrorKind::Overloaded);
        assert_eq!(
            error.native.error_token,
            Some("overloaded_error".to_string())
        );
        assert_eq!(error.exchange, exchange());
    }

    #[test]
    fn delta_for_an_unopened_index_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":3,\
              \"delta\":{\"type\":\"text_delta\",\"text\":\"ghost\"}}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a delta for an unopened block must surface as a protocol violation");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn message_stop_with_an_open_block_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("message_stop with an open block must surface as a protocol violation");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn any_message_event_before_message_start_is_a_protocol_violation() {
        let (terminal, _) = drive(&[b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n"]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("message events before message_start must surface as protocol violations");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn duplicate_message_start_is_a_protocol_violation() {
        let (terminal, _) = drive(&[message_start(), message_start()]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a duplicate message_start must surface as a protocol violation");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn invalid_accumulated_tool_argument_json_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\
              \"name\":\"lookup\",\"input\":{}}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("truncated tool-argument JSON at block close must surface as a violation");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn message_start_without_the_documented_envelope_is_a_protocol_violation() {
        let (terminal, _) = drive(&[b"event: message_start\n\
              data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\
              \"model\":\"model-exact-1\",\"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n"]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("an opening envelope missing its discriminators must not start the stream");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn message_start_with_embedded_content_is_a_protocol_violation() {
        let (terminal, _) = drive(&[b"event: message_start\n\
              data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\
              \"role\":\"assistant\",\"id\":\"msg_1\",\"model\":\"model-exact-1\",\
              \"content\":[{\"type\":\"text\",\"text\":\"lost\"}],\
              \"usage\":{\"input_tokens\":1}}}\n\n"]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("opening content must not be silently discarded");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn a_second_stop_reason_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"}}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a replayed stop reason must not rewrite the terminal disposition");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn malformed_message_stop_payload_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            b"event: message_stop\ndata: {\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a malformed terminal payload must not cross the integrity gate");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn message_stop_with_the_wrong_discriminator_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"ping\"}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a mismatched terminal discriminator must not cross the integrity gate");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn non_terminal_event_with_the_wrong_discriminator_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"ping\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a contradictory known-event discriminator must be a protocol violation");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn sparse_content_block_indices_are_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":1,\
              \"content_block\":{\"type\":\"text\",\"text\":\"second\"}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("sparse provider indices must not be compacted into completion content");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn blocks_closing_out_of_index_order_assemble_in_index_order() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"text\",\"text\":\"first\"}}\n\n",
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":1,\
              \"content_block\":{\"type\":\"text\",\"text\":\"second\"}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
              \"usage\":{\"output_tokens\":2}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        let Some(TerminalEvidence::Completed(completion)) = terminal else {
            panic!("out-of-order closes with a clean terminal still complete");
        };
        assert_eq!(
            completion.content,
            vec![
                AssistantPart::Text("first".to_string()),
                AssistantPart::Text("second".to_string()),
            ]
        );
    }

    #[test]
    fn thinking_block_retains_text_and_signature_in_final_content() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":null}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"step one\"}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_1\"}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
              \"usage\":{\"output_tokens\":2}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        let Some(TerminalEvidence::Completed(completion)) = terminal else {
            panic!("a thinking stream gated on message_stop must complete");
        };
        assert_eq!(
            completion.content,
            vec![AssistantPart::Thinking {
                text: "step one".to_string(),
                signature: Some("sig_1".to_string()),
            }]
        );
    }
}
