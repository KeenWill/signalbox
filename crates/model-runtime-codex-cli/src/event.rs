//! Stateful decoding of one Codex app-server conversation.

use std::collections::HashSet;

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

use crate::app_server::{
    classify::{FailureClass, classify, input_too_large},
    client::{Client, Event},
    decode::{fold_uninterpreted, parse},
    frame::{AgentMessage, TurnError, TurnStatus, UsageBreakdown},
};
use crate::translate::{ToolRequirement, TranslatedOperation};
use crate::wire::{EnvelopeOutcome, ModelEnvelope};

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
    client: Client,
    last_error: Option<TurnError>,
    terminal_message_limit: usize,
    next_part_index: u32,
    usage: TokenUsage,
    terminal: Option<TurnStatus>,
    rejection: Option<(&'static str, crate::app_server::frame::RpcError)>,
}

impl<C: Clone> EventDecoder<C> {
    pub(crate) fn new(
        correlation: C,
        delivery: DeliveryMode,
        translated: &TranslatedOperation,
        client: Client,
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
            client,
            last_error: None,
            terminal_message_limit,
            next_part_index: 0,
            usage: TokenUsage::unreported(),
            terminal: None,
            rejection: None,
        }
    }

    pub(crate) fn push(
        &mut self,
        line: &[u8],
        sink: &mut RedactingSink<'_, C>,
    ) -> Result<(), DecodeFailure> {
        let value = parse(line).map_err(|error| DecodeFailure::stream_protocol(error.0))?;
        let event = self
            .client
            .receive(&value)
            .map_err(|error| DecodeFailure::stream_protocol(error.0))?;
        match event {
            Event::ThreadStarted(id) => {
                fold_uninterpreted(sink, &value, &[&["result", "thread", "id"]]);
                let sanitized = sink.redact_terminal_failure_text(&id);
                self.exchange.provider_request_id = Some(ProviderRequestId::new(sanitized.clone()));
                sink.observe(Observation {
                    correlation: self.correlation.clone(),
                    fact: ObservationFact::ExchangeEstablished(self.exchange.clone()),
                });
                sink.seed_emitted_context(&sanitized);
            }
            Event::AgentMessage {
                message,
                completed: true,
            } => {
                self.fold_retained_agent_message(sink);
                fold_uninterpreted(
                    sink,
                    &value,
                    &[
                        &["method"],
                        &["params", "item", "type"],
                        &["params", "item", "id"],
                        &["params", "item", "text"],
                    ],
                );
                self.retain_message(message)?;
            }
            Event::Delta(delta) => {
                // Deltas are envelope fragments, not user-visible prose. The
                // completed item supplies the authoritative bounded envelope.
                let _ = (&delta.item_id, &delta.delta);
                fold_uninterpreted(sink, &value, &[&["method"]]);
            }
            Event::Usage(event) => {
                fold_uninterpreted(sink, &value, &[&["method"]]);
                self.usage.absorb(usage(event.token_usage.total)?);
            }
            Event::Error(event) => {
                self.fold_retained_agent_message(sink);
                self.fold_retained_error(sink);
                fold_uninterpreted(
                    sink,
                    &value,
                    &[&["method"], &["params", "error", "message"]],
                );
                if event.will_retry {
                    sink.extend_dropped_context(&event.error.message);
                } else {
                    self.last_error = Some(event.error);
                }
            }
            Event::Terminal(turn) => {
                self.fold_retained_error(sink);
                if turn.status == TurnStatus::Completed {
                    if self.agent_message.is_none() {
                        if let Some(item) = turn
                            .items
                            .iter()
                            .rev()
                            .find(|item| item["type"] == "agentMessage")
                        {
                            let message = crate::app_server::decode::decode(item)
                                .map_err(|error| DecodeFailure::new(error.0))?;
                            // The selected summary is sanitized as the final envelope.
                            let mut folded = value.clone();
                            if let Some(items) = folded["params"]["turn"]["items"].as_array_mut()
                                && let Some(selected) = items
                                    .iter_mut()
                                    .rev()
                                    .find(|item| item["type"] == "agentMessage")
                                && let Some(fields) = selected.as_object_mut()
                            {
                                fields.remove("id");
                                fields.remove("text");
                            }
                            fold_uninterpreted(sink, &folded, &[&["method"]]);
                            self.retain_message(message)?;
                        } else {
                            fold_uninterpreted(sink, &value, &[&["method"]]);
                        }
                    } else {
                        fold_uninterpreted(sink, &value, &[&["method"]]);
                    }
                } else {
                    self.fold_retained_agent_message(sink);
                    fold_uninterpreted(
                        sink,
                        &value,
                        &[&["method"], &["params", "turn", "error", "message"]],
                    );
                }
                self.last_error = turn.error;
                self.terminal = Some(turn.status);
            }
            Event::Rejected { method, error } => {
                fold_uninterpreted(sink, &value, &[]);
                self.rejection = Some((method, error));
            }
            Event::Ignored
            | Event::AgentMessage {
                completed: false, ..
            } => fold_uninterpreted(sink, &value, &[]),
        }
        Ok(())
    }

    fn fold_retained_error(&mut self, sink: &mut RedactingSink<'_, C>) {
        if let Some(error) = self.last_error.take() {
            sink.extend_dropped_context(&error.message);
        }
    }

    fn retain_message(&mut self, message: AgentMessage) -> Result<(), DecodeFailure> {
        if message.id.is_empty() || message.text.len() > self.terminal_message_limit {
            return Err(DecodeFailure::stream_protocol(
                "agent message has invalid identity or exceeds the event-size bound",
            ));
        }
        self.message_id = Some(message.id);
        self.agent_message = Some(message.text);
        Ok(())
    }

    pub(crate) fn finish(mut self, sink: &mut RedactingSink<'_, C>) -> TerminalEvidence {
        if let Some((method, error)) = self.rejection.take() {
            if input_too_large(method, &error) {
                return TerminalEvidence::ProviderError(ProviderErrorEvidence {
                    exchange: self.exchange,
                    reported_model: None,
                    kind: ProviderErrorKind::RequestTooLarge,
                    non_acceptance_proven: true,
                    native: NativeErrorFacts {
                        error_token: Some("input_too_large".into()),
                        error_code: Some(error.code.to_string()),
                        message: None,
                    },
                    usage: self.usage,
                });
            }
            return TerminalEvidence::ProvenUnsent(signalbox_model_runtime::ProvenUnsentEvidence {
                cause: signalbox_model_runtime::UnsentCause::SendIncompleteProvenUnacceptable(
                    signalbox_model_runtime::TransportFacts::new(format!(
                        "Codex app-server {method} rejected request with code {}",
                        error.code
                    )),
                ),
            });
        }
        match self.terminal {
            Some(TurnStatus::Completed) => self.completed(sink),
            Some(TurnStatus::Interrupted) => self.boundary_loss(LossCause::TransportFailed(
                signalbox_model_runtime::TransportFacts::new(
                    "Codex app-server turn status is interrupted",
                ),
            )),
            Some(TurnStatus::Failed) => self.failure(sink),
            _ if self.last_error.is_some() => self.failure(sink),
            _ => self.boundary_loss(LossCause::StreamEndedWithoutTerminalMarker {
                interruption: signalbox_model_runtime::StreamInterruption::EndOfStream,
            }),
        }
    }

    fn failure(self, sink: &RedactingSink<'_, C>) -> TerminalEvidence {
        let info = self
            .last_error
            .as_ref()
            .and_then(|error| error.codex_error_info.as_ref());
        match classify(info) {
            FailureClass::PolicyRefusal(reason) => TerminalEvidence::Refused(RefusalEvidence {
                reason,
                exchange: self.exchange,
                message_id: None,
                reported_model: None,
                content: Vec::new(),
                usage: self.usage,
                retained_input_tokens: None,
                retained_output_tokens: None,
            }),
            FailureClass::Provider(kind) => {
                let mut exchange = self.exchange;
                exchange.http_status = info.and_then(|info| info.http_status());
                TerminalEvidence::ProviderError(ProviderErrorEvidence {
                    exchange,
                    reported_model: None,
                    kind,
                    non_acceptance_proven: self.terminal.is_some_and(|status| {
                        self.client.activity.proves_non_acceptance(status, info)
                    }),
                    native: NativeErrorFacts {
                        error_token: info.map(|info| sink.redact_terminal_failure_text(info.tag())),
                        error_code: info
                            .and_then(|info| info.http_status())
                            .map(|status| status.to_string()),
                        message: self
                            .last_error
                            .as_ref()
                            .map(|error| sink.redact_terminal_failure_text(&error.message)),
                    },
                    usage: self.usage,
                })
            }
        }
    }

    pub(crate) fn boundary_loss(self, cause: LossCause) -> TerminalEvidence {
        boundary_loss_before_envelope(self.exchange, self.usage, cause)
    }

    pub(crate) fn boundary_loss_unless_provider_failure(
        self,
        cause: LossCause,
        sink: &mut RedactingSink<'_, C>,
    ) -> TerminalEvidence {
        if self.rejection.is_some()
            || matches!(
                self.terminal,
                Some(TurnStatus::Failed | TurnStatus::Interrupted)
            )
            || self.last_error.is_some()
        {
            self.finish(sink)
        } else {
            self.boundary_loss(cause)
        }
    }

    pub(crate) fn terminal_observed(&self) -> bool {
        self.client.is_terminal()
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
            None => {
                report_response_envelope_rejection("missing");
                return boundary_loss_before_envelope(
                    self.exchange,
                    self.usage,
                    LossCause::ResponseUnintelligible {
                        detail: "turn/completed carried no response envelope".into(),
                    },
                );
            }
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
                reason: signalbox_model_runtime::RefusalReason::Unspecified,
                exchange: self.exchange,
                message_id: message_id.map(ProviderMessageId::new),
                reported_model: None,
                content,
                usage: self.usage,
                retained_input_tokens: None,
                retained_output_tokens: None,
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
        if (content.is_empty() || (envelope.text.is_empty() && envelope.tool_calls.is_empty()))
            && self.output_contract_name.is_none()
        {
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
                    | AssistantPart::ProviderCompaction { .. }
                    | AssistantPart::ProviderReasoning { .. }
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

fn usage(usage: UsageBreakdown) -> Result<TokenUsage, DecodeFailure> {
    optional_usage(usage.reasoning_output_tokens, "reasoningOutputTokens")?;
    optional_usage(usage.total_tokens, "totalTokens")?;
    Ok(TokenUsage {
        input_tokens: optional_usage(usage.input_tokens, "inputTokens")?,
        output_tokens: optional_usage(usage.output_tokens, "outputTokens")?,
        cache_creation_input_tokens: optional_usage(
            usage.cache_write_input_tokens,
            "cacheWriteInputTokens",
        )?,
        cache_read_input_tokens: optional_usage(usage.cached_input_tokens, "cachedInputTokens")?,
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

    fn keeps_stdin_open(&self) -> bool {
        true
    }

    fn take_stdin_frame(&mut self) -> Option<Vec<u8>> {
        self.client.take_stdin_frame()
    }

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
            CliDecodeFailureClass::ProviderDecode => {
                self.boundary_loss(LossCause::ResponseUnintelligible { detail })
            }
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
        classification: &str,
        sink: &mut RedactingSink<'_, C>,
    ) -> TerminalEvidence {
        let _ = (message, classification);
        self.finish(sink)
    }
}
