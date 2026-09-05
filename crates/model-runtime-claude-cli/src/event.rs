//! Stateful decoding of one Claude Code streamed-JSON event stream.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CliDecodeFailure, CliDecodeFailureClass, CliProcessLabels,
    CliSession, CliTerminalTextCapture, CompletionEvidence, CompletionFinish, DeliveryMode,
    DiscardedField, ExchangeFacts, FinishReason, LossCause, NativeErrorFacts, Observation,
    ObservationFact, ObservationSink, ProviderErrorEvidence, ProviderErrorKind, ProviderMessageId,
    ProviderReportedModel, ProviderRequestId, REDACTED, RedactingSink, RefusalEvidence,
    TerminalEvidence, TerminalTextCapture, TokenUsage, ToolArgumentRedaction, ToolCallId,
    ToolCallProposal, ToolCallsAtLoss, ToolName, provider_json_has_duplicate_members, redact_json,
    redact_text, validate_provider_json_nesting,
};

use crate::SUPPORTED_CLAUDE_CLI_VERSION;
use crate::bridge::{SERVER_NAME, TOOL_ACKNOWLEDGEMENT, TOOL_PREFIX};
use crate::status::classify_status;
use crate::translate::{ToolRequirement, TranslatedOperation};
use crate::wire::{
    AssistantContent, AssistantEvent, AssistantRawEvent, RawToolUse, ResultEvent, SystemInit,
    UserEvent,
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
    allowed_tools: HashSet<String>,
    tool_requirement: ToolRequirement,
    exchange: ExchangeFacts,
    reported_model: Option<ProviderReportedModel>,
    native_session_id: Option<String>,
    native_assistant_model: Option<String>,
    message_id: Option<ProviderMessageId>,
    native_message_id: Option<String>,
    content: Vec<AssistantPart>,
    /// Sticky: the CLI announced at least one `tool_use` block. Set before the
    /// block is validated, so a call this adapter rejects — an empty or
    /// duplicate id, a name outside the private MCP namespace, an undeclared
    /// tool — is still recorded as opened. `proposal_indexes` cannot answer
    /// for those: the rejection returns before the insert.
    opened_tool_calls: bool,
    current_event_examined: bool,
    undelivered_line: bool,
    proposal_indexes: HashMap<String, usize>,
    result_ids: HashSet<String>,
    emitted_tool_ids: HashSet<String>,
    redacted_tool_id_cursor: usize,
    next_part_index: u32,
    usage: TokenUsage,
    usage_reported: bool,
    finish_reported: Option<FinishReason>,
    initialized: bool,
    terminal: Option<CliTerminal>,
}

enum CliTerminal {
    Success {
        stop_reason: String,
        retained_stop_reason: String,
    },
    Error {
        subtype: String,
        status: Option<u16>,
        message: String,
    },
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
            allowed_tools: translated
                .catalog
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
            tool_requirement: translated.tool_requirement.clone(),
            exchange: ExchangeFacts::default(),
            reported_model: None,
            native_session_id: None,
            native_assistant_model: None,
            message_id: None,
            native_message_id: None,
            content: Vec::new(),
            opened_tool_calls: false,
            current_event_examined: false,
            undelivered_line: false,
            proposal_indexes: HashMap::new(),
            result_ids: HashSet::new(),
            emitted_tool_ids: HashSet::new(),
            redacted_tool_id_cursor: 1,
            next_part_index: 0,
            usage: TokenUsage::unreported(),
            usage_reported: false,
            finish_reported: None,
            initialized: false,
            terminal: None,
        }
    }

    /// Emits the reported usage exactly once, at the first terminal-path point
    /// that reaches it.
    ///
    /// Usage is a boundary-progress fact the provider states in its `result`
    /// event, so it is emitted when that event is processed — before the
    /// `FinishReported` fact drawn from the same event, matching the observation
    /// order the substrate specification enumerates and the Codex adapter
    /// follows. The later terminal paths keep calling this so a stream that
    /// ended without a `result` still reports its (unreported) usage exactly as
    /// before, and so the nonzero-exit path cannot drop the fact entirely.
    fn report_usage(&mut self, sink: &mut RedactingSink<'_, C>) {
        if self.usage_reported {
            return;
        }
        self.usage_reported = true;
        sink.observe(Observation {
            correlation: self.correlation.clone(),
            fact: ObservationFact::UsageReported(self.usage),
        });
    }

    pub(crate) fn push(
        &mut self,
        line: &[u8],
        sink: &mut RedactingSink<'_, C>,
    ) -> Result<(), DecodeFailure> {
        // Each line is examined on its own merits; nothing carries over except
        // `opened_tool_calls`, which is sticky by design.
        self.current_event_examined = false;
        if self.terminal.is_some() {
            return Err(DecodeFailure::stream_protocol(
                "Claude emitted an event after its terminal result",
            ));
        }
        let text = std::str::from_utf8(line).map_err(|error| {
            DecodeFailure::stream_protocol(format!("event is not UTF-8: {error}"))
        })?;
        validate_provider_json_nesting(line)
            .map_err(|error| DecodeFailure::stream_protocol(error.to_string()))?;
        reject_duplicate_json_members(text)?;
        let value: Value = serde_json::from_str(text).map_err(|error| {
            DecodeFailure::stream_protocol(format!("event is not JSON: {error}"))
        })?;
        let event_type = value.get("type").and_then(Value::as_str).ok_or_else(|| {
            DecodeFailure::stream_protocol("event has no string `type` discriminator")
        })?;
        if event_type != "assistant" {
            // Only an `assistant` event carries content blocks, so for every
            // other type the discriminator alone settles the tool question: no
            // tool call can have opened in material of this shape.
            self.current_event_examined = true;
        }
        match event_type {
            "system" => self.system(value, sink),
            "assistant" => self.assistant(text, sink),
            "user" => self.user(value),
            "result" => self.result(value, sink),
            "rate_limit_event" | "stream_event" => {
                sink.extend_dropped_context(text);
                Ok(())
            }
            _ => Err(DecodeFailure::stream_protocol(format!(
                "unrecognized Claude event type `{event_type}`"
            ))),
        }
    }

    fn system(
        &mut self,
        value: Value,
        sink: &mut RedactingSink<'_, C>,
    ) -> Result<(), DecodeFailure> {
        let subtype = value.get("subtype").and_then(Value::as_str);
        if matches!(
            subtype,
            Some(
                "status"
                    | "hook_started"
                    | "hook_progress"
                    | "hook_response"
                    | "api_retry"
                    | "thinking_tokens"
            )
        ) {
            let mut event = value;
            if let Value::Object(members) = &mut event {
                // Closed protocol metadata describes the envelope, not
                // discarded provider content. In particular,
                // `thinking_tokens` must not create a credential-shaped
                // lookbehind merely because its name contains `token`, and
                // repeated copies of the already-retained session identity
                // must not accumulate as dropped content.
                members.remove("type");
                members.remove("subtype");
                // That last exemption is a claim about the value, so it is
                // checked rather than assumed. `session_id` is dropped only
                // where it is provably the identity `system/init` retained; a
                // differing value contradicts that correlation exactly as it
                // does on a `result` event, and a value arriving before any
                // init is not a repeated identity at all. Anything this
                // predicate cannot vouch for stays provider-controlled content
                // and seeds the dropped lookbehind (INV-035), because a
                // credential prefix discarded here would let its continuation
                // escape the shape redactor in a later field.
                match (
                    members.get("session_id").and_then(Value::as_str),
                    self.native_session_id.as_deref(),
                ) {
                    (Some(session), Some(native)) if session == native => {
                        members.remove("session_id");
                    }
                    (Some(_), Some(_)) => {
                        return Err(DecodeFailure::stream_protocol(
                            "Claude lifecycle session contradicts system init",
                        ));
                    }
                    _ => {}
                }
                members.retain(|_, value| !value.is_null());
                if members.is_empty() {
                    return Ok(());
                }
            }
            let mut context = String::new();
            let members = event.as_object().ok_or_else(|| {
                DecodeFailure::stream_protocol("Claude system event is not an object")
            })?;
            for (name, value) in members {
                if !context.is_empty() {
                    context.push('\n');
                }
                context.push_str(name);
                context.push(':');
                if let Some(value) = value.as_str() {
                    context.push_str(value);
                } else {
                    let value = serde_json::to_string(value).map_err(|error| {
                        DecodeFailure::stream_protocol(format!(
                            "Claude lifecycle event could not be retained for redaction: {error}"
                        ))
                    })?;
                    context.push_str(&value);
                }
            }
            sink.extend_dropped_context(&context);
            return Ok(());
        }
        if subtype != Some("init") || self.initialized {
            let subtype = subtype.unwrap_or("<missing>");
            return Err(DecodeFailure::stream_protocol(format!(
                "unexpected or duplicate Claude system event subtype `{subtype}`"
            )));
        }
        let event: SystemInit = decode(value)?;
        if event.session_id.is_empty() || event.model.is_empty() {
            return Err(DecodeFailure::stream_protocol(
                "Claude init carries an empty session or model id",
            ));
        }
        let observed_version =
            semver::Version::parse(&event.claude_code_version).map_err(|_| {
                DecodeFailure::stream_protocol(format!(
                    "Claude Code version `{}` is not SemVer",
                    event.claude_code_version
                ))
            })?;
        let supported_version =
            semver::Version::parse(SUPPORTED_CLAUDE_CLI_VERSION).map_err(|_| {
                DecodeFailure::stream_protocol("the pinned Claude Code version is not SemVer")
            })?;
        if observed_version != supported_version {
            return Err(DecodeFailure::stream_protocol(format!(
                "Claude Code version `{}` does not match pinned `{SUPPORTED_CLAUDE_CLI_VERSION}`",
                event.claude_code_version
            )));
        }
        let expected = self
            .allowed_tools
            .iter()
            .map(|name| format!("{TOOL_PREFIX}{name}"))
            .collect::<HashSet<_>>();
        let actual = event.tools.into_iter().collect::<HashSet<_>>();
        if actual != expected {
            return Err(DecodeFailure::stream_protocol(
                "Claude init tool inventory differs from the declared MCP surface",
            ));
        }
        if event.mcp_servers.len() != 1
            || event.mcp_servers[0].name != SERVER_NAME
            || event.mcp_servers[0].status != "connected"
        {
            return Err(DecodeFailure::stream_protocol(
                "Claude init did not report the private MCP server connected",
            ));
        }
        if !event.slash_commands.is_empty() || !event.skills.is_empty() || !event.plugins.is_empty()
        {
            return Err(DecodeFailure::stream_protocol(
                "Claude init exposed an ambient instruction or plugin surface",
            ));
        }
        let request_id = sink.redact_provider_id("", &event.session_id);
        let model = sink.redact_provider_id(&request_id, &event.model);
        self.native_session_id = Some(event.session_id);
        self.exchange.provider_request_id = Some(ProviderRequestId::new(request_id.clone()));
        self.reported_model = Some(ProviderReportedModel::new(model.clone()));
        self.initialized = true;
        sink.observe(Observation {
            correlation: self.correlation.clone(),
            fact: ObservationFact::ExchangeEstablished(self.exchange.clone()),
        });
        sink.observe(Observation {
            correlation: self.correlation.clone(),
            fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new(model.clone())),
        });
        sink.seed_emitted_context(&model);
        Ok(())
    }

    fn assistant(
        &mut self,
        text: &str,
        sink: &mut RedactingSink<'_, C>,
    ) -> Result<(), DecodeFailure> {
        self.require_initialized()?;
        let event: AssistantEvent = decode_text(text)?;
        let raw_event: AssistantRawEvent = decode_text(text)?;
        // Read the tool fact off the decoded content before any check below can
        // reject the event. The identity, nesting, message-id, and model checks
        // all return before `assistant_block` runs, so a flag set during the
        // block walk would miss a call this event plainly announced.
        self.current_event_examined = true;
        if event
            .message
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolUse { .. }))
        {
            self.opened_tool_calls = true;
        }
        if event.parent_tool_use_id.is_some()
            || event.message.role != "assistant"
            || event.message.id.is_empty()
            || event.message.model.is_empty()
        {
            return Err(DecodeFailure::stream_protocol(
                "Claude assistant event has invalid identity or nesting",
            ));
        }
        if self
            .native_message_id
            .as_ref()
            .is_some_and(|message_id| message_id != &event.message.id)
        {
            return Err(DecodeFailure::stream_protocol(
                "Claude assistant message id contradicts prior assistant content",
            ));
        }
        if self.native_message_id.is_none() {
            // The id leaves the adapter inside `CompletionEvidence.message_id`,
            // where this message's own text sits beside it. Sanitizing alone is
            // not enough: an id ending in a credential marker prefix (`api_`)
            // beside a first text block opening `key=value` reconstructs the
            // credential across the two retained fields. Register the id so the
            // continuation is suppressed, without discarding the model chain the
            // system-init event may still have live.
            let sanitized = sink.redact_provider_id("", &event.message.id);
            sink.add_emitted_identifier(&sanitized);
            self.message_id = Some(ProviderMessageId::new(sanitized));
            self.native_message_id = Some(event.message.id.clone());
        }
        if self
            .native_assistant_model
            .as_ref()
            .is_some_and(|model| model != &event.message.model)
        {
            return Err(DecodeFailure::stream_protocol(
                "Claude assistant model contradicts prior assistant content",
            ));
        }
        // The provider-resolved model is accepted and then discarded: it is
        // retained only to detect a later contradiction and leaves the adapter
        // in no record, so an ambient delivery has no exact value to redact
        // downstream. A marker prefix ending it (`api_`) beside a text block
        // opening `key=value` still reconstructs the credential, so register it
        // as a lookbehind chain of its own — the emitted chain belongs to the
        // message id above, and the dropped chain to provider content this
        // field does not sit in.
        //
        // Every assistant envelope repeats and discards this same field, and
        // each repetition sits beside that envelope's own content blocks, so
        // registration happens on every event rather than only the first:
        // content that spends the lookbehind in one event would otherwise leave
        // the next event's text unguarded. The check above has already proven a
        // repeat equal to the stored value.
        let repetition = if self.native_assistant_model.is_none() {
            self.native_assistant_model = Some(event.message.model.clone());
            DiscardedField::New
        } else {
            DiscardedField::Repeated
        };
        sink.add_discarded_field_identifier(&event.message.model, repetition);
        if let Some(usage) = event.message.usage {
            self.usage.absorb(message_usage(usage));
        }
        if event.message.content.len() != raw_event.message.content.len() {
            return Err(DecodeFailure::stream_protocol(
                "Claude assistant content views have different lengths",
            ));
        }
        for (block, raw_block) in event
            .message
            .content
            .into_iter()
            .zip(raw_event.message.content)
        {
            self.assistant_block(block, raw_block.get(), sink)?;
        }
        Ok(())
    }

    fn assistant_block(
        &mut self,
        block: AssistantContent,
        raw_block: &str,
        sink: &mut RedactingSink<'_, C>,
    ) -> Result<(), DecodeFailure> {
        match block {
            AssistantContent::Text { text } => {
                if self.result_ids.is_empty() {
                    let index = self.take_part_index()?;
                    sink.observe(Observation {
                        correlation: self.correlation.clone(),
                        fact: ObservationFact::TextDelta {
                            index,
                            text: text.clone(),
                        },
                    });
                    self.content.push(AssistantPart::Text(text));
                } else {
                    sink.extend_dropped_context(&text);
                }
            }
            AssistantContent::Thinking {
                thinking,
                signature,
            } => {
                let index = self.take_part_index()?;
                sink.observe(Observation {
                    correlation: self.correlation.clone(),
                    fact: ObservationFact::ThinkingDelta {
                        index,
                        text: thinking.clone(),
                    },
                });
                self.content.push(AssistantPart::Thinking {
                    text: thinking,
                    signature: signature.map(|value| sink.redact_retained_metadata(&value)),
                });
            }
            AssistantContent::RedactedThinking { data } => {
                self.take_part_index()?;
                self.content.push(AssistantPart::RedactedThinking {
                    data: sink.redact_retained_metadata(&data),
                });
            }
            AssistantContent::ToolUse { id, name, input } => {
                self.opened_tool_calls = true;
                if id.is_empty() || self.proposal_indexes.contains_key(&id) {
                    return Err(DecodeFailure::stream_protocol(
                        "Claude tool_use carries an empty or duplicate id",
                    ));
                }
                let Some(name) = name.strip_prefix(TOOL_PREFIX) else {
                    return Err(DecodeFailure::stream_protocol(
                        "Claude proposed a tool outside the private MCP namespace",
                    ));
                };
                let raw: RawToolUse = decode_text(raw_block)?;
                let raw_arguments = raw.input.get();
                if !self.allowed_tools.contains(name) || !input.is_object() {
                    return Err(DecodeFailure::stream_protocol(
                        "Claude proposed an undeclared tool or non-object arguments",
                    ));
                }
                let index = self.take_part_index()?;
                let arguments = sink.redact_tool_arguments("", raw_arguments);
                self.proposal_indexes.insert(id.clone(), self.content.len());
                match arguments {
                    ToolArgumentRedaction::Admitted(arguments) => {
                        // The proposal id leaves the adapter in `ToolCallProposed`
                        // and in the retained assistant content, so later text sits
                        // beside it for the same reason the message id does: an id
                        // ending `api_` next to a following text block opening
                        // `key=value` reconstructs the credential across the two
                        // emitted fields.
                        let sanitized_id = sink.redact_provider_id("", &id);
                        sink.add_emitted_identifier(&sanitized_id);
                        let proposal_id = self.unique_tool_id(&id, sanitized_id);
                        let proposal = ToolCallProposal {
                            id: ToolCallId::new(proposal_id),
                            name: ToolName::new(name),
                            arguments_json: arguments.clone(),
                        };
                        self.content.push(AssistantPart::ToolCall(proposal.clone()));
                        if self.delivery == DeliveryMode::Streamed {
                            sink.observe(Observation {
                                correlation: self.correlation.clone(),
                                fact: ObservationFact::ToolArgumentsDelta {
                                    index,
                                    fragment: arguments,
                                },
                            });
                        }
                        sink.observe(Observation {
                            correlation: self.correlation.clone(),
                            fact: ObservationFact::ToolCallProposed(proposal),
                        });
                    }
                    ToolArgumentRedaction::Suppressed => {
                        self.content
                            .push(AssistantPart::SuppressedToolCall(ToolName::new(name)));
                    }
                }
            }
            AssistantContent::Other => {
                return Err(DecodeFailure::stream_protocol(
                    "Claude assistant event contains an unsupported content block",
                ));
            }
        }
        Ok(())
    }

    fn user(&mut self, value: Value) -> Result<(), DecodeFailure> {
        self.require_initialized()?;
        let event: UserEvent = decode(value)?;
        if event.message.role != "user" || event.message.content.is_empty() {
            return Err(DecodeFailure::stream_protocol(
                "Claude user event is not a tool result",
            ));
        }
        for result in event.message.content {
            if result.content_type != "tool_result"
                || !self.proposal_indexes.contains_key(&result.tool_use_id)
                || !self.result_ids.insert(result.tool_use_id)
                || tool_result_text(&result.content) != Some(TOOL_ACKNOWLEDGEMENT)
            {
                return Err(DecodeFailure::stream_protocol(
                    "Claude tool_result does not acknowledge exactly one declared proposal",
                ));
            }
        }
        Ok(())
    }

    fn result(
        &mut self,
        value: Value,
        sink: &mut RedactingSink<'_, C>,
    ) -> Result<(), DecodeFailure> {
        self.require_initialized()?;
        let event: ResultEvent = decode(value)?;
        if self.native_session_id.as_deref() != Some(event.session_id.as_str()) {
            return Err(DecodeFailure::stream_protocol(
                "Claude result session contradicts system init",
            ));
        }
        if let Some(usage) = event.usage {
            self.usage.absorb(result_usage(usage));
        }
        // The provider stated usage in this event; emit it here so consumers see
        // it in provider-fact order ahead of the `FinishReported` fact this same
        // event produces, and so a nonzero process exit after this point cannot
        // strand the progress fact.
        self.report_usage(sink);
        if event.subtype == "success" && !event.is_error {
            if !event.errors.is_empty() || event.api_error_status.is_some() {
                return Err(DecodeFailure::stream_protocol(
                    "Claude success carries contradictory error facts",
                ));
            }
            let stop_reason = event.stop_reason.ok_or_else(|| {
                DecodeFailure::stream_protocol("Claude success lacks a stop reason")
            })?;
            if event.terminal_reason.as_deref() != Some("completed") {
                return Err(DecodeFailure::stream_protocol(
                    "Claude success lacks the completed terminal reason",
                ));
            }
            let retained_stop_reason = match finish_reason(&stop_reason) {
                FinishReason::Unrecognized { .. } => sink.redact_retained_metadata(&stop_reason),
                _ => stop_reason.clone(),
            };
            let finish = finish_reason_with_token(&stop_reason, &retained_stop_reason);
            sink.finish();
            self.finish_reported = Some(finish.clone());
            sink.observe(Observation {
                correlation: self.correlation.clone(),
                fact: ObservationFact::FinishReported(finish),
            });
            self.terminal = Some(CliTerminal::Success {
                stop_reason,
                retained_stop_reason,
            });
        } else if event.is_error {
            let message = event.errors.join("; ");
            let message = if message.is_empty() {
                event
                    .result
                    .unwrap_or_else(|| "Claude reported an error".to_string())
            } else {
                message
            };
            self.exchange.http_status = event.api_error_status;
            self.terminal = Some(CliTerminal::Error {
                subtype: event.subtype,
                status: event.api_error_status,
                message,
            });
        } else {
            return Err(DecodeFailure::stream_protocol(
                "Claude result carries contradictory success fields",
            ));
        }
        Ok(())
    }

    pub(crate) fn finish(mut self, sink: &mut RedactingSink<'_, C>) -> TerminalEvidence {
        let Some(terminal) = self.terminal.take() else {
            return self.loss(LossCause::StreamEndedWithoutTerminalMarker {
                interruption: signalbox_model_runtime::StreamInterruption::EndOfStream,
            });
        };
        let stop_reason = match terminal {
            CliTerminal::Error {
                subtype,
                status,
                message,
            } => {
                let kind = classify_status(status);
                let subtype = sink.redact_retained_metadata(&subtype);
                let message = sink.redact_retained_metadata(&message);
                self.report_usage(sink);
                return TerminalEvidence::ProviderError(ProviderErrorEvidence {
                    exchange: self.exchange,
                    reported_model: self.reported_model,
                    kind,
                    non_acceptance_proven: false,
                    native: NativeErrorFacts {
                        error_token: Some(subtype),
                        error_code: status.map(|value| value.to_string()),
                        message: Some(message),
                    },
                    usage: self.usage,
                });
            }
            CliTerminal::Success {
                stop_reason,
                retained_stop_reason,
            } => (stop_reason, retained_stop_reason),
        };
        let (stop_reason, retained_stop_reason) = stop_reason;
        if self.proposal_indexes.len() != self.result_ids.len() {
            self.report_usage(sink);
            return self.loss(LossCause::ResponseUnintelligible {
                detail: "Claude terminal success did not include a tool_result for every tool_use"
                    .to_string(),
            });
        }
        if stop_reason == "refusal" {
            if !self.proposal_indexes.is_empty() {
                self.report_usage(sink);
                return self.loss(LossCause::ResponseUnintelligible {
                    detail: "Claude refusal also proposed a tool".to_string(),
                });
            }
            let mut capture = sink.take_terminal_text_capture();
            let content = self.redacted_content(&mut capture);
            self.report_usage(sink);
            return TerminalEvidence::Refused(RefusalEvidence {
                exchange: self.exchange,
                message_id: self.message_id,
                reported_model: self.reported_model,
                content,
                usage: self.usage,
            });
        }
        let has_proposals = !self.proposal_indexes.is_empty();
        if (stop_reason == "tool_use") != has_proposals {
            self.report_usage(sink);
            return self.loss(LossCause::ResponseUnintelligible {
                detail: "Claude stop reason contradicts its tool proposal content".to_string(),
            });
        }
        if let Err(detail) = self.validate_tool_requirement() {
            self.report_usage(sink);
            return self.loss(LossCause::ResponseUnintelligible { detail });
        }
        let mut capture = sink.take_terminal_text_capture();
        let content = self.redacted_content(&mut capture);
        self.report_usage(sink);
        if content.is_empty() {
            return self.loss(LossCause::ResponseUnintelligible {
                detail: "Claude terminal success carried no typed assistant content".to_string(),
            });
        }
        let finish = completion_finish(&stop_reason, &retained_stop_reason);
        TerminalEvidence::Completed(CompletionEvidence {
            exchange: self.exchange,
            message_id: self.message_id,
            reported_model: self.reported_model,
            finish,
            content,
            usage: self.usage,
        })
    }

    pub(crate) fn provider_error_after_exit(
        mut self,
        fallback: &str,
        sink: &mut RedactingSink<'_, C>,
    ) -> TerminalEvidence {
        // Every other terminal path reports usage before returning its evidence;
        // this one must too, or a result that stated usage and was then followed
        // by a nonzero exit drops the progress fact from the observation stream
        // entirely.
        self.report_usage(sink);
        if let Some(CliTerminal::Error {
            subtype,
            status,
            message,
        }) = self.terminal
        {
            // Machine-readable HTTP status determines the kind; stderr remains
            // opaque and supplies no exit classification for a generic error.
            let kind = classify_status(status);
            let subtype = sink.redact_retained_metadata(&subtype);
            let message = sink.redact_retained_metadata(&message);
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: self.exchange,
                reported_model: self.reported_model,
                kind,
                non_acceptance_proven: false,
                native: NativeErrorFacts {
                    error_token: Some(subtype),
                    error_code: status.map(|value| value.to_string()),
                    message: Some(message),
                },
                usage: self.usage,
            })
        } else {
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: self.exchange,
                reported_model: self.reported_model,
                kind: ProviderErrorKind::Unrecognized,
                non_acceptance_proven: false,
                native: NativeErrorFacts {
                    error_token: Some("claude_cli_exit".to_string()),
                    error_code: None,
                    message: Some(fallback.to_string()),
                },
                usage: self.usage,
            })
        }
    }

    pub(crate) fn boundary_loss(self, cause: LossCause) -> TerminalEvidence {
        self.loss(cause)
    }

    pub(crate) fn boundary_loss_unless_provider_failure(
        self,
        cause: LossCause,
        sink: &mut RedactingSink<'_, C>,
    ) -> TerminalEvidence {
        if matches!(self.terminal, Some(CliTerminal::Error { .. })) {
            self.provider_error_after_exit("Claude reported an error", sink)
        } else {
            self.loss(cause)
        }
    }

    pub(crate) fn terminal_observed(&self) -> bool {
        self.terminal.is_some()
    }

    fn require_initialized(&self) -> Result<(), DecodeFailure> {
        if self.initialized {
            Ok(())
        } else {
            Err(DecodeFailure::stream_protocol(
                "Claude event arrived before system init",
            ))
        }
    }

    fn take_part_index(&mut self) -> Result<u32, DecodeFailure> {
        let index = self.next_part_index;
        self.next_part_index = self.next_part_index.checked_add(1).ok_or_else(|| {
            DecodeFailure::stream_protocol("Claude content-part index overflowed")
        })?;
        Ok(index)
    }

    /// Preserves typed correlation when credential redaction changes native tool ids.
    fn unique_tool_id(&mut self, native: &str, sanitized: String) -> String {
        if sanitized == native && self.emitted_tool_ids.insert(sanitized.clone()) {
            return sanitized;
        }
        loop {
            let candidate = format!("claude-redacted-call-{}", self.redacted_tool_id_cursor);
            self.redacted_tool_id_cursor += 1;
            if self.emitted_tool_ids.insert(candidate.clone()) {
                return candidate;
            }
        }
    }

    fn validate_tool_requirement(&self) -> Result<(), String> {
        match &self.tool_requirement {
            ToolRequirement::Optional => Ok(()),
            ToolRequirement::Any if self.proposal_indexes.is_empty() => {
                Err("Claude did not satisfy the required any-tool choice".to_string())
            }
            ToolRequirement::Any => Ok(()),
            // Suppression withholds a proposal's arguments, never its admitted
            // tool name, so a suppressed foreign proposal violates the choice
            // exactly as an admitted one does.
            ToolRequirement::Named(name)
                if self.proposal_indexes.is_empty()
                    || self.content.iter().any(|part| match part {
                        AssistantPart::ToolCall(call) => call.name.as_str() != name,
                        AssistantPart::SuppressedToolCall(suppressed) => {
                            suppressed.as_str() != name
                        }
                        AssistantPart::Text(_)
                        | AssistantPart::Thinking { .. }
                        | AssistantPart::RedactedThinking { .. } => false,
                    }) =>
            {
                Err(format!("Claude tool choice permits only `{name}`"))
            }
            ToolRequirement::Named(_) => Ok(()),
        }
    }

    fn redacted_content(&mut self, capture: &mut TerminalTextCapture) -> Vec<AssistantPart> {
        self.content
            .drain(..)
            .zip(0_u32..)
            .filter_map(|(part, index)| match part {
                AssistantPart::Text(text) => {
                    let text = capture.take_text(index).unwrap_or_else(|| {
                        if text.is_empty() {
                            String::new()
                        } else {
                            REDACTED.to_string()
                        }
                    });
                    (!text.is_empty()).then_some(AssistantPart::Text(text))
                }
                AssistantPart::Thinking { text, signature } => {
                    let text = capture.take_thinking(index).unwrap_or_else(|| {
                        if text.is_empty() {
                            text
                        } else {
                            REDACTED.to_string()
                        }
                    });
                    Some(AssistantPart::Thinking {
                        text,
                        signature: signature.map(|value| redact_text(&value)),
                    })
                }
                AssistantPart::RedactedThinking { data } => Some(AssistantPart::RedactedThinking {
                    data: redact_text(&data),
                }),
                AssistantPart::ToolCall(mut call) => {
                    call.arguments_json = redact_json(&call.arguments_json);
                    Some(AssistantPart::ToolCall(call))
                }
                AssistantPart::SuppressedToolCall(name) => {
                    Some(AssistantPart::SuppressedToolCall(name))
                }
            })
            .collect()
    }

    /// Whether a tool call had opened in the events decoded so far.
    ///
    /// The adapter classifies every structured CLI event it reads, so the
    /// negative case is the stated fact rather than an absence.
    fn tool_calls_at_loss(&self) -> ToolCallsAtLoss {
        if self.opened_tool_calls {
            ToolCallsAtLoss::Opened
        } else if self.undelivered_line {
            ToolCallsAtLoss::Unobserved
        } else {
            ToolCallsAtLoss::NoneOpened
        }
    }

    /// The tool fact for a loss raised by an event that never decoded.
    ///
    /// The dividing line is whether *this* event's content was examined, not
    /// whether it was accepted. A line that failed UTF-8, nesting,
    /// duplicate-member, JSON, or `type`-discriminator decoding was never
    /// classified and could itself have been the `tool_use` event, so the
    /// negative is withheld. An event whose content did decode — every
    /// non-`assistant` type, whose discriminator alone precludes content
    /// blocks, and an `assistant` event whose blocks were scanned before the
    /// semantic checks ran — states the negative, because the adapter read the
    /// material and no tool call was in it. A tool call an earlier event
    /// already established outranks both.
    fn tool_calls_at_decode_failure(&self) -> ToolCallsAtLoss {
        if self.opened_tool_calls {
            ToolCallsAtLoss::Opened
        } else if self.undelivered_line {
            ToolCallsAtLoss::Unobserved
        } else if self.current_event_examined {
            ToolCallsAtLoss::NoneOpened
        } else {
            ToolCallsAtLoss::Unobserved
        }
    }

    /// Boundary-loss evidence for a line that never decoded.
    pub(crate) fn loss_at_decode_failure(self, cause: LossCause) -> TerminalEvidence {
        let tool_calls = self.tool_calls_at_decode_failure();
        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause,
            exchange: self.exchange,
            reported_model: self.reported_model,
            finish_reported: self.finish_reported,
            tool_calls,
            usage: self.usage,
        })
    }

    fn loss(self, cause: LossCause) -> TerminalEvidence {
        let tool_calls = self.tool_calls_at_loss();
        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause,
            exchange: self.exchange,
            reported_model: self.reported_model,
            finish_reported: self.finish_reported,
            tool_calls,
            usage: self.usage,
        })
    }
}

fn tool_result_text(value: &Value) -> Option<&str> {
    let blocks = value.as_array()?;
    let [block] = blocks.as_slice() else {
        return None;
    };
    if block.get("type")?.as_str()? != "text" {
        return None;
    }
    block.get("text")?.as_str()
}

fn message_usage(value: crate::wire::MessageUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: value.input_tokens,
        output_tokens: value.output_tokens,
        cache_creation_input_tokens: value.cache_creation_input_tokens,
        cache_read_input_tokens: value.cache_read_input_tokens,
    }
}

fn result_usage(value: crate::wire::ResultUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: value.input_tokens,
        output_tokens: value.output_tokens,
        cache_creation_input_tokens: value.cache_creation_input_tokens,
        cache_read_input_tokens: value.cache_read_input_tokens,
    }
}

fn finish_reason(token: &str) -> FinishReason {
    finish_reason_with_token(token, token)
}

fn finish_reason_with_token(token: &str, retained_token: &str) -> FinishReason {
    match token {
        "end_turn" => FinishReason::EndTurn,
        "refusal" => FinishReason::Refusal,
        "max_tokens" => FinishReason::MaxOutputTokens,
        "tool_use" => FinishReason::ToolUse,
        other => FinishReason::Unrecognized {
            provider_token: if retained_token == token {
                other.to_string()
            } else {
                retained_token.to_string()
            },
        },
    }
}

fn completion_finish(token: &str, retained_token: &str) -> CompletionFinish {
    finish_reason_with_token(token, retained_token)
        .completion_finish()
        .unwrap_or(CompletionFinish::Unrecognized {
            provider_token: retained_token.to_string(),
        })
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, DecodeFailure> {
    serde_json::from_value(value).map_err(|error| DecodeFailure::stream_protocol(error.to_string()))
}

fn decode_text<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, DecodeFailure> {
    serde_json::from_str(text).map_err(|error| DecodeFailure::stream_protocol(error.to_string()))
}

pub(crate) struct DecodeFailure {
    detail: String,
}

impl DecodeFailure {
    fn stream_protocol(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
    pub(crate) fn into_detail(self) -> String {
        self.detail
    }
}

impl<C: Clone> CliSession<C> for EventDecoder<C> {
    const LABELS: CliProcessLabels = CliProcessLabels {
        provider: "Claude",
        process: "Claude CLI",
        decode_event: "Claude event",
        bounded_event: "Claude JSONL event",
    };

    fn correlation(&self) -> &C {
        &self.correlation
    }

    fn terminal_text_capture(&self) -> CliTerminalTextCapture {
        match self.delivery {
            DeliveryMode::Buffered => CliTerminalTextCapture::TerminalOnly,
            DeliveryMode::Streamed => CliTerminalTextCapture::StreamAndTerminal,
        }
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
            CliDecodeFailure::new(
                CliDecodeFailureClass::StreamProtocolViolation,
                error.into_detail(),
            )
        })
    }

    fn decode_failure(self, _class: CliDecodeFailureClass, detail: String) -> TerminalEvidence {
        // Not `boundary_loss`: the failing line was never classified, so the
        // tool fact is withheld rather than stated negative.
        EventDecoder::loss_at_decode_failure(self, LossCause::StreamProtocolViolation { detail })
    }

    fn finish(self, sink: &mut RedactingSink<'_, C>) -> TerminalEvidence {
        EventDecoder::finish(self, sink)
    }

    fn boundary_loss(self, cause: LossCause) -> TerminalEvidence {
        EventDecoder::boundary_loss(self, cause)
    }

    fn note_undelivered_line(&mut self) {
        self.undelivered_line = true;
    }

    fn undelivered_line_loss(mut self, cause: LossCause) -> TerminalEvidence {
        // The rejected line may itself have been the `assistant` event carrying
        // a `tool_use` block. Nothing about it was examined, and the previous
        // event's examination says nothing about this one.
        self.current_event_examined = false;
        EventDecoder::loss_at_decode_failure(self, cause)
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
