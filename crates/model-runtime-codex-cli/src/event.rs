//! Stateful decoding of one Codex app-server conversation.

use std::collections::HashSet;

use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CliDecodeFailure, CliDecodeFailureClass, CliProcessLabels,
    CliSession, CompletionEvidence, CompletionFinish, DeliveryMode, ExchangeFacts, FinishReason,
    LossCause, NativeErrorFacts, Observation, ObservationFact, ObservationSink,
    ProviderErrorEvidence, ProviderErrorKind, ProviderMessageId, ProviderRequestId,
    RefusalEvidence, ResponseEnvelopeRejectionStage, TerminalEvidence, TokenUsage, ToolCallId,
    ToolCallProposal, ToolCallsAtLoss, ToolName, provider_json_has_duplicate_members,
    validate_provider_json_nesting,
};

use crate::app_server::{
    classify::{FailureClass, classify, input_too_large},
    client::{Client, Event},
    decode::parse,
    frame::{AgentMessage, TurnError, TurnStatus, UsageBreakdown},
};
use crate::translate::{ToolRequirement, TranslatedOperation};
use crate::wire::{EnvelopeOutcome, EnvelopeToolCall, ModelEnvelope};

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
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> Result<(), DecodeFailure> {
        let value = parse(line).map_err(|error| DecodeFailure::stream_protocol(error.0))?;
        let event = self
            .client
            .receive(&value)
            .map_err(|error| DecodeFailure::stream_protocol(error.0))?;
        match event {
            Event::RateLimitsUpdated => {
                if let Some(snapshot) = self
                    .client
                    .rate_limits
                    .capacity_snapshot(std::time::SystemTime::now())
                {
                    sink.observe_rate_limits(self.correlation.clone(), snapshot);
                }
            }
            Event::ThreadStarted(id) => {
                self.exchange.provider_request_id = Some(ProviderRequestId::new(id));
                sink.observe(Observation {
                    correlation: self.correlation.clone(),
                    fact: ObservationFact::ExchangeEstablished(self.exchange.clone()),
                });
            }
            Event::AgentMessage {
                message,
                completed: true,
            } => self.retain_message(message)?,
            Event::Delta(delta) => {
                // The completed item supplies the authoritative response envelope.
                let _ = (&delta.item_id, &delta.delta);
            }
            Event::Usage(event) => self.usage.absorb(usage(event.token_usage.total)?),
            Event::Error(event) => {
                self.agent_message = None;
                self.last_error = (!event.will_retry).then_some(event.error);
            }
            Event::Terminal(turn) => {
                if turn.status == TurnStatus::Completed
                    && self.agent_message.is_none()
                    && let Some(item) = turn
                        .items
                        .iter()
                        .rev()
                        .find(|item| item["type"] == "agentMessage")
                {
                    let message = crate::app_server::decode::decode(item)
                        .map_err(|error| DecodeFailure::new(error.0))?;
                    self.retain_message(message)?;
                }
                self.last_error = turn.error;
                self.terminal = Some(turn.status);
            }
            Event::Rejected { method, error } => self.rejection = Some((method, error)),
            Event::Ignored
            | Event::AgentMessage {
                completed: false, ..
            } => {}
        }
        Ok(())
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

    pub(crate) fn finish(mut self, sink: &mut (dyn ObservationSink<C> + Send)) -> TerminalEvidence {
        if let Some((method, error)) = self.rejection.take() {
            if input_too_large(method, &error) {
                return TerminalEvidence::ProviderError(ProviderErrorEvidence {
                    credential_recovery: None,
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
            Some(TurnStatus::Failed) => self.failure(),
            _ if self.last_error.is_some() => self.failure(),
            _ => self.boundary_loss(LossCause::StreamEndedWithoutTerminalMarker {
                interruption: signalbox_model_runtime::StreamInterruption::EndOfStream,
            }),
        }
    }

    fn failure(self) -> TerminalEvidence {
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
                if matches!(
                    kind,
                    ProviderErrorKind::RateLimited | ProviderErrorKind::QuotaExhausted
                ) {
                    exchange.retry_after = self
                        .client
                        .rate_limits
                        .retry_after(std::time::SystemTime::now());
                }
                exchange.http_status = info.and_then(|info| info.http_status());
                TerminalEvidence::ProviderError(ProviderErrorEvidence {
                    credential_recovery: None,
                    exchange,
                    reported_model: None,
                    kind,
                    non_acceptance_proven: self.terminal.is_some_and(|status| {
                        self.client.activity.proves_non_acceptance(status, info)
                    }),
                    native: NativeErrorFacts {
                        error_token: info.map(|info| info.tag().to_string()),
                        error_code: info
                            .and_then(|info| info.http_status())
                            .map(|status| status.to_string()),
                        message: self.last_error.as_ref().map(|error| error.message.clone()),
                    },
                    usage: self.usage,
                })
            }
        }
    }

    pub(crate) fn boundary_loss(self, cause: LossCause) -> TerminalEvidence {
        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            response_content_observed: self.client.activity.response_content_observed,
            cause,
            exchange: self.exchange,
            reported_model: None,
            finish_reported: None,
            tool_calls: ToolCallsAtLoss::Unobserved,
            usage: self.usage,
        })
    }

    pub(crate) fn boundary_loss_unless_provider_failure(
        self,
        cause: LossCause,
        sink: &mut (dyn ObservationSink<C> + Send),
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

    fn completed(mut self, sink: &mut (dyn ObservationSink<C> + Send)) -> TerminalEvidence {
        let agent_message = match self.agent_message.take() {
            Some(agent_message) => agent_message,
            None => {
                return boundary_loss_before_envelope(
                    self.exchange,
                    self.usage,
                    LossCause::ResponseEnvelopeRejected {
                        stage: ResponseEnvelopeRejectionStage::Missing,
                        detail: "turn/completed carried no response envelope".into(),
                    },
                );
            }
        };
        if let Err(error) = validate_provider_json_nesting(agent_message.as_bytes()) {
            return boundary_loss_before_envelope(
                self.exchange,
                self.usage,
                LossCause::ResponseEnvelopeRejected {
                    stage: ResponseEnvelopeRejectionStage::NestingBound,
                    detail: format!("last agent message exceeds JSON nesting bounds: {error}"),
                },
            );
        }
        if let Err(error) = reject_duplicate_json_members(&agent_message) {
            return boundary_loss_before_envelope(
                self.exchange,
                self.usage,
                LossCause::ResponseEnvelopeRejected {
                    stage: ResponseEnvelopeRejectionStage::DuplicateMembers,
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
                return boundary_loss_before_envelope(
                    self.exchange,
                    self.usage,
                    LossCause::ResponseEnvelopeRejected {
                        stage: ResponseEnvelopeRejectionStage::Shape,
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
        let content = match self.decode_content(&envelope) {
            Ok(content) => content,
            Err(failure) => {
                return boundary_loss_after_envelope(
                    self.exchange,
                    self.usage,
                    Some(reported_finish.clone()),
                    &envelope,
                    LossCause::ResponseEnvelopeRejected {
                        stage: failure.stage,
                        detail: failure.detail,
                    },
                );
            }
        };
        let message_id = self.message_id.take();
        if let Err(detail) = self.emit_completion_observations(
            sink,
            &content,
            &envelope.text,
            reported_finish.clone(),
        ) {
            return boundary_loss_after_envelope(
                self.exchange,
                self.usage,
                Some(reported_finish.clone()),
                &envelope,
                LossCause::ResponseEnvelopeRejected {
                    stage: ResponseEnvelopeRejectionStage::ObservationProjection,
                    detail,
                },
            );
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
    ) -> Result<Vec<AssistantPart>, ResponseEnvelopeFailure> {
        if envelope.outcome == EnvelopeOutcome::Refused && !envelope.tool_calls.is_empty() {
            return Err(ResponseEnvelopeFailure::new(
                ResponseEnvelopeRejectionStage::RefusalWithTools,
                "a refusal envelope also proposed tools",
            ));
        }
        let mut content = Vec::new();
        let text = envelope.text.clone();
        if !text.is_empty() {
            content.push(AssistantPart::Text(text));
        }
        if envelope.outcome == EnvelopeOutcome::Refused {
            return Ok(content);
        }

        let mut raw_ids = HashSet::new();
        for call in &envelope.tool_calls {
            if call.id.is_empty() || !raw_ids.insert(call.id.as_str()) {
                return Err(ResponseEnvelopeFailure::new(
                    ResponseEnvelopeRejectionStage::ToolCallId,
                    "tool call ids must be nonempty and distinct",
                ));
            }
        }
        for call in &envelope.tool_calls {
            let allowed = self.declared_tools.contains(&call.name)
                || self.output_contract_name.as_deref() == Some(call.name.as_str());
            if !allowed {
                return Err(ResponseEnvelopeFailure::new(
                    ResponseEnvelopeRejectionStage::UndeclaredTool,
                    format!("response proposed undeclared tool `{}`", call.name),
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
            validate_tool_argument_nesting(call)?;
            content.push(AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new(call.id.clone()),
                name: ToolName::new(call.name.clone()),
                arguments_json: call.arguments.clone(),
            }));
        }
        if let Some(contract_name) = &self.output_contract_name {
            if !envelope
                .tool_calls
                .iter()
                .all(|call| &call.name == contract_name)
            {
                return Err(ResponseEnvelopeFailure::new(
                    ResponseEnvelopeRejectionStage::StructuredOutputTool,
                    format!("structured output permits only `{contract_name}` proposals"),
                ));
            }
        } else {
            match &self.tool_requirement {
                ToolRequirement::Optional => {}
                ToolRequirement::Any if envelope.tool_calls.is_empty() => {
                    return Err(ResponseEnvelopeFailure::new(
                        ResponseEnvelopeRejectionStage::RequiredToolMissing,
                        "tool choice requires a proposal",
                    ));
                }
                ToolRequirement::Named(name)
                    if envelope.tool_calls.is_empty()
                        || !envelope.tool_calls.iter().all(|call| &call.name == name) =>
                {
                    return Err(ResponseEnvelopeFailure::new(
                        ResponseEnvelopeRejectionStage::NamedToolMismatch,
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
                ResponseEnvelopeRejectionStage::CompletionEmpty,
                "response envelope carries no completion material",
            ));
        }
        Ok(content)
    }

    fn emit_completion_observations(
        &mut self,
        sink: &mut (dyn ObservationSink<C> + Send),
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
    stage: ResponseEnvelopeRejectionStage,
    detail: String,
}

impl ResponseEnvelopeFailure {
    fn new(stage: ResponseEnvelopeRejectionStage, detail: impl Into<String>) -> Self {
        Self {
            stage,
            detail: detail.into(),
        }
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
/// detail names only the tool name, never the argument text itself.
fn validate_tool_argument_nesting(call: &EnvelopeToolCall) -> Result<(), ResponseEnvelopeFailure> {
    validate_provider_json_nesting(call.arguments.as_bytes()).map_err(|error| {
        ResponseEnvelopeFailure::new(
            ResponseEnvelopeRejectionStage::ToolArgumentsNesting,
            format!("tool `{}` arguments: {error}", call.name),
        )
    })
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
        response_content_observed: false,
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

    fn terminal_observed(&self) -> bool {
        EventDecoder::terminal_observed(self)
    }

    fn push(
        &mut self,
        line: &[u8],
        sink: &mut (dyn ObservationSink<C> + Send),
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

    fn finish(self, sink: &mut (dyn ObservationSink<C> + Send)) -> TerminalEvidence {
        EventDecoder::finish(self, sink)
    }

    fn boundary_loss(self, cause: LossCause) -> TerminalEvidence {
        EventDecoder::boundary_loss(self, cause)
    }

    fn boundary_loss_unless_provider_failure(
        self,
        cause: LossCause,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> TerminalEvidence {
        EventDecoder::boundary_loss_unless_provider_failure(self, cause, sink)
    }

    fn provider_error_after_exit(
        self,
        message: &str,
        classification: &str,
        sink: &mut (dyn ObservationSink<C> + Send),
    ) -> TerminalEvidence {
        let _ = (message, classification);
        self.finish(sink)
    }
}

#[cfg(test)]
mod passthrough_tests {
    use super::*;
    use crate::app_server::frame::{ThreadOptions, TurnInput, UserInput};
    use serde_json::json;

    #[test]
    fn repository_review_output_passes_through_ambient_cli() {
        let prior_user_content = r###"Target pull request: KeenWill/signalbox#1860 ("Remove retired repository watch checkouts"), branch agent/repo-watch-dispatch-checkout-retire, current head e906aa33e. Work only on that branch in a detached worktree of that head as your tools allow: fix every actionable unresolved review thread below with the smallest correct change (add a regression test when the thread names a wrong result in code), validate with `cargo fmt --all -- --check`, `cargo check --workspace --all-targets --all-features` and `cargo test -p <touched crate> --all-features` for code, or `python3 scripts/check_docs_consistency.py` for docs, commit with a plain subject, push to the same branch, reply on each thread naming the commit, and resolve it. Do not rebase or force-push, and do not touch any other branch. Unresolved threads at this head:

- PRRT_kwDOTWhy-86gCyhi, apps/signalboxd/src/repo_watch_checkout.rs:153 — **  Make published checkouts recoverable before marker creation**  When shutdown cancels provisioning after this publication rename—for example, while credential lookup or `git clone` is awaiting—the ledger already records creation ownership and identity, but `.git/signalbox-dispatch` is not written until later. If the session becomes terminal before replay, `marker_matches` returns false and `remove` reports success, so scavenging sets `checkout_removed = true` while permanently leaving the checkout on disk. Establish durable ownership evidence before publication, or keep this markerless interrupted state eligible for safe cleanup.  AGENTS.md reference: [AGENTS.md:L38-L41](https://github.com/KeenWill/signalbox/blob/6b165156d516d0ee6f905848b61c53f53a9e082b/AGENTS.md#L38-L41)  Useful? React with 👍 / 👎.

Finish with one short summary of the commit and the thread ids.
"###;
        let text = "I'll inspect the thread: Make published checkouts recoverable before marker creation. Then I will read the checkout cleanup code.";
        let calls = vec![
            ToolCallProposal {
                id: ToolCallId::new("call-summary"),
                name: ToolName::new("change_request_summary"),
                arguments_json: r#"{ "number": 1860 }"#.into(),
            },
            ToolCallProposal {
                id: ToolCallId::new("call-status"),
                name: ToolName::new("git_status"),
                arguments_json: "{}".into(),
            },
            ToolCallProposal {
                id: ToolCallId::new("call-read"),
                name: ToolName::new("read_file"),
                arguments_json: r#"{"path":"apps/signalboxd/src/repo_watch_checkout.rs"}"#.into(),
            },
        ];
        for delivery in [DeliveryMode::Buffered, DeliveryMode::Streamed] {
            let translated = TranslatedOperation {
                images: Vec::new(),
                prompt: prior_user_content.as_bytes().to_vec(),
                declared_tools: calls
                    .iter()
                    .map(|call| call.name.as_str().to_string())
                    .collect(),
                output_contract_name: None,
                tool_requirement: ToolRequirement::Optional,
            };
            let client = Client::new(
                ThreadOptions {
                    model: "gpt-5.6-sol".into(),
                    cwd: "/workspace".into(),
                    service_tier: None,
                },
                TurnInput {
                    input: vec![UserInput::Text {
                        text: prior_user_content.into(),
                    }],
                    output_schema: json!({"type":"object"}),
                    effort: None,
                },
            );
            let mut decoder = EventDecoder::new((), delivery, &translated, client, 1024 * 1024);
            let mut observed = Vec::new();
            let sink = &mut observed;
            for response in [
                json!({"id":1,"result":{"userAgent":"codex","codexHome":"/workspace/.codex"}}),
                json!({"id":4,"result":{"rateLimits":{}}}),
                json!({"id":2,"result":{"thread":{"id":"thread-fixture"}}}),
                json!({"id":3,"result":{"turn":{"id":"turn-fixture","status":"inProgress","items":[]}}}),
                json!({"method":"item/started","params":{"threadId":"thread-fixture","turnId":"turn-fixture","item":{"id":"user-fixture","type":"userMessage","content":[{"type":"text","text":prior_user_content}]}}}),
                json!({"method":"item/completed","params":{"threadId":"thread-fixture","turnId":"turn-fixture","item":{"id":"message-fixture","type":"agentMessage","text":json!({"outcome":"completed","text":text,"tool_calls":calls.iter().map(|call| json!({"id":call.id.as_str(),"name":call.name.as_str(),"arguments":call.arguments_json})).collect::<Vec<_>>()}).to_string()}}}),
                json!({"method":"turn/completed","params":{"threadId":"thread-fixture","turn":{"id":"turn-fixture","status":"completed","items":[],"error":null}}}),
            ] {
                decoder
                    .push(&serde_json::to_vec(&response).unwrap(), sink)
                    .unwrap_or_else(|error| panic!("{}", error.into_detail()));
            }
            let evidence = decoder.finish(sink);
            let TerminalEvidence::Completed(completion) = evidence else {
                panic!("{evidence:?}")
            };
            let mut expected = vec![AssistantPart::Text(text.into())];
            expected.extend(calls.iter().cloned().map(AssistantPart::ToolCall));
            assert_eq!(completion.content, expected);
            let proposals = observed
                .iter()
                .filter_map(|observation| match &observation.fact {
                    ObservationFact::ToolCallProposed(call) => Some(call.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(proposals, calls);
        }
    }
}
