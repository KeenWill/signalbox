//! Stateful decoding of one Claude Code streamed-JSON event stream.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CompletionEvidence, CompletionFinish, DeliveryMode,
    ExchangeFacts, FinishReason, LossCause, NativeErrorFacts, Observation, ObservationFact,
    ObservationSink, ProviderErrorEvidence, ProviderMessageId, ProviderReportedModel,
    ProviderRequestId, RefusalEvidence, TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal,
    ToolName, validate_provider_json_nesting,
};

use crate::SUPPORTED_CLAUDE_CLI_VERSION;
use crate::bridge::{SERVER_NAME, TOOL_ACKNOWLEDGEMENT, TOOL_PREFIX};
use crate::redaction::{REDACTED, RedactingSink, TerminalTextCapture, redact_json, redact_text};
use crate::status::classify_error;
use crate::translate::{ToolRequirement, TranslatedOperation};
use crate::wire::{
    AssistantContent, AssistantEvent, AssistantRawEvent, RawToolUse, ResultEvent, SystemInit,
    UserEvent,
};

/// A validation-only JSON walk that rejects repeated object members at every
/// nesting depth before serde projects the event into its last-value-wins
/// [`Value`] representation.
struct DuplicateFreeJson<'a> {
    duplicate_found: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for DuplicateFreeJson<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateFreeVisitor {
            duplicate_found: self.duplicate_found,
        })
    }
}

struct DuplicateFreeVisitor<'a> {
    duplicate_found: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for DuplicateFreeVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without repeated object members")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        DuplicateFreeJson {
            duplicate_found: self.duplicate_found,
        }
        .deserialize(deserializer)
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        DuplicateFreeJson {
            duplicate_found: self.duplicate_found,
        }
        .deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence
            .next_element_seed(DuplicateFreeJson {
                duplicate_found: self.duplicate_found,
            })?
            .is_some()
        {}
        Ok(())
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut members = HashSet::new();
        while let Some(member) = object.next_key::<String>()? {
            if !members.insert(member) {
                self.duplicate_found.set(true);
                return Err(serde::de::Error::custom("duplicate JSON member"));
            }
            object.next_value_seed(DuplicateFreeJson {
                duplicate_found: self.duplicate_found,
            })?;
        }
        Ok(())
    }
}

fn reject_duplicate_json_members(line: &str) -> Result<(), DecodeFailure> {
    let duplicate_found = Cell::new(false);
    let mut deserializer = serde_json::Deserializer::from_str(line);
    let result = DuplicateFreeJson {
        duplicate_found: &duplicate_found,
    }
    .deserialize(&mut deserializer)
    .and_then(|()| deserializer.end());
    match result {
        Ok(()) => Ok(()),
        Err(_) if duplicate_found.get() => Err(DecodeFailure::stream_protocol(
            "JSON input has duplicate object members",
        )),
        // The ordinary decoder below owns every other JSON-shape failure and
        // preserves its established protocol-failure detail.
        Err(_) => Ok(()),
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
    native_model: Option<String>,
    message_id: Option<ProviderMessageId>,
    native_message_id: Option<String>,
    content: Vec<AssistantPart>,
    proposal_indexes: HashMap<String, usize>,
    result_ids: HashSet<String>,
    emitted_tool_ids: HashSet<String>,
    redacted_tool_id_cursor: usize,
    next_part_index: u32,
    usage: TokenUsage,
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
            native_model: None,
            message_id: None,
            native_message_id: None,
            content: Vec::new(),
            proposal_indexes: HashMap::new(),
            result_ids: HashSet::new(),
            emitted_tool_ids: HashSet::new(),
            redacted_tool_id_cursor: 1,
            next_part_index: 0,
            usage: TokenUsage::unreported(),
            finish_reported: None,
            initialized: false,
            terminal: None,
        }
    }

    pub(crate) fn push(
        &mut self,
        line: &[u8],
        sink: &mut RedactingSink<'_, C>,
    ) -> Result<(), DecodeFailure> {
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
        if value.get("subtype").and_then(Value::as_str) != Some("init") || self.initialized {
            return Err(DecodeFailure::stream_protocol(
                "unexpected or duplicate Claude system event",
            ));
        }
        let event: SystemInit = decode(value)?;
        if event.session_id.is_empty() || event.model.is_empty() {
            return Err(DecodeFailure::stream_protocol(
                "Claude init carries an empty session or model id",
            ));
        }
        if event.claude_code_version != SUPPORTED_CLAUDE_CLI_VERSION {
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
        self.native_model = Some(event.model);
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
            self.message_id = Some(ProviderMessageId::new(
                sink.redact_provider_id("", &event.message.id),
            ));
            self.native_message_id = Some(event.message.id.clone());
        }
        if self.native_model.as_deref() != Some(event.message.model.as_str()) {
            return Err(DecodeFailure::stream_protocol(
                "Claude assistant model contradicts system init",
            ));
        }
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
                let sanitized_id = sink.redact_provider_id("", &id);
                let proposal_id = self.unique_tool_id(&id, sanitized_id);
                let proposal = ToolCallProposal {
                    id: ToolCallId::new(proposal_id),
                    name: ToolName::new(name),
                    arguments_json: arguments.clone(),
                };
                self.proposal_indexes.insert(id, self.content.len());
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
                let kind = classify_error(status, &subtype, &message);
                let subtype = sink.redact_retained_metadata(&subtype);
                let message = sink.redact_retained_metadata(&message);
                sink.observe(Observation {
                    correlation: self.correlation.clone(),
                    fact: ObservationFact::UsageReported(self.usage),
                });
                return TerminalEvidence::ProviderError(ProviderErrorEvidence {
                    exchange: self.exchange,
                    reported_model: self.reported_model,
                    kind,
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
            sink.observe(Observation {
                correlation: self.correlation.clone(),
                fact: ObservationFact::UsageReported(self.usage),
            });
            return self.loss(LossCause::ResponseUnintelligible {
                detail: "Claude terminal success did not include a tool_result for every tool_use"
                    .to_string(),
            });
        }
        if stop_reason == "refusal" {
            if !self.proposal_indexes.is_empty() {
                sink.observe(Observation {
                    correlation: self.correlation.clone(),
                    fact: ObservationFact::UsageReported(self.usage),
                });
                return self.loss(LossCause::ResponseUnintelligible {
                    detail: "Claude refusal also proposed a tool".to_string(),
                });
            }
            let mut capture = sink.take_terminal_text_capture();
            let content = self.redacted_content(&mut capture);
            sink.observe(Observation {
                correlation: self.correlation.clone(),
                fact: ObservationFact::UsageReported(self.usage),
            });
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
            sink.observe(Observation {
                correlation: self.correlation.clone(),
                fact: ObservationFact::UsageReported(self.usage),
            });
            return self.loss(LossCause::ResponseUnintelligible {
                detail: "Claude stop reason contradicts its tool proposal content".to_string(),
            });
        }
        if let Err(detail) = self.validate_tool_requirement() {
            sink.observe(Observation {
                correlation: self.correlation.clone(),
                fact: ObservationFact::UsageReported(self.usage),
            });
            return self.loss(LossCause::ResponseUnintelligible { detail });
        }
        let mut capture = sink.take_terminal_text_capture();
        let content = self.redacted_content(&mut capture);
        sink.observe(Observation {
            correlation: self.correlation.clone(),
            fact: ObservationFact::UsageReported(self.usage),
        });
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
        self,
        fallback: &str,
        classification: &str,
        sink: &mut RedactingSink<'_, C>,
    ) -> TerminalEvidence {
        if let Some(CliTerminal::Error {
            subtype,
            status,
            message,
        }) = self.terminal
        {
            let kind = classify_error(status, &subtype, &message);
            let subtype = sink.redact_retained_metadata(&subtype);
            let message = sink.redact_retained_metadata(&message);
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: self.exchange,
                reported_model: self.reported_model,
                kind,
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
                kind: classify_error(None, "process_exit", classification),
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
            self.provider_error_after_exit(
                "Claude reported an error",
                "Claude reported an error",
                sink,
            )
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
            ToolRequirement::Any if self.proposal_indexes.is_empty() => Err("Claude did not satisfy the required any-tool choice".to_string()),
            ToolRequirement::Any => Ok(()),
            ToolRequirement::Named(name)
                if self.proposal_indexes.is_empty()
                    || self.content.iter().any(|part| {
                        matches!(part, AssistantPart::ToolCall(call) if call.name.as_str() != name)
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
            })
            .collect()
    }

    fn loss(self, cause: LossCause) -> TerminalEvidence {
        TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause,
            exchange: self.exchange,
            reported_model: self.reported_model,
            finish_reported: self.finish_reported,
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

#[derive(Clone, Copy)]
pub(crate) enum DecodeFailureClass {
    StreamProtocolViolation,
}

pub(crate) struct DecodeFailure {
    detail: String,
    class: DecodeFailureClass,
}

impl DecodeFailure {
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
