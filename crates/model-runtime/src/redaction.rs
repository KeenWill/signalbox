//! Exact-credential redaction at a provider-adapter boundary.
//!
//! Provider-controlled observations and evidence are sanitized with the exact
//! credential captured while preparing the physical request.

use std::collections::VecDeque;

use crate::{
    AssistantPart, CompletionFinish, CredentialValue, ExchangeFacts, FinishReason, LossCause,
    NativeErrorFacts, Observation, ObservationFact, ObservationSink, ProvenUnsentEvidence,
    ProviderMessageId, ProviderReportedModel, ProviderRequestId, StreamInterruption,
    TerminalEvidence, ToolCallId, ToolCallProposal, ToolName, TransportFacts, UnsentCause,
};

const NATIVE_MESSAGE_TRUNCATION_SUFFIX: &str = " … [truncated]";

/// An observation sink that sanitizes provider-controlled text with the exact
/// request credential before forwarding any fact across the adapter boundary.
pub struct CredentialRedactingSink<'a, C> {
    inner: &'a mut (dyn ObservationSink<C> + Send),
    credential: &'a CredentialValue,
    credential_text: &'a str,
    pending_stream_text: Option<PendingStreamText<C>>,
    pending_tool_arguments: Option<PendingToolArguments<C>>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum StreamField {
    Text,
    Thinking,
}

struct PendingStreamText<C> {
    field: StreamField,
    index: u32,
    correlation: C,
    text: String,
}

struct PendingToolArguments<C> {
    index: u32,
    correlation: C,
    fragment: String,
}

impl<'a, C: Clone> CredentialRedactingSink<'a, C> {
    /// Wraps an adapter observation sink with exact-credential sanitization.
    pub fn new(
        inner: &'a mut (dyn ObservationSink<C> + Send),
        credential: &'a CredentialValue,
    ) -> Self {
        Self {
            inner,
            credential,
            credential_text: std::str::from_utf8(credential.expose_bytes()).unwrap_or_default(),
            pending_stream_text: None,
            pending_tool_arguments: None,
        }
    }

    fn flush_stream_text(&mut self) {
        if let Some(pending) = self.pending_stream_text.take() {
            // Pending text is exactly a credential prefix. Once another fact
            // must cross the boundary, retain ordering and fail closed.
            self.emit_stream_text(
                pending.field,
                pending.index,
                pending.correlation,
                "[redacted]".to_string(),
            );
        }
    }

    fn flush_tool_arguments(&mut self) {
        if let Some(pending) = self.pending_tool_arguments.take() {
            self.emit_tool_arguments(pending.index, pending.correlation, "[redacted]".to_string());
        }
    }

    /// Flushes held credential prefixes, replacing them rather than allowing a
    /// later observation to reconstruct the credential across fact boundaries.
    pub fn flush(&mut self) {
        self.flush_stream_text();
        self.flush_tool_arguments();
    }

    fn emit_stream_text(&mut self, field: StreamField, index: u32, correlation: C, text: String) {
        if text.is_empty() {
            return;
        }
        let fact = match field {
            StreamField::Text => ObservationFact::TextDelta { index, text },
            StreamField::Thinking => ObservationFact::ThinkingDelta { index, text },
        };
        self.inner.observe(Observation { correlation, fact });
    }

    fn emit_tool_arguments(&mut self, index: u32, correlation: C, fragment: String) {
        if !fragment.is_empty() {
            self.inner.observe(Observation {
                correlation,
                fact: ObservationFact::ToolArgumentsDelta { index, fragment },
            });
        }
    }

    fn redact_stream_delta(
        &mut self,
        field: StreamField,
        index: u32,
        correlation: C,
        text: String,
    ) {
        self.flush_tool_arguments();
        if self
            .pending_stream_text
            .as_ref()
            .is_some_and(|pending| pending.field != field || pending.index != index)
        {
            self.flush_stream_text();
        }
        let mut combined = self
            .pending_stream_text
            .take()
            .map_or_else(String::new, |pending| pending.text);
        combined.push_str(&text);
        let (emitted, pending) =
            redact_complete_credentials_and_hold_prefix(combined, self.credential_text);
        self.emit_stream_text(field, index, correlation.clone(), emitted);
        if !pending.is_empty() {
            self.pending_stream_text = Some(PendingStreamText {
                field,
                index,
                correlation,
                text: pending,
            });
        }
    }

    fn redact_tool_delta(&mut self, index: u32, correlation: C, fragment: String) {
        self.flush_stream_text();
        if self
            .pending_tool_arguments
            .as_ref()
            .is_some_and(|pending| pending.index != index)
        {
            self.flush_tool_arguments();
        }
        let mut combined = self
            .pending_tool_arguments
            .take()
            .map_or_else(String::new, |pending| pending.fragment);
        combined.push_str(&fragment);
        let (emitted, pending) = redact_json_stream_fragment(combined, self.credential_text);
        self.emit_tool_arguments(index, correlation.clone(), emitted);
        if !pending.is_empty() {
            self.pending_tool_arguments = Some(PendingToolArguments {
                index,
                correlation,
                fragment: pending,
            });
        }
    }
}

impl<C: Clone> ObservationSink<C> for CredentialRedactingSink<'_, C> {
    fn observe(&mut self, observation: Observation<C>) {
        match observation.fact {
            ObservationFact::TextDelta { index, text } => {
                self.redact_stream_delta(StreamField::Text, index, observation.correlation, text)
            }
            ObservationFact::ThinkingDelta { index, text } => self.redact_stream_delta(
                StreamField::Thinking,
                index,
                observation.correlation,
                text,
            ),
            ObservationFact::ToolArgumentsDelta { index, fragment } => {
                self.redact_tool_delta(index, observation.correlation, fragment);
            }
            ObservationFact::ToolCallProposed(proposal) => {
                self.flush();
                self.inner.observe(Observation {
                    correlation: observation.correlation,
                    fact: ObservationFact::ToolCallProposed(redact_tool_proposal(
                        proposal,
                        self.credential,
                    )),
                });
            }
            fact => {
                self.flush();
                self.inner.observe(Observation {
                    correlation: observation.correlation,
                    fact: redact_observation_fact(fact, self.credential),
                });
            }
        }
    }
}

fn redact_complete_credentials_and_hold_prefix(
    mut text: String,
    credential: &str,
) -> (String, String) {
    if credential.is_empty() {
        return (text, String::new());
    }
    // Only the tail after the last complete, non-overlapping match can become
    // a credential when the next provider chunk arrives. A proper suffix that
    // starts inside an already-complete match must be redacted with that match,
    // not retained and emitted later.
    let unmatched_tail_start = text
        .match_indices(credential)
        .last()
        .map_or(0, |(start, matched)| start + matched.len());
    let unmatched_tail = &text[unmatched_tail_start..];
    let longest_prefix = (1..credential.len())
        .rev()
        .filter(|length| credential.is_char_boundary(*length))
        .find(|length| unmatched_tail.ends_with(&credential[..*length]));
    let split = longest_prefix.map_or(text.len(), |length| text.len() - length);
    let pending = text.split_off(split);
    (text.replace(credential, "[redacted]"), pending)
}

struct PendingJsonUnit {
    raw_start: usize,
    raw_end: usize,
}

/// Sanitizes one accumulated streamed JSON fragment while retaining only a
/// suffix whose decoded characters could still complete the credential.
fn redact_json_stream_fragment(raw: String, credential: &str) -> (String, String) {
    if credential.is_empty() {
        return (raw, String::new());
    }

    let fallback = |raw: String| {
        if json_escapes_decode_to_credential(&raw, credential) {
            ("[redacted]".to_string(), String::new())
        } else {
            redact_complete_credentials_and_hold_prefix(raw, credential)
        }
    };
    let bytes = raw.as_bytes();
    let pattern: Vec<char> = credential.chars().collect();
    let mut prefix_lengths = vec![0; pattern.len()];
    for index in 1..pattern.len() {
        let mut prefix = prefix_lengths[index - 1];
        while prefix > 0 && pattern[index] != pattern[prefix] {
            prefix = prefix_lengths[prefix - 1];
        }
        if pattern[index] == pattern[prefix] {
            prefix += 1;
        }
        prefix_lengths[index] = prefix;
    }

    let mut cursor = 0;
    let mut matched = 0;
    let mut pending: VecDeque<PendingJsonUnit> = VecDeque::with_capacity(pattern.len());
    let mut emitted = String::with_capacity(raw.len());
    while cursor < raw.len() {
        let raw_start = cursor;
        let character = if bytes[cursor] != b'\\' {
            let Some(character) = raw[cursor..].chars().next() else {
                return fallback(raw);
            };
            cursor += character.len_utf8();
            character
        } else {
            if cursor + 1 >= raw.len() {
                break;
            }
            match bytes[cursor + 1] {
                b'"' => {
                    cursor += 2;
                    '"'
                }
                b'\\' => {
                    cursor += 2;
                    '\\'
                }
                b'/' => {
                    cursor += 2;
                    '/'
                }
                b'b' => {
                    cursor += 2;
                    '\u{0008}'
                }
                b'f' => {
                    cursor += 2;
                    '\u{000c}'
                }
                b'n' => {
                    cursor += 2;
                    '\n'
                }
                b'r' => {
                    cursor += 2;
                    '\r'
                }
                b't' => {
                    cursor += 2;
                    '\t'
                }
                b'u' => {
                    if cursor + 6 > raw.len() {
                        break;
                    }
                    let Ok(hex) = std::str::from_utf8(&bytes[cursor + 2..cursor + 6]) else {
                        return fallback(raw);
                    };
                    let Ok(first) = u16::from_str_radix(hex, 16) else {
                        return fallback(raw);
                    };
                    if (0xd800..=0xdbff).contains(&first) {
                        if cursor + 12 > raw.len() {
                            break;
                        }
                        if &bytes[cursor + 6..cursor + 8] != b"\\u" {
                            return fallback(raw);
                        }
                        let Ok(hex) = std::str::from_utf8(&bytes[cursor + 8..cursor + 12]) else {
                            return fallback(raw);
                        };
                        let Ok(second) = u16::from_str_radix(hex, 16) else {
                            return fallback(raw);
                        };
                        if !(0xdc00..=0xdfff).contains(&second) {
                            return fallback(raw);
                        }
                        let scalar = 0x1_0000
                            + ((u32::from(first) - 0xd800) << 10)
                            + (u32::from(second) - 0xdc00);
                        let Some(character) = char::from_u32(scalar) else {
                            return fallback(raw);
                        };
                        cursor += 12;
                        character
                    } else {
                        if (0xdc00..=0xdfff).contains(&first) {
                            return fallback(raw);
                        }
                        let Some(character) = char::from_u32(u32::from(first)) else {
                            return fallback(raw);
                        };
                        cursor += 6;
                        character
                    }
                }
                _ => return fallback(raw),
            }
        };

        while matched > 0 && pattern[matched] != character {
            let retained = prefix_lengths[matched - 1];
            for _ in retained..matched {
                let Some(unit) = pending.pop_front() else {
                    return fallback(raw);
                };
                emitted.push_str(&raw[unit.raw_start..unit.raw_end]);
            }
            matched = retained;
        }
        if pattern[matched] == character {
            pending.push_back(PendingJsonUnit {
                raw_start,
                raw_end: cursor,
            });
            matched += 1;
            if matched == pattern.len() {
                emitted.push_str("[redacted]");
                pending.clear();
                matched = 0;
            }
        } else {
            emitted.push_str(&raw[raw_start..cursor]);
        }
    }

    let pending_start = pending
        .front()
        .map_or(cursor, |unit: &PendingJsonUnit| unit.raw_start.min(cursor));
    (emitted, raw[pending_start..].to_string())
}

fn redact_observation_fact(fact: ObservationFact, credential: &CredentialValue) -> ObservationFact {
    match fact {
        ObservationFact::ExchangeEstablished(exchange) => {
            ObservationFact::ExchangeEstablished(redact_exchange(exchange, credential))
        }
        ObservationFact::ProviderModelReported(model) => ObservationFact::ProviderModelReported(
            ProviderReportedModel::new(redact_text(model.as_str().to_string(), credential)),
        ),
        ObservationFact::TextDelta { index, text } => ObservationFact::TextDelta {
            index,
            text: redact_text(text, credential),
        },
        ObservationFact::ThinkingDelta { index, text } => ObservationFact::ThinkingDelta {
            index,
            text: redact_text(text, credential),
        },
        ObservationFact::ToolArgumentsDelta { index, fragment } => {
            ObservationFact::ToolArgumentsDelta {
                index,
                fragment: redact_text(fragment, credential),
            }
        }
        ObservationFact::ToolCallProposed(proposal) => {
            ObservationFact::ToolCallProposed(redact_tool_proposal(proposal, credential))
        }
        ObservationFact::FinishReported(finish) => {
            ObservationFact::FinishReported(redact_finish(finish, credential))
        }
        fact @ (ObservationFact::SendCommenced | ObservationFact::UsageReported(_)) => fact,
    }
}

/// Credential-sanitizes every provider-controlled or transport-rendered
/// text in the evidence, per the runtime-substrate spec: a reflected key
/// value in an error message, raw body, or rendered detail is replaced
/// before the evidence leaves the adapter boundary. Non-text typed facts are
/// untouched; text-bearing typed fields (reported model, message id,
/// unrecognized tokens, content) are sanitized when they carry the key.
pub fn redact_evidence(
    evidence: TerminalEvidence,
    api_key: &CredentialValue,
    native_message_limit: Option<usize>,
) -> TerminalEvidence {
    let key_text = std::str::from_utf8(api_key.expose_bytes()).unwrap_or_default();
    let redact = move |text: String| -> String {
        if key_text.is_empty() {
            text
        } else {
            text.replace(key_text, "[redacted]")
        }
    };
    let redact_native = |mut native: NativeErrorFacts| -> NativeErrorFacts {
        native.error_token = native.error_token.map(redact);
        native.error_code = native.error_code.map(redact);
        native.message = native
            .message
            .map(|message| redact_native_message(message, api_key, native_message_limit));
        native
    };
    let redact_transport =
        |facts: TransportFacts| -> TransportFacts { TransportFacts::new(redact(facts.detail)) };
    match evidence {
        TerminalEvidence::ProviderError(mut error) => {
            error.exchange = redact_exchange(error.exchange, api_key);
            error.reported_model = error.reported_model.map(|model| {
                ProviderReportedModel::new(redact_text(model.as_str().to_string(), api_key))
            });
            error.native = redact_native(error.native);
            TerminalEvidence::ProviderError(error)
        }
        TerminalEvidence::CancellationConfirmed(mut confirmed) => {
            confirmed.exchange = redact_exchange(confirmed.exchange, api_key);
            confirmed.reported_model = confirmed.reported_model.map(|model| {
                ProviderReportedModel::new(redact_text(model.as_str().to_string(), api_key))
            });
            confirmed.native = redact_native(confirmed.native);
            TerminalEvidence::CancellationConfirmed(confirmed)
        }
        TerminalEvidence::ProvenUnsent(unsent) => {
            let cause = match unsent.cause {
                UnsentCause::ConnectFailed(facts) => {
                    UnsentCause::ConnectFailed(redact_transport(facts))
                }
                UnsentCause::SendIncompleteProvenUnacceptable(facts) => {
                    UnsentCause::SendIncompleteProvenUnacceptable(redact_transport(facts))
                }
                UnsentCause::CancelledBeforeSend => UnsentCause::CancelledBeforeSend,
            };
            TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence { cause })
        }
        TerminalEvidence::BoundaryLoss(mut loss) => {
            loss.exchange = redact_exchange(loss.exchange, api_key);
            loss.reported_model = loss.reported_model.map(|model| {
                ProviderReportedModel::new(redact_text(model.as_str().to_string(), api_key))
            });
            loss.finish_reported = loss
                .finish_reported
                .map(|finish| redact_finish(finish, api_key));
            loss.cause = match loss.cause {
                LossCause::TimedOut(facts) => LossCause::TimedOut(redact_transport(facts)),
                LossCause::TransportFailed(facts) => {
                    LossCause::TransportFailed(redact_transport(facts))
                }
                LossCause::ResponseBodyLost(facts) => {
                    LossCause::ResponseBodyLost(redact_transport(facts))
                }
                LossCause::ResponseUnintelligible { detail } => LossCause::ResponseUnintelligible {
                    detail: redact(detail),
                },
                LossCause::StreamProtocolViolation { detail } => {
                    LossCause::StreamProtocolViolation {
                        detail: redact(detail),
                    }
                }
                LossCause::StreamEndedWithoutTerminalMarker { interruption } => {
                    LossCause::StreamEndedWithoutTerminalMarker {
                        interruption: match interruption {
                            StreamInterruption::TransportFailure(facts) => {
                                StreamInterruption::TransportFailure(redact_transport(facts))
                            }
                            StreamInterruption::TimedOut(facts) => {
                                StreamInterruption::TimedOut(redact_transport(facts))
                            }
                            StreamInterruption::EndOfStream => StreamInterruption::EndOfStream,
                        },
                    }
                }
                cause @ (LossCause::CancellationRequested | LossCause::UnexpectedHttpStatus) => {
                    cause
                }
            };
            TerminalEvidence::BoundaryLoss(loss)
        }
        TerminalEvidence::Completed(mut completion) => {
            completion.exchange = redact_exchange(completion.exchange, api_key);
            completion.message_id = completion
                .message_id
                .map(|id| ProviderMessageId::new(redact_text(id.as_str().to_string(), api_key)));
            completion.reported_model = completion.reported_model.map(|model| {
                ProviderReportedModel::new(redact_text(model.as_str().to_string(), api_key))
            });
            completion.finish = redact_completion_finish(completion.finish, api_key);
            completion.content = completion
                .content
                .into_iter()
                .map(|part| redact_assistant_part(part, api_key))
                .collect();
            TerminalEvidence::Completed(completion)
        }
        TerminalEvidence::Refused(mut refusal) => {
            refusal.exchange = redact_exchange(refusal.exchange, api_key);
            refusal.message_id = refusal
                .message_id
                .map(|id| ProviderMessageId::new(redact_text(id.as_str().to_string(), api_key)));
            refusal.reported_model = refusal.reported_model.map(|model| {
                ProviderReportedModel::new(redact_text(model.as_str().to_string(), api_key))
            });
            refusal.content = refusal
                .content
                .into_iter()
                .map(|part| redact_assistant_part(part, api_key))
                .collect();
            TerminalEvidence::Refused(refusal)
        }
    }
}

fn redact_text(text: String, credential: &CredentialValue) -> String {
    let key = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    if key.is_empty() {
        text
    } else {
        text.replace(key, "[redacted]")
    }
}

/// Redacts complete credentials and fails closed on a trailing prefix. Final
/// content parts are independently persisted values, so a prefix may not
/// leave one part and be completed by the next.
fn redact_bounded_text(text: String, credential: &CredentialValue) -> String {
    let key = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let (mut redacted, pending) = redact_complete_credentials_and_hold_prefix(text, key);
    if !pending.is_empty() {
        redacted.push_str("[redacted]");
    }
    redacted
}

fn redact_native_message(
    text: String,
    credential: &CredentialValue,
    limit: Option<usize>,
) -> String {
    let redacted = if let Some(body) = text.strip_suffix(NATIVE_MESSAGE_TRUNCATION_SUFFIX) {
        let mut redacted = redact_native_body(body.to_string(), credential);
        redacted.push_str(NATIVE_MESSAGE_TRUNCATION_SUFFIX);
        redacted
    } else {
        redact_native_body(text, credential)
    };
    lossy_truncated(redacted.as_bytes(), limit)
}

fn redact_native_body(text: String, credential: &CredentialValue) -> String {
    if serde_json::value::RawValue::from_string(text.clone()).is_ok() {
        return redact_json(text, credential);
    }
    let key = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    if json_escapes_decode_to_credential(&text, key) {
        return "\"[redacted]\"".to_string();
    }
    let (mut redacted, pending) = redact_complete_credentials_and_hold_prefix(text, key);
    if !pending.is_empty() {
        redacted.push_str("[redacted]");
    }
    redacted
}

fn redact_finish(finish: FinishReason, credential: &CredentialValue) -> FinishReason {
    match finish {
        FinishReason::StopSequence { sequence } => FinishReason::StopSequence {
            sequence: sequence.map(|value| redact_text(value, credential)),
        },
        FinishReason::Unrecognized { provider_token } => FinishReason::Unrecognized {
            provider_token: redact_text(provider_token, credential),
        },
        finish => finish,
    }
}

fn redact_completion_finish(
    finish: CompletionFinish,
    credential: &CredentialValue,
) -> CompletionFinish {
    match finish {
        CompletionFinish::StopSequence { sequence } => CompletionFinish::StopSequence {
            sequence: sequence.map(|value| redact_text(value, credential)),
        },
        CompletionFinish::Unrecognized { provider_token } => CompletionFinish::Unrecognized {
            provider_token: redact_text(provider_token, credential),
        },
        finish => finish,
    }
}

fn redact_exchange(mut exchange: ExchangeFacts, credential: &CredentialValue) -> ExchangeFacts {
    exchange.provider_request_id = exchange
        .provider_request_id
        .map(|id| ProviderRequestId::new(redact_text(id.as_str().to_string(), credential)));
    exchange
}

fn redact_tool_proposal(
    proposal: ToolCallProposal,
    credential: &CredentialValue,
) -> ToolCallProposal {
    ToolCallProposal {
        id: ToolCallId::new(redact_text(proposal.id.as_str().to_string(), credential)),
        name: ToolName::new(redact_text(proposal.name.as_str().to_string(), credential)),
        arguments_json: redact_json(proposal.arguments_json, credential),
    }
}

fn redact_json(raw: String, credential: &CredentialValue) -> String {
    let key = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    if key.is_empty() {
        return raw;
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        // A partial or malformed JSON value can encode a credential or its
        // trailing prefix with escapes that literal replacement cannot see.
        // Reuse the streaming decoder, then fail closed on any held prefix;
        // other malformed provider bytes remain for typed decoding to judge.
        let (mut redacted, pending) = redact_json_stream_fragment(raw, key);
        if !pending.is_empty() {
            redacted.push_str("[redacted]");
        }
        return redacted;
    };
    redact_json_value(&mut value, key);
    value.to_string()
}

fn redact_json_value(value: &mut serde_json::Value, credential: &str) {
    match value {
        serde_json::Value::String(text) => {
            *text = text.replace(credential, "[redacted]");
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_value(value, credential);
            }
        }
        serde_json::Value::Object(object) => {
            let entries = std::mem::take(object);
            for (key, mut value) in entries {
                redact_json_value(&mut value, credential);
                object.insert(key.replace(credential, "[redacted]"), value);
            }
        }
        primitive if primitive.to_string().contains(credential) => {
            *primitive = serde_json::Value::String("[redacted]".to_string());
        }
        _ => {}
    }
}

fn json_escapes_decode_to_credential(raw: &str, credential: &str) -> bool {
    let mut decoded = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        let Some(escaped) = chars.next() else {
            decoded.push('\\');
            break;
        };
        match escaped {
            '"' | '\\' | '/' => decoded.push(escaped),
            'b' => decoded.push('\u{0008}'),
            'f' => decoded.push('\u{000c}'),
            'n' => decoded.push('\n'),
            'r' => decoded.push('\r'),
            't' => decoded.push('\t'),
            'u' => {
                let digits: String = chars.by_ref().take(4).collect();
                if digits.len() != 4 {
                    continue;
                }
                let Ok(first) = u16::from_str_radix(&digits, 16) else {
                    continue;
                };
                if (0xd800..=0xdbff).contains(&first) {
                    let mut pair = chars.clone();
                    if pair.next() != Some('\\') || pair.next() != Some('u') {
                        continue;
                    }
                    let low_digits: String = pair.by_ref().take(4).collect();
                    let Ok(second) = u16::from_str_radix(&low_digits, 16) else {
                        continue;
                    };
                    if !(0xdc00..=0xdfff).contains(&second) {
                        continue;
                    }
                    let scalar = 0x1_0000
                        + ((u32::from(first) - 0xd800) << 10)
                        + (u32::from(second) - 0xdc00);
                    if let Some(decoded_character) = char::from_u32(scalar) {
                        decoded.push(decoded_character);
                        chars = pair;
                    }
                } else if !(0xdc00..=0xdfff).contains(&first)
                    && let Some(decoded_character) = char::from_u32(u32::from(first))
                {
                    decoded.push(decoded_character);
                }
            }
            other => decoded.push(other),
        }
    }
    decoded.contains(credential)
}

fn redact_assistant_part(part: AssistantPart, credential: &CredentialValue) -> AssistantPart {
    match part {
        AssistantPart::Text(text) => AssistantPart::Text(redact_bounded_text(text, credential)),
        AssistantPart::Thinking { text, signature } => AssistantPart::Thinking {
            text: redact_bounded_text(text, credential),
            signature: signature.map(|value| redact_bounded_text(value, credential)),
        },
        AssistantPart::RedactedThinking { data } => AssistantPart::RedactedThinking {
            data: redact_bounded_text(data, credential),
        },
        AssistantPart::ToolCall(proposal) => {
            AssistantPart::ToolCall(redact_tool_proposal(proposal, credential))
        }
        AssistantPart::SuppressedToolCall(name) => AssistantPart::SuppressedToolCall(
            ToolName::new(redact_text(name.as_str().to_string(), credential)),
        ),
    }
}

fn lossy_truncated(body: &[u8], limit: Option<usize>) -> String {
    let text = String::from_utf8_lossy(body);
    let Some(limit) = limit else {
        return text.into_owned();
    };
    if text.len() <= limit {
        return text.into_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} … [truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use crate::{
        AssistantPart, BoundaryLossEvidence, CancellationConfirmedEvidence, CompletionEvidence,
        CompletionFinish, CredentialValue, ExchangeFacts, FinishReason, LossCause,
        NativeErrorFacts, Observation, ObservationFact, ObservationSink, ProvenUnsentEvidence,
        ProviderErrorEvidence, ProviderErrorKind, ProviderMessageId, ProviderReportedModel,
        ProviderRequestId, RefusalEvidence, StreamInterruption, TerminalEvidence, TokenUsage,
        ToolCallId, ToolCallProposal, ToolCallsAtLoss, ToolName, TransportFacts, UnsentCause,
    };

    use super::{
        CredentialRedactingSink, NATIVE_MESSAGE_TRUNCATION_SUFFIX,
        redact_evidence as redact_evidence_with_limit, redact_json, redact_json_stream_fragment,
        redact_native_message as redact_native_message_with_limit,
    };

    fn redact_evidence(evidence: TerminalEvidence, key: &CredentialValue) -> TerminalEvidence {
        redact_evidence_with_limit(evidence, key, None)
    }

    fn redact_native_message(text: String, key: &CredentialValue) -> String {
        redact_native_message_with_limit(text, key, None)
    }

    fn credential(value: &str) -> CredentialValue {
        CredentialValue::new(value.as_bytes().to_vec())
    }

    #[test]
    fn split_streamed_credentials_are_redacted_before_observation() {
        let key = credential("fixture_secret");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "fixture_".to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "secret".to_string(),
            },
        });
        sink.flush();
        drop(sink);

        assert_eq!(
            observed,
            vec![Observation {
                correlation: "call-1".to_string(),
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: "[redacted]".to_string(),
                },
            }]
        );
    }

    #[test]
    fn overlapping_credential_prefixes_stay_held_between_deltas() {
        let key = credential("aaaa");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "aaaaa".to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "aaab".to_string(),
            },
        });
        sink.flush();
        drop(sink);

        assert_eq!(
            observed[0].fact,
            ObservationFact::TextDelta {
                index: 0,
                text: "[redacted]".to_string()
            }
        );
        assert_eq!(
            observed[1].fact,
            ObservationFact::TextDelta {
                index: 0,
                text: "[redacted]b".to_string()
            }
        );
    }

    #[test]
    fn complete_self_overlapping_credentials_are_redacted_before_suffix_retention() {
        let key = credential("abcab");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "abcab".to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "!".to_string(),
            },
        });
        sink.flush();
        drop(sink);

        assert_eq!(
            observed[0].fact,
            ObservationFact::TextDelta {
                index: 0,
                text: "[redacted]".to_string()
            }
        );
        assert_eq!(
            observed[1].fact,
            ObservationFact::TextDelta {
                index: 0,
                text: "!".to_string()
            }
        );
    }

    #[test]
    fn json_escaped_credentials_are_redacted_from_tool_arguments() {
        let key = credential("key_loop");

        assert_eq!(
            redact_json(r#"{"token":"key_\u006coop"}"#.to_string(), &key),
            r#"{"token":"[redacted]"}"#
        );
        assert_eq!(redact_json(r#"{"city":"#.to_string(), &key), r#"{"city":"#);
    }

    #[test]
    fn numeric_json_credential_is_redacted() {
        let key = credential("23");

        assert_eq!(
            redact_json(r#"{"value":1234}"#.to_string(), &key),
            r#"{"value":"[redacted]"}"#
        );
    }

    #[test]
    fn boolean_json_credential_is_redacted() {
        let key = credential("true");

        assert_eq!(
            redact_json(r#"{"value":true}"#.to_string(), &key),
            r#"{"value":"[redacted]"}"#
        );
    }

    #[test]
    fn null_json_credential_is_redacted() {
        let key = credential("null");

        assert_eq!(
            redact_json(r#"{"value":null}"#.to_string(), &key),
            r#"{"value":"[redacted]"}"#
        );
    }

    #[test]
    fn json_redaction_traverses_the_deserialized_value() {
        let key = credential("key_loop");
        let raw = r#"{"token":"key_loop","id":184467440737095516160,"dup":1,"dup":2}"#;

        assert_eq!(
            redact_json(raw.to_string(), &key),
            r#"{"dup":2,"id":184467440737095516160,"token":"[redacted]"}"#
        );
    }

    #[test]
    fn provider_error_redaction_covers_every_provider_controlled_field() {
        let key = credential("key_loop");
        let evidence = TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange: ExchangeFacts {
                provider_request_id: Some(ProviderRequestId::new("request-key_loop")),
                http_status: Some(400),
                retry_after: None,
            },
            reported_model: Some(ProviderReportedModel::new("model-key_loop")),
            kind: ProviderErrorKind::InvalidRequest,
            non_acceptance_proven: false,
            native: NativeErrorFacts {
                error_token: Some("token-key_loop".to_string()),
                error_code: Some("code-key_loop".to_string()),
                message: Some(r#"{"detail":"key_\u006coop"}"#.to_string()),
            },
            usage: TokenUsage::unreported(),
        });

        let TerminalEvidence::ProviderError(error) = redact_evidence(evidence, &key) else {
            panic!("provider error remains provider error evidence");
        };
        assert_eq!(
            error.exchange.provider_request_id,
            Some(ProviderRequestId::new("request-[redacted]"))
        );
        assert_eq!(
            error.reported_model,
            Some(ProviderReportedModel::new("model-[redacted]"))
        );
        assert_eq!(
            error.native.error_token.as_deref(),
            Some("token-[redacted]")
        );
        assert_eq!(error.native.error_code.as_deref(), Some("code-[redacted]"));
        assert_eq!(
            error.native.message.as_deref(),
            Some(r#"{"detail":"[redacted]"}"#)
        );
    }

    #[test]
    fn cancellation_confirmation_redaction_covers_every_text_field() {
        let key = credential("key_loop");
        let evidence = TerminalEvidence::CancellationConfirmed(CancellationConfirmedEvidence {
            exchange: ExchangeFacts {
                provider_request_id: Some(ProviderRequestId::new("request-key_loop")),
                http_status: Some(200),
                retry_after: None,
            },
            reported_model: Some(ProviderReportedModel::new("model-key_loop")),
            native: NativeErrorFacts {
                error_token: Some("token-key_loop".to_string()),
                error_code: Some("code-key_loop".to_string()),
                message: Some(r#"{"message":"key_loop"}"#.to_string()),
            },
        });

        let TerminalEvidence::CancellationConfirmed(confirmed) = redact_evidence(evidence, &key)
        else {
            panic!("cancellation confirmation remains cancellation evidence");
        };
        assert_eq!(
            confirmed.exchange.provider_request_id,
            Some(ProviderRequestId::new("request-[redacted]"))
        );
        assert_eq!(
            confirmed.reported_model,
            Some(ProviderReportedModel::new("model-[redacted]"))
        );
        assert_eq!(
            confirmed.native.error_token.as_deref(),
            Some("token-[redacted]")
        );
        assert_eq!(
            confirmed.native.error_code.as_deref(),
            Some("code-[redacted]")
        );
        assert_eq!(
            confirmed.native.message.as_deref(),
            Some(r#"{"message":"[redacted]"}"#)
        );
    }

    #[test]
    fn proven_unsent_redaction_covers_both_transport_causes() {
        let key = credential("key_loop");
        let connect = TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
            cause: UnsentCause::ConnectFailed(TransportFacts::new("connect-key_loop")),
        });
        let incomplete = TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
            cause: UnsentCause::SendIncompleteProvenUnacceptable(TransportFacts::new(
                "send-key_loop",
            )),
        });

        assert_eq!(
            redact_evidence(connect, &key),
            TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                cause: UnsentCause::ConnectFailed(TransportFacts::new("connect-[redacted]")),
            })
        );
        assert_eq!(
            redact_evidence(incomplete, &key),
            TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                cause: UnsentCause::SendIncompleteProvenUnacceptable(TransportFacts::new(
                    "send-[redacted]",
                )),
            })
        );
    }

    #[test]
    fn boundary_loss_redaction_covers_exchange_finish_model_and_detail() {
        let key = credential("key_loop");
        let evidence = TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::ResponseUnintelligible {
                detail: "decode-key_loop".to_string(),
            },
            exchange: ExchangeFacts {
                provider_request_id: Some(ProviderRequestId::new("request-key_loop")),
                http_status: Some(200),
                retry_after: None,
            },
            reported_model: Some(ProviderReportedModel::new("model-key_loop")),
            finish_reported: Some(FinishReason::StopSequence {
                sequence: Some("stop-key_loop".to_string()),
            }),
            tool_calls: ToolCallsAtLoss::Opened,
            usage: TokenUsage::unreported(),
        });

        let TerminalEvidence::BoundaryLoss(loss) = redact_evidence(evidence, &key) else {
            panic!("boundary loss remains boundary-loss evidence");
        };
        assert_eq!(
            loss.exchange.provider_request_id,
            Some(ProviderRequestId::new("request-[redacted]"))
        );
        assert_eq!(
            loss.reported_model,
            Some(ProviderReportedModel::new("model-[redacted]"))
        );
        assert_eq!(
            loss.finish_reported,
            Some(FinishReason::StopSequence {
                sequence: Some("stop-[redacted]".to_string()),
            })
        );
        assert_eq!(
            loss.cause,
            LossCause::ResponseUnintelligible {
                detail: "decode-[redacted]".to_string(),
            }
        );
        // The tool fact carries no provider text, so redaction passes it
        // through rather than weakening it to `Unobserved`.
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Opened);
    }

    #[test]
    fn refusal_redaction_covers_identifiers_model_and_content() {
        let key = credential("key_loop");
        let evidence = TerminalEvidence::Refused(RefusalEvidence {
            exchange: ExchangeFacts {
                provider_request_id: Some(ProviderRequestId::new("request-key_loop")),
                http_status: Some(200),
                retry_after: None,
            },
            message_id: Some(ProviderMessageId::new("message-key_loop")),
            reported_model: Some(ProviderReportedModel::new("model-key_loop")),
            content: vec![AssistantPart::Thinking {
                text: "thought-key_loop".to_string(),
                signature: Some("signature-key_loop".to_string()),
            }],
            usage: TokenUsage::unreported(),
        });

        let TerminalEvidence::Refused(refusal) = redact_evidence(evidence, &key) else {
            panic!("refusal remains refusal evidence");
        };
        assert_eq!(
            refusal.exchange.provider_request_id,
            Some(ProviderRequestId::new("request-[redacted]"))
        );
        assert_eq!(
            refusal.message_id,
            Some(ProviderMessageId::new("message-[redacted]"))
        );
        assert_eq!(
            refusal.reported_model,
            Some(ProviderReportedModel::new("model-[redacted]"))
        );
        assert_eq!(
            refusal.content[0],
            AssistantPart::Thinking {
                text: "thought-[redacted]".to_string(),
                signature: Some("signature-[redacted]".to_string()),
            }
        );
        assert_eq!(refusal.usage, TokenUsage::unreported());
    }

    #[test]
    fn interrupted_stream_transport_detail_is_redacted() {
        let key = credential("key_loop");
        let evidence = TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::StreamEndedWithoutTerminalMarker {
                interruption: StreamInterruption::TransportFailure(TransportFacts::new(
                    "stream-key_loop",
                )),
            },
            exchange: ExchangeFacts::default(),
            reported_model: None,
            finish_reported: None,
            tool_calls: ToolCallsAtLoss::NoneOpened,
            usage: TokenUsage::unreported(),
        });

        let TerminalEvidence::BoundaryLoss(loss) = redact_evidence(evidence, &key) else {
            panic!("interruption remains boundary-loss evidence");
        };
        assert_eq!(
            loss.cause,
            LossCause::StreamEndedWithoutTerminalMarker {
                interruption: StreamInterruption::TransportFailure(TransportFacts::new(
                    "stream-[redacted]",
                )),
            }
        );
    }

    #[test]
    fn completion_redaction_covers_identifiers_model_and_stop_sequence() {
        let key = credential("key_loop");
        let evidence = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts {
                provider_request_id: Some(ProviderRequestId::new("request-key_loop")),
                http_status: Some(200),
                retry_after: None,
            },
            message_id: Some(ProviderMessageId::new("message-key_loop")),
            reported_model: Some(ProviderReportedModel::new("model-key_loop")),
            finish: CompletionFinish::StopSequence {
                sequence: Some("stop-key_loop".to_string()),
            },
            content: Vec::new(),
            usage: TokenUsage::unreported(),
        });

        let TerminalEvidence::Completed(completion) = redact_evidence(evidence, &key) else {
            panic!("completion remains completion evidence");
        };
        assert_eq!(
            completion.exchange.provider_request_id,
            Some(ProviderRequestId::new("request-[redacted]"))
        );
        assert_eq!(
            completion.message_id,
            Some(ProviderMessageId::new("message-[redacted]"))
        );
        assert_eq!(
            completion.reported_model,
            Some(ProviderReportedModel::new("model-[redacted]"))
        );
        assert_eq!(
            completion.finish,
            CompletionFinish::StopSequence {
                sequence: Some("stop-[redacted]".to_string()),
            }
        );
    }

    #[test]
    fn unrecognized_completion_finish_is_credential_sanitized() {
        let key = credential("key_loop");
        let evidence = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: None,
            finish: CompletionFinish::Unrecognized {
                provider_token: "echo-key_loop".to_string(),
            },
            content: Vec::new(),
            usage: TokenUsage::unreported(),
        });

        let TerminalEvidence::Completed(completion) = redact_evidence(evidence, &key) else {
            panic!("completion remains completion evidence");
        };
        assert_eq!(
            completion.finish,
            CompletionFinish::Unrecognized {
                provider_token: "echo-[redacted]".to_string()
            }
        );
    }

    #[test]
    fn stop_sequence_observation_is_credential_sanitized() {
        let key = credential("key_loop");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::FinishReported(FinishReason::StopSequence {
                sequence: Some("stop-key_loop".to_string()),
            }),
        });

        assert_eq!(
            observed[0].fact,
            ObservationFact::FinishReported(FinishReason::StopSequence {
                sequence: Some("stop-[redacted]".to_string()),
            })
        );
    }

    #[test]
    fn truncated_native_body_redacts_a_credential_prefix_at_the_cut() {
        let key = credential("key_loop");

        assert_eq!(
            redact_native_message("safe key_ … [truncated]".to_string(), &key),
            "safe [redacted] … [truncated]"
        );
    }

    #[test]
    fn json_escaped_credential_in_a_fallback_error_body_is_redacted() {
        let key = credential("key_loop");

        assert_eq!(
            redact_native_message(r#"{"message":"key_\u006coop"}"#.to_string(), &key),
            r#"{"message":"[redacted]"}"#
        );
    }

    #[test]
    fn fallback_error_body_is_sanitized_before_escape_splitting_truncation() {
        const PADDING_BEFORE_ESCAPED_CREDENTIAL_BYTES: usize = 1_987;
        const TRAILING_BODY_BYTES: usize = 200;
        let credential_text = "fixture_abcdefghijklmnopqrstuvwxyzZ";
        let key = credential(credential_text);
        let body = format!(
            r#"{{"padding":"{}","message":"fixture_abcdefghijklmnopqrstuvwxyz\u005a{}"}}"#,
            "x".repeat(PADDING_BEFORE_ESCAPED_CREDENTIAL_BYTES),
            "y".repeat(TRAILING_BODY_BYTES)
        );
        let configured_limit = body.find(r"\u").expect("fixture carries one JSON escape") + 2;
        assert!(body.as_bytes()[..configured_limit].ends_with(br"\u"));

        let message = redact_native_message_with_limit(body, &key, Some(configured_limit));

        assert!(message.contains("[redacted]"));
        assert!(message.ends_with(NATIVE_MESSAGE_TRUNCATION_SUFFIX));
        assert!(!message.contains(credential_text));
        assert!(!message.contains(r"fixture_abcdefghijklmnopqrstuvwxyz\u005a"));
    }

    #[test]
    fn provider_controlled_truncation_suffix_cannot_bypass_native_message_bound() {
        const RETAINED_BYTES_AFTER_CREDENTIAL: usize = 32;
        const PROVIDER_OVERFLOW_BYTES: usize = 200;
        const CONFIGURED_LIMIT: usize = 257;
        let credential_text = "fixture_provider_key";
        let key = credential(credential_text);
        let prefix_bytes =
            CONFIGURED_LIMIT - credential_text.len() - RETAINED_BYTES_AFTER_CREDENTIAL;
        let tail_bytes = RETAINED_BYTES_AFTER_CREDENTIAL + PROVIDER_OVERFLOW_BYTES;
        let body = format!(
            "{}{credential_text}{}{}",
            "x".repeat(prefix_bytes),
            "y".repeat(tail_bytes),
            NATIVE_MESSAGE_TRUNCATION_SUFFIX
        );

        let message = redact_native_message_with_limit(body, &key, Some(CONFIGURED_LIMIT));

        assert!(message.len() <= CONFIGURED_LIMIT + NATIVE_MESSAGE_TRUNCATION_SUFFIX.len());
        assert!(message.ends_with(NATIVE_MESSAGE_TRUNCATION_SUFFIX));
        assert!(!message.contains(credential_text));
    }

    #[test]
    fn unbounded_native_message_policy_preserves_detail() {
        let key = credential("fixture_secret");
        let body = "x".repeat(4_096);

        let message = redact_native_message_with_limit(body.clone(), &key, None);

        assert_eq!(message, body);
    }

    #[test]
    fn surrogate_pair_credential_in_a_malformed_fallback_body_is_redacted() {
        let key = credential("key_🔑");

        assert_eq!(
            redact_native_message(r#"gateway key_\ud83d\udd11"#.to_string(), &key),
            r#""[redacted]""#
        );
    }

    #[test]
    fn parallel_tool_argument_deltas_preserve_provider_arrival_order() {
        let key = credential("key_loop");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 1,
                fragment: r#"{"later":1}"#.to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"{"earlier":0}"#.to_string(),
            },
        });
        sink.flush();
        drop(sink);

        assert_eq!(
            observed[0].fact,
            ObservationFact::ToolArgumentsDelta {
                index: 1,
                fragment: r#"{"later":1}"#.to_string()
            }
        );
        assert_eq!(
            observed[1].fact,
            ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"{"earlier":0}"#.to_string()
            }
        );
    }

    #[test]
    fn final_content_cannot_reconstruct_a_credential_across_parts() {
        let key = credential("key_loop");
        let evidence = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: None,
            finish: CompletionFinish::EndTurn,
            content: vec![
                AssistantPart::Text("safe key_".to_string()),
                AssistantPart::Text("loop tail".to_string()),
            ],
            usage: TokenUsage::unreported(),
        });

        let TerminalEvidence::Completed(completion) = redact_evidence(evidence, &key) else {
            panic!("completion remains completion");
        };
        assert_eq!(
            completion.content,
            vec![
                AssistantPart::Text("safe [redacted]".to_string()),
                AssistantPart::Text("loop tail".to_string())
            ]
        );
    }

    #[test]
    fn malformed_tool_arguments_cannot_reconstruct_a_credential_across_parts() {
        let key = credential("key_loop");
        let evidence = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: None,
            finish: CompletionFinish::ToolUse,
            content: vec![
                AssistantPart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("call_1"),
                    name: ToolName::new("lookup"),
                    arguments_json: r#"safe key_\u006c"#.to_string(),
                }),
                AssistantPart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("call_2"),
                    name: ToolName::new("lookup"),
                    arguments_json: "oop".to_string(),
                }),
            ],
            usage: TokenUsage::unreported(),
        });

        let TerminalEvidence::Completed(completion) = redact_evidence(evidence, &key) else {
            panic!("completion remains completion");
        };
        assert_eq!(
            completion.content,
            vec![
                AssistantPart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("call_1"),
                    name: ToolName::new("lookup"),
                    arguments_json: "safe [redacted]".to_string()
                }),
                AssistantPart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("call_2"),
                    name: ToolName::new("lookup"),
                    arguments_json: "oop".to_string()
                }),
            ]
        );
    }

    #[test]
    fn buffered_prefix_is_redacted_before_metadata_and_cannot_join_a_later_tail() {
        let key = credential("key_loop");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "safe key_".to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::UsageReported(TokenUsage::unreported()),
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "loop".to_string(),
            },
        });
        sink.flush();
        drop(sink);

        assert_eq!(
            observed,
            vec![
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::TextDelta {
                        index: 0,
                        text: "safe ".to_string(),
                    },
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::TextDelta {
                        index: 0,
                        text: "[redacted]".to_string(),
                    },
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::UsageReported(TokenUsage::unreported()),
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::TextDelta {
                        index: 0,
                        text: "loop".to_string(),
                    },
                },
            ]
        );
    }

    #[test]
    fn complete_repeated_credential_is_redacted_before_a_prefix_tail() {
        let key = credential("aaaa");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "aaaaa".to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "x".to_string(),
            },
        });
        sink.flush();
        drop(sink);

        assert_eq!(
            observed,
            vec![
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::TextDelta {
                        index: 0,
                        text: "[redacted]".to_string(),
                    },
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::TextDelta {
                        index: 0,
                        text: "ax".to_string(),
                    },
                },
            ]
        );
    }

    #[test]
    fn json_escaped_credential_is_redacted_before_a_tool_delta_is_forwarded() {
        let key = credential("key_loop");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"{"token":"key_\u006coop"}"#.to_string(),
            },
        });
        sink.flush();
        drop(sink);

        assert_eq!(
            observed,
            vec![Observation {
                correlation: "call-1".to_string(),
                fact: ObservationFact::ToolArgumentsDelta {
                    index: 0,
                    fragment: r#"{"token":"[redacted]"}"#.to_string(),
                },
            }]
        );
    }

    #[test]
    fn pending_text_is_flushed_before_a_tool_delta_is_forwarded() {
        let key = credential("key_loop");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "safe key_".to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 1,
                fragment: "{}".to_string(),
            },
        });
        sink.flush();
        drop(sink);

        assert_eq!(
            observed,
            vec![
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::TextDelta {
                        index: 0,
                        text: "safe ".to_string(),
                    },
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::TextDelta {
                        index: 0,
                        text: "[redacted]".to_string(),
                    },
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::ToolArgumentsDelta {
                        index: 1,
                        fragment: "{}".to_string(),
                    },
                },
            ]
        );
    }

    #[test]
    fn pending_stream_text_is_flushed_before_a_different_field() {
        let key = credential("key_loop");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ThinkingDelta {
                index: 0,
                text: "k".to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 1,
                text: "later".to_string(),
            },
        });
        sink.flush();
        drop(sink);

        assert_eq!(
            observed,
            vec![
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::ThinkingDelta {
                        index: 0,
                        text: "[redacted]".to_string(),
                    },
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::TextDelta {
                        index: 1,
                        text: "later".to_string(),
                    },
                },
            ]
        );
    }

    #[test]
    fn streamed_argument_redaction_handles_many_matches_in_one_forward_pass() {
        let raw = r#"\u006b\u0065\u0079"#.repeat(512);

        let (emitted, pending) = redact_json_stream_fragment(raw, "key");

        assert_eq!(emitted, "[redacted]".repeat(512));
        assert!(pending.is_empty());
    }

    #[test]
    fn streamed_argument_redaction_retains_only_a_credential_sized_suffix() {
        let raw = format!("{}ke", "safe".repeat(1024 * 1024));

        let (emitted, pending) = redact_json_stream_fragment(raw, "key");

        assert_eq!(emitted.len(), 4 * 1024 * 1024);
        assert_eq!(pending, "ke");
    }

    /// INV-035: a credential the provider echoes back with ordinary JSON
    /// escapes is caught even when the arrival boundary falls inside the
    /// escape itself, which is where `input_json_delta` is free to split.
    #[test]
    fn inv_035_simple_escaped_credential_split_mid_escape_is_redacted() {
        let key = credential("fixture/secret");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"{"path":"fixture\"#.to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"/secret"}"#.to_string(),
            },
        });
        sink.flush();
        drop(sink);

        // Checked against every emitted fragment first: a leak forwarded on
        // another correlation or tool index never reaches the projection below.
        assert_no_stream_carries(&observed, "fixture/secret");
        assert_no_stream_carries(&observed, "secret");
        let arguments = joined_arguments(&observed, "call-1", 0);
        assert_eq!(arguments, r#"{"path":"[redacted]"}"#);
    }

    /// Which of a correlation's parallel delta streams a fragment extends.
    ///
    /// Part of the reassembly key: two facts may share a correlation and an
    /// index yet belong to streams no consumer ever concatenates together.
    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    enum EmittedStream {
        Text,
        Thinking,
        ToolArguments,
    }

    /// How a consumer reads one emitted value on its way out.
    ///
    /// A JSON-bearing value is decoded before anyone sees it, so a credential
    /// spelled with `\uXXXX` escapes is recovered whole even though the raw
    /// bytes contain no literal secret. An absence check comparing only raw
    /// text would declare such a stream safe.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum TextEncoding {
        /// Reaches the consumer exactly as emitted.
        Plain,
        /// A JSON consumer decodes escapes before reading it.
        Json,
    }

    impl EmittedStream {
        /// How a consumer of this stream reads its reassembled text.
        const fn encoding(self) -> TextEncoding {
            match self {
                Self::Text | Self::Thinking => TextEncoding::Plain,
                Self::ToolArguments => TextEncoding::Json,
            }
        }
    }

    /// Where one emitted fact's provider-controlled text belongs.
    #[derive(Clone, Debug, Eq, PartialEq)]
    enum EmittedText<'a> {
        /// A fragment extending one identified delta stream.
        Fragment {
            stream: EmittedStream,
            index: u32,
            text: &'a str,
        },
        /// Complete values standing alone rather than extending a stream,
        /// each with the encoding its consumer reads it through.
        Complete(Vec<(&'a str, TextEncoding)>),
        /// Carries no provider-controlled text at all.
        Absent,
    }

    /// Classifies every provider-controlled string one fact carries.
    ///
    /// Deliberately mirrors [`redact_observation_fact`]: production decides
    /// which fields a credential can reach by redacting them, so a field it
    /// scrubs and this classifier reports as `Absent` is a field no INV-035
    /// case would ever inspect — deleting that production redaction would
    /// leave the suite green while the credential is emitted. The two matches
    /// are meant to name the same surface, and only `SendCommenced` and
    /// `UsageReported` carry nothing.
    ///
    /// Exhaustive over `ObservationFact` and over `FinishReason` so a new
    /// text-bearing shape cannot be added without deciding the question.
    fn emitted_text(fact: &ObservationFact) -> EmittedText<'_> {
        match fact {
            ObservationFact::TextDelta { index, text } => EmittedText::Fragment {
                stream: EmittedStream::Text,
                index: *index,
                text: text.as_str(),
            },
            ObservationFact::ThinkingDelta { index, text } => EmittedText::Fragment {
                stream: EmittedStream::Thinking,
                index: *index,
                text: text.as_str(),
            },
            ObservationFact::ToolArgumentsDelta { index, fragment } => EmittedText::Fragment {
                stream: EmittedStream::ToolArguments,
                index: *index,
                text: fragment.as_str(),
            },
            ObservationFact::ToolCallProposed(proposal) => EmittedText::Complete(vec![
                (proposal.id.as_str(), TextEncoding::Plain),
                (proposal.name.as_str(), TextEncoding::Plain),
                // The one JSON-bearing value here: a consumer decodes it.
                (proposal.arguments_json.as_str(), TextEncoding::Json),
            ]),
            ObservationFact::ProviderModelReported(model) => {
                EmittedText::Complete(vec![(model.as_str(), TextEncoding::Plain)])
            }
            ObservationFact::ExchangeEstablished(exchange) => EmittedText::Complete(
                exchange
                    .provider_request_id
                    .iter()
                    .map(|id| (id.as_str(), TextEncoding::Plain))
                    .collect(),
            ),
            ObservationFact::FinishReported(finish) => EmittedText::Complete(match finish {
                FinishReason::StopSequence { sequence } => sequence
                    .as_deref()
                    .map(|value| (value, TextEncoding::Plain))
                    .into_iter()
                    .collect(),
                FinishReason::Unrecognized { provider_token } => {
                    vec![(provider_token.as_str(), TextEncoding::Plain)]
                }
                FinishReason::EndTurn
                | FinishReason::MaxOutputTokens
                | FinishReason::ContextWindowExceeded
                | FinishReason::ToolUse
                | FinishReason::Refusal => Vec::new(),
            }),
            ObservationFact::SendCommenced | ObservationFact::UsageReported(_) => {
                EmittedText::Absent
            }
        }
    }

    /// Reassembles every emitted delta stream, keyed by the stream it extends.
    fn reconstructed_streams(
        observed: &[Observation<String>],
    ) -> BTreeMap<(String, EmittedStream, u32), String> {
        let mut streams: BTreeMap<(String, EmittedStream, u32), String> = BTreeMap::new();
        for observation in observed {
            if let EmittedText::Fragment {
                stream,
                index,
                text,
            } = emitted_text(&observation.fact)
            {
                streams
                    .entry((observation.correlation.clone(), stream, index))
                    .or_default()
                    .push_str(text);
            }
        }
        streams
    }

    /// Names every delta stream the sink emitted, in a stable order.
    fn emitted_stream_keys(observed: &[Observation<String>]) -> Vec<(String, EmittedStream, u32)> {
        reconstructed_streams(observed).into_keys().collect()
    }

    /// The credential a check searches for.
    ///
    /// A newtype because the subject beside it is also textual: transposing
    /// two bare `&str` arguments would compile and quietly make this
    /// classifier hunt emitted text for its own diagnostic label instead of
    /// the secret, which passes for every leak.
    #[derive(Clone, Copy, Debug)]
    struct Secret<'a>(&'a str);

    /// What is being inspected, named for the failure message.
    #[derive(Clone, Copy, Debug)]
    struct Subject<'a>(&'a str);

    /// Every string a JSON consumer could read out of `raw`, already decoded.
    ///
    /// Every quote is treated as a potential string start rather than trusting
    /// one left-to-right pairing: a stray quote in malformed text otherwise
    /// shifts the scan out of phase and hides an encoded credential behind it.
    /// Each candidate is still bounded by its own closing quote, so nothing is
    /// decoded across a boundary — unescaping the whole buffer would report
    /// `{"line\n":0}` as leaking the credential `line\n":0`, which no consumer
    /// recovers.
    ///
    /// Scanning string spans rather than parsing the enclosing value matters
    /// three further ways:
    ///
    /// - A streamed argument can end after a complete string but before its
    ///   document — `{"k":"\u0066ixture/secret"` with no closing brace. A
    ///   consumer reassembling the stream still recovers that string, and
    ///   `redact_json` treats partial JSON as credential-bearing for exactly
    ///   this reason, so requiring the whole value to parse would let an
    ///   escaped secret through.
    /// - Duplicate names survive. Deserializing `{"k":"…","k":"safe"}` into a
    ///   map drops the first member before anything can inspect it, hiding a
    ///   credential spelled in the shadowed value.
    /// - Only quoted spans are read, so bytes outside a string are never
    ///   unescaped: for the credential `line\n":0` and the document
    ///   `{"line\n":0}` a consumer reads the key `line\n` and the number `0`,
    ///   never the secret.
    ///
    /// The scan itself finds boundaries only. Every decode is `serde_json`'s,
    /// deliberately, so this stays an independent oracle rather than a second
    /// copy of the escape semantics under test.
    fn decoded_json_strings(raw: &str) -> BTreeSet<String> {
        raw.char_indices()
            .filter(|(_, unit)| *unit == '"')
            .map(|(at, _)| match completed_string_token(raw, at) {
                Some(after) => {
                    let token = &raw[at..after];
                    match serde_json::from_str::<String>(token) {
                        Ok(text) => text,
                        // A token `serde_json` rejects is not discarded. One
                        // invalid escape before an encoded credential would
                        // otherwise hide the whole token, and production
                        // decodes malformed fragments rather than trusting
                        // them.
                        Err(_) => decoded_escapes(&token[1..token.len() - 1]),
                    }
                }
                // A token the text never closes still carries content the
                // stream would recover once it continues, so its interior is
                // inspected rather than assumed safe.
                None => decoded_escapes(&raw[at + 1..]),
            })
            .collect()
    }

    /// The decoded contents of a span `serde_json` will not accept whole.
    ///
    /// Reached for the two shapes a strict token decode drops: a string the
    /// stream never closed, and one carrying an invalid escape before an
    /// encoded credential. Production redacts both — `redact_json_stream_fragment`
    /// decodes a partial or malformed fragment rather than trusting it, which
    /// I verified against the real sink — so treating either as safe here
    /// would leave a regression on those paths invisible.
    ///
    /// Every escape decision is still `serde_json`'s. This walks the span and
    /// offers each candidate escape to `serde_json` longest first, taking the
    /// first spelling it accepts; a spelling it rejects contributes its
    /// backslash literally and the scan continues past it, so one bad escape
    /// cannot mask what follows. Nothing here decides what an escape *means*,
    /// which is what keeps it from becoming a second copy of the semantics
    /// under test.
    fn decoded_escapes(content: &str) -> String {
        // Longest first: a surrogate pair spans twelve characters, a lone
        // `\uXXXX` six, and the single-character escapes two.
        const CANDIDATE_LENGTHS: [usize; 3] = [12, 6, 2];

        let units = content.chars().collect::<Vec<_>>();
        let mut decoded = String::with_capacity(content.len());
        let mut at = 0;
        while at < units.len() {
            if units[at] != '\\' {
                decoded.push(units[at]);
                at += 1;
                continue;
            }
            let accepted = CANDIDATE_LENGTHS.into_iter().find_map(|length| {
                let span = units.get(at..at + length)?.iter().collect::<String>();
                serde_json::from_str::<String>(&format!("\"{span}\""))
                    .ok()
                    .map(|text| (length, text))
            });
            match accepted {
                Some((length, text)) => {
                    decoded.push_str(&text);
                    at += length;
                }
                None => {
                    decoded.push('\\');
                    at += 1;
                }
            }
        }
        decoded
    }

    /// The index just past the string token opening at `from`, when it closes.
    ///
    /// Tracks backslash escaping only far enough to know which quote ends the
    /// token — an escaped quote does not — and reports `None` for a token the
    /// text never finishes.
    fn completed_string_token(raw: &str, from: usize) -> Option<usize> {
        let units = raw.as_bytes();
        let mut at = from + 1;
        while at < units.len() {
            match units[at] {
                b'\\' => at += 2,
                b'"' => return Some(at + 1),
                _ => at += 1,
            }
        }
        None
    }

    /// Asserts the credential is unrecoverable from `text` as its consumer
    /// reads it.
    #[track_caller]
    fn assert_text_hides(
        text: &str,
        encoding: TextEncoding,
        secret: Secret<'_>,
        subject: Subject<'_>,
    ) {
        let Secret(secret) = secret;
        let Subject(subject) = subject;
        assert!(
            !text.contains(secret),
            "the credential must not be recoverable; found it literally in {subject}: {text}"
        );
        // Exhaustive rather than an equality test: a new encoding must state
        // its observation semantics instead of defaulting to the raw-only
        // path, which would silently weaken this classifier.
        match encoding {
            TextEncoding::Plain => {}
            TextEncoding::Json => {
                for decoded in decoded_json_strings(text) {
                    assert!(
                        !decoded.contains(secret),
                        "the credential must not be recoverable; {subject} carries a string \
                         decoding to it: {decoded}"
                    );
                }
            }
        }
    }

    /// Asserts `secret` is unrecoverable from every stream the sink emitted.
    ///
    /// [`joined_arguments`] deliberately projects one correlation and one tool
    /// index, so a credential forwarded on a *different* correlation or index
    /// is invisible to it. Checking each observation on its own is not enough
    /// either: a leak split across two deltas — `fixture/sec` then `ret` — is
    /// recoverable by any consumer that concatenates the stream while no
    /// single fragment contains the credential. So every emitted fragment is
    /// grouped by the stream it extends and the absence check runs on each
    /// reconstruction, which is what a consumer actually sees.
    #[track_caller]
    fn assert_no_stream_carries(observed: &[Observation<String>], secret: &str) {
        for ((correlation, stream, index), reconstructed) in &reconstructed_streams(observed) {
            assert_text_hides(
                reconstructed,
                stream.encoding(),
                Secret(secret),
                Subject(&format!("stream {correlation} {stream:?} index {index}")),
            );
        }
        for observation in observed {
            if let EmittedText::Complete(values) = emitted_text(&observation.fact) {
                for (text, encoding) in values {
                    assert_text_hides(
                        text,
                        encoding,
                        Secret(secret),
                        Subject(&format!(
                            "a complete value on correlation {}",
                            observation.correlation
                        )),
                    );
                }
            }
        }
    }

    /// The reassembly is the only reason the check is stronger than a
    /// per-observation scan, and a helper carrying logic the INV-035 cases
    /// depend on is verified rather than assumed: a credential split across
    /// two deltas leaks recoverably even though neither fragment contains it,
    /// and the projection those cases use never reconstructs that stream.
    #[test]
    #[should_panic(expected = "must not be recoverable")]
    fn split_credential_on_an_uninspected_stream_is_caught() {
        let observed = vec![
            Observation {
                correlation: "call-9".to_string(),
                fact: ObservationFact::ToolArgumentsDelta {
                    index: 7,
                    fragment: "fixture/sec".to_string(),
                },
            },
            Observation {
                correlation: "call-9".to_string(),
                fact: ObservationFact::ToolArgumentsDelta {
                    index: 7,
                    fragment: "ret".to_string(),
                },
            },
        ];

        assert_no_stream_carries(&observed, "fixture/secret");
    }

    /// The classifier decides which text every later check inspects, so its
    /// three text-bearing shapes are pinned directly.
    #[test]
    fn emitted_text_classifies_each_bearing_fact() {
        assert_eq!(
            emitted_text(&ObservationFact::TextDelta {
                index: 3,
                text: String::from("answer"),
            }),
            EmittedText::Fragment {
                stream: EmittedStream::Text,
                index: 3,
                text: "answer",
            }
        );
        assert_eq!(
            emitted_text(&ObservationFact::ThinkingDelta {
                index: 4,
                text: String::from("reasoning"),
            }),
            EmittedText::Fragment {
                stream: EmittedStream::Thinking,
                index: 4,
                text: "reasoning",
            }
        );
        assert_eq!(
            emitted_text(&ObservationFact::ToolArgumentsDelta {
                index: 5,
                fragment: String::from("{\"a\":1}"),
            }),
            EmittedText::Fragment {
                stream: EmittedStream::ToolArguments,
                index: 5,
                text: "{\"a\":1}",
            }
        );
        // Every field production redacts is reported as inspectable text. A
        // field scrubbed there but `Absent` here is one no case would check.
        assert_eq!(
            emitted_text(&ObservationFact::ProviderModelReported(
                ProviderReportedModel::new("fixture-model")
            )),
            EmittedText::Complete(vec![("fixture-model", TextEncoding::Plain)])
        );
        assert_eq!(
            emitted_text(&ObservationFact::ExchangeEstablished(ExchangeFacts {
                provider_request_id: Some(ProviderRequestId::new("fixture-request")),
                http_status: Some(200),
                retry_after: None,
            })),
            EmittedText::Complete(vec![("fixture-request", TextEncoding::Plain)])
        );
        assert_eq!(
            emitted_text(&ObservationFact::FinishReported(
                FinishReason::Unrecognized {
                    provider_token: String::from("fixture-token"),
                }
            )),
            EmittedText::Complete(vec![("fixture-token", TextEncoding::Plain)])
        );
        assert_eq!(
            emitted_text(&ObservationFact::FinishReported(FinishReason::EndTurn)),
            EmittedText::Complete(Vec::new())
        );
        // Pinned rather than left to exhaustiveness: a match arm forces the
        // variant to be handled, not the right encoding to be chosen, and
        // flipping `arguments_json` to `Plain` would stop the check decoding
        // escaped credentials in complete proposals with nothing failing.
        assert_eq!(
            emitted_text(&ObservationFact::ToolCallProposed(ToolCallProposal {
                id: ToolCallId::new("id-1"),
                name: ToolName::new("tool"),
                arguments_json: String::from("{}"),
            })),
            EmittedText::Complete(vec![
                ("id-1", TextEncoding::Plain),
                ("tool", TextEncoding::Plain),
                ("{}", TextEncoding::Json),
            ])
        );
        assert_eq!(
            emitted_text(&ObservationFact::SendCommenced),
            EmittedText::Absent
        );
    }

    /// Facts sharing a correlation and index but not a kind stay distinct
    /// streams, which is what keeps the reassembly from inventing a leak by
    /// concatenating text a consumer never joins.
    #[test]
    fn same_index_on_different_kinds_reconstructs_separately() {
        let observed = vec![
            Observation {
                correlation: "call-1".to_string(),
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: String::from("fixture/sec"),
                },
            },
            Observation {
                correlation: "call-1".to_string(),
                fact: ObservationFact::ToolArgumentsDelta {
                    index: 0,
                    fragment: String::from("ret"),
                },
            },
        ];

        assert_eq!(
            emitted_stream_keys(&observed),
            vec![
                (String::from("call-1"), EmittedStream::Text, 0),
                (String::from("call-1"), EmittedStream::ToolArguments, 0),
            ]
        );
        assert_no_stream_carries(&observed, "fixture/secret");
    }

    /// Grouping is per stream, not one buffer: fragments that would spell the
    /// credential only if separate streams were concatenated are not a leak,
    /// because no consumer reassembles across them.
    #[test]
    fn fragments_spanning_separate_streams_are_not_a_leak() {
        let observed = vec![
            Observation {
                correlation: "call-1".to_string(),
                fact: ObservationFact::ToolArgumentsDelta {
                    index: 7,
                    fragment: "fixture/sec".to_string(),
                },
            },
            Observation {
                correlation: "call-1".to_string(),
                fact: ObservationFact::ToolArgumentsDelta {
                    index: 8,
                    fragment: "ret".to_string(),
                },
            },
        ];

        assert_no_stream_carries(&observed, "fixture/secret");
    }

    /// The reconstructed arguments for one tool index, as a reader of the
    /// stream would see them.
    ///
    /// INV-035 constrains the *content* a consumer reassembles — safe bytes
    /// preserved, credential absent — not how the scrubber chops it into
    /// deltas. Asserting exact fragment boundaries would fail a
    /// behaviour-preserving change that buffered the safe prefix or coalesced
    /// it with the replacement, while producing identical secure output.
    fn joined_arguments(observed: &[Observation<String>], correlation: &str, index: u32) -> String {
        observed
            .iter()
            .filter_map(|observation| match &observation.fact {
                ObservationFact::ToolArgumentsDelta {
                    index: observed_index,
                    fragment,
                } if observation.correlation == correlation && *observed_index == index => {
                    Some(fragment.as_str())
                }
                _ => None,
            })
            .collect()
    }

    /// INV-035: the credential is scrubbed from the facts that carry a single
    /// provider-controlled value, not only from the delta streams.
    ///
    /// Each of these is redacted by `redact_observation_fact`, so each is a
    /// way a credential could reach a consumer; without a case that emits one
    /// carrying the credential, deleting that production redaction would leave
    /// this suite green.
    #[test]
    fn inv_035_single_value_facts_are_credential_scrubbed() {
        let key = credential("fixture/secret");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new(
                "model-fixture/secret-v1",
            )),
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ExchangeEstablished(ExchangeFacts {
                provider_request_id: Some(ProviderRequestId::new("req-fixture/secret")),
                http_status: Some(200),
                retry_after: None,
            }),
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::FinishReported(FinishReason::Unrecognized {
                provider_token: String::from("stop-fixture/secret"),
            }),
        });
        sink.flush();
        drop(sink);

        // Pinned exactly rather than by credential absence and a count. Those
        // two hold just as well when redaction replaces the *whole* value, so
        // a regression that scrubbed `model-` and `-v1` away with the
        // credential would satisfy them while losing the safe bytes INV-035
        // preserves. Comparing the forwarded facts states both halves at once:
        // the credential is gone, the surrounding bytes and the non-credential
        // `http_status` are untouched, and each variant is still itself.
        assert_eq!(
            observed,
            vec![
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new(
                        "model-[redacted]-v1"
                    )),
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::ExchangeEstablished(ExchangeFacts {
                        provider_request_id: Some(ProviderRequestId::new("req-[redacted]")),
                        http_status: Some(200),
                        retry_after: None,
                    }),
                },
                Observation {
                    correlation: "call-1".to_string(),
                    fact: ObservationFact::FinishReported(FinishReason::Unrecognized {
                        provider_token: String::from("stop-[redacted]"),
                    }),
                },
            ]
        );
    }

    /// One way a provider could disguise the fixture credential in JSON.
    ///
    /// Named fields rather than a positional pair: both members are `&str`,
    /// so a transposed tuple would compile and quietly swap a test's subject
    /// for its own description.
    struct DisguisedSpelling {
        /// What the disguise is, for failure messages.
        label: &'static str,
        /// The credential as the provider would write it inside JSON.
        spelling: &'static str,
    }

    /// Representative disguised spellings of the fixture credential.
    ///
    /// Deliberately *not* claimed as exhaustive: each character may
    /// independently be literal or `\uXXXX`, `\/` composes with any subset of
    /// those, and hex digits admit case variants, so the full set is
    /// combinatorial. These five are the shapes a provider realistically
    /// emits, chosen to cover whole-value escaping, single-character escaping
    /// at the head and tail, the escaped solidus, and hex case. Each decodes
    /// to `fixture/secret`; all are synthetic.
    const REPRESENTATIVE_DISGUISES: [DisguisedSpelling; 5] = [
        DisguisedSpelling {
            label: "fully escaped",
            spelling: r"\u0066\u0069\u0078\u0074\u0075\u0072\u0065\u002f\u0073\u0065\u0063\u0072\u0065\u0074",
        },
        DisguisedSpelling {
            label: "escaped solidus",
            spelling: r"fixture\/secret",
        },
        DisguisedSpelling {
            label: "partial escape",
            spelling: r"\u0066ixture/secret",
        },
        DisguisedSpelling {
            label: "uppercase hex",
            spelling: r"fixture\u002Fsecret",
        },
        DisguisedSpelling {
            label: "escaped tail",
            spelling: r"fixture/sec\u0072\u0065\u0074",
        },
    ];

    /// Wraps one disguised spelling in the JSON a provider would emit.
    fn disguised_document(disguise: &DisguisedSpelling) -> String {
        format!(r#"{{"k":"{}"}}"#, disguise.spelling)
    }

    /// A JSON consumer recovers the credential from every representative
    /// disguise, so the absence check sees what that consumer would.
    ///
    /// Straight-line per spelling, and destructured by position so each name
    /// binds the disguise it claims: a partial slice pattern would silently
    /// bind the last entry to every name.
    #[test]
    fn every_representative_disguise_decodes_to_the_credential() {
        let [fully, solidus, partial, uppercase, tail] = &REPRESENTATIVE_DISGUISES;

        assert!(
            decoded_json_strings(&disguised_document(fully)).contains("fixture/secret"),
            "{}",
            fully.label
        );
        assert!(
            decoded_json_strings(&disguised_document(solidus)).contains("fixture/secret"),
            "{}",
            solidus.label
        );
        assert!(
            decoded_json_strings(&disguised_document(partial)).contains("fixture/secret"),
            "{}",
            partial.label
        );
        assert!(
            decoded_json_strings(&disguised_document(uppercase)).contains("fixture/secret"),
            "{}",
            uppercase.label
        );
        assert!(
            decoded_json_strings(&disguised_document(tail)).contains("fixture/secret"),
            "{}",
            tail.label
        );
    }

    /// Only string spans are decoded, so bytes outside one never become a leak
    /// no consumer can recover.
    #[test]
    fn only_json_string_spans_are_decoded() {
        assert_eq!(
            decoded_json_strings(r#"{"line\n":0}"#),
            BTreeSet::from([String::from("line\n"), String::from(":0}")])
        );
    }

    /// Text carrying no string span yields nothing.
    #[test]
    fn text_without_a_span_yields_nothing() {
        assert_eq!(decoded_json_strings("plain text"), BTreeSet::new());
    }

    /// A span the stream never closed is still inspected: its contents are
    /// what a consumer recovers once the stream continues, and production
    /// redacts such a fragment rather than trusting it.
    #[test]
    fn an_unfinished_span_is_still_inspected() {
        let [_, _, partial, ..] = &REPRESENTATIVE_DISGUISES;

        assert!(
            decoded_json_strings(&format!(r#"{{"k":"{}"#, partial.spelling))
                .contains("fixture/secret"),
            "{}",
            partial.label
        );
        assert!(decoded_json_strings(r#"{"k":"fixture"#).contains("fixture"));
    }

    /// One invalid escape does not hide the rest of its span.
    ///
    /// `serde_json` rejects `\q`, so a strict decode would drop the whole span
    /// and with it the encoded credential behind it.
    #[test]
    fn a_malformed_escape_does_not_hide_the_rest_of_a_span() {
        let [_, _, partial, ..] = &REPRESENTATIVE_DISGUISES;

        assert!(
            decoded_json_strings(&format!(r#"{{"k":"\q{}"}}"#, partial.spelling))
                .contains("\\qfixture/secret"),
            "{}",
            partial.label
        );
    }

    /// A stray quote does not shift the scan out of phase.
    ///
    /// Trusting one left-to-right pairing let `{"k":bad","j":"…"}` consume the
    /// real opening quote of `j` as a closing delimiter, so an encoded
    /// credential after it was never read.
    #[test]
    fn a_stray_quote_does_not_desynchronise_the_scan() {
        let [_, _, partial, ..] = &REPRESENTATIVE_DISGUISES;

        assert!(
            decoded_json_strings(&format!(r#"{{"k":bad","j":"{}"}}"#, partial.spelling))
                .contains("fixture/secret"),
            "{}",
            partial.label
        );
    }

    /// Every member is inspected, including one a map would drop.
    ///
    /// Deserializing `{"k":"…","k":"safe"}` keeps only the last `k`, so a
    /// credential spelled in the shadowed member would vanish before any check
    /// saw it.
    #[test]
    fn duplicate_members_are_all_inspected() {
        let [fully, ..] = &REPRESENTATIVE_DISGUISES;
        let shadowed = decoded_json_strings(&format!(r#"{{"k":"{}","k":"safe"}}"#, fully.spelling));

        assert!(shadowed.contains("fixture/secret"));
        assert!(shadowed.contains("safe"));
    }

    /// An escaped quote does not end a span, so the scan cannot be walked out
    /// of a string and made to miss what follows.
    #[test]
    fn an_escaped_quote_does_not_end_a_span() {
        let read = decoded_json_strings(r#"{"k":"a\"b","j":"fixture/secret"}"#);

        assert!(read.contains("a\"b"));
        assert!(read.contains("fixture/secret"));
    }

    /// A surrogate pair decodes to its single character, which is the case a
    /// credential containing an astral character depends on.
    #[test]
    fn surrogate_pairs_decode_to_one_character() {
        assert!(decoded_json_strings(r#"{"k":"\ud83d\ude00"}"#).contains("\u{1f600}"));
    }

    /// A raw comparison declares a disguised leak safe; the JSON-aware check
    /// does not. This is the regression the check exists to catch, pinned as a
    /// difference between the two rather than described in prose.
    #[test]
    fn a_raw_match_would_miss_what_the_json_aware_check_catches() {
        let [fully, ..] = &REPRESENTATIVE_DISGUISES;
        let leaked = disguised_document(fully);

        assert!(
            !leaked.contains("fixture/secret"),
            "the disguised spelling contains no literal credential, which is the point"
        );
        assert!(decoded_json_strings(&leaked).contains("fixture/secret"));
    }

    /// The best-effort decoder is what the two salvage paths rely on, so its
    /// own behaviour is pinned rather than inferred from its callers.
    ///
    /// Longest-first candidate selection is the load-bearing part: a
    /// surrogate pair must be offered whole, because each half alone is not a
    /// character `serde_json` will accept.
    #[test]
    fn decoded_escapes_delegates_each_spelling() {
        assert_eq!(decoded_escapes(r"\u0066ixture"), "fixture");
        assert_eq!(decoded_escapes(r"\ud83d\ude00"), "\u{1f600}");
        assert_eq!(decoded_escapes(r"a\/b"), "a/b");
        assert_eq!(decoded_escapes("plain"), "plain");
    }

    /// A spelling `serde_json` rejects contributes its backslash and nothing
    /// more, so one bad escape cannot mask what follows it.
    #[test]
    fn a_rejected_spelling_does_not_consume_what_follows() {
        assert_eq!(decoded_escapes(r"\q\u0066"), "\\qf");
        assert_eq!(decoded_escapes(r"\ud83d"), "\\ud83d");
    }

    /// INV-035: a disguised credential emitted on a stream the intended-stream
    /// assertion never reconstructs is still caught.
    #[test]
    #[should_panic(expected = "must not be recoverable")]
    fn disguised_credential_on_an_uninspected_stream_is_caught() {
        let [fully, ..] = &REPRESENTATIVE_DISGUISES;
        let observed = vec![Observation {
            correlation: "call-9".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 7,
                fragment: disguised_document(fully),
            },
        }];

        assert_no_stream_carries(&observed, "fixture/secret");
    }

    /// INV-035: a proposal's provider-controlled id and name are scrubbed
    /// alongside its arguments, pinned to their exact redacted values.
    ///
    /// `redact_tool_proposal` scrubs all three fields, but the sibling case
    /// below carries the credential only in `arguments_json`, so without this
    /// one nothing drives a credential through a proposal's `id` or `name`
    /// and a regression there would reach no assertion. Exact values rather
    /// than mere absence: replacing a whole field would satisfy an absence
    /// check while discarding the safe bytes around the credential.
    #[test]
    fn inv_035_proposed_identifiers_and_names_are_credential_scrubbed() {
        let key = credential("fixture/secret");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolCallProposed(ToolCallProposal {
                id: ToolCallId::new("id-fixture/secret-1"),
                name: ToolName::new("name-fixture/secret-tool"),
                arguments_json: r#"{"k":"fixture/secret"}"#.to_string(),
            }),
        });
        sink.flush();
        drop(sink);

        assert_eq!(
            observed,
            vec![Observation {
                correlation: "call-1".to_string(),
                fact: ObservationFact::ToolCallProposed(ToolCallProposal {
                    id: ToolCallId::new("id-[redacted]-1"),
                    name: ToolName::new("name-[redacted]-tool"),
                    arguments_json: r#"{"k":"[redacted]"}"#.to_string(),
                }),
            }]
        );
    }

    /// INV-035: a disguised credential in proposed arguments is scrubbed by
    /// the sink itself, pinned to the exact forwarded proposal.
    ///
    /// Driven through `CredentialRedactingSink` rather than starting from an
    /// already-leaked fact: a test that only feeds the absence helper proves
    /// the helper works and nothing about the production path, so a regression
    /// in `redact_tool_proposal` would not reach it.
    #[test]
    fn inv_035_disguised_credential_in_proposed_arguments_is_scrubbed() {
        let key = credential("fixture/secret");
        let [fully, ..] = &REPRESENTATIVE_DISGUISES;
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolCallProposed(ToolCallProposal {
                id: ToolCallId::new("id-1"),
                name: ToolName::new("t"),
                arguments_json: disguised_document(fully),
            }),
        });
        sink.flush();
        drop(sink);

        assert_eq!(
            observed,
            vec![Observation {
                correlation: "call-1".to_string(),
                fact: ObservationFact::ToolCallProposed(ToolCallProposal {
                    id: ToolCallId::new("id-1"),
                    name: ToolName::new("t"),
                    arguments_json: r#"{"k":"[redacted]"}"#.to_string(),
                }),
            }]
        );
        assert_no_stream_carries(&observed, "fixture/secret");
    }

    /// INV-035: the sink scrubs a disguised credential from a stream that
    /// stops before closing its document, pinned to the exact forwarded value.
    ///
    /// Driven through production rather than from an already-leaked fact: a
    /// case that only feeds the absence helper proves the helper works and
    /// nothing about `redact_json_stream_fragment`, so a regression requiring
    /// a complete document before redacting would not reach it.
    #[test]
    fn inv_035_sink_scrubs_a_disguise_in_an_unfinished_document() {
        let [fully, ..] = &REPRESENTATIVE_DISGUISES;

        assert_eq!(
            forwarded_arguments(&format!(r#"{{"k":"{}""#, fully.spelling)),
            r#"{"k":"[redacted]""#
        );
    }

    /// INV-035: the same holds for a token the stream never closed.
    #[test]
    fn inv_035_sink_scrubs_a_disguise_in_an_unclosed_token() {
        let [fully, ..] = &REPRESENTATIVE_DISGUISES;

        assert_eq!(
            forwarded_arguments(&format!(r#"{{"k":"{}"#, fully.spelling)),
            r#"{"k":"[redacted]"#
        );
    }

    /// INV-035: and for a disguise behind an invalid escape, which production
    /// redacts through its malformed-fragment path.
    #[test]
    fn inv_035_sink_scrubs_a_disguise_behind_a_malformed_escape() {
        let [_, _, partial, ..] = &REPRESENTATIVE_DISGUISES;

        assert_eq!(
            forwarded_arguments(&format!(r#"{{"k":"\q{}"}}"#, partial.spelling)),
            "[redacted]"
        );
    }

    /// The arguments the sink forwards for one tool-argument fragment.
    ///
    /// # Panics
    ///
    /// Panics unless the sink forwards exactly one tool-argument delta, which
    /// is what these fixtures emit.
    #[track_caller]
    fn forwarded_arguments(fragment: &str) -> String {
        let key = credential("fixture/secret");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: fragment.to_string(),
            },
        });
        sink.flush();
        drop(sink);
        assert_no_stream_carries(&observed, "fixture/secret");
        let [
            Observation {
                fact: ObservationFact::ToolArgumentsDelta { fragment, .. },
                ..
            },
        ] = observed.as_slice()
        else {
            panic!("the fixture forwards exactly one tool-argument delta, got {observed:?}")
        };
        fragment.clone()
    }

    /// INV-035: a disguised credential in a token the stream never closed is
    /// caught, the shape a truncated argument delta actually takes.
    #[test]
    #[should_panic(expected = "must not be recoverable")]
    fn disguised_credential_in_an_unclosed_token_is_caught() {
        let [fully, ..] = &REPRESENTATIVE_DISGUISES;
        let observed = vec![Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: format!(r#"{{"k":"{}"#, fully.spelling),
            },
        }];

        assert_no_stream_carries(&observed, "fixture/secret");
    }

    /// INV-035: an invalid escape ahead of a disguised credential does not
    /// hide it.
    #[test]
    #[should_panic(expected = "must not be recoverable")]
    fn disguised_credential_behind_a_malformed_escape_is_caught() {
        let [fully, ..] = &REPRESENTATIVE_DISGUISES;
        let observed = vec![Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: format!(r#"{{"k":"\q{}"}}"#, fully.spelling),
            },
        }];

        assert_no_stream_carries(&observed, "fixture/secret");
    }

    /// A plain-text stream is matched raw: escape-shaped bytes in assistant
    /// text are literal there, so decoding them would invent a leak.
    #[test]
    fn escape_shaped_plain_text_is_not_decoded_into_a_false_positive() {
        let [fully, ..] = &REPRESENTATIVE_DISGUISES;
        let observed = vec![Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: fully.spelling.to_string(),
            },
        }];

        assert_no_stream_carries(&observed, "fixture/secret");
    }

    /// INV-035: a credential spelled as a surrogate pair survives a boundary
    /// that falls between the pair's two halves.
    #[test]
    fn inv_035_surrogate_pair_credential_split_mid_escape_is_redacted() {
        let key = credential("key\u{1f600}loop");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"{"emoji":"key\ud83d\ud"#.to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"e00loop"}"#.to_string(),
            },
        });
        sink.flush();
        drop(sink);

        assert_no_stream_carries(&observed, "key\u{1f600}loop");
        assert_no_stream_carries(&observed, "loop");
        let arguments = joined_arguments(&observed, "call-1", 0);
        assert_eq!(arguments, r#"{"emoji":"[redacted]"}"#);
    }

    /// INV-035: a held credential prefix ending inside an escape is replaced
    /// rather than forwarded when another tool call's arguments arrive, so no
    /// later observation can reassemble it across the fact boundary.
    #[test]
    fn inv_035_held_partial_escape_is_flushed_closed_before_another_tool_index() {
        let key = credential("fixture/secret");
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"{"path":"fixture\"#.to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 1,
                fragment: r#"{"other":1}"#.to_string(),
            },
        });
        sink.flush();
        drop(sink);

        // The held prefix must be replaced rather than forwarded — on this
        // index, on the index that displaced it, or on any other stream.
        assert_no_stream_carries(&observed, "fixture");
        let held = joined_arguments(&observed, "call-1", 0);
        assert_eq!(held, r#"{"path":"[redacted]"#);
        // The other index is untouched, and — the point of this case — no
        // later observation can reassemble the credential across the boundary.
        assert_eq!(joined_arguments(&observed, "call-1", 1), r#"{"other":1}"#);
    }

    /// Escapes the scrubber decodes but never matches are re-emitted from the
    /// raw fragment, so tool arguments carrying a multi-line string or a
    /// quoted path reach the model byte for byte across a mid-escape split.
    #[test]
    fn streamed_tool_arguments_preserve_non_credential_escapes_byte_for_byte() {
        let key = credential("fixture_secret");
        // Split mid-escape: the scrubber must hold `\` and decide about it only
        // once the next chunk arrives.
        let first = r#"{"text":"first\"#;
        let second = r#"nsecond \"quoted\" \\ \t last"}"#;
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &key);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: first.to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: second.to_string(),
            },
        });
        sink.flush();
        drop(sink);

        // The claim is byte preservation of the reconstructed stream, not any
        // particular delta segmentation: provider chunk boundaries are
        // arbitrary, and an implementation that buffered or coalesced these
        // safe fragments would still be correct.
        assert_eq!(
            joined_arguments(&observed, "call-1", 0),
            format!("{first}{second}"),
            "non-credential escapes reach the model byte for byte"
        );
        // ...and none of it is smuggled onto another correlation or index:
        // exactly one stream was emitted, and it is this call's own.
        assert_eq!(
            emitted_stream_keys(&observed),
            vec![(String::from("call-1"), EmittedStream::ToolArguments, 0)],
            "the preserved bytes stay on their own correlation and index"
        );
    }

    #[test]
    fn native_error_code_is_credential_sanitized() {
        let key = credential("key_loop");
        let evidence = TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange: ExchangeFacts::default(),
            reported_model: None,
            kind: ProviderErrorKind::Unrecognized,
            non_acceptance_proven: false,
            native: NativeErrorFacts {
                error_token: None,
                error_code: Some("echo-key_loop".to_string()),
                message: None,
            },
            usage: TokenUsage::unreported(),
        });

        let TerminalEvidence::ProviderError(error) = redact_evidence(evidence, &key) else {
            panic!("provider error remains provider error");
        };
        assert_eq!(error.native.error_code.as_deref(), Some("echo-[redacted]"));
    }
}
