//! Stateful decoding of one Codex exec JSONL event stream.
//!
//! **Dropped-content rule.** Provider-controlled text this adapter drops
//! without interpreting still governs the output that follows it: a credential
//! marker the dropped bytes end in (`api_`) marks a value that opens the next
//! emitted text (`key=value`) as a secret. Every uninterpreted dropped field
//! therefore reaches the redactor's match-only lookbehind — match state, never
//! itself emitted — before that following text is emitted or retained, so the
//! value is suppressed wherever it next surfaces, in a streamed delta or in
//! the buffered final text alike. Text the adapter models and then drops goes
//! in whole through `extend_dropped_context`; fields it does not model go
//! through `fold_uninterpreted`, which weighs them as independent units.
//!
//! Interpreted fields are outside the rule even where they are also dropped,
//! and merely reading a field does not interpret it. `validate_item_identity`
//! checks that `item.id` and `item.type` are nonempty, which leaves both
//! arbitrary provider text; an arm that then drops them folds them like any
//! other dropped field, so a credential marker one of them ends in governs the
//! text that follows. Two identity fields stay outside the fold, each because
//! something else already governs it: an item `type` that selected a *known*
//! arm was matched against the adapter's own literal (`agent_message`,
//! `reasoning`, `error`) and carries no provider bytes at all, and the agent
//! message's `id` is retained rather than dropped — `completed` sanitizes it
//! against the held lookbehind and against the final text it precedes, which
//! breaks the marker in the field that carries it instead of suppressing the
//! text.
//!
//! The identity weighs in the fold as ordinary units, which is also its cost:
//! an id or type whose trailing bytes are a credential candidate can be the
//! second live marker that makes `fold_dropped_units` fail closed for the rest
//! of the stream. That is the direction to err in — the alternative is bytes
//! that arm nothing — but it is why the identity is folded only where the arm
//! actually drops it.
//!
//! Each arm of `EventDecoder::push` cites this rule and adds only what is
//! local to it.

use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde_json::Value;
use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CliDecodeFailure, CliDecodeFailureClass, CliProcessLabels,
    CliSession, CliTerminalTextCapture, CompletionEvidence, CompletionFinish, DeliveryMode,
    ExchangeFacts, FinishReason, LossCause, NativeErrorFacts, Observation, ObservationFact,
    ObservationSink, ProviderErrorEvidence, ProviderErrorKind, ProviderMessageId,
    ProviderRequestId, REDACTED, RedactingSink, RefusalEvidence, TerminalEvidence, TokenUsage,
    ToolArgumentRedaction, ToolCallId, ToolCallProposal, ToolCallsAtLoss, ToolName,
    provider_json_has_duplicate_members, redact_text, trailing_credential_context,
    validate_provider_json_nesting,
};

use crate::translate::{ToolRequirement, TranslatedOperation};
use crate::wire::{
    EnvelopeOutcome, ItemDetails, ItemEvent, ItemIdentity, ItemLifecycleEvent, ModelEnvelope,
    ThreadError, ThreadStarted, TurnCompleted, TurnFailed,
};

fn reject_duplicate_json_members(line: &str) -> Result<(), DecodeFailure> {
    let duplicate = provider_json_has_duplicate_members(line)
        .map_err(|error| DecodeFailure::stream_protocol(error.to_string()))?;
    if duplicate {
        Err(DecodeFailure::stream_protocol(
            "JSON input has duplicate object members",
        ))
    } else {
        Ok(())
    }
}

pub(crate) struct EventDecoder<C> {
    correlation: C,
    delivery: DeliveryMode,
    declared_tools: HashSet<String>,
    output_contract_name: Option<String>,
    tool_requirement: ToolRequirement,
    exchange: ExchangeFacts,
    message_id: Option<String>,
    agent_message: Option<String>,
    output_last_message: PathBuf,
    terminal_message_limit: usize,
    next_part_index: u32,
    usage: TokenUsage,
    terminal: Option<CliTerminal>,
}

enum CliTerminal {
    Completed,
    /// The turn lifecycle is fully closed; any further event fails closed.
    Failed(String),
    /// A stream-level `error` event was recorded. The pinned CLI still
    /// closes the failed turn with a `turn.failed` lifecycle echo — the
    /// sequencing the `error_then_turn_failed` fixture records — so exactly
    /// that one trailer is still admissible. Any other continuation, including
    /// the run of reconnect notices the CLI emits as further `error` events
    /// when its transport cannot be established, fails closed as an
    /// unrecognized provider error.
    Unrecoverable(String),
}

impl<C: Clone> EventDecoder<C> {
    pub(crate) fn new(
        correlation: C,
        delivery: DeliveryMode,
        translated: &TranslatedOperation,
        output_last_message: PathBuf,
        terminal_message_limit: usize,
    ) -> Self {
        Self {
            correlation,
            delivery,
            declared_tools: translated.declared_tools.iter().cloned().collect(),
            output_contract_name: translated.output_contract_name.clone(),
            tool_requirement: translated.tool_requirement.clone(),
            exchange: ExchangeFacts::default(),
            message_id: None,
            agent_message: None,
            output_last_message,
            terminal_message_limit,
            next_part_index: 0,
            usage: TokenUsage::unreported(),
            terminal: None,
        }
    }

    pub(crate) fn push(
        &mut self,
        line: &[u8],
        sink: &mut RedactingSink<'_, C>,
    ) -> Result<(), DecodeFailure> {
        match &self.terminal {
            Some(CliTerminal::Completed | CliTerminal::Failed(_)) => {
                return Err(DecodeFailure::new(
                    "Codex emitted an event after its terminal marker",
                ));
            }
            Some(CliTerminal::Unrecoverable(_)) | None => {}
        }
        let line = std::str::from_utf8(line)
            .map_err(|error| DecodeFailure::new(format!("event is not UTF-8: {error}")))?;
        validate_provider_json_nesting(line.as_bytes())
            .map_err(|error| DecodeFailure::new(error.to_string()))?;
        reject_duplicate_json_members(line)?;
        let value: Value = serde_json::from_str(line)
            .map_err(|error| DecodeFailure::new(format!("event is not JSON: {error}")))?;
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| DecodeFailure::new("event has no string `type` discriminator"))?;
        if let Some(CliTerminal::Unrecoverable(stream_error)) = &self.terminal {
            // The pinned CLI reports a failed exchange as a stream-level
            // `error` event followed by its `turn.failed` lifecycle echo
            // (observed live by the gated compatibility smoke). Exactly that
            // echo closes the turn; the stream-level message, which arrived
            // first and already classified, is the provider error the caller
            // receives. Any other trailer contradicts the recorded terminal
            // and still fails closed.
            if event_type != "turn.failed" {
                return Err(DecodeFailure::new(
                    "Codex emitted an event after its terminal marker",
                ));
            }
            let stream_error = stream_error.clone();
            let event: TurnFailed = decode(value.clone())?;
            // Only the echo closes the turn: a trailer carrying a different
            // failure contradicts the recorded terminal and fails closed
            // rather than silently keeping either message.
            if event.error.message != stream_error {
                return Err(DecodeFailure::new(
                    "Codex contradicted its stream error with a different turn.failed failure",
                ));
            }
            fold_uninterpreted(sink, &value, &["type"], Some(("error", &["message"])));
            self.terminal = Some(CliTerminal::Failed(stream_error));
            return Ok(());
        }
        match event_type {
            "thread.started" => {
                let event: ThreadStarted = decode(value.clone())?;
                if event.thread_id.is_empty() || self.exchange.provider_request_id.is_some() {
                    return Err(DecodeFailure::new(
                        "thread.started carries an empty or duplicate thread id",
                    ));
                }
                // A drifted thread.started can carry additive provider-controlled
                // fields beyond `thread_id`; fold their uninterpreted content
                // into the lookbehind (the `thread_id` itself is separately
                // sanitized and seeded below), as for turn.started, lifecycle,
                // and unknown events.
                fold_uninterpreted(sink, &value, &["type", "thread_id"], None);
                // Sanitized against the held lookbehind before the
                // ExchangeEstablished observation (which flushes it), so a
                // thread id that extends a credential marker held from a
                // drifted earlier reasoning delta is suppressed instead of
                // escaping in the observation and terminal exchange facts.
                let sanitized = sink.redact_terminal_failure_text(&event.thread_id);
                self.exchange.provider_request_id = Some(ProviderRequestId::new(sanitized.clone()));
                sink.observe(Observation {
                    correlation: self.correlation.clone(),
                    fact: ObservationFact::ExchangeEstablished(self.exchange.clone()),
                });
                // The id just left the adapter inside `ExchangeEstablished`,
                // where later text sits beside it in observations and terminal
                // evidence. Seeding its unsafe trailing suffix (a credential
                // marker prefix such as `api_`) into the lookbehind makes text
                // completing that marker (`key=value`) suppress instead of
                // emitting as the marker's reconstructable continuation.
                sink.seed_emitted_context(&sanitized);
            }
            "turn.started" => {
                // A drifted but accepted event can carry additive
                // provider-controlled string fields; fold their uninterpreted
                // content so a credential marker they hold governs the
                // following text, as for lifecycle and unknown events.
                fold_uninterpreted(sink, &value, &["type"], None);
            }
            "item.started" | "item.updated" => {
                let identity: ItemLifecycleEvent = decode(value.clone())?;
                validate_item_identity(&identity.item)?;
                // A lifecycle event is dropped whole. Its identity is only
                // validated as nonempty, never matched against anything the
                // adapter chose, so `id` and `item.type` are provider text the
                // adapter drops — they fold with the additively-tolerated
                // fields beside and inside the item, and a marker ending any
                // of them governs the text that follows.
                fold_uninterpreted(sink, &value, &["type"], Some(("item", &[])));
            }
            "item.completed" => {
                let identity: ItemLifecycleEvent = decode(value.clone())?;
                validate_item_identity(&identity.item)?;
                let event: ItemEvent = decode(value.clone())?;
                match event.item.details {
                    ItemDetails::AgentMessage { text } => {
                        // Only the last agent message is decoded as the response
                        // envelope, but an earlier one being superseded here
                        // must not silently drop a credential it carries: a
                        // marker ending a displaced message and its value
                        // beginning the final one would otherwise reconstruct
                        // across them. Fold any displaced text (and id) into the
                        // lookbehind before replacing.
                        self.fold_retained_agent_message(sink);
                        // A known item can also carry additively tolerated
                        // sibling fields serde accepts and discards; fold them
                        // before the interpreted id and text are retained, so a
                        // marker one of them ends in still governs the text that
                        // follows. The item's `id` and `text` are retained and
                        // separately sanitized in `completed`, and its `type`
                        // matched the adapter's own `agent_message` literal, so
                        // none of the three is dropped content.
                        fold_uninterpreted(
                            sink,
                            &value,
                            &["type"],
                            Some(("item", &["id", "type", "text"])),
                        );
                        // The id is retained raw and sanitized against the
                        // held stream state in `completed`, so a value that
                        // extends a credential marker from earlier reasoning
                        // cannot reach `ProviderMessageId` unredacted.
                        self.message_id = Some(event.item.id.clone());
                        self.agent_message = Some(text);
                    }
                    ItemDetails::Reasoning { text } => {
                        // Additive siblings serde accepted and discarded are
                        // dropped content, and so is the item id this arm
                        // never retains; fold them before the modeled text is
                        // emitted or held. The item `type` matched the
                        // adapter's own `reasoning` literal and is the only
                        // interpreted field left inside the item.
                        fold_uninterpreted(
                            sink,
                            &value,
                            &["type"],
                            Some(("item", &["type", "text"])),
                        );
                        if self.delivery == DeliveryMode::Streamed {
                            let index = self.take_part_index()?;
                            sink.observe(Observation {
                                correlation: self.correlation.clone(),
                                fact: ObservationFact::ThinkingDelta { index, text },
                            });
                        } else {
                            // Dropped from the buffered output; the bytes feed
                            // the lookbehind instead (dropped-content rule).
                            sink.extend_dropped_context(&text);
                        }
                    }
                    ItemDetails::Error { message } => {
                        // An error item is dropped in both delivery modes:
                        // additive siblings and the unretained item id fold,
                        // then the modeled message extends the lookbehind
                        // (dropped-content rule). The item `type` matched the
                        // adapter's own `error` literal.
                        fold_uninterpreted(
                            sink,
                            &value,
                            &["type"],
                            Some(("item", &["type", "message"])),
                        );
                        sink.extend_dropped_context(&message);
                    }
                    ItemDetails::Other => {
                        // An unsupported item is dropped from the output. Its
                        // shape is unmodeled — provider text can live in any
                        // field or nested within one, and its `type` matched no
                        // literal of the adapter's — so every field of the item,
                        // its merely-validated identity included, folds as an
                        // independent unit (dropped-content rule).
                        fold_uninterpreted(sink, &value, &["type"], Some(("item", &[])));
                    }
                }
            }
            "turn.completed" => {
                // Completion claims a complete correlated response; without
                // the thread that establishes the exchange there is nothing
                // the evidence can be correlated to, so drifted streams fail
                // closed instead of completing with empty exchange facts.
                if self.exchange.provider_request_id.is_none() {
                    return Err(DecodeFailure::new(
                        "turn.completed arrived before thread.started established the exchange",
                    ));
                }
                let event: TurnCompleted = decode(value.clone())?;
                // Only the usage counters are interpreted; every other member
                // folds before the completion this terminal marker unlocks
                // emits the final text (dropped-content rule).
                fold_uninterpreted(sink, &value, &["type"], None);
                self.usage = usage(event.usage)?;
                self.terminal = Some(CliTerminal::Completed);
            }
            "turn.failed" => {
                let event: TurnFailed = decode(value.clone())?;
                // The retained agent message is no longer completion material;
                // fold it so a marker it ends in still governs the failure
                // message that supersedes it, as agent-message supersession does.
                self.fold_retained_agent_message(sink);
                // Additive failure fields fold before the interpreted message
                // is retained (dropped-content rule).
                fold_uninterpreted(sink, &value, &["type"], Some(("error", &["message"])));
                self.terminal = Some(CliTerminal::Failed(event.error.message));
            }
            "error" => {
                let event: ThreadError = decode(value.clone())?;
                self.fold_retained_agent_message(sink);
                fold_uninterpreted(sink, &value, &["type", "message"], None);
                self.terminal = Some(CliTerminal::Unrecoverable(event.message));
            }
            _ => {
                // Nothing in an additively-tolerated unknown top-level event
                // is interpreted — its `type` discriminator matched no known
                // event, so that value is provider-chosen text like every
                // other field — and all of it folds (dropped-content rule).
                fold_uninterpreted(sink, &value, &[], None);
            }
        }
        Ok(())
    }

    pub(crate) fn finish(self, sink: &mut RedactingSink<'_, C>) -> TerminalEvidence {
        match self.terminal {
            Some(CliTerminal::Failed(message)) => {
                provider_failure(self.exchange, self.usage, &message, sink)
            }
            Some(CliTerminal::Unrecoverable(message)) => {
                provider_failure(self.exchange, self.usage, &message, sink)
            }
            Some(CliTerminal::Completed) => self.completed(sink),
            None => boundary_loss_before_envelope(
                self.exchange,
                self.usage,
                LossCause::StreamEndedWithoutTerminalMarker {
                    interruption: signalbox_model_runtime::StreamInterruption::EndOfStream,
                },
            ),
        }
    }

    pub(crate) fn provider_error(self, message: &str) -> TerminalEvidence {
        provider_error(self.exchange, self.usage, message)
    }

    pub(crate) fn provider_error_after_exit(
        self,
        fallback: &str,
        sink: &RedactingSink<'_, C>,
    ) -> TerminalEvidence {
        match self.terminal {
            Some(CliTerminal::Failed(message)) => {
                provider_failure(self.exchange, self.usage, &message, sink)
            }
            Some(CliTerminal::Unrecoverable(message)) => {
                provider_failure(self.exchange, self.usage, &message, sink)
            }
            Some(CliTerminal::Completed) | None => {
                provider_failure(self.exchange, self.usage, fallback, sink)
            }
        }
    }

    pub(crate) fn boundary_loss(self, cause: LossCause) -> TerminalEvidence {
        boundary_loss_before_envelope(self.exchange, self.usage, cause)
    }

    pub(crate) fn boundary_loss_unless_provider_failure(
        self,
        cause: LossCause,
        sink: &RedactingSink<'_, C>,
    ) -> TerminalEvidence {
        match self.terminal {
            Some(CliTerminal::Failed(message)) => {
                provider_failure(self.exchange, self.usage, &message, sink)
            }
            Some(CliTerminal::Unrecoverable(message)) => {
                provider_failure(self.exchange, self.usage, &message, sink)
            }
            Some(CliTerminal::Completed) | None => {
                boundary_loss_before_envelope(self.exchange, self.usage, cause)
            }
        }
    }

    pub(crate) fn terminal_observed(&self) -> bool {
        self.terminal.is_some()
    }

    /// Folds a retained agent message (and its id) into the dropped lookbehind
    /// when it is displaced — by a later agent message or a failure terminal —
    /// so a credential marker ending it still governs the text that follows.
    /// The two are chained *in wire order*, id before text: the message text is
    /// the last of the pair the provider wrote, so it is what the following
    /// output continues, and a clean id can no longer resolve away the live
    /// marker (`api_`) that the text ends in. Chaining stays precise rather
    /// than taking the fail-closed multi-unit path — a benign message the
    /// streaming lookbehind holds but no value completes still flows unchanged.
    fn fold_retained_agent_message(&mut self, sink: &mut RedactingSink<'_, C>) {
        if let Some(superseded) = self.agent_message.take() {
            if let Some(previous_id) = self.message_id.clone() {
                sink.extend_dropped_context(&previous_id);
            }
            sink.extend_dropped_context(&superseded);
        }
    }

    fn completed(mut self, sink: &mut RedactingSink<'_, C>) -> TerminalEvidence {
        let agent_message = match self.agent_message.take() {
            Some(agent_message) => agent_message,
            None => match self.read_output_last_message() {
                Ok(Some(agent_message)) => agent_message,
                Ok(None) => {
                    report_response_envelope_rejection("missing");
                    return boundary_loss_before_envelope(
                        self.exchange,
                        self.usage,
                        LossCause::ResponseUnintelligible {
                            detail: "turn.completed carried no response envelope".to_string(),
                        },
                    );
                }
                Err(detail) => {
                    report_response_envelope_rejection("retained_message_unavailable");
                    return boundary_loss_before_envelope(
                        self.exchange,
                        self.usage,
                        LossCause::ResponseUnintelligible { detail },
                    );
                }
            },
        };
        if let Err(error) = validate_provider_json_nesting(agent_message.as_bytes()) {
            report_response_envelope_rejection("nesting_bound");
            return boundary_loss_before_envelope(
                self.exchange,
                self.usage,
                LossCause::ResponseUnintelligible {
                    detail: format!("last agent message exceeds JSON nesting bounds: {error}"),
                },
            );
        }
        if let Err(error) = reject_duplicate_json_members(&agent_message) {
            return boundary_loss_before_envelope(
                self.exchange,
                self.usage,
                LossCause::StreamProtocolViolation {
                    detail: format!(
                        "undecodable Codex response envelope: {}",
                        error.into_detail()
                    ),
                },
            );
        }
        let envelope: ModelEnvelope = match serde_json::from_str(&agent_message) {
            Ok(envelope) => envelope,
            Err(_) => {
                report_response_envelope_rejection("shape");
                return boundary_loss_before_envelope(
                    self.exchange,
                    self.usage,
                    LossCause::ResponseUnintelligible {
                        detail: "last agent message does not match the response envelope"
                            .to_string(),
                    },
                );
            }
        };
        // Derived from the parsed envelope alone, so it is a fact from here on
        // and every loss below retains it — a finish observed before a loss is
        // retained per docs/spec/runtime-substrate.md, and these losses were
        // discarding one the adapter had already decoded.
        let reported_finish = match envelope.outcome {
            EnvelopeOutcome::Refused => FinishReason::Refusal,
            EnvelopeOutcome::Completed if envelope.tool_calls.is_empty() => FinishReason::EndTurn,
            EnvelopeOutcome::Completed => FinishReason::ToolUse,
        };
        let mut content = match self.decode_content(&envelope, &mut *sink) {
            Ok(content) => content,
            Err(failure) => {
                report_response_envelope_rejection(failure.stage);
                return boundary_loss_after_envelope(
                    self.exchange,
                    self.usage,
                    Some(reported_finish.clone()),
                    &envelope,
                    LossCause::ResponseUnintelligible {
                        detail: failure.detail,
                    },
                );
            }
        };
        // Sanitized against the held lookbehind plus the same-envelope final
        // text — exactly as the tool-call id path is — before the usage
        // barrier below flushes the lookbehind, so a message id extending a
        // credential marker held from reasoning, or one ending the final text
        // itself, is suppressed instead of surfacing as `ProviderMessageId`
        // beside the independently redacted text.
        let message_id_context = trailing_credential_context(&envelope.text);
        let message_id = self.message_id.take().map(|id| {
            // The id both continues the final text (redact_provider_id's held
            // + preceding-text join) and, as the field preceding the content
            // in terminal evidence, can be continued by it: an id ending
            // `api_` beside content opening `key=opaque` reconstructs
            // `api_key=opaque` across the two fields. Redact the id in that
            // second direction too, breaking the shape before it is emitted.
            let sanitized = sink.redact_provider_id(message_id_context, &id);
            let id_precedes_text = [id.as_str(), envelope.text.as_str()].concat();
            if sanitized == id && redact_text(&id_precedes_text) != id_precedes_text {
                REDACTED.to_string()
            } else {
                sanitized
            }
        });
        if self.delivery == DeliveryMode::Streamed {
            sink.begin_streaming_terminal_text_capture();
        }
        if let Err(detail) = self.emit_completion_observations(
            sink,
            &content,
            &envelope.text,
            reported_finish.clone(),
        ) {
            report_response_envelope_rejection("observation_projection");
            return boundary_loss_after_envelope(
                self.exchange,
                self.usage,
                Some(reported_finish.clone()),
                &envelope,
                LossCause::ResponseUnintelligible { detail },
            );
        }
        if self.delivery == DeliveryMode::Streamed {
            // The usage barrier inside the observation emission above flushed
            // every held lookbehind fragment, so the capture now holds the
            // stateful cross-fragment redaction of the final text. Terminal
            // evidence carries exactly those bytes; an independent stateless
            // re-redaction of the raw text would miss a credential value whose
            // marker arrived in an earlier fragment.
            let captured = sink.take_terminal_text_capture().into_text();
            if let Some(index) = content
                .iter()
                .position(|part| matches!(part, AssistantPart::Text(_)))
            {
                // An empty capture means no final-text delta reached the caller
                // (the raw text was empty, or fully held and never emitted); a
                // provisional `[redacted]` text part — which a held credential
                // makes `redact_terminal_failure_text("")` return, passing the
                // decode-time non-empty check — must not survive as empty
                // completion material. Drop it and re-check that some material
                // remains, so an otherwise-empty completion fails closed as
                // ResponseUnintelligible rather than a contentless Completed.
                if captured.is_empty() {
                    content.remove(index);
                } else {
                    content[index] = AssistantPart::Text(captured);
                }
            }
            // An explicit `refused` outcome is definitive evidence on its own,
            // so a textless refusal stays a refusal here exactly as buffered
            // delivery already returns it; only an ordinary completion needs
            // material to be intelligible, and only it fails closed when the
            // capture leaves none.
            if content.is_empty()
                && self.output_contract_name.is_none()
                && envelope.outcome != EnvelopeOutcome::Refused
            {
                report_response_envelope_rejection("streamed_completion_empty");
                return boundary_loss_after_envelope(
                    self.exchange,
                    self.usage,
                    Some(reported_finish),
                    &envelope,
                    LossCause::ResponseUnintelligible {
                        detail: "streamed response envelope carries no completion material"
                            .to_string(),
                    },
                );
            }
        }

        match envelope.outcome {
            EnvelopeOutcome::Refused => TerminalEvidence::Refused(RefusalEvidence {
                exchange: self.exchange,
                message_id: message_id.map(ProviderMessageId::new),
                reported_model: None,
                content,
                usage: self.usage,
            }),
            EnvelopeOutcome::Completed => {
                let finish = if envelope.tool_calls.is_empty() {
                    CompletionFinish::EndTurn
                } else {
                    CompletionFinish::ToolUse
                };
                TerminalEvidence::Completed(CompletionEvidence {
                    exchange: self.exchange,
                    message_id: message_id.map(ProviderMessageId::new),
                    reported_model: None,
                    finish,
                    content,
                    usage: self.usage,
                })
            }
        }
    }

    /// Reads the pinned CLI's independently retained final message only when
    /// JSONL did not deliver an agent-message item. The process has exited
    /// before `finish` runs, and the same event-size ceiling bounds this
    /// secondary representation before allocation or UTF-8 decoding.
    fn read_output_last_message(&self) -> Result<Option<String>, String> {
        let file = std::fs::File::open(&self.output_last_message).map_err(|_| {
            "Codex output-last-message file could not be opened after completion".to_string()
        })?;
        let read_limit = u64::try_from(self.terminal_message_limit)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::new();
        file.take(read_limit).read_to_end(&mut bytes).map_err(|_| {
            "Codex output-last-message file could not be read after completion".to_string()
        })?;
        if bytes.len() > self.terminal_message_limit {
            return Err("Codex output-last-message exceeded the event-size bound".to_string());
        }
        if bytes.is_empty() {
            return Ok(None);
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| "Codex output-last-message was not UTF-8".to_string())
    }

    fn decode_content(
        &self,
        envelope: &ModelEnvelope,
        sink: &mut RedactingSink<'_, C>,
    ) -> Result<Vec<AssistantPart>, ResponseEnvelopeFailure> {
        if envelope.outcome == EnvelopeOutcome::Refused && !envelope.tool_calls.is_empty() {
            return Err(ResponseEnvelopeFailure::new(
                "refusal_with_tools",
                "a refusal envelope also proposed tools",
            ));
        }
        let mut content = Vec::new();
        // Consults the held lookbehind (including the emitted thread-id and
        // dropped-reasoning contexts), not just the stateless scan: a buffered
        // final text whose start completes a credential marker ending either
        // chain must be suppressed whole, exactly as the streamed path
        // suppresses it delta by delta. Buffered delivery also resolves both
        // chains through this text — a chain the text broke must not misfire
        // on the clean provider ids that follow. Streamed delivery must NOT
        // consume the chains here: the same text is about to flow through the
        // delta machinery, which needs the live context to suppress the
        // continuation and resolves the chains itself.
        let text = if self.delivery == DeliveryMode::Buffered {
            sink.redact_final_envelope_text(&envelope.text)
        } else {
            sink.redact_terminal_failure_text(&envelope.text)
        };
        if !text.is_empty() {
            content.push(AssistantPart::Text(text));
        }
        if envelope.outcome == EnvelopeOutcome::Refused {
            return Ok(content);
        }

        // The tool fields need only the final text's trailing credential
        // context, not the whole (possibly multi-megabyte) text, so a
        // credential spanning the text end and a field is caught without
        // rescanning the full text per call.
        let final_text_context = trailing_credential_context(&envelope.text);
        let mut raw_ids = HashSet::new();
        let mut clean_ids = HashSet::new();
        for call in &envelope.tool_calls {
            if call.id.is_empty() || !raw_ids.insert(call.id.as_str()) {
                return Err(ResponseEnvelopeFailure::new(
                    "tool_call_id",
                    "tool call ids must be nonempty and distinct",
                ));
            }
            // An id is clean only if neither the stateless scan nor the held
            // cross-fragment lookbehind (including the same-envelope final
            // text) would redact it, so a marker held from reasoning or ending
            // the final text cannot leave a matching id in the clean set.
            let sanitized = sink.redact_provider_id(final_text_context, &call.id);
            if sanitized == call.id {
                clean_ids.insert(sanitized);
            }
        }
        let mut redacted_id_cursor = 1_usize;
        for call in &envelope.tool_calls {
            let allowed = self.declared_tools.contains(&call.name)
                || self.output_contract_name.as_deref() == Some(call.name.as_str());
            if !allowed {
                return Err(ResponseEnvelopeFailure::new(
                    "undeclared_tool",
                    format!(
                        "response proposed undeclared tool `{}`",
                        sink.redact_provider_id(final_text_context, &call.name)
                    ),
                ));
            }
            // The envelope carries the provider's argument text inside a
            // string because strict structured output forbids a free-form
            // object (see `wire::EnvelopeToolCall`). Preserve that text even
            // when it is malformed or not an object: `ToolCallProposal`
            // deliberately owns raw provider text, and the provider-independent
            // typed decoders report a typed decode failure the caller acts on,
            // which the tool loop projects as its `invalid_arguments` result
            // for the next model round. The shared nesting bound still applies
            // to the contained text, as it does in the sibling adapters that
            // receive string-carried arguments: string content is invisible to
            // the line-level and agent-message-level checks, and the shared
            // typed decoders admit only serde_json's recursion boundary.
            validate_tool_argument_nesting(&call.arguments, &call.name)?;
            // The id consults the same held lookbehind the arguments do —
            // including the same-envelope final text — so an id extending a
            // credential marker gets a safe surrogate instead of leaking.
            let sanitized = sink.redact_provider_id(final_text_context, &call.id);
            let id = if sanitized == call.id {
                sanitized
            } else {
                next_redacted_call_id(&mut redacted_id_cursor, &clean_ids)
            };
            // The arguments consult the held cross-fragment lookbehind before
            // stateless JSON-aware redaction. A whole-object suppression is
            // typed separately so no executable sentinel request can cross
            // the adapter boundary and churn through tool rounds.
            match sink.redact_tool_arguments(final_text_context, &call.arguments) {
                ToolArgumentRedaction::Admitted(arguments_json) => {
                    content.push(AssistantPart::ToolCall(ToolCallProposal {
                        id: ToolCallId::new(id),
                        name: ToolName::new(call.name.clone()),
                        arguments_json,
                    }));
                }
                ToolArgumentRedaction::Suppressed => {
                    content.push(AssistantPart::SuppressedToolCall(ToolName::new(
                        call.name.clone(),
                    )));
                }
            }
        }
        if let Some(contract_name) = &self.output_contract_name {
            if !envelope
                .tool_calls
                .iter()
                .all(|call| &call.name == contract_name)
            {
                return Err(ResponseEnvelopeFailure::new(
                    "structured_output_tool",
                    format!("structured output permits only `{contract_name}` proposals"),
                ));
            }
        } else {
            match &self.tool_requirement {
                ToolRequirement::Optional => {}
                ToolRequirement::Any if envelope.tool_calls.is_empty() => {
                    return Err(ResponseEnvelopeFailure::new(
                        "required_tool_missing",
                        "tool choice requires a proposal",
                    ));
                }
                ToolRequirement::Named(name)
                    if envelope.tool_calls.is_empty()
                        || !envelope.tool_calls.iter().all(|call| &call.name == name) =>
                {
                    return Err(ResponseEnvelopeFailure::new(
                        "named_tool_mismatch",
                        format!("tool choice permits only `{name}`"),
                    ));
                }
                ToolRequirement::Any | ToolRequirement::Named(_) => {}
            }
        }
        if content.is_empty() && self.output_contract_name.is_none() {
            return Err(ResponseEnvelopeFailure::new(
                "completion_empty",
                "response envelope carries no completion material",
            ));
        }
        Ok(content)
    }

    fn emit_completion_observations(
        &mut self,
        sink: &mut RedactingSink<'_, C>,
        content: &[AssistantPart],
        raw_text: &str,
        finish: FinishReason,
    ) -> Result<(), String> {
        if self.delivery == DeliveryMode::Streamed {
            let content_len = u32::try_from(content.len())
                .map_err(|_| "response has too many ordered parts".to_string())?;
            self.next_part_index
                .checked_add(content_len)
                .ok_or_else(|| "response has too many ordered parts".to_string())?;
            for (index, part) in content.iter().enumerate() {
                let offset = u32::try_from(index)
                    .map_err(|_| "response has too many ordered parts".to_string())?;
                let index = self.next_part_index + offset;
                match part {
                    AssistantPart::Text(_) => sink.observe(Observation {
                        correlation: self.correlation.clone(),
                        fact: ObservationFact::TextDelta {
                            index,
                            text: raw_text.to_string(),
                        },
                    }),
                    AssistantPart::ToolCall(call) => sink.observe(Observation {
                        correlation: self.correlation.clone(),
                        fact: ObservationFact::ToolArgumentsDelta {
                            index,
                            fragment: call.arguments_json.clone(),
                        },
                    }),
                    AssistantPart::Thinking { .. }
                    | AssistantPart::RedactedThinking { .. }
                    | AssistantPart::SuppressedToolCall(_) => {}
                }
            }
            self.next_part_index += content_len;
        }
        for part in content {
            if let AssistantPart::ToolCall(call) = part {
                sink.observe(Observation {
                    correlation: self.correlation.clone(),
                    fact: ObservationFact::ToolCallProposed(call.clone()),
                });
            }
        }
        sink.observe(Observation {
            correlation: self.correlation.clone(),
            fact: ObservationFact::UsageReported(self.usage),
        });
        sink.observe(Observation {
            correlation: self.correlation.clone(),
            fact: ObservationFact::FinishReported(finish),
        });
        Ok(())
    }

    fn take_part_index(&mut self) -> Result<u32, DecodeFailure> {
        let index = self.next_part_index;
        self.next_part_index = self
            .next_part_index
            .checked_add(1)
            .ok_or_else(|| DecodeFailure::new("response has too many ordered parts"))?;
        Ok(index)
    }
}

struct ResponseEnvelopeFailure {
    stage: &'static str,
    detail: String,
}

impl ResponseEnvelopeFailure {
    fn new(stage: &'static str, detail: impl Into<String>) -> Self {
        Self {
            stage,
            detail: detail.into(),
        }
    }
}

fn report_response_envelope_rejection(stage: &'static str) {
    tracing::warn!(
        cause_code = "codex_response_envelope_rejected",
        stage,
        "Codex completed-turn response envelope was rejected"
    );
}

fn validate_item_identity(item: &ItemIdentity) -> Result<(), DecodeFailure> {
    if item.id.is_empty() || item.item_type.is_empty() {
        return Err(DecodeFailure::new(
            "item lifecycle event carries an empty item identity or type",
        ));
    }
    Ok(())
}

/// Collects the independent credential *units* of provider-controlled content
/// the adapter drops without interpreting. An object's field values are
/// separate units at every nesting depth — they are not one continuous text,
/// and serde's `BTreeMap` iteration is key-sorted rather than wire order, so a
/// benign sibling must not erase another's marker. An array's elements are
/// wire-adjacent, so the string leaves running between nested objects join into
/// a single unit (an array `["api", "_key="]` forms `api_key=`).
fn collect_units(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => {
            let mut adjacent = String::new();
            for item in items {
                collect_array_element(item, &mut adjacent, out);
            }
            push_unit(adjacent, out);
        }
        Value::Object(fields) => {
            for field in fields.values() {
                collect_units(field, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Collects one array element into the enclosing array's wire-adjacent run.
/// String and nested-array leaves extend the run; a nested object's fields stay
/// independent units of their own, so the object ends the run rather than being
/// flattened into it merely because an ancestor is an array.
fn collect_array_element(value: &Value, adjacent: &mut String, out: &mut Vec<String>) {
    match value {
        Value::String(text) => adjacent.push_str(text),
        Value::Array(items) => {
            for item in items {
                collect_array_element(item, adjacent, out);
            }
        }
        Value::Object(fields) => {
            push_unit(std::mem::take(adjacent), out);
            for field in fields.values() {
                collect_units(field, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn push_unit(unit: String, out: &mut Vec<String>) {
    if !unit.is_empty() {
        out.push(unit);
    }
}

/// Appends the independent dropped units of `object`'s fields other than the
/// `interpreted` ones. A field counts as interpreted only when the adapter
/// matched it against a known constant (the `type` discriminator that selected
/// a known arm, whose value is therefore the adapter's own literal) or
/// separately sanitizes and emits it (`thread.started`'s thread id, an agent
/// message's retained id and text). Every other field — including the metadata
/// the adapter merely validates and then drops — is provider-controlled content
/// whose credential candidate must still govern the output that follows.
fn collect_uninterpreted_units(
    object: Option<&Value>,
    interpreted: &[&str],
    out: &mut Vec<String>,
) {
    match object {
        Some(Value::Object(fields)) => {
            for (key, field) in fields {
                if !interpreted.contains(&key.as_str()) {
                    collect_units(field, out);
                }
            }
        }
        // A non-object member models no interpreted field, so all of it is
        // dropped content.
        Some(value) => collect_units(value, out),
        None => {}
    }
}

/// Folds the fields of a decoded event that the adapter did not interpret —
/// and, when `member` names one, those of a nested member object such as
/// `item` or `error` — into the redaction lookbehind. Both levels are decided
/// together in one call, so the event's and the member's independent units are
/// weighed against each other rather than seeded one chain after the other.
/// The member itself is excluded at the event level and governed by its own
/// interpreted list instead.
fn fold_uninterpreted<C: Clone>(
    sink: &mut RedactingSink<'_, C>,
    value: &Value,
    interpreted: &[&str],
    member: Option<(&str, &[&str])>,
) {
    let mut units = Vec::new();
    let mut event_interpreted = interpreted.to_vec();
    if let Some((name, _)) = member {
        event_interpreted.push(name);
    }
    collect_uninterpreted_units(Some(value), &event_interpreted, &mut units);
    if let Some((name, member_interpreted)) = member {
        collect_uninterpreted_units(value.get(name), member_interpreted, &mut units);
    }
    fold_dropped_units(sink, units.iter().map(String::as_str));
}

/// Seeds independent dropped credential units into the lookbehind. The
/// sink holds one dropped chain, so exactly one live marker unit is seeded
/// precisely (a benign prefix no value completes then flows unchanged, keeping
/// the keepalive flood among unknown events from being suppressed). When two or
/// more *independent* units each end in an unterminated credential candidate,
/// a following value could complete any one of them and the single chain
/// cannot track them all, so the sink fails closed. Units with no credential
/// candidate are ignored.
fn fold_dropped_units<'a, C: Clone>(
    sink: &mut RedactingSink<'_, C>,
    units: impl IntoIterator<Item = &'a str>,
) {
    let markers: Vec<&str> = units
        .into_iter()
        .filter(|unit| !trailing_credential_context(unit).is_empty())
        .collect();
    match markers.as_slice() {
        [] => {}
        [only] => sink.extend_dropped_context(only),
        _ => sink.suppress_remaining(),
    }
}

/// Requires a string-carried tool-argument payload to stay within the
/// provider nesting bound, whatever its syntax.
///
/// The argument text arrives inside a JSON string, so the line-level nesting
/// validation in `push` never saw its structure, and the shared typed decoders
/// that consume the preserved text admit only serde_json's recursion boundary.
/// Syntax and shape are deliberately not judged here: malformed and non-object
/// text is authoritative proposal material the typed decoders classify. Failure
/// detail names only the redacted tool name, never the argument text itself.
fn validate_tool_argument_nesting(
    arguments: &str,
    tool_name: &str,
) -> Result<(), ResponseEnvelopeFailure> {
    validate_provider_json_nesting(arguments.as_bytes()).map_err(|error| {
        ResponseEnvelopeFailure::new(
            "tool_arguments_nesting",
            format!("tool `{}` arguments: {error}", redact_text(tool_name)),
        )
    })
}

/// Allocates the next distinct redacted-call surrogate from a monotonic
/// cursor, skipping only names that collide with a clean provider id. The
/// cursor never resets, so total work across a completion is linear in the
/// tool-call count plus the fixed clean-id set rather than quadratic.
fn next_redacted_call_id(cursor: &mut usize, clean_ids: &HashSet<String>) -> String {
    loop {
        let candidate = format!("codex-redacted-call-{cursor}");
        *cursor += 1;
        if !clean_ids.contains(&candidate) {
            return candidate;
        }
    }
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, DecodeFailure> {
    serde_json::from_value(value)
        .map_err(|error| DecodeFailure::new(format!("known event has invalid shape: {error}")))
}

fn usage(usage: crate::wire::Usage) -> Result<TokenUsage, DecodeFailure> {
    Ok(TokenUsage {
        input_tokens: optional_usage(usage.input_tokens, "input_tokens")?,
        output_tokens: optional_usage(usage.output_tokens, "output_tokens")?,
        cache_creation_input_tokens: optional_usage(
            usage.cache_write_input_tokens,
            "cache_write_input_tokens",
        )?,
        cache_read_input_tokens: optional_usage(usage.cached_input_tokens, "cached_input_tokens")?,
    })
}

fn optional_usage(value: Option<i64>, name: &str) -> Result<Option<u64>, DecodeFailure> {
    value
        .map(|value| {
            u64::try_from(value)
                .map_err(|_| DecodeFailure::new(format!("usage {name} is negative")))
        })
        .transpose()
}

fn provider_error(exchange: ExchangeFacts, usage: TokenUsage, message: &str) -> TerminalEvidence {
    TerminalEvidence::ProviderError(ProviderErrorEvidence {
        exchange,
        reported_model: None,
        kind: ProviderErrorKind::Unrecognized,
        non_acceptance_proven: false,
        native: NativeErrorFacts {
            error_token: Some("codex_cli_error".to_string()),
            error_code: None,
            message: Some(redact_text(message)),
        },
        usage,
    })
}

/// Builds provider-failure evidence whose message consulted the stateful
/// stream redactor: a failure message that extends a credential candidate
/// held from streamed text is suppressed whole, exactly as the streamed
/// fragments it continues would have been, instead of receiving an
/// independent stateless re-redaction that cannot see the held marker.
fn provider_failure<C: Clone>(
    exchange: ExchangeFacts,
    usage: TokenUsage,
    message: &str,
    sink: &RedactingSink<'_, C>,
) -> TerminalEvidence {
    TerminalEvidence::ProviderError(ProviderErrorEvidence {
        exchange,
        reported_model: None,
        kind: ProviderErrorKind::Unrecognized,
        non_acceptance_proven: false,
        native: NativeErrorFacts {
            error_token: Some("codex_cli_error".to_string()),
            error_code: None,
            message: Some(sink.redact_terminal_failure_text(message)),
        },
        usage,
    })
}

/// Boundary loss raised before the response envelope was parsed.
///
/// This adapter cannot answer whether a tool call had opened at such a loss.
/// The CLI's item lifecycle carries no tool item — `ItemDetails` has no tool
/// variant — and a turn's tool calls exist only inside the agent-message
/// envelope, which stays unparsed JSON text in `agent_message` until the
/// terminal event. The fact is therefore [`ToolCallsAtLoss::Unobserved`], not
/// a claim that none opened.
fn boundary_loss_before_envelope(
    exchange: ExchangeFacts,
    usage: TokenUsage,
    cause: LossCause,
) -> TerminalEvidence {
    boundary_loss_with_finish(exchange, usage, None, ToolCallsAtLoss::Unobserved, cause)
}

/// Boundary loss raised once the response envelope parsed, so the turn's tool
/// announcements are in hand and the fact is stated rather than withheld.
fn boundary_loss_after_envelope(
    exchange: ExchangeFacts,
    usage: TokenUsage,
    finish_reported: Option<FinishReason>,
    envelope: &ModelEnvelope,
    cause: LossCause,
) -> TerminalEvidence {
    let tool_calls = if envelope.tool_calls.is_empty() {
        ToolCallsAtLoss::NoneOpened
    } else {
        ToolCallsAtLoss::Opened
    };
    boundary_loss_with_finish(exchange, usage, finish_reported, tool_calls, cause)
}

fn boundary_loss_with_finish(
    exchange: ExchangeFacts,
    usage: TokenUsage,
    finish_reported: Option<FinishReason>,
    tool_calls: ToolCallsAtLoss,
    cause: LossCause,
) -> TerminalEvidence {
    TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
        cause,
        exchange,
        reported_model: None,
        finish_reported,
        tool_calls,
        usage,
    })
}

#[derive(Clone, Copy)]
pub(crate) enum DecodeFailureClass {
    ProviderDecode,
    StreamProtocolViolation,
}

pub(crate) struct DecodeFailure {
    detail: String,
    class: DecodeFailureClass,
}

impl DecodeFailure {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            class: DecodeFailureClass::ProviderDecode,
        }
    }

    fn stream_protocol(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            class: DecodeFailureClass::StreamProtocolViolation,
        }
    }

    pub(crate) fn class(&self) -> DecodeFailureClass {
        self.class
    }

    pub(crate) fn into_detail(self) -> String {
        self.detail
    }
}

impl<C: Clone> CliSession<C> for EventDecoder<C> {
    const LABELS: CliProcessLabels = CliProcessLabels {
        provider: "Codex",
        process: "Codex CLI",
        decode_event: "Codex event",
        bounded_event: "Codex JSONL event",
    };

    fn correlation(&self) -> &C {
        &self.correlation
    }

    fn terminal_text_capture(&self) -> CliTerminalTextCapture {
        CliTerminalTextCapture::Disabled
    }

    fn terminal_observed(&self) -> bool {
        EventDecoder::terminal_observed(self)
    }

    fn push(
        &mut self,
        line: &[u8],
        sink: &mut RedactingSink<'_, C>,
    ) -> Result<(), CliDecodeFailure> {
        EventDecoder::push(self, line, sink).map_err(|error| {
            let class = match error.class() {
                DecodeFailureClass::ProviderDecode => CliDecodeFailureClass::ProviderDecode,
                DecodeFailureClass::StreamProtocolViolation => {
                    CliDecodeFailureClass::StreamProtocolViolation
                }
            };
            CliDecodeFailure::new(class, error.into_detail())
        })
    }

    fn decode_failure(self, class: CliDecodeFailureClass, detail: String) -> TerminalEvidence {
        match class {
            CliDecodeFailureClass::ProviderDecode => self.provider_error(&detail),
            CliDecodeFailureClass::StreamProtocolViolation => {
                self.boundary_loss(LossCause::StreamProtocolViolation { detail })
            }
        }
    }

    fn finish(self, sink: &mut RedactingSink<'_, C>) -> TerminalEvidence {
        EventDecoder::finish(self, sink)
    }

    fn boundary_loss(self, cause: LossCause) -> TerminalEvidence {
        EventDecoder::boundary_loss(self, cause)
    }

    fn boundary_loss_unless_provider_failure(
        self,
        cause: LossCause,
        sink: &mut RedactingSink<'_, C>,
    ) -> TerminalEvidence {
        EventDecoder::boundary_loss_unless_provider_failure(self, cause, sink)
    }

    fn provider_error_after_exit(
        self,
        message: &str,
        sink: &mut RedactingSink<'_, C>,
    ) -> TerminalEvidence {
        EventDecoder::provider_error_after_exit(self, message, sink)
    }
}
