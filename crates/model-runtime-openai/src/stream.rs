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
    LossCause, Observation, ObservationFact, ObservationSink, ProviderErrorEvidence,
    ProviderReportedModel, RefusalEvidence, SseRecord, StreamInterruption, TerminalEvidence,
    TokenUsage, ToolCallId, ToolCallProposal, ToolName, validate_provider_json_nesting,
};

use crate::response::{convert_usage, map_finish};
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

#[derive(Default)]
struct ToolBuilder {
    id: Option<String>,
    name: Option<String>,
    saw_function_type: bool,
    saw_arguments: bool,
    arguments: String,
}

/// Incremental decoder for one chat-completion stream.
pub(crate) struct StreamDecoder {
    exchange: ExchangeFacts,
    completion_id: Option<String>,
    reported_model: Option<ProviderReportedModel>,
    stop_sequences_declared: bool,
    saw_assistant_role: bool,
    usage: TokenUsage,
    finish: Option<FinishReason>,
    content_text: String,
    refusal_text: String,
    tool_builders: BTreeMap<u32, ToolBuilder>,
    completed_tools: Vec<ToolCallProposal>,
    final_usage_reported: bool,
}

impl StreamDecoder {
    pub(crate) fn new(exchange: ExchangeFacts, stop_sequences_declared: bool) -> Self {
        Self {
            exchange,
            completion_id: None,
            reported_model: None,
            stop_sequences_declared,
            saw_assistant_role: false,
            usage: TokenUsage::unreported(),
            finish: None,
            content_text: String::new(),
            refusal_text: String::new(),
            tool_builders: BTreeMap::new(),
            completed_tools: Vec::new(),
            final_usage_reported: false,
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
            return self.violation(format!("stream chunk exceeds the JSON bound: {error}"));
        }
        let chunk: ChatChunk = match serde_json::from_str(&record.data) {
            Ok(chunk) => chunk,
            Err(error) => {
                return self.violation(format!("malformed stream chunk payload: {error}"));
            }
        };
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
            return StreamStep::Terminal(Box::new(TerminalEvidence::ProviderError(
                ProviderErrorEvidence {
                    exchange: self.exchange.clone(),
                    reported_model: self.reported_model.clone(),
                    kind,
                    native: error.into_native_facts(),
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
                let mut finish = map_finish(&token, self.stop_sequences_declared);
                if matches!(finish, FinishReason::Unrecognized { .. }) {
                    self.finish = Some(finish);
                    return self.violation("stream carries an unrecognized finish_reason");
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
    pub(crate) fn cancelled(&self) -> TerminalEvidence {
        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::CancellationRequested,
            exchange: self.exchange.clone(),
            reported_model: self.reported_model.clone(),
            finish_reported: self.finish.clone(),
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
        AssistantPart, CompletionFinish, ExchangeFacts, FinishReason, LossCause, Observation,
        ObservationFact, PROVIDER_JSON_NESTING_LIMIT, ProviderErrorKind, ProviderReportedModel,
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
        drive_with_stop_sequences(chunks, false)
    }

    fn drive_with_stop_sequences(
        chunks: &[&[u8]],
        stop_sequences_declared: bool,
    ) -> (Option<TerminalEvidence>, Vec<Observation<String>>) {
        let mut framing = SseFraming::new(1024 * 1024);
        let mut decoder = StreamDecoder::new(exchange(), stop_sequences_declared);
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
        let mut decoder = StreamDecoder::new(exchange(), false);
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
        let depth = PROVIDER_JSON_NESTING_LIMIT + 1;
        let opening = "[".repeat(depth);
        let closing = format!("null{}", "]".repeat(depth));
        let opening = serde_json::to_string(&opening).expect("fixture JSON string serializes");
        let closing = serde_json::to_string(&closing).expect("fixture JSON string serializes");
        let first_arguments = format!(
            "data: {{\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
             \"model\":\"model-exact-1\",\"choices\":[{{\"index\":0,\"delta\":{{\
             \"role\":\"assistant\",\"tool_calls\":[{{\"index\":0,\"id\":\"call_1\",\
             \"type\":\"function\",\"function\":{{\"name\":\"lookup\",\
             \"arguments\":{opening}}}}}]}}}}]}}\n\n"
        );
        let second_arguments = format!(
            "data: {{\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
             \"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\
             \"function\":{{\"arguments\":{closing}}}}}]}}}}]}}\n\n"
        );
        let finish = b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
            \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";
        let (terminal, _) = drive(&[
            first_arguments.as_bytes(),
            second_arguments.as_bytes(),
            finish,
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("overdeep reassembled tool arguments must fail the stream");
        };
        let LossCause::StreamProtocolViolation { detail } = loss.cause else {
            panic!("deep streamed arguments must surface as protocol loss");
        };
        assert!(detail.contains("128-container nesting limit"));
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
        ]);

        let Some(TerminalEvidence::BoundaryLoss(loss)) = terminal else {
            panic!("ambiguous finish must remain boundary-loss evidence");
        };
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
        assert_eq!(error.usage.input_tokens, Some(25));
        assert_eq!(error.usage.output_tokens, Some(7));
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
        assert!(detail.contains("128-container nesting limit"));
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
            ],
            true,
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
