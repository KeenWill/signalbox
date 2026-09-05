//! SSE stream decoding with terminal-integrity evidence.
//!
//! The decoder consumes framed SSE records and enforces the Messages API
//! stream protocol: `message_start` first, block bookkeeping by index, a
//! stop reason before `message_stop`, and `message_stop` itself as the only
//! terminal marker. A stream that ends any other way is explicit
//! incomplete-stream or protocol-violation evidence with the partial facts
//! retained — never silent success (the ambiguous branch of
//! `docs/spec/model-call-execution.md`).
//!
//! Unknown SSE *event names* and unknown *delta types* are tolerated, as the
//! provider documents additive evolution of both. An unrecognized *content
//! block type* or any malformed known event ends interpretation with
//! protocol-violation evidence: later records about material this adapter
//! cannot interpret would not be trustworthy.

use std::collections::{BTreeMap, BTreeSet};

use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CompletionEvidence, ExchangeFacts, FinishReason,
    LossCause, Observation, ObservationFact, ObservationSink, ProviderErrorEvidence,
    ProviderJsonNestingValidator, ProviderMessageId, ProviderReportedModel, RefusalEvidence,
    SseRecord, StreamInterruption, TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal,
    ToolCallsAtLoss, ToolName, validate_provider_json_nesting,
};

use crate::response::{
    convert_usage, iteration_usage_is_complete, map_finish, retained_input_tokens,
};
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
    Terminal(Box<TerminalEvidence>),
}

/// Whether records this chunk framed sit behind the one being applied now.
///
/// This is the axis that separates a withheld answer from a stated negative: a
/// terminal raised on the last record of a chunk discards nothing, while one
/// raised earlier drops records the decoder never scanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaterRecords {
    /// Records the caller framed but has not applied yet follow this one.
    Unapplied,
    /// This is the last record the chunk framed, so nothing follows it.
    AllApplied,
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
    Compaction {
        delta: Option<(crate::wire::WireCompactionContent, Option<String>)>,
    },
    ToolUse {
        id: String,
        name: String,
        start_input: String,
        accumulated: String,
        argument_nesting: ProviderJsonNestingValidator,
    },
}

/// Incremental decoder for one message stream.
/// The one SSE event whose payload can open a tool call.
///
/// Named once because two rules depend on it: dispatch, and whether a payload
/// that never parsed leaves the tool question unexamined.
const CONTENT_BLOCK_START_EVENT: &str = "content_block_start";

pub(crate) struct StreamDecoder {
    exchange: ExchangeFacts,
    started: bool,
    message_id: Option<ProviderMessageId>,
    reported_model: Option<ProviderReportedModel>,
    usage: TokenUsage,
    retained_input_tokens: Option<u64>,
    input_usage_reported: bool,
    final_output_usage_reported: bool,
    finish: Option<FinishReason>,
    declared_stop_sequences: Vec<String>,
    tool_call_ids: BTreeSet<String>,
    discarded_unexamined_bytes: bool,
    later_records: LaterRecords,
    provider_compaction_enabled: bool,
    open_blocks: BTreeMap<u32, BlockBuilder>,
    closed: BTreeMap<u32, AssistantPart>,
}

impl StreamDecoder {
    pub(crate) fn with_stop_sequences(
        exchange: ExchangeFacts,
        declared_stop_sequences: Vec<String>,
        provider_compaction_enabled: bool,
    ) -> Self {
        Self {
            exchange,
            started: false,
            message_id: None,
            reported_model: None,
            usage: TokenUsage::unreported(),
            retained_input_tokens: None,
            input_usage_reported: false,
            final_output_usage_reported: false,
            finish: None,
            declared_stop_sequences,
            tool_call_ids: BTreeSet::new(),
            discarded_unexamined_bytes: false,
            later_records: LaterRecords::AllApplied,
            provider_compaction_enabled,
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
            return self.undecoded_violation("SSE record without an event name");
        };
        if let Err(error) = validate_provider_json_nesting(record.data.as_bytes()) {
            return self.unparsed_payload_violation(
                event,
                format!("SSE event payload exceeds the JSON bound: {error}"),
            );
        }
        match event {
            "ping" => StreamStep::Continue,
            "error" => self.apply_error(record),
            "message_start" => self.apply_message_start(record, correlation, sink),
            CONTENT_BLOCK_START_EVENT => self.apply_block_start(record, correlation, sink),
            "content_block_delta" => self.apply_block_delta(record, correlation, sink),
            "content_block_stop" => self.apply_block_stop(record, correlation, sink),
            "message_delta" => self.apply_message_delta(record, correlation, sink),
            "message_stop" => self.apply_message_stop(record),
            // The provider documents that new event types may be added and
            // must be tolerated.
            _ => StreamStep::Continue,
        }
    }

    /// Whether a tool call had opened in the events decoded so far.
    ///
    /// `tool_call_ids` records every `tool_use` block the stream opened and is
    /// never drained, so it answers this at any point in the decode. Across
    /// events the decoder read and classified, the negative case is the stated
    /// fact rather than an absence; material that never decoded is answered by
    /// [`Self::tool_calls_at_decode_failure`] instead.
    fn tool_calls_at_loss(&self) -> ToolCallsAtLoss {
        if !self.tool_call_ids.is_empty() {
            ToolCallsAtLoss::Opened
        } else if self.discarded_unexamined_bytes {
            ToolCallsAtLoss::Unobserved
        } else {
            match self.later_records {
                LaterRecords::Unapplied => ToolCallsAtLoss::Unobserved,
                LaterRecords::AllApplied => ToolCallsAtLoss::NoneOpened,
            }
        }
    }

    /// Records that the transport accepted bytes no record ever carried into
    /// this decoder — a partial record held by the framer when the stream was
    /// cancelled or failed. Those bytes are discarded unexamined, so from this
    /// point the decoder can no longer state that no tool call opened.
    pub(crate) fn note_discarded_unexamined_bytes(&mut self) {
        self.discarded_unexamined_bytes = true;
    }

    /// Records whether this chunk framed records the caller has not applied yet.
    ///
    /// Not sticky: it holds only while such records exist. A terminal built
    /// during the apply below discards them, and the evidence is constructed
    /// inside `apply`, so the decoder has to know before it is called rather
    /// than be corrected afterwards.
    pub(crate) fn note_later_records(&mut self, later_records: LaterRecords) {
        self.later_records = later_records;
    }

    /// The tool fact for a violation raised by material that never decoded.
    ///
    /// An SSE record with no event name, a payload rejected by the JSON bound
    /// or by typed-event parsing, a stream whose framing ended inside an
    /// incomplete record, and every `content_block_start` exit taken before its
    /// inner `content_block` parses all leave unexamined the one place a
    /// `tool_use` block can open, so "none opened" would claim a negative about
    /// bytes the decoder never read. A block already recorded still stands.
    ///
    /// The scope is deliberately that narrow. The SSE event name is examined —
    /// it is what dispatched the handler — and no other event type opens a tool
    /// call: a `content_block_delta` carries arguments for a block already
    /// opened, and `message_start` is rejected outright if it carries content
    /// blocks. Their unparsed payloads therefore cannot hide a tool call
    /// opening, and those handlers state the negative rather than withhold it.
    fn tool_calls_at_decode_failure(&self) -> ToolCallsAtLoss {
        if self.tool_call_ids.is_empty() {
            ToolCallsAtLoss::Unobserved
        } else {
            ToolCallsAtLoss::Opened
        }
    }

    /// Protocol-violation evidence for material that never decoded.
    pub(crate) fn undecoded_violation_evidence(
        &self,
        detail: impl Into<String>,
    ) -> TerminalEvidence {
        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::StreamProtocolViolation {
                detail: detail.into(),
            },
            exchange: self.exchange.clone(),
            reported_model: self.reported_model.clone(),
            finish_reported: self.finish.clone(),
            tool_calls: self.tool_calls_at_decode_failure(),
            usage: self.usage,
        })
    }

    fn undecoded_violation(&self, detail: impl Into<String>) -> StreamStep {
        StreamStep::Terminal(Box::new(self.undecoded_violation_evidence(detail)))
    }

    /// Evidence for a stream that ended without `message_stop`.
    pub(crate) fn lost(self, interruption: StreamInterruption) -> TerminalEvidence {
        let tool_calls = self.tool_calls_at_loss();
        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::StreamEndedWithoutTerminalMarker { interruption },
            exchange: self.exchange,
            reported_model: self.reported_model,
            finish_reported: self.finish,
            tool_calls,
            usage: self.usage,
        })
    }

    /// Evidence for a caller cancellation observed mid-stream.
    pub(crate) fn cancelled(&self) -> TerminalEvidence {
        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::CancellationRequested,
            exchange: self.exchange.clone(),
            reported_model: self.reported_model.clone(),
            finish_reported: self.finish.clone(),
            tool_calls: self.tool_calls_at_loss(),
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
            tool_calls: self.tool_calls_at_loss(),
            usage: self.usage,
        })
    }

    fn violation(&self, detail: impl Into<String>) -> StreamStep {
        StreamStep::Terminal(Box::new(self.violation_evidence(detail)))
    }

    /// A violation raised without examining the payload of the named event.
    ///
    /// The event name is already decoded — it is what dispatches the handler —
    /// and only `content_block_start` carries material that can open a tool
    /// call. For every other name the discriminator settles the question, so
    /// the negative is stated rather than withheld even though the payload
    /// itself never parsed.
    fn unparsed_payload_violation(&self, event: &str, detail: impl Into<String>) -> StreamStep {
        if event == CONTENT_BLOCK_START_EVENT {
            self.undecoded_violation(detail)
        } else {
            self.violation(detail)
        }
    }

    /// Whether an error classification names no failure at all, and so adds
    /// nothing to an already-reported stop reason.
    ///
    /// Every variant is enumerated rather than compared for equality: this
    /// decides whether a provider error outranks a reported finish, so a new
    /// classification must fail to compile here and have its post-finish
    /// precedence chosen deliberately.
    fn names_no_classified_failure(kind: signalbox_model_runtime::ProviderErrorKind) -> bool {
        use signalbox_model_runtime::ProviderErrorKind as Kind;
        match kind {
            Kind::Unrecognized => true,
            Kind::CredentialRejected
            | Kind::PermissionDenied
            | Kind::InvalidRequest
            | Kind::TargetNotFound
            | Kind::RequestTooLarge
            | Kind::RateLimited
            | Kind::QuotaExhausted
            | Kind::Overloaded
            | Kind::ProviderInternal => false,
        }
    }

    fn parse<'a, T: serde::Deserialize<'a>>(
        &self,
        record: &'a SseRecord,
        event: &str,
    ) -> Result<T, Box<StreamStep>> {
        serde_json::from_str(&record.data).map_err(|error| {
            Box::new(self.unparsed_payload_violation(
                event,
                format!("malformed {event} event payload: {error}"),
            ))
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
        if self.finish.is_some() && Self::names_no_classified_failure(kind) {
            // The provider already reported why generation stopped, and this
            // event names no failure the adapter can classify, so it supersedes
            // that stop reason with nothing. Worse, it is then indistinguishable
            // from the refusal downgrade `execute` applies — an HTTP 200
            // exchange, `Unrecognized`, and the same fabricated native facts —
            // which would let a genuine failure pass as a decoded refusal. An
            // event that *does* classify still outranks the stop reason,
            // because it carries information the stop reason does not.
            return self.violation("unclassifiable error event follows the reported stop_reason");
        }
        StreamStep::Terminal(Box::new(TerminalEvidence::ProviderError(
            ProviderErrorEvidence {
                exchange: self.exchange.clone(),
                reported_model: self.reported_model.clone(),
                kind,
                non_acceptance_proven: false,
                native: error.into_native_facts(),
                usage: self.usage,
            },
        )))
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
        let Some(model) = event.message.model.as_deref() else {
            return self.violation("message_start is missing required field model");
        };
        let model = ProviderReportedModel::new(model);
        self.reported_model = Some(model.clone());
        Self::emit(
            correlation,
            sink,
            ObservationFact::ProviderModelReported(model),
        );
        if !event.message.content.is_empty() {
            return self.violation("message_start must not carry content blocks");
        }
        if event.message.stop_reason.is_some() || event.message.stop_sequence.is_some() {
            return self.violation("message_start must not carry terminal metadata");
        }
        let (Some(id), Some(usage)) = (event.message.id, event.message.usage.as_ref()) else {
            return self.violation("message_start is missing required fields (id, usage)");
        };
        if usage.input_tokens.is_none() {
            return self.violation("message_start usage is missing input_tokens");
        }
        if !iteration_usage_is_complete(usage) {
            return self.violation("message_start carries incomplete required iteration usage");
        }
        self.started = true;
        self.input_usage_reported = true;
        self.message_id = Some(ProviderMessageId::new(id));
        self.retained_input_tokens = retained_input_tokens(usage);
        let usage = convert_usage(usage);
        self.usage.absorb(usage);
        Self::emit(correlation, sink, ObservationFact::UsageReported(usage));
        StreamStep::Continue
    }

    fn apply_block_start<C: Clone>(
        &mut self,
        record: &SseRecord,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> StreamStep {
        if !self.started {
            return self.undecoded_violation("content_block_start before message_start");
        }
        if self.finish.is_some() {
            return self.undecoded_violation("content_block_start after the stop_reason");
        }
        let event: ContentBlockStartEvent = match self.parse(record, "content_block_start") {
            Ok(event) => event,
            Err(step) => return *step,
        };
        if event.event_type != "content_block_start" {
            return self
                .undecoded_violation("content_block_start payload has the wrong discriminator");
        }
        if self.open_blocks.contains_key(&event.index) || self.closed.contains_key(&event.index) {
            return self
                .undecoded_violation(format!("content_block_start reopens index {}", event.index));
        }
        let content_block = match parse_response_block(&event.content_block) {
            Ok(block) => block,
            Err(error) => {
                return self.undecoded_violation(format!(
                    "malformed content_block_start payload: {error}"
                ));
            }
        };
        let builder = match content_block {
            WireResponseBlock::Text { text } => BlockBuilder::Text(text),
            WireResponseBlock::ToolUse { id, name, input } => {
                if !self.tool_call_ids.insert(id.clone()) {
                    return self.violation(format!("stream repeats tool-call identifier {id:?}"));
                }
                let start_input = input.get().to_string();
                let documented_empty_input = start_input
                    .bytes()
                    .filter(|byte| !byte.is_ascii_whitespace())
                    .eq(b"{}".iter().copied());
                if !documented_empty_input {
                    return self.violation(
                        "streamed tool_use block start must carry the documented empty input object",
                    );
                }
                BlockBuilder::ToolUse {
                    id,
                    name,
                    start_input,
                    accumulated: String::new(),
                    argument_nesting: ProviderJsonNestingValidator::new(),
                }
            }
            WireResponseBlock::Thinking {
                thinking,
                signature,
            } => BlockBuilder::Thinking {
                text: thinking,
                // The provider's public thinking documentation states the
                // streamed shape: the block opens with empty-string
                // `thinking` and `signature` placeholder fields and the
                // real signature arrives through a later `signature_delta`
                // (under the newest models' default omitted display, that
                // single delta is the block's only content). An empty
                // opening value is therefore "not delivered yet", never a
                // first signature: counting it would reject the documented
                // shape as a duplicate. The close-time law is unchanged —
                // the block must still end with exactly one non-empty
                // signature.
                signature: signature.filter(|value| !value.is_empty()),
            },
            WireResponseBlock::RedactedThinking { data } => BlockBuilder::RedactedThinking { data },
            WireResponseBlock::Compaction { raw } => {
                if !self.provider_compaction_enabled {
                    return self.violation(format!(
                        "compaction block opened at index {}, but this operation did not enable \
                         provider compaction",
                        event.index
                    ));
                }
                let Ok(start) = serde_json::from_str::<serde_json::Value>(raw.get()) else {
                    return self.violation(format!(
                        "compaction block {} could not be reinspected",
                        event.index
                    ));
                };
                if start.get("content") != Some(&serde_json::Value::Null)
                    || start
                        .get("encrypted_content")
                        .is_some_and(|value| !value.is_null())
                {
                    return self.violation(format!(
                        "compaction block {} opened with non-placeholder content",
                        event.index
                    ));
                }
                BlockBuilder::Compaction { delta: None }
            }
            WireResponseBlock::Fallback { to_model } => {
                // This adapter never enables server-side fallback, so a
                // fallback marker mid-stream means the stream is no longer
                // the resolved target's response. Report the continuing
                // identity before terminating, so the caller's
                // provider-target rule sees the same served-target evidence
                // the buffered path preserves.
                if let Some(model) = to_model {
                    sink.observe(Observation {
                        correlation: correlation.clone(),
                        fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new(
                            model,
                        )),
                    });
                }
                return self.violation(format!(
                    "server-side fallback block opened at index {}, but this operation never \
                     enabled provider fallback",
                    event.index
                ));
            }
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
        if self.finish.is_some() {
            return self.violation("content_block_delta after the stop_reason");
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
                if value.is_empty() {
                    return self.violation("thinking block carries an empty signature delta");
                }
                if signature.is_some() {
                    return self.violation("thinking block carries more than one signature");
                }
                *signature = Some(value);
                StreamStep::Continue
            }
            (
                BlockBuilder::ToolUse {
                    accumulated,
                    argument_nesting,
                    ..
                },
                WireDelta::InputJson { partial_json },
            ) => {
                if let Err(error) = argument_nesting.validate_fragment(partial_json.as_bytes()) {
                    return self.violation(format!(
                        "tool_use block {index} arguments exceed the JSON bound: {error}"
                    ));
                }
                accumulated.push_str(&partial_json);
                StreamStep::Continue
            }
            (
                BlockBuilder::Compaction { delta },
                WireDelta::Compaction {
                    content,
                    encrypted_content,
                },
            ) => {
                if delta.is_some() {
                    return self
                        .violation("compaction block carries more than one compaction delta");
                }
                *delta = Some((content, encrypted_content));
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
        if self.finish.is_some() {
            return self.violation("content_block_stop after the stop_reason");
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
                let Some(signature) = signature.filter(|value| !value.is_empty()) else {
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
            BlockBuilder::Compaction { delta } => {
                let Some((content, encrypted_content)) = delta else {
                    return self.violation(format!(
                        "compaction block {} closed without its compaction delta",
                        event.index
                    ));
                };
                let content = match content {
                    crate::wire::WireCompactionContent::Missing => {
                        return self.violation(format!(
                            "compaction block {} closed without delta content",
                            event.index
                        ));
                    }
                    crate::wire::WireCompactionContent::Null => None,
                    crate::wire::WireCompactionContent::Text(content) if content.is_empty() => {
                        return self.violation(format!(
                            "compaction block {} closed with empty content",
                            event.index
                        ));
                    }
                    crate::wire::WireCompactionContent::Text(content) => Some(content),
                };
                let Ok(content_json) = serde_json::to_string(&content) else {
                    return self.violation(format!(
                        "compaction block {} content cannot be encoded",
                        event.index
                    ));
                };
                let Ok(encrypted_content_json) = serde_json::to_string(&encrypted_content) else {
                    return self.violation(format!(
                        "compaction block {} encrypted content cannot be encoded",
                        event.index
                    ));
                };
                let block_json = format!(
                    r#"{{"content":{content_json},"encrypted_content":{encrypted_content_json},"type":"compaction"}}"#
                );
                AssistantPart::ProviderCompaction { block_json }
            }
            BlockBuilder::ToolUse {
                id,
                name,
                start_input,
                accumulated,
                ..
            } => {
                let arguments_json = if accumulated.is_empty() {
                    start_input
                } else {
                    accumulated
                };
                if let Err(error) = validate_provider_json_nesting(arguments_json.as_bytes()) {
                    return self.violation(format!(
                        "tool_use block {} arguments exceed the JSON bound: {error}",
                        event.index
                    ));
                }
                if !serde_json::value::RawValue::from_string(arguments_json.clone())
                    .is_ok_and(|raw| crate::wire::raw_json_is_object(&raw))
                {
                    return self.violation(format!(
                        "tool_use block {} closed with arguments that are not a JSON object",
                        event.index
                    ));
                }
                // Partial JSON cannot be safely decoded for credential
                // redaction. Emit one complete delta only after validation;
                // the boundary sink can then decode JSON escapes before any
                // argument bytes leave the adapter.
                Self::emit(
                    correlation,
                    sink,
                    ObservationFact::ToolArgumentsDelta {
                        index: event.index,
                        fragment: arguments_json.clone(),
                    },
                );
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
        if self.finish.is_some() {
            return self.violation("message_delta after the stop_reason");
        }
        let event: MessageDeltaEvent = match self.parse(record, "message_delta") {
            Ok(event) => event,
            Err(step) => return *step,
        };
        if event.event_type != "message_delta" {
            return self.violation("message_delta payload has the wrong discriminator");
        }
        if let Some(delta) = event.delta {
            let Some(stop_reason) = delta.stop_reason else {
                if delta.stop_sequence.is_some() {
                    return self
                        .violation("message_delta carries a stop_sequence without a stop_reason");
                }
                if let Some(usage) = event.usage.as_ref() {
                    if !iteration_usage_is_complete(usage) {
                        return self.violation(
                            "message_delta carries incomplete required iteration usage",
                        );
                    }
                    if usage
                        .iterations
                        .as_ref()
                        .is_some_and(|items| !items.is_empty())
                    {
                        self.retained_input_tokens = retained_input_tokens(usage);
                    }
                    let usage = convert_usage(usage);
                    if event
                        .usage
                        .as_ref()
                        .and_then(|wire| wire.iterations.as_ref())
                        .is_some_and(|iterations| !iterations.is_empty())
                    {
                        self.usage = usage;
                    } else {
                        self.usage.absorb(usage);
                    }
                    Self::emit(correlation, sink, ObservationFact::UsageReported(usage));
                }
                return StreamStep::Continue;
            };
            if !self.open_blocks.is_empty() {
                return self
                    .violation("message_delta reports a stop_reason with open content blocks");
            }
            if event
                .usage
                .as_ref()
                .and_then(|usage| usage.output_tokens)
                .is_none()
            {
                return self
                    .violation("message_delta reports a stop_reason without final output usage");
            }
            if (stop_reason == "stop_sequence") != delta.stop_sequence.is_some() {
                return self
                    .violation("message_delta stop_reason contradicts its stop_sequence metadata");
            }
            if let Some(sequence) = delta.stop_sequence.as_deref()
                && !self
                    .declared_stop_sequences
                    .iter()
                    .any(|declared| declared == sequence)
            {
                return self.violation(
                    "message_delta reports a stop sequence not declared by the request",
                );
            }
            let finish = map_finish(&stop_reason, delta.stop_sequence);
            self.finish = Some(finish.clone());
            if matches!(finish, FinishReason::Unrecognized { .. }) {
                return self.violation("message_delta carries an unrecognized stop_reason");
            }
            self.final_output_usage_reported = true;
            Self::emit(correlation, sink, ObservationFact::FinishReported(finish));
        }
        if let Some(usage) = event.usage.as_ref() {
            if !iteration_usage_is_complete(usage) {
                return self.violation("message_delta carries incomplete required iteration usage");
            }
            if usage
                .iterations
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            {
                self.retained_input_tokens = retained_input_tokens(usage);
            }
            let usage = convert_usage(usage);
            if event
                .usage
                .as_ref()
                .and_then(|wire| wire.iterations.as_ref())
                .is_some_and(|iterations| !iterations.is_empty())
            {
                // Iteration usage is the complete physical-request total, not
                // a delta from the message_start input report.
                self.usage = usage;
            } else {
                self.usage.absorb(usage);
            }
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
        if matches!(finish, FinishReason::ToolUse) && !has_tool_calls {
            return self.violation("stream content contradicts its stop_reason");
        }
        let has_provider_compaction = self
            .closed
            .values()
            .any(|part| matches!(part, AssistantPart::ProviderCompaction { .. }));
        if has_provider_compaction && self.retained_input_tokens.is_none() {
            return self.violation(
                "provider compaction response omits final-iteration retained input usage",
            );
        }
        let evidence = match finish.completion_finish() {
            None => TerminalEvidence::Refused(RefusalEvidence {
                exchange: self.exchange.clone(),
                message_id: self.message_id.clone(),
                reported_model: self.reported_model.clone(),
                content: std::mem::take(&mut self.closed).into_values().collect(),
                usage: self.usage,
            }),
            Some(finish) => {
                let completion = CompletionEvidence {
                    exchange: self.exchange.clone(),
                    message_id: self.message_id.clone(),
                    reported_model: self.reported_model.clone(),
                    finish,
                    content: std::mem::take(&mut self.closed).into_values().collect(),
                    usage: self.usage,
                };
                match self.retained_input_tokens {
                    Some(retained_input_tokens) if has_provider_compaction => {
                        TerminalEvidence::CompletedWithProviderCompaction {
                            completion,
                            retained_input_tokens,
                        }
                    }
                    _ => TerminalEvidence::Completed(completion),
                }
            }
        };
        StreamStep::Terminal(Box::new(evidence))
    }
}

#[cfg(test)]
mod tests {
    use signalbox_model_runtime::{
        AssistantPart, CompletionFinish, ExchangeFacts, FinishReason, LossCause, Observation,
        ObservationFact, PROVIDER_JSON_NESTING_LIMIT, ProviderErrorKind, ProviderMessageId,
        ProviderReportedModel, ProviderRequestId, SseFraming, SseRecord, StreamInterruption,
        TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal, ToolCallsAtLoss, ToolName,
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
            retry_after: None,
        }
    }

    /// Runs byte chunks through real SSE framing and the decoder, exactly as
    /// the runtime does, correlating to `"call-1"`. Returns the terminal
    /// evidence when one record produced it (later chunks are then
    /// rejected by the panic below, keeping fixtures honest) and the
    /// observation log.
    fn drive(chunks: &[&[u8]]) -> (Option<TerminalEvidence>, Vec<Observation<String>>) {
        drive_with_provider_compaction(chunks, true)
    }

    fn drive_with_provider_compaction(
        chunks: &[&[u8]],
        provider_compaction_enabled: bool,
    ) -> (Option<TerminalEvidence>, Vec<Observation<String>>) {
        let mut framing = SseFraming::new(1024 * 1024);
        let mut decoder = StreamDecoder::with_stop_sequences(
            exchange(),
            vec!["END".to_string()],
            provider_compaction_enabled,
        );
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
                    StreamStep::Terminal(evidence) => terminal = Some(*evidence),
                }
            }
        }
        (terminal, observations)
    }

    /// Drives chunks that must not terminate, then reports the end-of-stream
    /// loss evidence for the resulting decoder state.
    fn drive_to_eof(chunks: &[&[u8]]) -> (TerminalEvidence, Vec<Observation<String>>) {
        let mut framing = SseFraming::new(1024 * 1024);
        let mut decoder =
            StreamDecoder::with_stop_sequences(exchange(), vec!["END".to_string()], true);
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
    fn message_start_rejects_incomplete_required_iteration_usage() {
        let (terminal, _) = drive(&[b"event: message_start\n\
          data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\
          \"role\":\"assistant\",\"id\":\"msg_1\",\
          \"model\":\"model-exact-1\",\"content\":[],\"usage\":{\"input_tokens\":25,\
          \"iterations\":[{\"input_tokens\":25}]}}}\n\n"]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    #[test]
    fn message_start_rejects_overflowing_iteration_usage() {
        let event = format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\
             \"type\":\"message\",\"role\":\"assistant\",\"id\":\"msg_1\",\
             \"model\":\"model-exact-1\",\"content\":[],\"usage\":{{\"input_tokens\":1,\
             \"iterations\":[{{\"input_tokens\":{},\"output_tokens\":1}},\
             {{\"input_tokens\":1,\"output_tokens\":1}}]}}}}}}\n\n",
            u64::MAX
        );
        let (terminal, _) = drive(&[event.as_bytes()]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    fn tool_input_delta(partial_json: &str) -> Vec<u8> {
        let data = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "input_json_delta",
                "partial_json": partial_json,
            },
        });
        format!("event: content_block_delta\ndata: {data}\n\n").into_bytes()
    }

    #[track_caller]
    fn assert_message_delta_is_a_protocol_violation(delta: &str) {
        let event = format!(
            "event: message_delta\ndata: {{\"type\":\"message_delta\",\
             \"delta\":{delta},\"usage\":{{\"output_tokens\":1}}}}\n\n"
        );
        let (terminal, _) = drive(&[message_start(), event.as_bytes()]);
        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
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
              \"usage\":{\"output_tokens\":7,\"iterations\":[{\"input_tokens\":25,\"output_tokens\":7}]}}\n\n",
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
        assert!(observations.contains(&Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"{"city":"Oslo"}"#.to_string(),
            },
        }));
    }

    #[test]
    fn streamed_compaction_delta_becomes_replayable_block() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"compaction\",\"content\":null,\"encrypted_content\":null}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"compaction_delta\",\"content\":\"summary\",\"encrypted_content\":\"opaque==\"}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
              \"usage\":{\"output_tokens\":7,\"iterations\":[{\"input_tokens\":25,\"output_tokens\":7}]}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);
        let Some(TerminalEvidence::CompletedWithProviderCompaction {
            completion,
            retained_input_tokens,
        }) = terminal
        else {
            panic!("compaction stream gated on message_stop must complete");
        };
        assert_eq!(retained_input_tokens, 25);
        let [AssistantPart::ProviderCompaction { block_json }] = completion.content.as_slice()
        else {
            panic!("compaction stream must retain exactly one opaque block");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(block_json)
                .expect("assembled compaction block is valid JSON"),
            serde_json::json!({
                "type": "compaction",
                "content": "summary",
                "encrypted_content": "opaque==",
            })
        );
    }

    #[test]
    fn streamed_compaction_is_rejected_when_the_request_disabled_it() {
        let (terminal, _) = drive_with_provider_compaction(
            &[
                message_start(),
                b"event: content_block_start\n\
                  data: {\"type\":\"content_block_start\",\"index\":0,\
                  \"content_block\":{\"type\":\"compaction\",\"content\":null,\"encrypted_content\":null}}\n\n",
            ],
            false,
        );

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    #[test]
    fn streamed_compaction_start_rejects_non_placeholder_material() {
        for start in [
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"compaction\",\"content\":\"summary\",\"encrypted_content\":null}}\n\n"
                .as_slice(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"compaction\",\"content\":null,\"encrypted_content\":\"opaque==\"}}\n\n"
                .as_slice(),
        ] {
            let (terminal, _) = drive(&[message_start(), start]);
            assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
        }
    }

    #[test]
    fn streamed_compaction_rejects_empty_content() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"compaction\",\"content\":null,\"encrypted_content\":null}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"compaction_delta\",\"content\":\"\",\"encrypted_content\":null}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("empty compaction content must be protocol loss");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn streamed_compaction_requires_the_delta_content_field() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"compaction\",\"content\":null,\"encrypted_content\":null}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"compaction_delta\",\"encrypted_content\":null}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    #[test]
    fn overdeep_tool_arguments_fail_on_the_first_prohibited_fragment() {
        let exact_limit = "[".repeat(PROVIDER_JSON_NESTING_LIMIT);
        let exact_limit_delta = tool_input_delta(&exact_limit);
        let prohibited_delta = tool_input_delta("[");
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\
              \"name\":\"lookup\",\"input\":{}}}\n\n",
            exact_limit_delta.as_slice(),
            prohibited_delta.as_slice(),
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("the first over-limit argument fragment must terminate decoding");
        };
        let LossCause::StreamProtocolViolation { detail } = loss.cause else {
            panic!("over-limit tool arguments must be a stream protocol violation");
        };
        let expected = format!("{PROVIDER_JSON_NESTING_LIMIT}-container nesting limit");
        assert!(detail.contains(&expected));
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
    fn duplicate_streamed_tool_call_ids_are_a_protocol_violation() {
        let (terminal, observations) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\
              \"name\":\"first\",\"input\":{}}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":1,\
              \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\
              \"name\":\"second\",\"input\":{}}}\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
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
    fn max_token_stream_retains_a_partial_tool_call() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\
              \"name\":\"lookup\",\"input\":{}}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\
              \"usage\":{\"output_tokens\":7}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        let Some(TerminalEvidence::Completed(completion)) = terminal else {
            panic!("token exhaustion with partial tool material is definitive completion");
        };
        assert_eq!(completion.finish, CompletionFinish::MaxOutputTokens);
        assert!(matches!(
            completion.content.as_slice(),
            [AssistantPart::ToolCall(_)]
        ));
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
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::NoneOpened);
        assert_eq!(loss.usage.input_tokens, Some(25));
    }

    /// a stream cut off mid-tool-call carries that fact typed.
    ///
    /// `tool_call_ids` records the opened block and is never drained, so the
    /// fact survives to the loss whether or not the call ever produced an
    /// argument delta or a proposal.
    #[test]
    fn a_stream_lost_after_a_tool_call_opened_reports_it_on_the_loss() {
        let (evidence, _) = drive_to_eof(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\
              \"name\":\"lookup\",\"input\":{}}}\n\n",
        ]);

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!("EOF before message_stop must never read as success");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Opened);
    }

    /// An event that never decoded withholds the fact rather than stating a
    /// negative: a `tool_use` block opens inside a `content_block_start`
    /// payload, so an unparsed event could itself have been one.
    #[test]
    fn an_event_that_never_decodes_withholds_the_tool_fact() {
        let (evidence, _) = drive(&[
            message_start(),
            b"event: content_block_start\ndata: {not-json\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = evidence else {
            panic!("an undecodable event is a protocol violation");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
    }

    /// The nested `content_block_start` payload is the block's own material, so
    /// a malformed one leaves exactly the tool question unexamined.
    #[test]
    fn a_malformed_content_block_start_payload_withholds_the_tool_fact() {
        let (evidence, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"tool_use\"}}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = evidence else {
            panic!("a malformed content_block_start is a protocol violation");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
    }

    /// An undecodable event does not erase a block already recorded, so the
    /// withholding above is not a blanket refusal to answer.
    #[test]
    fn an_undecodable_event_after_a_tool_call_still_reports_it() {
        let (evidence, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\
              \"name\":\"lookup\",\"input\":{}}}\n\n",
            b"event: content_block_delta\ndata: {not-json\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = evidence else {
            panic!("an undecodable event is a protocol violation");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Opened);
    }

    /// A semantic rejection of a *decoded* event still states the negative, so
    /// the withholding is scoped to material that never parsed.
    #[test]
    fn a_decoded_event_rejected_on_semantics_still_reports_none_opened() {
        let (evidence, _) = drive(&[message_start(), message_start()]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = evidence else {
            panic!("a duplicate message_start is a protocol violation");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::NoneOpened);
    }

    /// A well-formed `content_block_start` opening a text block at index 0.
    ///
    /// Plumbing: three exits below reject this same record for reasons that have
    /// nothing to do with its payload, which is exactly the point — the payload
    /// never parses, whatever the reason.
    fn text_block_start() -> &'static [u8] {
        b"event: content_block_start\n\
          data: {\"type\":\"content_block_start\",\"index\":0,\
          \"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n"
    }

    #[track_caller]
    fn assert_block_start_withholds(chunks: &[&[u8]]) {
        let (evidence, _) = drive(chunks);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = evidence else {
            panic!("a rejected content_block_start is a protocol violation");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
    }

    /// Rejected before `message_start`: the exit precedes even the outer parse,
    /// so the block payload is unexamined.
    #[test]
    fn a_content_block_start_before_message_start_withholds() {
        assert_block_start_withholds(&[text_block_start()]);
    }

    /// Rejected for its outer discriminator: the event decoded, but its inner
    /// `content_block` is still a `RawValue`.
    #[test]
    fn a_content_block_start_with_a_wrong_discriminator_withholds() {
        assert_block_start_withholds(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"quasar\",\"index\":0,\
              \"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        ]);
    }

    /// Rejected for reopening an index: likewise decided before the payload.
    #[test]
    fn a_content_block_start_reopening_an_index_withholds() {
        assert_block_start_withholds(&[message_start(), text_block_start(), text_block_start()]);
    }

    /// A payload that never parsed still states the negative when its event
    /// name precludes a tool call: the name is decoded, and only
    /// `content_block_start` opens one.
    #[test]
    fn an_unparsed_non_start_event_payload_states_the_negative() {
        let (evidence, _) = drive(&[
            message_start(),
            b"event: message_delta\ndata: {not-json\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = evidence else {
            panic!("an unparsed event payload is a protocol violation");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::NoneOpened);
    }

    /// The same unparsed payload under the one event name that can open a tool
    /// call withholds, which is what makes the rule a statement about the name.
    #[test]
    fn an_unparsed_content_block_start_payload_withholds() {
        let (evidence, _) = drive(&[
            message_start(),
            b"event: content_block_start\ndata: {not-json\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = evidence else {
            panic!("an unparsed event payload is a protocol violation");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
    }

    /// A record with no event name at all withholds: without a name the rule
    /// above has nothing to decide on.
    #[test]
    fn a_record_without_an_event_name_withholds() {
        let (evidence, _) = drive(&[message_start(), b"data: {\"type\":\"ping\"}\n\n"]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = evidence else {
            panic!("a record without an event name is a protocol violation");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
    }

    /// A `content_block_delta` cannot open a tool call — it carries arguments
    /// for a block already opened — so its rejection states the negative. This
    /// is what keeps the withholding above scoped to `content_block_start`.
    #[test]
    fn a_rejected_content_block_delta_still_reports_none_opened() {
        let (evidence, _) = drive(&[
            message_start(),
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":9,\
              \"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = evidence else {
            panic!("a delta for an unopened index is a protocol violation");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::NoneOpened);
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
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"},\
              \"usage\":{\"output_tokens\":2}}\n\n",
        ]);

        let TerminalEvidence::BoundaryLoss(loss) = evidence else {
            panic!(
                "an incomplete exchange must not classify as refusal (classification precondition)"
            );
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
    fn usage_only_iteration_totals_replace_all_earlier_axes() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":3,\"output_tokens\":4,\
              \"iterations\":[{\"input_tokens\":3,\"output_tokens\":4}]}}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
              \"usage\":{\"output_tokens\":4}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);
        let Some(TerminalEvidence::Completed(completion)) = terminal else {
            panic!("complete iteration usage must remain completion evidence");
        };
        assert_eq!(completion.usage.input_tokens, Some(3));
        assert_eq!(completion.usage.output_tokens, Some(4));
        assert_eq!(completion.usage.cache_creation_input_tokens, None);
        assert_eq!(completion.usage.cache_read_input_tokens, None);
    }

    #[test]
    fn earlier_output_usage_does_not_satisfy_the_terminal_delta() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":11}}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
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
    fn overdeep_unknown_event_payload_is_a_protocol_violation() {
        let nested = format!(
            "{}null{}",
            "[".repeat(PROVIDER_JSON_NESTING_LIMIT + 1),
            "]".repeat(PROVIDER_JSON_NESTING_LIMIT + 1)
        );
        let event = format!(
            "event: future_event\ndata: {{\"type\":\"future_event\",\"raw\":{nested}}}\n\n"
        );
        let (terminal, _) = drive(&[event.as_bytes()]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("overdeep unknown event material must not bypass the depth bound");
        };
        let LossCause::StreamProtocolViolation { detail } = loss.cause else {
            panic!("deep SSE JSON must surface as a stream protocol violation");
        };
        let expected = format!("{PROVIDER_JSON_NESTING_LIMIT}-container nesting limit");
        assert!(detail.contains(&expected));
    }

    /// The provider identities the stream reported, in observation order.
    fn reported_models(observations: &[Observation<String>]) -> Vec<&str> {
        observations
            .iter()
            .filter_map(|observation| match &observation.fact {
                ObservationFact::ProviderModelReported(reported) => Some(reported.as_str()),
                _ => None,
            })
            .collect()
    }

    /// S20: a streamed server-side fallback marker terminates the stream, and
    /// the continuing identity still reaches the caller — the same served-target
    /// evidence the buffered path preserves, so the provider-target rule can
    /// classify the substitution rather than seeing generic ambiguity.
    #[test]
    fn s20_streamed_fallback_block_reports_the_substituting_model() {
        let (terminal, observations) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":\
              {\"type\":\"fallback\",\"from\":{\"model\":\"model-exact-1\"},\
              \"to\":{\"model\":\"substitute-model-2\"}}}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a streamed fallback marker must terminate the stream");
        };
        let LossCause::StreamProtocolViolation { detail } = loss.cause else {
            panic!("an unrequested fallback block is a protocol violation");
        };
        assert!(detail.contains("server-side fallback block"));
        assert_eq!(
            reported_models(&observations),
            vec!["model-exact-1", "substitute-model-2"],
            "both the envelope identity and the substituting identity reach the caller"
        );
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
        assert!(!error.non_acceptance_proven);
        assert_eq!(
            error.native.error_token,
            Some("overloaded_error".to_string())
        );
        assert_eq!(error.exchange, exchange());
        assert_eq!(error.usage.input_tokens, Some(25));
    }

    #[test]
    fn a_classified_error_event_after_the_stop_reason_stays_definitive() {
        // The other half of the post-finish rule, and the half a blanket
        // `self.finish.is_some()` condition would silently break: a typed
        // error still outranks a reported stop reason, because it names a
        // failure the stop reason does not.
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
              \"usage\":{\"output_tokens\":2}}\n\n",
            b"event: error\n\
              data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\
              \"message\":\"Overloaded\"}}\n\n",
        ]);

        let Some(TerminalEvidence::ProviderError(error)) = terminal else {
            panic!("a classified error event outranks the reported stop reason");
        };
        // Classification is the whole subject here: it is what decides
        // precedence over the reported stop reason. Native-token propagation
        // is a separate fact, already covered by
        // `mid_stream_error_event_is_definitive_provider_error_evidence`, so
        // re-asserting the fixture's token spelling would only couple this
        // case to a literal it does not care about.
        assert_eq!(error.kind, ProviderErrorKind::Overloaded);
    }

    #[test]
    fn an_unclassifiable_error_event_after_the_stop_reason_is_protocol_loss() {
        // A *typed* error event still outranks a reported stop reason, because
        // it carries information the stop reason does not (the sibling above).
        // One whose type classifies as nothing carries none, and would reach
        // the caller wearing the exact shape `execute` gives a downgraded
        // refusal — HTTP 200, `Unrecognized`, and the same fabricated
        // `error_token` — so a genuine failure could pass as a decoded
        // refusal. It must stay a protocol violation.
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"},\
              \"usage\":{\"output_tokens\":2}}\n\n",
            b"event: error\n\
              data: {\"type\":\"error\",\"error\":{\"type\":\"refusal\"}}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("an unclassifiable post-stop-reason error event is protocol loss");
        };
        assert_eq!(
            loss.cause,
            LossCause::StreamProtocolViolation {
                detail: "unclassifiable error event follows the reported stop_reason".to_string()
            }
        );
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
    fn stop_reason_with_an_open_block_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
              \"usage\":{\"output_tokens\":1}}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a stop reason with an open block must surface as a protocol violation");
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
    fn nonempty_streamed_tool_start_input_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\
              \"name\":\"lookup\",\"input\":{\"city\":\"Oslo\"}}}\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
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
    fn message_start_with_terminal_metadata_is_a_protocol_violation() {
        let (terminal, observations) = drive(&[b"event: message_start\n\
              data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\
              \"role\":\"assistant\",\"id\":\"msg_1\",\"model\":\"model-exact-1\",\
              \"content\":[],\"stop_reason\":\"refusal\",\"stop_sequence\":null,\
              \"usage\":{\"input_tokens\":1}}}\n\n"]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("opening terminal metadata must not be silently discarded");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
        assert_eq!(
            loss.reported_model,
            Some(ProviderReportedModel::new("model-exact-1"))
        );
        assert!(observations.iter().any(|observation| matches!(
            &observation.fact,
            ObservationFact::ProviderModelReported(model)
                if model.as_str() == "model-exact-1"
        )));
    }

    #[test]
    fn stop_sequence_reason_without_sequence_is_a_protocol_violation() {
        assert_message_delta_is_a_protocol_violation(
            r#"{"stop_reason":"stop_sequence","stop_sequence":null}"#,
        );
    }

    #[test]
    fn sequence_metadata_with_a_different_reason_is_a_protocol_violation() {
        assert_message_delta_is_a_protocol_violation(
            r#"{"stop_reason":"end_turn","stop_sequence":"END"}"#,
        );
    }

    #[test]
    fn stop_sequence_without_a_reason_is_a_protocol_violation() {
        assert_message_delta_is_a_protocol_violation(r#"{"stop_sequence":"END"}"#);
    }

    #[test]
    fn undeclared_stop_sequence_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"stop_sequence\",\
              \"stop_sequence\":\"OTHER\"},\"usage\":{\"output_tokens\":1}}\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    #[test]
    fn empty_thinking_signature_delta_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":null}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"signature_delta\",\"signature\":\"\"}}\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    /// The Claude 5-family streamed tool-turn shape the provider's public
    /// thinking documentation states for the default omitted display, with
    /// synthetic content: the thinking block opens with empty-string
    /// `thinking` and `signature` placeholders, a single `signature_delta`
    /// delivers the real signature with no thinking deltas at all, and the
    /// tool_use start carries a `caller` field with its sole
    /// `input_json_delta` empty. Rejecting the placeholder as a first
    /// signature is the regression that wedged every streamed sonnet-5 tool
    /// turn as ambiguous.
    #[test]
    fn five_family_thinking_tool_stream_with_placeholder_signature_completes() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            b"event: ping\ndata: {\"type\": \"ping\"}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_synthetic_1\"}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":1,\
              \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\
              \"name\":\"current_time\",\"input\":{},\"caller\":{\"type\":\"direct\"}}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":1,\
              \"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\"}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\
              \"stop_sequence\":null,\"stop_details\":null},\
              \"usage\":{\"output_tokens\":5,\
              \"output_tokens_details\":{\"thinking_tokens\":0}}}\n\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);

        let Some(TerminalEvidence::Completed(completion)) = terminal else {
            panic!("the 5-family placeholder-signature tool stream must complete");
        };
        assert_eq!(completion.finish, CompletionFinish::ToolUse);
        assert_eq!(
            completion.content,
            vec![
                AssistantPart::Thinking {
                    text: String::new(),
                    signature: Some("sig_synthetic_1".to_string()),
                },
                AssistantPart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("toolu_1"),
                    name: ToolName::new("current_time"),
                    arguments_json: "{}".to_string(),
                }),
            ]
        );
    }

    /// The duplicate-signature law survives the placeholder tolerance: a
    /// non-empty opening signature is a delivered first signature, so a
    /// later `signature_delta` is still one signature too many.
    #[test]
    fn signature_delta_after_a_nonempty_start_signature_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\
              \"signature\":\"sig_synthetic_1\"}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_synthetic_2\"}}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a second signature must remain a protocol violation");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn non_object_streamed_tool_arguments_are_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\
              \"name\":\"lookup\",\"input\":{}}}\n\n",
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\
              \"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"[]\"}}\n\n",
            b"event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    #[test]
    fn a_second_stop_reason_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"},\
              \"usage\":{\"output_tokens\":1}}\n\n",
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
              \"usage\":{\"output_tokens\":1}}\n\n",
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
    fn content_events_after_the_stop_reason_are_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
              \"usage\":{\"output_tokens\":1}}\n\n",
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\
              \"content_block\":{\"type\":\"text\",\"text\":\"late\"}}\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    #[test]
    fn malformed_message_stop_payload_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            message_start(),
            b"event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
              \"usage\":{\"output_tokens\":1}}\n\n",
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
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
              \"usage\":{\"output_tokens\":1}}\n\n",
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
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
              \"usage\":{\"output_tokens\":1}}\n\n",
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
