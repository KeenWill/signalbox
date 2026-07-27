//! Stateful decoding of one Codex exec JSONL event stream.

use std::collections::HashSet;

use serde::de::DeserializeOwned;
use serde_json::Value;
use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CompletionEvidence, CompletionFinish, DeliveryMode,
    ExchangeFacts, FinishReason, LossCause, NativeErrorFacts, Observation, ObservationFact,
    ObservationSink, ProviderErrorEvidence, ProviderMessageId, ProviderRequestId, RefusalEvidence,
    TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal, ToolName,
    validate_provider_json_nesting,
};

use crate::redaction::{RedactingSink, redact_text};
use crate::status::classify_error;
use crate::translate::{ToolRequirement, TranslatedOperation};
use crate::wire::{
    EnvelopeOutcome, ItemDetails, ItemEvent, ItemIdentity, ItemLifecycleEvent, ModelEnvelope,
    ThreadError, ThreadStarted, TurnCompleted, TurnFailed,
};

pub(crate) struct EventDecoder<C> {
    correlation: C,
    delivery: DeliveryMode,
    declared_tools: HashSet<String>,
    output_contract_name: Option<String>,
    tool_requirement: ToolRequirement,
    exchange: ExchangeFacts,
    message_id: Option<String>,
    agent_message: Option<String>,
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
    /// sequencing the gated compatibility smoke observed live — so exactly
    /// that one trailer is still admissible.
    Unrecoverable(String),
}

impl<C: Clone> EventDecoder<C> {
    pub(crate) fn new(
        correlation: C,
        delivery: DeliveryMode,
        translated: &TranslatedOperation,
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
            let event: TurnFailed = decode(value)?;
            // Only the echo closes the turn: a trailer carrying a different
            // failure contradicts the recorded terminal and fails closed
            // rather than silently keeping either message.
            if event.error.message != stream_error {
                return Err(DecodeFailure::new(
                    "Codex contradicted its stream error with a different turn.failed failure",
                ));
            }
            self.terminal = Some(CliTerminal::Failed(stream_error));
            return Ok(());
        }
        match event_type {
            "thread.started" => {
                let event: ThreadStarted = decode(value)?;
                if event.thread_id.is_empty() || self.exchange.provider_request_id.is_some() {
                    return Err(DecodeFailure::new(
                        "thread.started carries an empty or duplicate thread id",
                    ));
                }
                // Sanitized against the held lookbehind before the
                // ExchangeEstablished observation (which flushes it), so a
                // thread id that extends a credential marker held from a
                // drifted earlier reasoning delta is suppressed instead of
                // escaping in the observation and terminal exchange facts.
                self.exchange.provider_request_id = Some(ProviderRequestId::new(
                    sink.redact_terminal_failure_text(&event.thread_id),
                ));
                sink.observe(Observation {
                    correlation: self.correlation.clone(),
                    fact: ObservationFact::ExchangeEstablished(self.exchange.clone()),
                });
            }
            "turn.started" => {}
            "item.started" | "item.updated" => {
                let event: ItemLifecycleEvent = decode(value)?;
                validate_item_identity(&event.item)?;
            }
            "item.completed" => {
                let identity: ItemLifecycleEvent = decode(value.clone())?;
                validate_item_identity(&identity.item)?;
                let event: ItemEvent = decode(value)?;
                match event.item.details {
                    ItemDetails::AgentMessage { text } => {
                        // The id is retained raw and sanitized against the
                        // held stream state in `completed`, so a value that
                        // extends a credential marker from earlier reasoning
                        // cannot reach `ProviderMessageId` unredacted.
                        self.message_id = Some(event.item.id.clone());
                        self.agent_message = Some(text);
                    }
                    ItemDetails::Reasoning { text } => {
                        if self.delivery == DeliveryMode::Streamed {
                            let index = self.take_part_index()?;
                            sink.observe(Observation {
                                correlation: self.correlation.clone(),
                                fact: ObservationFact::ThinkingDelta { index, text },
                            });
                        }
                    }
                    ItemDetails::Error { message } => {
                        let _ = redact_text(&message);
                    }
                    ItemDetails::Other => {}
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
                let event: TurnCompleted = decode(value)?;
                self.usage = usage(event.usage)?;
                self.terminal = Some(CliTerminal::Completed);
            }
            "turn.failed" => {
                let event: TurnFailed = decode(value)?;
                self.terminal = Some(CliTerminal::Failed(event.error.message));
            }
            "error" => {
                let event: ThreadError = decode(value)?;
                self.terminal = Some(CliTerminal::Unrecoverable(event.message));
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn finish(self, sink: &mut RedactingSink<'_, C>) -> TerminalEvidence {
        match self.terminal {
            Some(CliTerminal::Failed(message) | CliTerminal::Unrecoverable(message)) => {
                provider_failure(self.exchange, self.usage, &message, sink)
            }
            Some(CliTerminal::Completed) => self.completed(sink),
            None => boundary_loss(
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
        fallback_classification: &str,
        sink: &RedactingSink<'_, C>,
    ) -> TerminalEvidence {
        match self.terminal {
            Some(CliTerminal::Failed(message) | CliTerminal::Unrecoverable(message)) => {
                provider_failure(self.exchange, self.usage, &message, sink)
            }
            // The fallback message is already statefully sanitized, so it is
            // classified from the bounded raw text instead: an error phrase
            // sharing a line with a consumed credential marker must still
            // reach the classifier even though it is absent from what leaves
            // the adapter.
            Some(CliTerminal::Completed) | None => provider_failure_classified(
                self.exchange,
                self.usage,
                fallback,
                fallback_classification,
                sink,
            ),
        }
    }

    pub(crate) fn boundary_loss(self, cause: LossCause) -> TerminalEvidence {
        boundary_loss(self.exchange, self.usage, cause)
    }

    pub(crate) fn boundary_loss_unless_provider_failure(
        self,
        cause: LossCause,
        sink: &RedactingSink<'_, C>,
    ) -> TerminalEvidence {
        match self.terminal {
            Some(CliTerminal::Failed(message) | CliTerminal::Unrecoverable(message)) => {
                provider_failure(self.exchange, self.usage, &message, sink)
            }
            Some(CliTerminal::Completed) | None => boundary_loss(self.exchange, self.usage, cause),
        }
    }

    pub(crate) fn terminal_observed(&self) -> bool {
        self.terminal.is_some()
    }

    fn completed(mut self, sink: &mut RedactingSink<'_, C>) -> TerminalEvidence {
        let Some(agent_message) = self.agent_message.take() else {
            return boundary_loss(
                self.exchange,
                self.usage,
                LossCause::ResponseUnintelligible {
                    detail: "turn.completed carried no response envelope".to_string(),
                },
            );
        };
        if let Err(error) = validate_provider_json_nesting(agent_message.as_bytes()) {
            return boundary_loss(
                self.exchange,
                self.usage,
                LossCause::ResponseUnintelligible {
                    detail: format!("last agent message exceeds JSON nesting bounds: {error}"),
                },
            );
        }
        let envelope: ModelEnvelope = match serde_json::from_str(&agent_message) {
            Ok(envelope) => envelope,
            Err(_) => {
                return boundary_loss(
                    self.exchange,
                    self.usage,
                    LossCause::ResponseUnintelligible {
                        detail: "last agent message does not match the response envelope"
                            .to_string(),
                    },
                );
            }
        };
        let mut content = match self.decode_content(&envelope, sink) {
            Ok(content) => content,
            Err(detail) => {
                return boundary_loss(
                    self.exchange,
                    self.usage,
                    LossCause::ResponseUnintelligible { detail },
                );
            }
        };
        // Sanitized against the held lookbehind before the usage barrier below
        // flushes it, so a message id extending a credential marker held from
        // reasoning is suppressed instead of surfacing as `ProviderMessageId`.
        let message_id = self
            .message_id
            .take()
            .map(|id| sink.redact_terminal_failure_text(&id));
        let reported_finish = match envelope.outcome {
            EnvelopeOutcome::Refused => FinishReason::Refusal,
            EnvelopeOutcome::Completed if envelope.tool_calls.is_empty() => FinishReason::EndTurn,
            EnvelopeOutcome::Completed => FinishReason::ToolUse,
        };
        if self.delivery == DeliveryMode::Streamed {
            sink.begin_terminal_text_capture();
        }
        if let Err(detail) = self.emit_completion_observations(
            sink,
            &content,
            &envelope.text,
            reported_finish.clone(),
        ) {
            return boundary_loss(
                self.exchange,
                self.usage,
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
            let captured = sink.take_terminal_text_capture();
            if let Some(AssistantPart::Text(text)) = content
                .iter_mut()
                .find(|part| matches!(part, AssistantPart::Text(_)))
            {
                *text = captured;
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

    fn decode_content(
        &self,
        envelope: &ModelEnvelope,
        sink: &RedactingSink<'_, C>,
    ) -> Result<Vec<AssistantPart>, String> {
        if envelope.outcome == EnvelopeOutcome::Refused && !envelope.tool_calls.is_empty() {
            return Err("a refusal envelope also proposed tools".to_string());
        }
        let mut content = Vec::new();
        let text = redact_text(&envelope.text);
        if !text.is_empty() {
            content.push(AssistantPart::Text(text));
        }
        if envelope.outcome == EnvelopeOutcome::Refused {
            return Ok(content);
        }

        let mut raw_ids = HashSet::new();
        let mut clean_ids = HashSet::new();
        for call in &envelope.tool_calls {
            if call.id.is_empty() || !raw_ids.insert(call.id.as_str()) {
                return Err("tool call ids must be nonempty and distinct".to_string());
            }
            // An id is clean only if neither the stateless scan nor the held
            // cross-fragment lookbehind (including the same-envelope final
            // text) would redact it, so a marker held from reasoning or ending
            // the final text cannot leave a matching id in the clean set.
            let sanitized = sink.redact_provider_id(&envelope.text, &call.id);
            if sanitized == call.id {
                clean_ids.insert(sanitized);
            }
        }
        let mut redacted_id_cursor = 1_usize;
        for call in &envelope.tool_calls {
            let allowed = self.declared_tools.contains(&call.name)
                || self.output_contract_name.as_deref() == Some(call.name.as_str());
            if !allowed {
                return Err(format!(
                    "response proposed undeclared tool `{}`",
                    sink.redact_provider_id(&envelope.text, &call.name)
                ));
            }
            // The envelope carries the argument object as JSON text inside a
            // string, because strict structured output forbids a free-form
            // object (see `wire::EnvelopeToolCall`). Parsing here restores
            // the trait contract: the contained JSON object reaches the
            // caller byte-verbatim when it is credential-shape clean.
            validate_tool_arguments(&call.arguments, &call.name)?;
            // The id consults the same held lookbehind the arguments do —
            // including the same-envelope final text — so an id extending a
            // credential marker gets a safe surrogate instead of leaking.
            let sanitized = sink.redact_provider_id(&envelope.text, &call.id);
            let id = if sanitized == call.id {
                sanitized
            } else {
                next_redacted_call_id(&mut redacted_id_cursor, &clean_ids)
            };
            content.push(AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new(id),
                name: ToolName::new(call.name.clone()),
                // The arguments consult the held cross-fragment lookbehind
                // before the stateless JSON-aware redaction, and this same
                // sanitized value feeds the streamed argument delta and the
                // terminal proposal, so a credential whose marker arrived in
                // an earlier fragment cannot escape through tool arguments.
                arguments_json: sink.redact_tool_arguments(&envelope.text, &call.arguments),
            }));
        }
        if let Some(contract_name) = &self.output_contract_name {
            if !envelope
                .tool_calls
                .iter()
                .all(|call| &call.name == contract_name)
            {
                return Err(format!(
                    "structured output permits only `{contract_name}` proposals"
                ));
            }
        } else {
            match &self.tool_requirement {
                ToolRequirement::Optional => {}
                ToolRequirement::Any if envelope.tool_calls.is_empty() => {
                    return Err("tool choice requires a proposal".to_string());
                }
                ToolRequirement::Named(name)
                    if envelope.tool_calls.is_empty()
                        || !envelope.tool_calls.iter().all(|call| &call.name == name) =>
                {
                    return Err(format!("tool choice permits only `{name}`"));
                }
                ToolRequirement::Any | ToolRequirement::Named(_) => {}
            }
        }
        if content.is_empty() && self.output_contract_name.is_none() {
            return Err("response envelope carries no completion material".to_string());
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
                    AssistantPart::Thinking { .. } | AssistantPart::RedactedThinking { .. } => {}
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

fn validate_item_identity(item: &ItemIdentity) -> Result<(), DecodeFailure> {
    if item.id.is_empty() || item.item_type.is_empty() {
        return Err(DecodeFailure::new(
            "item lifecycle event carries an empty item identity or type",
        ));
    }
    Ok(())
}

/// Requires a string-carried tool-argument payload to hold one JSON object
/// within the provider nesting bound.
///
/// The argument text arrives inside a JSON string, so the line-level nesting
/// validation in `push` never saw its structure. Failure detail names only
/// the redacted tool name, never the argument text itself.
fn validate_tool_arguments(arguments: &str, tool_name: &str) -> Result<(), String> {
    validate_provider_json_nesting(arguments.as_bytes())
        .map_err(|error| format!("tool `{}` arguments: {error}", redact_text(tool_name)))?;
    let parsed: Value = serde_json::from_str(arguments).map_err(|_| {
        format!(
            "tool `{}` arguments are not valid JSON",
            redact_text(tool_name)
        )
    })?;
    if !parsed.is_object() {
        return Err(format!(
            "tool `{}` arguments are not a JSON object",
            redact_text(tool_name)
        ));
    }
    Ok(())
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
    let input_tokens = u64::try_from(usage.input_tokens)
        .map_err(|_| DecodeFailure::new("usage input_tokens is negative"))?;
    let output_tokens = u64::try_from(usage.output_tokens)
        .map_err(|_| DecodeFailure::new("usage output_tokens is negative"))?;
    Ok(TokenUsage {
        input_tokens: Some(input_tokens),
        output_tokens: Some(output_tokens),
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
        kind: classify_error(message),
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
    provider_failure_classified(exchange, usage, message, message, sink)
}

/// Builds provider-failure evidence whose typed kind is classified from
/// `classification` while the emitted native message is the stateful
/// redaction of `message`. The two coincide except on the exit-status
/// fallback, where the message is already sanitized and the raw stderr is the
/// classifier input.
fn provider_failure_classified<C: Clone>(
    exchange: ExchangeFacts,
    usage: TokenUsage,
    message: &str,
    classification: &str,
    sink: &RedactingSink<'_, C>,
) -> TerminalEvidence {
    TerminalEvidence::ProviderError(ProviderErrorEvidence {
        exchange,
        reported_model: None,
        kind: classify_error(classification),
        native: NativeErrorFacts {
            error_token: Some("codex_cli_error".to_string()),
            error_code: None,
            message: Some(sink.redact_terminal_failure_text(message)),
        },
        usage,
    })
}

fn boundary_loss(exchange: ExchangeFacts, usage: TokenUsage, cause: LossCause) -> TerminalEvidence {
    TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
        cause,
        exchange,
        reported_model: None,
        finish_reported: None,
        usage,
    })
}

pub(crate) struct DecodeFailure {
    detail: String,
}

impl DecodeFailure {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }

    pub(crate) fn into_detail(self) -> String {
        self.detail
    }
}
