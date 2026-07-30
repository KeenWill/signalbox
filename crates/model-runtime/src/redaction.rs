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

const MAX_NATIVE_MESSAGE_BYTES: usize = 2_048;
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
pub fn redact_evidence(evidence: TerminalEvidence, api_key: &CredentialValue) -> TerminalEvidence {
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
            .map(|message| redact_native_message(message, api_key));
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

fn redact_native_message(text: String, credential: &CredentialValue) -> String {
    let redacted = if let Some(body) = text.strip_suffix(NATIVE_MESSAGE_TRUNCATION_SUFFIX) {
        let mut redacted = redact_native_body(body.to_string(), credential);
        redacted.push_str(NATIVE_MESSAGE_TRUNCATION_SUFFIX);
        redacted
    } else {
        redact_native_body(text, credential)
    };
    lossy_truncated(redacted.as_bytes())
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
    if serde_json::value::RawValue::from_string(raw.clone()).is_err() {
        // A partial or malformed JSON value can encode a credential or its
        // trailing prefix with escapes that literal replacement cannot see.
        // Reuse the streaming decoder, then fail closed on any held prefix;
        // other malformed provider bytes remain for typed decoding to judge.
        let (mut redacted, pending) = redact_json_stream_fragment(raw, key);
        if !pending.is_empty() {
            redacted.push_str("[redacted]");
        }
        return redacted;
    }

    let mut redacted = String::with_capacity(raw.len());
    let mut cursor = 0;
    while cursor < raw.len() {
        if raw.as_bytes()[cursor] == b'"' {
            let mut end = cursor + 1;
            let mut escaped = false;
            while end < raw.len() {
                let byte = raw.as_bytes()[end];
                end += 1;
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    break;
                }
            }
            let token = &raw[cursor..end];
            let Ok(decoded) = serde_json::from_str::<String>(token) else {
                return redact_text(raw, credential);
            };
            if decoded.contains(key) {
                let Ok(sanitized) = serde_json::to_string(&decoded.replace(key, "[redacted]"))
                else {
                    return "\"[redacted]\"".to_string();
                };
                redacted.push_str(&sanitized);
            } else {
                redacted.push_str(token);
            }
            cursor = end;
            continue;
        }

        if matches!(
            raw.as_bytes()[cursor],
            b'{' | b'}' | b'[' | b']' | b',' | b':'
        ) || raw.as_bytes()[cursor].is_ascii_whitespace()
        {
            redacted.push(raw.as_bytes()[cursor] as char);
            cursor += 1;
            continue;
        }

        let start = cursor;
        while cursor < raw.len()
            && !matches!(
                raw.as_bytes()[cursor],
                b'{' | b'}' | b'[' | b']' | b',' | b':' | b' ' | b'\t' | b'\r' | b'\n'
            )
        {
            cursor += 1;
        }
        let token = &raw[start..cursor];
        if token.contains(key) {
            redacted.push_str("\"[redacted]\"");
        } else {
            redacted.push_str(token);
        }
    }
    redacted
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
    }
}

fn lossy_truncated(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    if text.len() <= MAX_NATIVE_MESSAGE_BYTES {
        return text.into_owned();
    }
    let mut end = MAX_NATIVE_MESSAGE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} … [truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use crate::{
        AssistantPart, BoundaryLossEvidence, CancellationConfirmedEvidence, CompletionEvidence,
        CompletionFinish, CredentialValue, ExchangeFacts, FinishReason, LossCause,
        NativeErrorFacts, Observation, ObservationFact, ObservationSink, ProvenUnsentEvidence,
        ProviderErrorEvidence, ProviderErrorKind, ProviderMessageId, ProviderReportedModel,
        ProviderRequestId, RefusalEvidence, StreamInterruption, TerminalEvidence, TokenUsage,
        ToolCallId, ToolCallProposal, ToolName, TransportFacts, UnsentCause,
    };

    use super::{
        CredentialRedactingSink, MAX_NATIVE_MESSAGE_BYTES, NATIVE_MESSAGE_TRUNCATION_SUFFIX,
        redact_evidence, redact_json, redact_json_stream_fragment, redact_native_message,
    };

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
    fn json_redaction_preserves_untouched_raw_lexemes_and_duplicate_keys() {
        let key = credential("key_loop");
        let raw = r#"{"token":"key_loop","id":184467440737095516160,"dup":1,"dup":2}"#;

        assert_eq!(
            redact_json(raw.to_string(), &key),
            r#"{"token":"[redacted]","id":184467440737095516160,"dup":1,"dup":2}"#
        );
    }

    #[test]
    fn provider_error_redaction_covers_every_provider_controlled_field() {
        let key = credential("key_loop");
        let evidence = TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange: ExchangeFacts {
                provider_request_id: Some(ProviderRequestId::new("request-key_loop")),
                http_status: Some(400),
            },
            reported_model: Some(ProviderReportedModel::new("model-key_loop")),
            kind: ProviderErrorKind::InvalidRequest,
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
            },
            reported_model: Some(ProviderReportedModel::new("model-key_loop")),
            finish_reported: Some(FinishReason::StopSequence {
                sequence: Some("stop-key_loop".to_string()),
            }),
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
    }

    #[test]
    fn refusal_redaction_covers_identifiers_model_and_content() {
        let key = credential("key_loop");
        let evidence = TerminalEvidence::Refused(RefusalEvidence {
            exchange: ExchangeFacts {
                provider_request_id: Some(ProviderRequestId::new("request-key_loop")),
                http_status: Some(200),
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
        assert!(body.as_bytes()[..MAX_NATIVE_MESSAGE_BYTES].ends_with(br"\u"));

        let message = redact_native_message(body, &key);

        assert!(message.contains("[redacted]"));
        assert!(message.ends_with(NATIVE_MESSAGE_TRUNCATION_SUFFIX));
        assert!(!message.contains(credential_text));
        assert!(!message.contains(r"fixture_abcdefghijklmnopqrstuvwxyz\u005a"));
    }

    #[test]
    fn provider_controlled_truncation_suffix_cannot_bypass_native_message_bound() {
        const RETAINED_BYTES_AFTER_CREDENTIAL: usize = 32;
        const PROVIDER_OVERFLOW_BYTES: usize = 200;
        let credential_text = "fixture_provider_key";
        let key = credential(credential_text);
        let prefix_bytes =
            MAX_NATIVE_MESSAGE_BYTES - credential_text.len() - RETAINED_BYTES_AFTER_CREDENTIAL;
        let tail_bytes = RETAINED_BYTES_AFTER_CREDENTIAL + PROVIDER_OVERFLOW_BYTES;
        let body = format!(
            "{}{credential_text}{}{}",
            "x".repeat(prefix_bytes),
            "y".repeat(tail_bytes),
            NATIVE_MESSAGE_TRUNCATION_SUFFIX
        );

        let message = redact_native_message(body, &key);

        assert!(message.len() <= MAX_NATIVE_MESSAGE_BYTES + NATIVE_MESSAGE_TRUNCATION_SUFFIX.len());
        assert!(message.ends_with(NATIVE_MESSAGE_TRUNCATION_SUFFIX));
        assert!(!message.contains(credential_text));
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

    #[test]
    fn native_error_code_is_credential_sanitized() {
        let key = credential("key_loop");
        let evidence = TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange: ExchangeFacts::default(),
            reported_model: None,
            kind: ProviderErrorKind::Unrecognized,
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
