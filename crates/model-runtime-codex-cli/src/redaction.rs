//! Credential-shape redaction for CLI-controlled output.

use serde_json::Value;
use signalbox_model_runtime::{Observation, ObservationFact, ObservationSink};

const REDACTED: &str = "[redacted]";
const LINE_CREDENTIAL_MARKERS: &[&str] =
    &["authorization=", "authorization:", "cookie=", "cookie:"];
const VALUE_CREDENTIAL_MARKERS: &[&str] = &[
    "bearer ",
    "\"authorization\":",
    "api_key=",
    "api-key=",
    "api_key:",
    "api-key:",
    "\"api_key\":",
    "\"api-key\":",
    "\"apiKey\":",
    "access_token=",
    "access_token:",
    "\"access_token\":",
    "\"accessToken\":",
    "refresh_token=",
    "refresh_token:",
    "\"refresh_token\":",
    "\"refreshToken\":",
    "password=",
    "password:",
    "\"password\":",
    "secret=",
    "secret:",
    "\"secret\":",
    "credential=",
    "credential:",
    "\"credential\":",
    "\"cookie\":",
    "id_token=",
    "id_token:",
    "\"id_token\":",
    "session_token=",
    "session_token:",
    "\"session_token\":",
];
const TOKEN_PREFIXES: &[&str] = &["sk-", "eyJ"];

pub(crate) fn redact_text(text: &str) -> String {
    let mut sanitized = text.to_string();
    for marker in LINE_CREDENTIAL_MARKERS {
        sanitized = redact_line_value(&sanitized, marker);
    }
    for marker in VALUE_CREDENTIAL_MARKERS {
        sanitized = redact_after_marker(&sanitized, marker);
    }
    for prefix in TOKEN_PREFIXES {
        sanitized = redact_prefixed_token(&sanitized, prefix);
    }
    sanitized
}

pub(crate) fn redact_json(raw: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(raw) else {
        return redact_text(raw);
    };
    let changed = redact_value(&mut value);
    if !changed {
        return raw.to_string();
    }
    serde_json::to_string(&value).unwrap_or_else(|_| REDACTED.to_string())
}

fn redact_value(value: &mut Value) -> bool {
    match value {
        Value::Object(entries) => {
            let mut changed = false;
            for (key, child) in entries {
                if credential_key(key) {
                    *child = Value::String(REDACTED.to_string());
                    changed = true;
                } else {
                    changed |= redact_value(child);
                }
            }
            changed
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= redact_value(item);
            }
            changed
        }
        Value::String(text) => {
            let sanitized = redact_text(text);
            let changed = sanitized != *text;
            *text = sanitized;
            changed
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn credential_key(key: &str) -> bool {
    let key = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect::<String>();
    [
        "authorization",
        "apikey",
        "accesstoken",
        "refreshtoken",
        "idtoken",
        "sessiontoken",
        "credential",
        "password",
        "secret",
        "cookie",
    ]
    .iter()
    .any(|shape| key.contains(shape))
}

fn redact_after_marker(text: &str, marker: &str) -> String {
    let mut remaining = text;
    let mut output = String::with_capacity(text.len());
    while let Some(index) = find_ascii_case_insensitive(remaining, marker) {
        let value_start = index + marker.len();
        output.push_str(&remaining[..value_start]);
        let whitespace = remaining[value_start..]
            .chars()
            .take_while(|character| character.is_whitespace())
            .map(char::len_utf8)
            .sum::<usize>();
        output.push_str(&remaining[value_start..value_start + whitespace]);
        let opening_quote = remaining[value_start + whitespace..]
            .chars()
            .next()
            .filter(|character| matches!(character, '"' | '\''));
        if opening_quote.is_some() {
            output.push_str(&remaining[value_start + whitespace..value_start + whitespace + 1]);
        }
        output.push_str(REDACTED);
        let token_start = value_start + whitespace + usize::from(opening_quote.is_some());
        let value_end = opening_quote.map_or_else(
            || {
                remaining[token_start..]
                    .find(|character: char| {
                        character.is_whitespace()
                            || matches!(character, '"' | '\'' | ',' | '}' | ']' | ';')
                    })
                    .map_or(remaining.len(), |length| token_start + length)
            },
            |quote| quoted_value_end(remaining, token_start, quote),
        );
        remaining = &remaining[value_end..];
    }
    output.push_str(remaining);
    output
}

fn quoted_value_end(text: &str, value_start: usize, quote: char) -> usize {
    let mut escaped = false;
    for (offset, character) in text[value_start..].char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == quote {
            return value_start + offset;
        }
    }
    text.len()
}

fn redact_line_value(text: &str, marker: &str) -> String {
    let mut remaining = text;
    let mut output = String::with_capacity(text.len());
    while let Some(index) = find_ascii_case_insensitive(remaining, marker) {
        let value_start = index + marker.len();
        output.push_str(&remaining[..value_start]);
        let whitespace = remaining[value_start..]
            .chars()
            .take_while(|character| {
                character.is_whitespace() && *character != '\r' && *character != '\n'
            })
            .map(char::len_utf8)
            .sum::<usize>();
        output.push_str(&remaining[value_start..value_start + whitespace]);
        output.push_str(REDACTED);
        let value_end = remaining[value_start + whitespace..]
            .find(['\r', '\n'])
            .map_or(remaining.len(), |length| value_start + whitespace + length);
        remaining = &remaining[value_end..];
    }
    output.push_str(remaining);
    output
}

fn find_ascii_case_insensitive(text: &str, needle: &str) -> Option<usize> {
    text.as_bytes()
        .windows(needle.len())
        .position(|candidate| candidate.eq_ignore_ascii_case(needle.as_bytes()))
}

fn redact_prefixed_token(text: &str, prefix: &str) -> String {
    let mut remaining = text;
    let mut output = String::with_capacity(text.len());
    while let Some(index) = remaining.find(prefix) {
        output.push_str(&remaining[..index]);
        output.push_str(REDACTED);
        let token_end = remaining[index..]
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '"' | '\'' | ',' | '}' | ']' | ';')
            })
            .map_or(remaining.len(), |length| index + length);
        remaining = &remaining[token_end..];
    }
    output.push_str(remaining);
    output
}

#[derive(Clone, Copy, PartialEq, Eq)]
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

/// Holds a possible trailing credential-shape prefix between streamed facts.
///
/// A fact-boundary change forces the held prefix to cross the boundary as a
/// redaction marker, never as its provider-controlled bytes.
pub(crate) struct RedactingSink<'a, C> {
    inner: &'a mut (dyn ObservationSink<C> + Send),
    pending: Option<PendingStreamText<C>>,
}

impl<'a, C: Clone> RedactingSink<'a, C> {
    pub(crate) fn new(inner: &'a mut (dyn ObservationSink<C> + Send)) -> Self {
        Self {
            inner,
            pending: None,
        }
    }

    pub(crate) fn flush(&mut self) {
        if let Some(pending) = self.pending.take() {
            let (emitted, held) = redact_complete_shapes_and_hold_prefix(pending.text);
            self.emit(
                pending.field,
                pending.index,
                pending.correlation.clone(),
                emitted,
            );
            if !held.is_empty() {
                self.emit(
                    pending.field,
                    pending.index,
                    pending.correlation,
                    REDACTED.to_string(),
                );
            }
        }
    }

    fn flush_at_stream_end(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.emit(
                pending.field,
                pending.index,
                pending.correlation,
                redact_text(&pending.text),
            );
        }
    }

    fn emit(&mut self, field: StreamField, index: u32, correlation: C, text: String) {
        if text.is_empty() {
            return;
        }
        let fact = match field {
            StreamField::Text => ObservationFact::TextDelta { index, text },
            StreamField::Thinking => ObservationFact::ThinkingDelta { index, text },
        };
        self.inner.observe(Observation { correlation, fact });
    }

    fn redact_delta(&mut self, field: StreamField, index: u32, correlation: C, text: String) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.field != field || pending.index != index)
        {
            self.flush();
        }
        match &mut self.pending {
            Some(pending) => pending.text.push_str(&text),
            None => {
                self.pending = Some(PendingStreamText {
                    field,
                    index,
                    correlation,
                    text,
                });
            }
        }
    }
}

impl<C: Clone> ObservationSink<C> for RedactingSink<'_, C> {
    fn observe(&mut self, observation: Observation<C>) {
        match observation.fact {
            ObservationFact::TextDelta { index, text } => {
                self.redact_delta(StreamField::Text, index, observation.correlation, text);
            }
            ObservationFact::ThinkingDelta { index, text } => {
                self.redact_delta(StreamField::Thinking, index, observation.correlation, text);
            }
            ObservationFact::UsageReported(usage) => {
                // No later provider text can follow the completion's usage
                // barrier, so a held proper prefix is harmless and may be
                // emitted without destroying clean output.
                self.flush_at_stream_end();
                self.inner.observe(Observation {
                    correlation: observation.correlation,
                    fact: ObservationFact::UsageReported(usage),
                });
            }
            fact => {
                self.flush();
                self.inner.observe(Observation {
                    correlation: observation.correlation,
                    fact,
                });
            }
        }
    }
}

fn redact_complete_shapes_and_hold_prefix(text: String) -> (String, String) {
    let mut redacted = redact_text(&text);
    let held_length = longest_trailing_credential_prefix(&redacted);
    let pending = redacted.split_off(redacted.len() - held_length);
    (redacted, pending)
}

fn longest_trailing_credential_prefix(text: &str) -> usize {
    let mut longest = 0;
    for marker in LINE_CREDENTIAL_MARKERS
        .iter()
        .chain(VALUE_CREDENTIAL_MARKERS)
    {
        longest = longest.max(trailing_marker_prefix(text, marker, true));
    }
    for marker in TOKEN_PREFIXES {
        longest = longest.max(trailing_marker_prefix(text, marker, false));
    }
    longest
}

fn trailing_marker_prefix(text: &str, marker: &str, ascii_case_insensitive: bool) -> usize {
    let maximum = text.len().min(marker.len().saturating_sub(1));
    (1..=maximum)
        .rev()
        .find(|length| {
            let tail = &text.as_bytes()[text.len() - length..];
            let prefix = &marker.as_bytes()[..*length];
            if ascii_case_insensitive {
                tail.eq_ignore_ascii_case(prefix)
            } else {
                tail == prefix
            }
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{redact_complete_shapes_and_hold_prefix, redact_json, redact_text};

    const CREDENTIAL_SHAPED_VALUE: &str = "sk-sensitive-value";
    const QUOTED_CREDENTIAL_VALUE: &str = "another-sensitive-value";
    const JSON_CREDENTIAL_VALUE: &str = "third-sensitive-value";
    const NESTED_CREDENTIAL_VALUE: &str = "sensitive-refresh-value";
    const AUTHORIZATION_VALUE: &str = "opaque-authorization-value";
    const BASIC_AUTHORIZATION_VALUE: &str = "dXNlcjpwYXNz";
    const COOKIE_VALUE_ONE: &str = "sensitive-cookie-one";
    const COOKIE_VALUE_TWO: &str = "sensitive-cookie-two";
    const JWT_SHAPED_VALUE: &str = "eyJsensitive.jwt.value";
    const ID_TOKEN_VALUE: &str = "sensitive-id-token";
    const SESSION_TOKEN_VALUE: &str = "sensitive-session-token";
    const QUOTED_SECRET_VALUE: &str = "sensitive value with spaces";
    const ESCAPED_QUOTED_SECRET_VALUE: &str = r#"sensitive \"quoted\" value"#;
    const MULTILINE_SECRET_VALUE: &str = "sensitive\nmultiline\nvalue";

    /// INV-035: credential-shaped text never leaves the CLI adapter.
    #[test]
    fn inv_035_redacts_bearer_and_prefixed_credentials() {
        let fixture = format!(
            "Authorization BEARER {CREDENTIAL_SHAPED_VALUE} and \
             API_KEY=\"{QUOTED_CREDENTIAL_VALUE}\" with \
             {{\"REFRESH_TOKEN\":\"{JSON_CREDENTIAL_VALUE}\"}}, \
             Authorization: {AUTHORIZATION_VALUE}, and {JWT_SHAPED_VALUE}\n\
             Authorization: Basic {BASIC_AUTHORIZATION_VALUE}\n\
             Cookie: session={COOKIE_VALUE_ONE}; refresh={COOKIE_VALUE_TWO}\n\
             safe-after-headers"
        );
        let output = redact_text(&fixture);

        assert!(!output.contains(CREDENTIAL_SHAPED_VALUE));
        assert!(!output.contains(QUOTED_CREDENTIAL_VALUE));
        assert!(!output.contains(JSON_CREDENTIAL_VALUE));
        assert!(!output.contains(AUTHORIZATION_VALUE));
        assert!(!output.contains(BASIC_AUTHORIZATION_VALUE));
        assert!(!output.contains(COOKIE_VALUE_ONE));
        assert!(!output.contains(COOKIE_VALUE_TWO));
        assert!(!output.contains(JWT_SHAPED_VALUE));
        assert!(output.contains("[redacted]"));
        assert!(output.contains("safe-after-headers"));
    }

    /// INV-035: credential-shaped JSON members are redacted recursively.
    #[test]
    fn inv_035_redacts_nested_credential_members() {
        let fixture = format!(
            r#"{{"safe":"kept","nested":{{"API_KEY":"{NESTED_CREDENTIAL_VALUE}","apiKey":"{NESTED_CREDENTIAL_VALUE}","accessToken":"{NESTED_CREDENTIAL_VALUE}","refreshToken":"{NESTED_CREDENTIAL_VALUE}","id_token":"{ID_TOKEN_VALUE}","sessionToken":"{SESSION_TOKEN_VALUE}"}}}}"#
        );
        let output = redact_json(&fixture);

        assert!(!output.contains(NESTED_CREDENTIAL_VALUE));
        assert!(!output.contains(ID_TOKEN_VALUE));
        assert!(!output.contains(SESSION_TOKEN_VALUE));
        assert!(output.contains(r#""safe":"kept""#));
    }

    /// INV-035: quoted credential-shaped values are removed as one value.
    #[test]
    fn inv_035_redacts_complete_quoted_values() {
        let fixture = format!(
            "secret: \"{QUOTED_SECRET_VALUE}\"\n\
             password: \"{ESCAPED_QUOTED_SECRET_VALUE}\"\n\
             credential: \"{MULTILINE_SECRET_VALUE}\"\n\
             safe-after-secrets"
        );
        let output = redact_text(&fixture);

        assert!(!output.contains(QUOTED_SECRET_VALUE));
        assert!(!output.contains(ESCAPED_QUOTED_SECRET_VALUE));
        assert!(!output.contains(MULTILINE_SECRET_VALUE));
        assert!(output.contains("safe-after-secrets"));
    }

    #[test]
    fn harmless_tool_arguments_remain_byte_exact() {
        let input = r#"{ "city" : "Oslo", "limit": 3 }"#;

        assert_eq!(redact_json(input), input);
    }

    #[test]
    fn inv_035_stream_redaction_holds_a_split_token_prefix() {
        let (emitted, pending) = redact_complete_shapes_and_hold_prefix("safe s".to_string());

        assert_eq!(emitted, "safe ");
        assert_eq!(pending, "s");
    }

    #[test]
    fn inv_035_stream_redaction_completes_and_redacts_a_held_token() {
        let (emitted, pending) = redact_complete_shapes_and_hold_prefix("sk-sensitive".to_string());

        assert_eq!(emitted, "[redacted]");
        assert_eq!(pending, "");
    }
}
