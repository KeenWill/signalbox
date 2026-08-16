//! Chat Completions stream decoding with terminal-integrity evidence.
//!
//! The stream's terminal marker is the literal `[DONE]` data record. The
//! decoder accumulates content, refusal, and per-index tool-call fragments,
//! and only a `[DONE]` preceded by a reported finish reason yields terminal
//! success or refusal evidence. A stream that ends any other way is explicit
//! incomplete-stream or protocol-violation evidence with the partial facts
//! retained — never silent success (the ambiguous branch of
//! `docs/spec/model-call-execution.md`).
//!
//! Because `stream_options.include_usage` is always requested (see the
//! request translation), a conforming stream reports usage before `[DONE]`;
//! a usage-only chunk carries empty `choices` and is absorbed as a usage
//! observation.

use std::collections::{BTreeMap, BTreeSet};

use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CompletionEvidence, ExchangeFacts, FinishReason,
    LossCause, NativeErrorFacts, Observation, ObservationFact, ObservationSink,
    ProviderErrorEvidence, ProviderJsonNestingValidator, ProviderReportedModel, RefusalEvidence,
    SseRecord, StreamInterruption, TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal,
    ToolCallsAtLoss, ToolName, validate_provider_json_nesting,
};

/// The violation detail the deferred unrecognized-finish verdict reports.
///
/// Identifies *which* violation this is, and nothing more. Whether a tool call
/// was involved rides `BoundaryLossEvidence::tool_calls`, so this string is no
/// longer varied and no caller reads a suffix off it.
///
/// It is still a rendered string, which the terminal-evidence rule in
/// `docs/spec/runtime-substrate.md` would rather no caller classified on. It
/// survives because the loss vocabulary has no typed way to say *this*
/// violation: `LossCause::StreamProtocolViolation` covers every stream defect,
/// and a stream that reports `length` and then trips a different defect before
/// `[DONE]` reaches identical typed evidence — same cause, same retained
/// finish, same tool fact. Closing that needs a `LossCause` variant of its own
/// and the durable operator-facing token that comes with it, which is a
/// deliberate vocabulary decision rather than a detail of this change.
pub const OUTPUT_CEILING_VIOLATION_DETAIL: &str = "stream carries an unrecognized finish_reason";

use crate::response::{StopSequences, convert_usage, map_finish};
use crate::status::classify_error_envelope;
use crate::translate::is_valid_function_name;
use crate::wire::ChatChunk;

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

#[derive(Default)]
struct ToolBuilder {
    id: Option<String>,
    name: Option<String>,
    saw_function_type: bool,
    saw_arguments: bool,
    arguments: String,
    argument_nesting: ProviderJsonNestingValidator,
}

/// Incremental decoder for one chat-completion stream.
pub(crate) struct StreamDecoder {
    exchange: ExchangeFacts,
    completion_id: Option<String>,
    reported_model: Option<ProviderReportedModel>,
    stop_sequences: StopSequences,
    saw_assistant_role: bool,
    usage: TokenUsage,
    finish: Option<FinishReason>,
    content_text: String,
    refusal_text: String,
    tool_builders: BTreeMap<u32, ToolBuilder>,
    completed_tools: Vec<ToolCallProposal>,
    /// Sticky: at least one tool call was announced by the provider. Neither
    /// `tool_builders` (emptied by `finalize_tools`) nor `completed_tools`
    /// (populated only there) survives every loss path.
    opened_tool_calls: bool,
    discarded_unexamined_bytes: bool,
    later_records: LaterRecords,
    final_usage_reported: bool,
    /// The violation an unrecognized finish will report at `[DONE]`. Held
    /// rather than returned so records following that finish are still
    /// examined — a definitive error among them must supersede it.
    pending_unrecognized_finish: Option<String>,
}

impl StreamDecoder {
    pub(crate) fn new(exchange: ExchangeFacts, stop_sequences: StopSequences) -> Self {
        Self {
            exchange,
            completion_id: None,
            reported_model: None,
            stop_sequences,
            saw_assistant_role: false,
            usage: TokenUsage::unreported(),
            finish: None,
            content_text: String::new(),
            refusal_text: String::new(),
            tool_builders: BTreeMap::new(),
            completed_tools: Vec::new(),
            opened_tool_calls: false,
            discarded_unexamined_bytes: false,
            later_records: LaterRecords::AllApplied,
            final_usage_reported: false,
            pending_unrecognized_finish: None,
        }
    }

    /// Applies one framed record.
    pub(crate) fn apply<C: Clone>(
        &mut self,
        record: &SseRecord,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> StreamStep {
        if record.data == "[DONE]" {
            return self.apply_done();
        }
        if let Err(error) = validate_provider_json_nesting(record.data.as_bytes()) {
            return self
                .undecoded_violation(format!("stream chunk exceeds the JSON bound: {error}"));
        }
        let chunk: ChatChunk = match serde_json::from_str(&record.data) {
            Ok(chunk) => chunk,
            Err(error) => {
                return self
                    .undecoded_violation(format!("malformed stream chunk payload: {error}"));
            }
        };
        // Recorded the moment the chunk deserializes, before any of the checks
        // below can end the stream, and never cleared: a decoded tool-call delta
        // is the provider demonstrably opening a call, whatever else about the
        // chunk is then rejected. Every validation between here and the choice
        // loop returns early — the conflicting-id, reported-model, final-usage,
        // object-type, choice-count, missing-id, post-finish, and choice-index
        // checks — so a flag set inside that loop would report "none opened" for
        // a chunk whose own bytes carry the announcement.
        if chunk.choices.iter().any(|choice| {
            choice
                .delta
                .as_ref()
                .is_some_and(|delta| !delta.tool_calls.is_empty())
        }) {
            self.opened_tool_calls = true;
        }
        if chunk.error.is_some()
            && let (Some(existing), Some(reported)) = (&self.completion_id, &chunk.id)
            && existing != reported
        {
            return self.violation("stream chunks report conflicting completion ids");
        }
        if chunk.error.is_some()
            && let Some(terminal) =
                self.apply_reported_model(chunk.model.as_deref(), correlation, sink)
        {
            return terminal;
        }
        if let Some(error) = chunk.error {
            // A mid-stream error record is a definitive provider error;
            // with no HTTP status of its own it classifies by native code.
            let code = error.code_text();
            let kind = classify_error_envelope(0, code.as_deref(), error.error_type.as_deref());
            let native = error.into_native_facts();
            if self.finish.is_some() && native == NativeErrorFacts::default() {
                // The provider already reported why generation stopped, and
                // this record carries no native material at all, so it
                // supersedes that finish with nothing a caller could act on.
                // It is also byte-identical to the refusal downgrade `execute`
                // applies — an HTTP 200 exchange, `Unrecognized`, and empty
                // native facts — which would let a genuine failure pass as a
                // decoded refusal.
                //
                // Keyed on the native material rather than the classification:
                // an error whose type or code is merely *unfamiliar* still
                // classifies `Unrecognized`, but it carries diagnostics worth
                // keeping and cannot be confused with the downgrade, so it
                // stays definitive provider evidence.
                return self
                    .violation("contentless error record follows the reported finish_reason");
            }
            return StreamStep::Terminal(Box::new(TerminalEvidence::ProviderError(
                ProviderErrorEvidence {
                    exchange: self.exchange.clone(),
                    reported_model: self.reported_model.clone(),
                    kind,
                    non_acceptance_proven: false,
                    native,
                    usage: self.usage,
                },
            )));
        }
        if self.final_usage_reported {
            return self.violation("stream record follows the requested final usage chunk");
        }
        if chunk.object.as_deref() != Some("chat.completion.chunk") {
            return self.violation("stream chunk is not a chat.completion.chunk object");
        }
        if chunk.choices.len() > 1 {
            return self.violation(format!(
                "stream chunk carries {} choices; at most one is permitted",
                chunk.choices.len()
            ));
        }
        let usage_only = chunk.choices.is_empty();
        let Some(id) = chunk.id else {
            return self.violation("stream chunk carries no completion id");
        };
        match &self.completion_id {
            None => self.completion_id = Some(id),
            Some(existing) if existing != &id => {
                return self.violation("stream chunks report conflicting completion ids");
            }
            Some(_) => {}
        }
        if let Some(terminal) = self.apply_reported_model(chunk.model.as_deref(), correlation, sink)
        {
            return terminal;
        }
        if let Some(usage) = chunk.usage.as_ref() {
            if usage_only && usage.prompt_tokens.is_some() && usage.completion_tokens.is_some() {
                if self.finish.is_none() {
                    return self.violation("final usage chunk precedes the finish_reason");
                }
                self.final_usage_reported = true;
            }
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
            if choice.index != Some(0) {
                return self.violation(format!(
                    "stream chunk carries choice index {:?}; exactly one choice is requested",
                    choice.index
                ));
            }
            if let Some(delta) = choice.delta {
                // The tool fact for this delta was already recorded by the
                // chunk-level pre-scan above; the decoder's own tool state
                // cannot answer for it later, since `tool_builders` is emptied
                // by `finalize_tools` and an entry is created only after the
                // index and type checks below pass.
                let mut known_indices: BTreeSet<u32> = self.tool_builders.keys().copied().collect();
                let Ok(mut next_index) = u32::try_from(known_indices.len()) else {
                    return self.violation("stream carries too many tool-call indices");
                };
                for call in &delta.tool_calls {
                    let Some(index) = call.index else {
                        return self.violation("streamed tool call carries no index");
                    };
                    if known_indices.insert(index) {
                        if index != next_index {
                            return self.violation(format!(
                                "streamed tool call index {index} is sparse; expected {next_index}"
                            ));
                        }
                        let Some(successor) = next_index.checked_add(1) else {
                            return self.violation("streamed tool call index space is exhausted");
                        };
                        next_index = successor;
                    }
                }
                if let Some(role) = delta.role {
                    if role != "assistant" {
                        return self.violation(format!(
                            "stream delta carries role {role:?}; assistant is required"
                        ));
                    }
                    self.saw_assistant_role = true;
                }
                if let Some(text) = delta.content
                    && !text.is_empty()
                {
                    if !self.refusal_text.is_empty() {
                        return self.violation("content delta follows refusal fragments");
                    }
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
                if let Some(refusal) = delta.refusal
                    && !refusal.is_empty()
                {
                    if !self.tool_builders.is_empty() || !delta.tool_calls.is_empty() {
                        return self.violation("refusal fragments cannot accompany tool calls");
                    }
                    let index = u32::from(!self.content_text.is_empty());
                    self.refusal_text.push_str(&refusal);
                    Self::emit(
                        correlation,
                        sink,
                        ObservationFact::TextDelta {
                            index,
                            text: refusal,
                        },
                    );
                }
                for call in delta.tool_calls {
                    let Some(call_index) = call.index else {
                        return self.violation("streamed tool call carries no index");
                    };
                    match call.kind.as_deref() {
                        Some("function") => {
                            self.tool_builders
                                .entry(call_index)
                                .or_default()
                                .saw_function_type = true;
                        }
                        Some(kind) => {
                            // The buffered decoder rejects non-function tool
                            // material; the streamed path must not assemble
                            // it into an ordinary proposal either.
                            return self.violation(format!(
                                "tool call at index {} carries unrecognized type {kind:?}",
                                call_index
                            ));
                        }
                        None => {}
                    }
                    let builder = self.tool_builders.entry(call_index).or_default();
                    if let Some(id) = call.id {
                        match &builder.id {
                            None => builder.id = Some(id),
                            Some(existing) if *existing != id => {
                                return self.violation(format!(
                                    "tool call at index {} reports conflicting ids",
                                    call_index
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
                                        call_index
                                    ));
                                }
                                Some(_) => {}
                            }
                        }
                        if let Some(fragment) = function.arguments {
                            let builder = self.tool_builders.entry(call_index).or_default();
                            builder.saw_arguments = true;
                            if let Err(error) = builder
                                .argument_nesting
                                .validate_fragment(fragment.as_bytes())
                            {
                                return self.violation(format!(
                                    "tool call at index {call_index} arguments exceed the JSON bound: {error}"
                                ));
                            }
                            builder.arguments.push_str(&fragment);
                            if !fragment.is_empty() {
                                let text_parts = u32::from(!self.content_text.is_empty());
                                let Some(index) = text_parts.checked_add(call_index) else {
                                    return self
                                        .violation("tool-argument observation index overflows");
                                };
                                Self::emit(
                                    correlation,
                                    sink,
                                    ObservationFact::ToolArgumentsDelta {
                                        // Part order: the text part (when one
                                        // exists) at 0, then tool call k. Stable
                                        // because content cannot arrive after
                                        // tool fragments (violation above).
                                        index,
                                        fragment,
                                    },
                                );
                            }
                        }
                    }
                }
            }
            if let Some(token) = choice.finish_reason {
                let mut finish = map_finish(&token, self.stop_sequences);
                if matches!(finish, FinishReason::Unrecognized { .. }) {
                    // The verdict is recorded here but *deferred* to `[DONE]`,
                    // so records that follow are still examined. Returning at
                    // once would let a stream report an output bound and then
                    // announce a definitive error that nobody ever consumed —
                    // the post-finish error rule could never fire for this
                    // finish, and a caller would accept the prefix as a clean
                    // ceiling stop.
                    //
                    // The two envelope checks that are already decidable run
                    // now rather than at `[DONE]`, so a malformed envelope
                    // reports the envelope defect and carries no
                    // `finish_reported`. A caller cannot otherwise tell
                    // "healthy stream hit an output bound" from "the envelope
                    // was never well formed and also said `length`".
                    if !self.saw_assistant_role {
                        return self.violation(
                            "stream terminated without establishing the assistant role",
                        );
                    }
                    if self.reported_model.is_none() {
                        return self.violation("stream terminated without a model identity");
                    }
                    // Accumulated tool state is deliberately *not* a reason to
                    // withhold the finish. A request carrying tools can
                    // legitimately exhaust the output ceiling partway through a
                    // tool call, so `length` is then an observed fact about the
                    // response rather than a contradiction: the buffered
                    // decoder keeps it in exactly that case, and a finish
                    // observed before a stream loss must survive in
                    // `finish_reported`. Both rules are stated in the
                    // unrecognized-finish paragraph of the runtime-substrate
                    // specification, which owns this behavior.
                    //
                    // Whether a tool call was involved rides
                    // `BoundaryLossEvidence::tool_calls`, not this detail. That
                    // fact needs a channel because a call opened here may emit
                    // no observation at all — `ToolArgumentsDelta` needs a
                    // non-empty fragment and `ToolCallProposed` needs
                    // `finalize_tools`, which the deferred verdict returns ahead
                    // of — but the channel is the typed field, so the detail
                    // stays a rendered diagnostic no caller classifies on.
                    self.finish = Some(finish);
                    self.pending_unrecognized_finish =
                        Some(OUTPUT_CEILING_VIOLATION_DETAIL.to_string());
                    return StreamStep::Continue;
                }
                if !self.refusal_text.is_empty() {
                    // Accumulated refusal material is the provider's refusal
                    // outcome; the observation must match the terminal
                    // evidence (the buffered path normalizes identically).
                    finish = FinishReason::Refusal;
                }
                self.finish = Some(finish.clone());
                let has_tool_calls = !self.tool_builders.is_empty();
                if (matches!(finish, FinishReason::ToolUse) && !has_tool_calls)
                    || (has_tool_calls && !matches!(finish, FinishReason::ToolUse))
                {
                    return self
                        .violation("tool-call content does not match the reported finish_reason");
                }
                // The choice is complete here, so its proposals are final:
                // emit them before announcing the finish, in index order.
                if let Some(step) = self.finalize_tools(correlation, sink) {
                    return step;
                }
                Self::emit(correlation, sink, ObservationFact::FinishReported(finish));
            }
        }
        StreamStep::Continue
    }

    /// Whether a tool call had opened in the records decoded so far.
    ///
    /// Every record that deserialized was scanned for tool material before any
    /// check could reject it, so across decoded records the negative case is
    /// the stated fact rather than an absence. Records that never deserialized
    /// are answered by [`Self::tool_calls_at_decode_failure`] instead.
    fn tool_calls_at_loss(&self) -> ToolCallsAtLoss {
        if self.opened_tool_calls {
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

    /// The tool fact for a violation raised by a record that never decoded.
    ///
    /// A record rejected by the JSON bound or by `ChatChunk` deserialization —
    /// and a stream whose framing ended inside an incomplete record — was never
    /// scanned, so it could itself have carried the tool-call delta. "None
    /// opened" would claim a negative about bytes the decoder never read. A
    /// tool call an earlier record already established still stands.
    fn tool_calls_at_decode_failure(&self) -> ToolCallsAtLoss {
        if self.opened_tool_calls {
            ToolCallsAtLoss::Opened
        } else {
            ToolCallsAtLoss::Unobserved
        }
    }

    /// Protocol-violation evidence for a record that never decoded.
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

    /// Evidence for a stream that ended without `[DONE]`.
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

    fn apply_reported_model<C: Clone>(
        &mut self,
        reported: Option<&str>,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> Option<StreamStep> {
        let model = ProviderReportedModel::new(reported?);
        match &self.reported_model {
            None => {
                self.reported_model = Some(model.clone());
                Self::emit(
                    correlation,
                    sink,
                    ObservationFact::ProviderModelReported(model),
                );
                None
            }
            Some(existing) if *existing != model => {
                // A spliced or corrupted stream reporting a second identity
                // must not complete or become an ordinary provider failure
                // under the first identity (the identity-precedence rule in
                // `docs/spec/runtime-substrate.md`).
                Some(self.violation("stream chunks report conflicting model identities"))
            }
            Some(_) => None,
        }
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
        // Indices must be contiguous from zero: terminal content is
        // assembled densely, so a sparse index would desynchronize the
        // already-reported fragment positions from the final parts.
        for (expected, actual) in builders.keys().enumerate() {
            if *actual != expected as u32 {
                return Some(self.violation(format!(
                    "tool call indices are not contiguous from zero (found {actual})"
                )));
            }
        }
        let mut tool_ids = BTreeSet::new();
        for (index, builder) in builders {
            let (Some(id), Some(name)) = (builder.id, builder.name) else {
                return Some(self.violation(format!(
                    "tool call at index {index} terminated without an id and name"
                )));
            };
            if !is_valid_function_name(&name) {
                return Some(self.violation(format!(
                    "tool call at index {index} carries invalid function name {name:?}"
                )));
            }
            if !tool_ids.insert(id.clone()) {
                return Some(
                    self.violation(format!("streamed tool calls repeat identifier {id:?}")),
                );
            }
            if !builder.saw_function_type {
                // Parity with the buffered decoder: material that never
                // established the function type is not an ordinary proposal.
                return Some(self.violation(format!(
                    "tool call at index {index} terminated without establishing its type"
                )));
            }
            if !builder.saw_arguments {
                return Some(self.violation(format!(
                    "tool call at index {index} terminated without reporting arguments"
                )));
            }
            if let Err(error) = validate_provider_json_nesting(builder.arguments.as_bytes()) {
                return Some(self.violation(format!(
                    "tool call at index {index} arguments exceed the JSON bound: {error}"
                )));
            }
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
        if let Some(detail) = self.pending_unrecognized_finish.take() {
            // Nothing between the finish and `[DONE]` superseded it, so the
            // deferred verdict stands. Reported here rather than at the finish
            // chunk so a trailing error record gets its chance first.
            //
            // The requested final usage chunk is still required: deferring to
            // `[DONE]` means it normally arrives, so a stream that omits it is
            // failing the `include_usage` contract and must not pass as an
            // ordinary stop at an output bound.
            if !self.final_usage_reported {
                return self.violation("stream terminated without the requested final usage chunk");
            }
            return self.violation(detail);
        }
        if !self.saw_assistant_role {
            return self.violation("stream terminated without establishing the assistant role");
        }
        if self.reported_model.is_none() {
            return self.violation("stream terminated without a model identity");
        }
        if !self.final_usage_reported {
            return self.violation("stream terminated without the requested final usage chunk");
        }
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
                    message_id: None,
                    reported_model: self.reported_model.clone(),
                    content,
                    usage: self.usage,
                })
            }
            Some(finish) => TerminalEvidence::Completed(CompletionEvidence {
                exchange: self.exchange.clone(),
                message_id: None,
                reported_model: self.reported_model.clone(),
                finish,
                content,
                usage: self.usage,
            }),
        };
        StreamStep::Terminal(Box::new(evidence))
    }
}

#[cfg(test)]
mod tests {
    use signalbox_model_runtime::{
        AssistantPart, BoundaryLossEvidence, CompletionFinish, ExchangeFacts, FinishReason,
        LossCause, Observation, ObservationFact, PROVIDER_JSON_NESTING_LIMIT, ProviderErrorKind,
        ProviderReportedModel, ProviderRequestId, SseFraming, SseRecord, StreamInterruption,
        TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal, ToolCallsAtLoss, ToolName,
    };

    use signalbox_model_runtime::ProviderErrorEvidence;

    use super::{StreamDecoder, StreamStep};
    use crate::response::StopSequences;

    /// Larger than any fixture record in this module. These tests exercise the
    /// decoder, not the framer's record bound, so the value only has to be out
    /// of reach — the bound's own behavior is covered in the framer's tests.
    const AMPLE_RECORD_LIMIT: usize = 1024 * 1024;

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
    /// the runtime does, correlating to `"call-1"`.
    fn drive(chunks: &[&[u8]]) -> (Option<TerminalEvidence>, Vec<Observation<String>>) {
        drive_with_stop_sequences(chunks, StopSequences::NotDeclared)
    }

    fn drive_with_stop_sequences(
        chunks: &[&[u8]],
        stop_sequences: StopSequences,
    ) -> (Option<TerminalEvidence>, Vec<Observation<String>>) {
        let mut framing = SseFraming::new(AMPLE_RECORD_LIMIT);
        let mut decoder = StreamDecoder::new(exchange(), stop_sequences);
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
        let mut framing = SseFraming::new(AMPLE_RECORD_LIMIT);
        let mut decoder = StreamDecoder::new(exchange(), StopSequences::NotDeclared);
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
        b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"model\":\"model-exact-1\",\
          \"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n"
    }

    fn final_usage_chunk() -> &'static [u8] {
        b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[],\
          \"usage\":{\"prompt_tokens\":25,\"completion_tokens\":7}}\n\n"
    }

    #[track_caller]
    fn assert_statusless_error_classifies(token: &str, expected: ProviderErrorKind) {
        let record =
            format!("data: {{\"error\":{{\"message\":\"failed\",\"type\":\"{token}\"}}}}\n\n");
        let (terminal, _) = drive(&[first_chunk(), record.as_bytes()]);
        let Some(TerminalEvidence::ProviderError(error)) = terminal else {
            panic!("a statusless stream error is definitive provider evidence");
        };
        assert_eq!(error.kind, expected, "native token {token}");
    }

    /// Reports the boundary loss a fixture ended on.
    #[track_caller]
    fn loss_of(terminal: Option<TerminalEvidence>) -> BoundaryLossEvidence {
        match terminal {
            Some(TerminalEvidence::BoundaryLoss(loss)) => loss,
            other => panic!("fixture expected boundary loss, got {other:?}"),
        }
    }

    /// an output-bound stop reached after a tool call opened is
    /// distinguishable from a plain one without reading the violation detail.
    ///
    /// The provider announces a call's id and name and is then cut off by the
    /// output bound before any argument fragment, so neither
    /// `ToolArgumentsDelta` (needs a non-empty fragment) nor `ToolCallProposed`
    /// (needs `finalize_tools`, which the deferred verdict returns ahead of in
    /// `apply_done`) is emitted. The observation stream cannot answer the
    /// question and the loss evidence must.
    #[test]
    fn a_tool_call_opened_before_an_unrecognized_finish_is_typed_on_the_loss() {
        let (terminal, observations) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\"}}]}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            final_usage_chunk(),
            b"data: [DONE]\n\n",
        ]);

        let loss = loss_of(terminal);
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Opened);
        assert_eq!(
            loss.finish_reported,
            Some(FinishReason::Unrecognized {
                provider_token: "length".to_string(),
            })
        );
        assert!(
            !observations.iter().any(|observation| matches!(
                observation.fact,
                ObservationFact::ToolCallProposed(_) | ObservationFact::ToolArgumentsDelta { .. }
            )),
            "the opened call reaches no observation, so only the loss carries it"
        );
    }

    /// The same stop with no tool call opened reports the other fact, so the
    /// two are told apart by type rather than by the rendered detail.
    #[test]
    fn an_unrecognized_finish_without_tool_calls_reports_none_opened() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            final_usage_chunk(),
            b"data: [DONE]\n\n",
        ]);

        assert_eq!(loss_of(terminal).tool_calls, ToolCallsAtLoss::NoneOpened);
    }

    /// `finalize_tools` takes `tool_builders` before it can raise a violation,
    /// so the decoder's tool state is already empty at that point; the fact
    /// must survive it.
    #[test]
    fn a_violation_raised_after_the_tool_builders_are_taken_still_reports_opened() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\"}]}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ]);

        assert_eq!(loss_of(terminal).tool_calls, ToolCallsAtLoss::Opened);
    }

    /// A sparse first index is rejected by the pre-scan, which runs before any
    /// builder exists for it — the other point where decoder tool state is
    /// blind to a call the provider announced.
    #[test]
    fn a_violation_raised_before_a_builder_exists_still_reports_opened() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\"}}]}}]}\n\n",
        ]);

        assert_eq!(loss_of(terminal).tool_calls, ToolCallsAtLoss::Opened);
    }

    /// The choice-index check rejects the chunk before the choice loop reaches
    /// its delta, so a flag set inside that loop would miss a tool announcement
    /// the chunk's own bytes carry. The pre-scan runs at deserialization.
    #[test]
    fn a_chunk_rejected_for_its_choice_index_still_reports_the_tool_call_it_carried() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":7,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\"}}]}}]}\n\n",
        ]);

        assert_eq!(loss_of(terminal).tool_calls, ToolCallsAtLoss::Opened);
    }

    /// The same rejection with no tool material still states the negative, so
    /// the pre-scan reports the chunk rather than defaulting to `Opened`.
    #[test]
    fn a_chunk_rejected_for_its_choice_index_without_tool_material_reports_none_opened() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":7,\"delta\":{\"content\":\"hi\"}}]}\n\n",
        ]);

        assert_eq!(loss_of(terminal).tool_calls, ToolCallsAtLoss::NoneOpened);
    }

    /// A stream that simply stops carries the negative fact, not an absence:
    /// every record it received deserialized and was scanned.
    #[test]
    fn a_stream_lost_before_any_tool_call_reports_none_opened() {
        let (terminal, _) = drive_to_eof(&[first_chunk()]);

        let TerminalEvidence::BoundaryLoss(loss) = terminal else {
            panic!("an unterminated stream is boundary loss");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::NoneOpened);
    }

    /// A record that never deserialized withholds the fact: the bytes the
    /// decoder could not read could themselves have carried the tool delta.
    #[test]
    fn a_record_that_never_deserializes_withholds_the_tool_fact() {
        let (terminal, _) = drive(&[first_chunk(), b"data: {not-json\n\n"]);

        assert_eq!(loss_of(terminal).tool_calls, ToolCallsAtLoss::Unobserved);
    }

    /// The JSON bound rejects the record before deserialization, so it is the
    /// same withholding rather than a serde-specific case.
    #[test]
    fn a_record_rejected_by_the_json_bound_withholds_the_tool_fact() {
        let deep = format!("data: {}1{}\n\n", "[".repeat(2048), "]".repeat(2048));
        let (terminal, _) = drive(&[first_chunk(), deep.as_bytes()]);

        assert_eq!(loss_of(terminal).tool_calls, ToolCallsAtLoss::Unobserved);
    }

    /// Frames exactly one record from `chunk` and applies it, asserting both
    /// the record count and that the decoder kept going.
    ///
    /// Plumbing: every fixture below sends one record per chunk, and stating
    /// that expectation here keeps the iteration out of the test bodies.
    #[track_caller]
    fn apply_one_record(
        framing: &mut SseFraming,
        decoder: &mut StreamDecoder,
        observations: &mut Vec<Observation<String>>,
        chunk: &[u8],
    ) {
        let records = push_ok(framing, chunk);
        let [record] = records.as_slice() else {
            panic!("fixture chunk frames exactly one record");
        };
        assert!(matches!(
            decoder.apply(record, &"call-1".to_string(), observations),
            StreamStep::Continue
        ));
    }

    /// A loss raised while the framer still holds bytes discards material the
    /// decoder never saw, so the fact is withheld even though every record that
    /// did reach the decoder was scanned.
    #[test]
    fn a_loss_with_unframed_bytes_held_withholds_the_tool_fact() {
        let mut framing = SseFraming::new(AMPLE_RECORD_LIMIT);
        let mut decoder = StreamDecoder::new(exchange(), StopSequences::NotDeclared);
        let mut observations: Vec<Observation<String>> = Vec::new();
        apply_one_record(&mut framing, &mut decoder, &mut observations, first_chunk());
        // A partial record: accepted by the transport, never framed, never seen.
        assert_eq!(
            push_ok(&mut framing, b"data: {\"choices\":[{\"index\""),
            vec![]
        );
        assert!(framing.holds_unframed_bytes());
        decoder.note_discarded_unexamined_bytes();

        let TerminalEvidence::BoundaryLoss(loss) = decoder.lost(StreamInterruption::EndOfStream)
        else {
            panic!("an interrupted stream is boundary loss");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
    }

    /// Records framed from one chunk but dropped before the decoder applied
    /// them are out of the framer's hands, so `holds_unframed_bytes` cannot see
    /// them and the decoder is told directly. Without that, a cancellation that
    /// lands mid-chunk would state a negative about records it discarded.
    #[test]
    fn records_dropped_before_the_decoder_applies_them_withhold() {
        let mut framing = SseFraming::new(AMPLE_RECORD_LIMIT);
        let mut decoder = StreamDecoder::new(exchange(), StopSequences::NotDeclared);
        let mut observations: Vec<Observation<String>> = Vec::new();
        apply_one_record(&mut framing, &mut decoder, &mut observations, first_chunk());
        // Framed cleanly and then dropped unapplied, exactly as the runtime's
        // mid-chunk cancellation does.
        assert_eq!(
            push_ok(
                &mut framing,
                b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\"}]}}]}\n\n"
            )
            .len(),
            1
        );
        assert!(
            !framing.holds_unframed_bytes(),
            "a framed record leaves the framer holding nothing"
        );
        decoder.note_discarded_unexamined_bytes();

        let TerminalEvidence::BoundaryLoss(loss) = decoder.lost(StreamInterruption::EndOfStream)
        else {
            panic!("an interrupted stream is boundary loss");
        };
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
    }

    /// An undecodable record does not erase a tool call an earlier record
    /// already established, so the withholding is not a blanket refusal.
    #[test]
    fn an_undecodable_record_after_a_tool_call_still_reports_it() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\"}}]}}]}\n\n",
            b"data: {not-json\n\n",
        ]);

        assert_eq!(loss_of(terminal).tool_calls, ToolCallsAtLoss::Opened);
    }

    #[test]
    fn content_stream_gated_on_done_completes_with_assembled_content() {
        let (terminal, observations) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[],\"usage\":{\"prompt_tokens\":25,\"completion_tokens\":7}}\n\n",
            b"data: [DONE]\n\n",
        ]);

        let Some(TerminalEvidence::Completed(completion)) = terminal else {
            panic!("a [DONE]-gated stream must complete");
        };
        assert_eq!(completion.exchange, exchange());
        assert_eq!(completion.message_id, None);
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
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"type\":\"function\",\
              \"function\":{\"name\":\"lookup\",\"arguments\":\"\"}}]}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"function\":{\"arguments\":\"{\\\"city\\\":\"}}]}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"function\":{\"arguments\":\"\\\"Oslo\\\"}\"}}]}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\
              \"finish_reason\":\"tool_calls\"}]}\n\n",
            final_usage_chunk(),
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
    fn overdeep_fragmented_tool_arguments_are_a_protocol_violation() {
        let at_limit = "[".repeat(PROVIDER_JSON_NESTING_LIMIT);
        let at_limit = serde_json::to_string(&at_limit).expect("fixture JSON string serializes");
        let first_arguments = format!(
            "data: {{\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
             \"model\":\"model-exact-1\",\"choices\":[{{\"index\":0,\"delta\":{{\
             \"role\":\"assistant\",\"tool_calls\":[{{\"index\":0,\"id\":\"call_1\",\
             \"type\":\"function\",\"function\":{{\"name\":\"lookup\",\
             \"arguments\":{at_limit}}}}}]}}}}]}}\n\n"
        );
        let prohibited_fragment = "[";
        let beyond_limit = serde_json::to_string(&serde_json::json!({
            "object": "chat.completion.chunk",
            "id": "chatcmpl_1",
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {"arguments": prohibited_fragment}
                    }]
                }
            }]
        }))
        .expect("fixture JSON serializes");
        let beyond_limit = format!("data: {beyond_limit}\n\n");
        let (terminal, observations) =
            drive(&[first_arguments.as_bytes(), beyond_limit.as_bytes()]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("overdeep accumulated tool arguments must fail immediately");
        };
        let LossCause::StreamProtocolViolation { detail } = loss.cause else {
            panic!("deep streamed arguments must surface as protocol loss");
        };
        let expected = format!("{PROVIDER_JSON_NESTING_LIMIT}-container nesting limit");
        assert!(detail.contains(&expected), "{detail}");
        assert!(!observations.iter().any(|observation| {
            observation.fact
                == ObservationFact::ToolArgumentsDelta {
                    index: 0,
                    fragment: prohibited_fragment.to_string(),
                }
        }));
    }

    #[test]
    fn ambiguous_length_finish_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\
              \"arguments\":\"{\\\"city\\\":\"}}]}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\
              \"finish_reason\":\"length\"}]}\n\n",
            final_usage_chunk(),
            b"data: [DONE]\n\n",
        ]);

        let loss = expect_boundary_loss(terminal);

        assert_eq!(
            loss.finish_reported,
            Some(FinishReason::Unrecognized {
                provider_token: "length".to_string(),
            })
        );
    }

    #[test]
    fn eof_without_done_is_explicit_incomplete_stream_evidence_with_partials() {
        let (evidence, _) = drive_to_eof(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
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
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done-ish\"},\
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
    fn final_usage_before_the_finish_reason_is_a_protocol_violation() {
        let (terminal, _) = drive(&[first_chunk(), final_usage_chunk()]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("usage sent before the terminal choice cannot complete the stream");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn a_record_after_final_usage_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            final_usage_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"choices\":[],\"usage\":{\"prompt_tokens\":999}}\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    #[test]
    fn a_non_literal_done_marker_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            final_usage_chunk(),
            b"data: [DONE] \n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    #[test]
    fn an_error_after_final_usage_remains_definitive_provider_evidence() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            final_usage_chunk(),
            b"data: {\"error\":{\"message\":\"quota exhausted\",\
              \"type\":\"insufficient_quota\",\"code\":\"insufficient_quota\"}}\n\n",
        ]);

        let Some(TerminalEvidence::ProviderError(error)) = terminal else {
            panic!("a definitive error record outranks weaker post-usage protocol loss");
        };
        assert_eq!(error.kind, ProviderErrorKind::QuotaExhausted);
        assert!(!error.non_acceptance_proven);
        assert_eq!(error.usage.input_tokens, Some(25));
        assert_eq!(error.usage.output_tokens, Some(7));
    }

    /// The boundary-loss payload of a terminal outcome, so a case that is
    /// about *which* loss occurred reads as setup and assertions with no
    /// branch of its own.
    #[track_caller]
    fn expect_boundary_loss(terminal: Option<TerminalEvidence>) -> BoundaryLossEvidence {
        match terminal {
            Some(TerminalEvidence::BoundaryLoss(loss)) => loss,
            other => panic!("expected boundary-loss evidence, got {other:?}"),
        }
    }

    /// The provider-error payload of a terminal outcome, so a case about
    /// *which* error occurred reads as setup and assertions with no branch.
    #[track_caller]
    fn expect_provider_error(terminal: Option<TerminalEvidence>) -> ProviderErrorEvidence {
        match terminal {
            Some(TerminalEvidence::ProviderError(error)) => error,
            other => panic!("expected provider-error evidence, got {other:?}"),
        }
    }

    /// Whether any tool-argument fragment reached the sink. Named because the
    /// case that uses it is *about* this fact being absent, and a search
    /// spelled inline would put the discriminator in the test body.
    #[track_caller]
    fn tool_argument_delta_observed(observations: &[Observation<String>]) -> bool {
        observations
            .iter()
            .any(|observation| match &observation.fact {
                ObservationFact::ToolArgumentsDelta { .. } => true,
                // Enumerated rather than wildcarded: a new observation class must
                // be considered here instead of silently classifying as "no tool
                // material".
                ObservationFact::SendCommenced
                | ObservationFact::ExchangeEstablished(_)
                | ObservationFact::ProviderModelReported(_)
                | ObservationFact::TextDelta { .. }
                | ObservationFact::ThinkingDelta { .. }
                | ObservationFact::ToolCallProposed(_)
                | ObservationFact::UsageReported(_)
                | ObservationFact::FinishReported(_) => false,
            })
    }

    /// The finish token the provider sends when generation reached an output
    /// bound. Bound once so a fixture and the expectation asserted against it
    /// cannot drift apart.
    const CEILING_FINISH_TOKEN: &str = "length";

    /// A finish-only chunk reporting `token`, built from the same binding the
    /// asserting case compares against.
    fn finish_chunk(token: &str) -> String {
        format!(
            "data: {{\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
             \"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"{token}\"}}]}}\n\n"
        )
    }

    /// The `StreamProtocolViolation` cause carrying `detail`, spelled once so
    /// each case names only the detail it is about.
    fn protocol_violation(detail: &str) -> LossCause {
        LossCause::StreamProtocolViolation {
            detail: detail.to_string(),
        }
    }

    #[test]
    fn an_unrecognized_finish_without_the_assistant_role_reports_the_envelope_defect() {
        // The unrecognized-finish branch ends the stream before `apply_done`,
        // so it applies the role check itself and reports no finish. That is
        // what lets a caller tell a malformed envelope from a healthy stream
        // that merely hit an output bound.
        let (terminal, _) = drive(&[
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"model\":\"model-exact-1\",\"choices\":[{\"index\":0,\"delta\":{},\
              \"finish_reason\":\"length\"}]}\n\n",
        ]);

        let loss = expect_boundary_loss(terminal);

        assert_eq!(
            loss.cause,
            protocol_violation("stream terminated without establishing the assistant role")
        );
        assert_eq!(loss.finish_reported, None);
    }

    #[test]
    fn a_truncated_tool_call_keeps_its_length_finish_like_the_buffered_path() {
        // Exhausting the output ceiling partway through a tool call is a real
        // outcome for a request that carries tools, not a contradiction, so
        // the observed token survives. This is the streamed twin of
        // `response.rs`'s
        // `ambiguous_length_finish_is_boundary_loss_even_with_partial_tool_material`;
        // the two decoders must not disagree about the same response.
        let (terminal, observations) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"function\":{\"name\":\"probe\",\"arguments\":\"{}\"}}]}}]}\n\n",
            finish_chunk(CEILING_FINISH_TOKEN).as_bytes(),
            final_usage_chunk(),
            b"data: [DONE]\n\n",
        ]);

        // The positive direction of the helper the sibling case relies on for
        // its negative claim: a non-empty argument fragment does emit the
        // delta, so a helper hard-coded either way fails one of the two.
        assert!(tool_argument_delta_observed(&observations));
        let loss = expect_boundary_loss(terminal);

        assert_eq!(
            loss.finish_reported,
            Some(FinishReason::Unrecognized {
                provider_token: CEILING_FINISH_TOKEN.to_string()
            })
        );
        // The token survives, and the typed field records that tools were
        // involved. Here the delta observation records it too, as the assertion
        // above shows; the field is the channel that still carries it when no
        // fragment is emitted — see the sibling case below.
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Opened);
    }

    #[test]
    fn a_tool_call_start_without_arguments_still_marks_the_ceiling_loss() {
        // The residual the observation-based check cannot see: a tool call
        // that opens with an id and name but no argument fragment emits
        // nothing, so only the cause can carry it.
        let (terminal, observations) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"function\":{\"name\":\"probe\"}}]}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            final_usage_chunk(),
            b"data: [DONE]\n\n",
        ]);

        assert!(
            !tool_argument_delta_observed(&observations),
            "a tool call opened with no argument fragment emits no delta"
        );
        let loss = expect_boundary_loss(terminal);
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Opened);
    }

    #[test]
    fn a_well_formed_unrecognized_finish_still_reports_its_token() {
        let (terminal, _) = drive(&[
            first_chunk(),
            finish_chunk(CEILING_FINISH_TOKEN).as_bytes(),
            final_usage_chunk(),
            b"data: [DONE]\n\n",
        ]);

        let loss = expect_boundary_loss(terminal);

        assert_eq!(
            loss.finish_reported,
            Some(FinishReason::Unrecognized {
                provider_token: CEILING_FINISH_TOKEN.to_string()
            })
        );
    }

    #[test]
    fn an_unfamiliar_but_described_error_after_the_finish_stays_definitive() {
        // Classification alone is the wrong test: an error whose type is
        // merely unfamiliar still classifies `Unrecognized`, but it carries
        // diagnostics a caller wants and cannot be confused with the refusal
        // downgrade, which fabricates no native material at all.
        let expected_message = "short and stout";
        let error_record = format!(
            "data: {{\"error\":{{\"type\":\"teapot_error\",\"message\":\"{expected_message}\"}}}}\n\n"
        );
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            final_usage_chunk(),
            error_record.as_bytes(),
        ]);

        let error = expect_provider_error(terminal);

        assert_eq!(error.kind, ProviderErrorKind::Unrecognized);
        assert_eq!(error.native.message, Some(expected_message.to_string()));
    }

    #[test]
    fn a_length_finish_without_final_usage_is_protocol_loss() {
        // Deferring to `[DONE]` means the usage chunk normally arrives, so a
        // stream that omits it is failing the `include_usage` contract rather
        // than stopping cleanly at an output bound.
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            b"data: [DONE]\n\n",
        ]);

        let loss = expect_boundary_loss(terminal);

        assert_eq!(
            loss.cause,
            protocol_violation("stream terminated without the requested final usage chunk")
        );
    }

    #[test]
    fn an_error_after_a_length_finish_supersedes_the_deferred_verdict() {
        // The reason the length verdict is deferred to `[DONE]`: returning at
        // the finish chunk would leave this error record unread, and a caller
        // would accept the prefix as a clean stop at an output bound.
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            b"data: {\"error\":{\"message\":\"quota exhausted\",\
              \"type\":\"insufficient_quota\",\"code\":\"insufficient_quota\"}}\n\n",
        ]);

        let error = expect_provider_error(terminal);

        assert_eq!(error.kind, ProviderErrorKind::QuotaExhausted);
    }

    #[test]
    fn a_contentless_error_after_a_length_finish_is_protocol_loss() {
        // The other half: a record adding nothing still supersedes the ceiling
        // verdict, but as loss rather than provider evidence.
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            b"data: {\"error\":{}}\n\n",
        ]);

        let loss = expect_boundary_loss(terminal);

        assert_eq!(
            loss.cause,
            protocol_violation("contentless error record follows the reported finish_reason")
        );
    }

    #[test]
    fn a_contentless_error_after_the_finish_is_protocol_loss() {
        // The sibling above keeps a *typed* error outranking the finish,
        // because it carries information the finish does not. One that names
        // no classifiable failure carries none, and would reach the caller
        // wearing the exact shape `execute` gives a downgraded refusal — HTTP
        // 200, `Unrecognized`, no native material — so a genuine failure could
        // pass as a decoded refusal. It must stay a protocol violation.
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            final_usage_chunk(),
            b"data: {\"error\":{}}\n\n",
        ]);

        let loss = expect_boundary_loss(terminal);

        assert_eq!(
            loss.cause,
            protocol_violation("contentless error record follows the reported finish_reason")
        );
    }

    #[test]
    fn refusal_deltas_accumulate_into_refusal_evidence_at_done() {
        let (terminal, observations) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"refusal\":\"I cannot \"}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"refusal\":\"help with that.\"},\
              \"finish_reason\":\"stop\"}]}\n\n",
            final_usage_chunk(),
            b"data: [DONE]\n\n",
        ]);

        let Some(TerminalEvidence::Refused(refusal)) = terminal else {
            panic!("accumulated refusal material is refusal evidence, never completion");
        };
        assert_eq!(
            refusal.content,
            vec![AssistantPart::Text("I cannot help with that.".to_string())]
        );
        assert!(observations.contains(&Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "I cannot ".to_string(),
            },
        }));
        assert!(observations.contains(&Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "help with that.".to_string(),
            },
        }));
    }

    #[test]
    fn content_filter_finish_is_refusal_evidence_at_done() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\
              \"finish_reason\":\"content_filter\"}]}\n\n",
            final_usage_chunk(),
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
    fn overdeep_stream_json_is_a_protocol_violation() {
        let nested = format!(
            "{}null{}",
            "[".repeat(PROVIDER_JSON_NESTING_LIMIT + 1),
            "]".repeat(PROVIDER_JSON_NESTING_LIMIT + 1)
        );
        let chunk = format!(
            "data: {{\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
             \"model\":\"model-exact-1\",\"choices\":[],\"future\":{nested}}}\n\n"
        );
        let (terminal, _) = drive(&[chunk.as_bytes()]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("overdeep stream material must not reach typed parsing");
        };
        let LossCause::StreamProtocolViolation { detail } = loss.cause else {
            panic!("deep SSE JSON must surface as a stream protocol violation");
        };
        let expected = format!("{PROVIDER_JSON_NESTING_LIMIT}-container nesting limit");
        assert!(detail.contains(&expected));
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
    fn an_error_first_stream_retains_and_observes_its_model_identity() {
        let (terminal, observations) = drive(&[
            b"data: {\"model\":\"model-error\",\"error\":{\"message\":\"quota exhausted\",\
              \"type\":\"insufficient_quota\",\"code\":\"insufficient_quota\"}}\n\n",
        ]);

        let Some(TerminalEvidence::ProviderError(error)) = terminal else {
            panic!("an error-first record is definitive provider error evidence");
        };
        assert_eq!(
            error.reported_model,
            Some(ProviderReportedModel::new("model-error"))
        );
        assert!(observations.contains(&Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new("model-error")),
        }));
    }

    #[test]
    fn a_conflicting_model_on_an_error_record_is_protocol_loss() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"model\":\"model-other\",\"error\":{\"message\":\"quota exhausted\",\
              \"type\":\"insufficient_quota\",\"code\":\"insufficient_quota\"}}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a conflicting error identity must not become ordinary provider failure");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn a_conflicting_completion_id_on_an_error_record_is_protocol_loss() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"id\":\"chatcmpl_other\",\"error\":{\"message\":\"quota exhausted\",\
              \"type\":\"insufficient_quota\",\"code\":\"insufficient_quota\"}}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a conflicting error completion must not become ordinary provider failure");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn mid_stream_error_type_classifies_when_code_is_absent() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"error\":{\"message\":\"quota exhausted\",\"type\":\"insufficient_quota\"}}\n\n",
        ]);

        let Some(TerminalEvidence::ProviderError(error)) = terminal else {
            panic!("a mid-stream error record is definitive provider error evidence");
        };
        assert_eq!(error.kind, ProviderErrorKind::QuotaExhausted);
        assert_eq!(error.native.error_code, None);
    }

    #[test]
    fn statusless_rate_limit_tokens_keep_their_native_class() {
        assert_statusless_error_classifies("rate_limit_exceeded", ProviderErrorKind::RateLimited);
        assert_statusless_error_classifies("rate_limit_error", ProviderErrorKind::RateLimited);
    }

    #[test]
    fn statusless_server_error_tokens_keep_their_native_class() {
        assert_statusless_error_classifies("server_error", ProviderErrorKind::ProviderInternal);
        assert_statusless_error_classifies(
            "internal_server_error",
            ProviderErrorKind::ProviderInternal,
        );
    }

    #[test]
    fn a_second_choice_index_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":1,\"delta\":{\"content\":\"ghost\"}}]}\n\n",
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
    fn a_missing_choice_index_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"delta\":{\"content\":\"ghost\"}}]}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a missing choice index must surface as a protocol violation");
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
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\
              \"arguments\":\"{\\\"city\\\":\"}}]}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\
              \"finish_reason\":\"tool_calls\"}]}\n\n",
            final_usage_chunk(),
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
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
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
    fn an_invalid_streamed_function_name_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"has space\",\"arguments\":\"{}\"}}]}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\
              \"finish_reason\":\"tool_calls\"}]}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("an invalid function name must not become a proposal");
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
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"refusal\":\"No.\"}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            final_usage_chunk(),
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
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"ping\",\"arguments\":\"{}\"}}]}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\
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
            b"data: {\"object\":\"chat.completion.chunk\",\"model\":\"other-model\",\"choices\":[]}\n\n",
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
    fn conflicting_streamed_completion_ids_are_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_other\",\"choices\":[]}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a second completion id must not complete under the first");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn a_stream_chunk_without_a_completion_id_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            b"data: {\"object\":\"chat.completion.chunk\",\"model\":\"model-exact-1\",\
              \"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    #[test]
    fn a_non_assistant_streamed_role_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"user\"}}]}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("a non-assistant streamed role must not become completion evidence");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn a_stream_without_an_assistant_role_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"model\":\"model-exact-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\
              \"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            final_usage_chunk(),
            b"data: [DONE]\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    #[test]
    fn stop_with_a_declared_sequence_is_a_protocol_violation() {
        let (terminal, _) = drive_with_stop_sequences(
            &[
                first_chunk(),
                b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
                  \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                final_usage_chunk(),
                b"data: [DONE]\n\n",
            ],
            StopSequences::Declared,
        );

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("ambiguous finish must remain boundary-loss evidence");
        };
        assert_eq!(
            loss.finish_reported,
            Some(FinishReason::Unrecognized {
                provider_token: "stop".to_string(),
            })
        );
    }

    #[test]
    fn choice_material_after_the_finish_reason_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late\"}}]}\n\n",
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
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{}\"}}]}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late text\"}}]}\n\n",
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
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"ping\",\"arguments\":\"\"}}]}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\
              \"finish_reason\":\"tool_calls\"}]}\n\n",
            final_usage_chunk(),
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
    fn absent_streamed_tool_arguments_are_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"ping\"}}]}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("absent argument material must surface as a protocol violation");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn a_streamed_tool_call_without_an_index_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\
              \"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"ping\",\
              \"arguments\":\"{}\"}}]}}]}\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    #[test]
    fn a_sparse_tool_index_is_rejected_before_its_deltas_are_emitted() {
        let (terminal, observations) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
              \"choices\":[{\"index\":0,\"delta\":{\"content\":\"must not emit\",\
              \"tool_calls\":[{\"index\":4294967295,\"id\":\"call_bad\",\"type\":\"function\",\
              \"function\":{\"name\":\"ping\",\"arguments\":\"{}\"}}]}}]}\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
        assert!(!observations.iter().any(|observation| {
            matches!(
                &observation.fact,
                ObservationFact::TextDelta { text, .. } if text == "must not emit"
            ) || matches!(observation.fact, ObservationFact::ToolArgumentsDelta { .. })
        }));
    }

    #[test]
    fn streamed_tool_content_and_finish_reason_must_agree() {
        let (tool_with_stop, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
              \"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"ping\",\
              \"arguments\":\"{}\"}}]}}]}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ]);
        let (tool_finish_without_tool, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\
              \"finish_reason\":\"tool_calls\"}]}\n\n",
        ]);

        let Some(TerminalEvidence::BoundaryLoss(tool_with_stop)) = tool_with_stop else {
            panic!("tool content with a stop finish must be boundary loss");
        };
        assert_eq!(tool_with_stop.finish_reported, Some(FinishReason::EndTurn));

        let Some(TerminalEvidence::BoundaryLoss(tool_finish_without_tool)) =
            tool_finish_without_tool
        else {
            panic!("a tool finish without tool content must be boundary loss");
        };
        assert_eq!(
            tool_finish_without_tool.finish_reported,
            Some(FinishReason::ToolUse)
        );
    }

    #[test]
    fn nonfinal_partial_usage_chunk_reports_usage_and_keeps_streaming() {
        let (terminal, observations) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[],\"usage\":{\"prompt_tokens\":9,\
              \"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            final_usage_chunk(),
            b"data: [DONE]\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::Completed(_))));
        assert!(observations.contains(&Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::UsageReported(TokenUsage {
                input_tokens: Some(9),
                output_tokens: None,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: Some(4),
            }),
        }));
    }

    #[test]
    fn multiple_choices_in_one_chunk_are_a_protocol_violation() {
        let (terminal, _) = drive(&[
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"first\"}},{\"index\":0,\"delta\":{\"content\":\"second\"}}]}\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    #[test]
    fn wrong_streamed_object_is_a_protocol_violation() {
        let (terminal, _) = drive(&[b"data: {\"object\":\"chat.completion\",\"choices\":[]}\n\n"]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    #[test]
    fn done_without_final_usage_chunk_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            first_chunk(),
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            b"data: [DONE]\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }

    #[test]
    fn done_without_model_identity_is_a_protocol_violation() {
        let (terminal, _) = drive(&[
            b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            final_usage_chunk(),
            b"data: [DONE]\n\n",
        ]);

        assert!(matches!(terminal, Some(TerminalEvidence::BoundaryLoss(_))));
    }
}
