//! Credential-shape redaction for CLI-controlled output.

use serde_json::Value;
use signalbox_model_runtime::{Observation, ObservationFact, ObservationSink};

const REDACTED: &str = "[redacted]";
const MAX_PENDING_STREAM_BYTES: usize = 64 * 1024;
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
    let mut sanitized = redact_json_credential_values(text);
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

fn redact_json_credential_values(text: &str) -> String {
    let mut remaining = text;
    let mut output = String::with_capacity(text.len());
    while let Some((_key_start, value_start)) = next_json_credential_value(remaining) {
        output.push_str(&remaining[..value_start]);
        let (prefix, token_start, value_end) =
            credential_value_bounds(remaining, value_start, ValueTermination::Token);
        output.push_str(prefix);
        output.push_str(REDACTED);
        let consumed = value_end.max(token_start);
        if let Some(quote @ (b'"' | b'\'')) = prefix.as_bytes().last()
            && remaining.as_bytes().get(consumed) == Some(quote)
        {
            output.push(char::from(*quote));
            remaining = &remaining[consumed + 1..];
        } else {
            remaining = &remaining[consumed..];
        }
    }
    output.push_str(remaining);
    output
}

fn next_json_credential_value(text: &str) -> Option<(usize, usize)> {
    let mut offset = 0;
    while let Some(relative_start) = text[offset..].find('"') {
        let key_start = offset + relative_start;
        if !json_key_can_start_at(text, key_start) {
            offset = key_start + 1;
            continue;
        }
        let key_end = quoted_value_end(text, key_start + 1, '"');
        if key_end == text.len() {
            return None;
        }
        let encoded_key = &text[key_start..=key_end];
        let Ok(key) = serde_json::from_str::<String>(encoded_key) else {
            offset = key_end + 1;
            continue;
        };
        let whitespace_end = key_end
            + 1
            + text[key_end + 1..]
                .chars()
                .take_while(|character| character.is_whitespace())
                .map(char::len_utf8)
                .sum::<usize>();
        if text.as_bytes().get(whitespace_end) == Some(&b':') && credential_key(&key) {
            return Some((key_start, whitespace_end + 1));
        }
        offset = key_end + 1;
    }
    None
}

fn json_key_can_start_at(text: &str, key_start: usize) -> bool {
    text[..key_start]
        .chars()
        .rev()
        .find(|character| !character.is_whitespace())
        .is_none_or(|character| matches!(character, '{' | ','))
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
        let (prefix, token_start, value_end) =
            credential_value_bounds(remaining, value_start, ValueTermination::Token);
        output.push_str(prefix);
        output.push_str(REDACTED);
        remaining = &remaining[value_end.max(token_start)..];
    }
    output.push_str(remaining);
    output
}

#[derive(Clone, Copy)]
enum ValueTermination {
    Line,
    Token,
}

fn credential_value_bounds(
    text: &str,
    value_start: usize,
    termination: ValueTermination,
) -> (&str, usize, usize) {
    let whitespace = text[value_start..]
        .chars()
        .take_while(|character| {
            character.is_whitespace()
                && (!matches!(termination, ValueTermination::Line)
                    || !matches!(character, '\r' | '\n'))
        })
        .map(char::len_utf8)
        .sum::<usize>();
    let opening_quote = text[value_start + whitespace..]
        .chars()
        .next()
        .filter(|character| matches!(character, '"' | '\''));
    let prefix_end = value_start + whitespace + usize::from(opening_quote.is_some());
    let value_end = opening_quote.map_or_else(
        || match termination {
            ValueTermination::Line => text[prefix_end..]
                .find(['\r', '\n'])
                .map_or(text.len(), |length| prefix_end + length),
            ValueTermination::Token => text[prefix_end..]
                .find(|character: char| {
                    character.is_whitespace()
                        || matches!(character, '"' | '\'' | ',' | '}' | ']' | ';')
                })
                .map_or(text.len(), |length| prefix_end + length),
        },
        |quote| quoted_value_end(text, prefix_end, quote),
    );
    (&text[value_start..prefix_end], prefix_end, value_end)
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
        let (prefix, token_start, value_end) =
            credential_value_bounds(remaining, value_start, ValueTermination::Line);
        output.push_str(prefix);
        output.push_str(REDACTED);
        remaining = &remaining[value_end.max(token_start)..];
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

struct StreamFragment<C> {
    field: StreamField,
    index: u32,
    correlation: C,
    text: String,
}

struct PendingStreamText<C> {
    fragments: Vec<StreamFragment<C>>,
    text: String,
}

/// Holds an incomplete credential shape between streamed facts.
pub(crate) struct RedactingSink<'a, C> {
    inner: &'a mut (dyn ObservationSink<C> + Send),
    pending: Option<PendingStreamText<C>>,
    suppressing: bool,
    terminal_text_capture: Option<String>,
}

impl<'a, C: Clone> RedactingSink<'a, C> {
    pub(crate) fn new(inner: &'a mut (dyn ObservationSink<C> + Send)) -> Self {
        Self {
            inner,
            pending: None,
            suppressing: false,
            terminal_text_capture: None,
        }
    }

    /// Starts recording every emitted final-text byte, so terminal evidence
    /// can carry exactly the stateful cross-fragment redaction the streamed
    /// deltas received instead of a stateless re-redaction of the raw text.
    pub(crate) fn begin_terminal_text_capture(&mut self) {
        self.terminal_text_capture = Some(String::new());
    }

    pub(crate) fn take_terminal_text_capture(&mut self) -> String {
        self.terminal_text_capture.take().unwrap_or_default()
    }

    fn flush_boundary(&mut self) {
        if let Some(pending) = self.pending.take() {
            if stream_candidate_starts_at_zero(&pending.text) {
                self.emit_redacted(pending.fragments);
            } else if let Some(unsafe_start) = unsafe_stream_suffix_start(&pending.text) {
                let (safe, unsafe_fragments) =
                    split_stream_fragments(pending.fragments, unsafe_start);
                self.emit_original(safe);
                self.emit_redacted(unsafe_fragments);
            } else {
                self.emit_original(pending.fragments);
            }
        }
    }

    /// Flushes already-decoded text when no later provider text can extend it.
    pub(crate) fn finish(&mut self) {
        self.suppressing = false;
        if let Some(pending) = self.pending.take() {
            if redact_text(&pending.text) == pending.text {
                self.emit_original(pending.fragments);
            } else {
                self.emit_redacted(pending.fragments);
            }
        }
    }

    fn emit_original(&mut self, fragments: Vec<StreamFragment<C>>) {
        for fragment in fragments {
            self.emit(
                fragment.field,
                fragment.index,
                fragment.correlation,
                redact_text(&fragment.text),
            );
        }
    }

    fn emit_redacted(&mut self, fragments: Vec<StreamFragment<C>>) {
        for fragment in fragments {
            self.emit(
                fragment.field,
                fragment.index,
                fragment.correlation,
                REDACTED.to_string(),
            );
        }
    }

    fn hold_or_suppress(&mut self, pending: PendingStreamText<C>) {
        if pending.text.len() > MAX_PENDING_STREAM_BYTES {
            self.emit_redacted(pending.fragments);
            self.suppressing = true;
        } else {
            self.pending = Some(pending);
        }
    }

    fn emit(&mut self, field: StreamField, index: u32, correlation: C, text: String) {
        if text.is_empty() {
            return;
        }
        if field == StreamField::Text
            && let Some(capture) = &mut self.terminal_text_capture
        {
            capture.push_str(&text);
        }
        let fact = match field {
            StreamField::Text => ObservationFact::TextDelta { index, text },
            StreamField::Thinking => ObservationFact::ThinkingDelta { index, text },
        };
        self.inner.observe(Observation { correlation, fact });
    }

    fn redact_delta(&mut self, field: StreamField, index: u32, correlation: C, text: String) {
        if self.suppressing {
            self.emit(field, index, correlation, REDACTED.to_string());
            return;
        }
        if let Some(mut pending) = self.pending.take() {
            if !stream_candidate_starts_at_zero(&pending.text)
                && let Some(unsafe_start) = unsafe_stream_suffix_start(&pending.text)
            {
                let (safe, unsafe_fragments) =
                    split_stream_fragments(pending.fragments, unsafe_start);
                self.emit_original(safe);
                pending.fragments = unsafe_fragments;
                pending.text = pending.text[unsafe_start..].to_string();
            }
            let mut combined = pending.text.clone();
            combined.push_str(&text);
            if stream_candidate_starts_at_zero(&combined) {
                // An empty delta extends neither the held candidate nor any
                // eventual emission; retaining a fragment for it would grow
                // held metadata without bound, since the pending-byte cap
                // below measures only text bytes.
                if !text.is_empty() {
                    pending.fragments.push(StreamFragment {
                        field,
                        index,
                        correlation,
                        text,
                    });
                }
                if let Some(unsafe_start) = unsafe_stream_suffix_start(&combined) {
                    if unsafe_start == 0 {
                        pending.text = combined;
                        self.hold_or_suppress(pending);
                        return;
                    }
                    let (redacted, unsafe_fragments) =
                        split_stream_fragments(pending.fragments, unsafe_start);
                    self.emit_redacted(redacted);
                    self.hold_or_suppress(PendingStreamText {
                        fragments: unsafe_fragments,
                        text: combined[unsafe_start..].to_string(),
                    });
                    return;
                }
                self.emit_redacted(pending.fragments);
                return;
            }
            self.emit_original(pending.fragments);
        }

        if let Some(unsafe_start) = unsafe_stream_suffix_start(&text) {
            let fragment = StreamFragment {
                field,
                index,
                correlation,
                text: text.clone(),
            };
            if text.len() <= MAX_PENDING_STREAM_BYTES {
                self.hold_or_suppress(PendingStreamText {
                    fragments: vec![fragment],
                    text,
                });
                return;
            }
            let (safe, unsafe_fragments) = split_stream_fragments(vec![fragment], unsafe_start);
            self.emit_original(safe);
            self.hold_or_suppress(PendingStreamText {
                fragments: unsafe_fragments,
                text: text[unsafe_start..].to_string(),
            });
        } else {
            self.emit(field, index, correlation, redact_text(&text));
        }
    }
}

fn split_stream_fragments<C: Clone>(
    fragments: Vec<StreamFragment<C>>,
    split_at: usize,
) -> (Vec<StreamFragment<C>>, Vec<StreamFragment<C>>) {
    let mut before = Vec::new();
    let mut after = Vec::new();
    let mut consumed = 0;
    for fragment in fragments {
        let end = consumed + fragment.text.len();
        if end <= split_at {
            before.push(fragment);
        } else if consumed >= split_at {
            after.push(fragment);
        } else {
            let local_split = split_at - consumed;
            before.push(StreamFragment {
                field: fragment.field,
                index: fragment.index,
                correlation: fragment.correlation.clone(),
                text: fragment.text[..local_split].to_string(),
            });
            after.push(StreamFragment {
                field: fragment.field,
                index: fragment.index,
                correlation: fragment.correlation,
                text: fragment.text[local_split..].to_string(),
            });
        }
        consumed = end;
    }
    (before, after)
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
                self.finish();
                self.inner.observe(Observation {
                    correlation: observation.correlation,
                    fact: ObservationFact::UsageReported(usage),
                });
            }
            fact => {
                self.flush_boundary();
                self.inner.observe(Observation {
                    correlation: observation.correlation,
                    fact,
                });
            }
        }
    }
}

fn stream_candidate_starts_at_zero(text: &str) -> bool {
    LINE_CREDENTIAL_MARKERS
        .iter()
        .chain(VALUE_CREDENTIAL_MARKERS)
        .any(|marker| {
            (text.len() <= marker.len()
                && marker.as_bytes()[..text.len()].eq_ignore_ascii_case(text.as_bytes()))
                || (text.len() > marker.len()
                    && text.as_bytes()[..marker.len()].eq_ignore_ascii_case(marker.as_bytes()))
        })
        || TOKEN_PREFIXES.iter().any(|prefix| {
            (text.len() <= prefix.len() && prefix.as_bytes()[..text.len()] == *text.as_bytes())
                || text.starts_with(prefix)
        })
        || json_credential_value_at_start(text).is_some()
        || unterminated_json_key_start(text) == Some(0)
}

fn unsafe_stream_suffix_start(text: &str) -> Option<usize> {
    let mut earliest = None;
    for marker in LINE_CREDENTIAL_MARKERS
        .iter()
        .chain(VALUE_CREDENTIAL_MARKERS)
    {
        let length = trailing_marker_prefix(text, marker, true);
        if length > 0 {
            earliest = Some(earliest.map_or(text.len() - length, |current: usize| {
                current.min(text.len() - length)
            }));
        }
    }
    for marker in TOKEN_PREFIXES {
        let length = trailing_marker_prefix(text, marker, false);
        if length > 0 {
            earliest = Some(earliest.map_or(text.len() - length, |current: usize| {
                current.min(text.len() - length)
            }));
        }
    }
    for marker in LINE_CREDENTIAL_MARKERS {
        earliest = unterminated_marker_start(text, marker, ValueTermination::Line)
            .map_or(earliest, |start| {
                Some(earliest.map_or(start, |current| current.min(start)))
            });
    }
    for marker in VALUE_CREDENTIAL_MARKERS {
        earliest = unterminated_marker_start(text, marker, ValueTermination::Token)
            .map_or(earliest, |start| {
                Some(earliest.map_or(start, |current| current.min(start)))
            });
    }
    for prefix in TOKEN_PREFIXES {
        earliest = unterminated_marker_start(text, prefix, ValueTermination::Token)
            .map_or(earliest, |start| {
                Some(earliest.map_or(start, |current| current.min(start)))
            });
    }
    if let Some(start) = unterminated_json_credential_start(text) {
        earliest = Some(earliest.map_or(start, |current| current.min(start)));
    }
    if let Some(start) = unterminated_json_key_start(text) {
        earliest = Some(earliest.map_or(start, |current| current.min(start)));
    }
    earliest
}

fn unterminated_json_credential_start(text: &str) -> Option<usize> {
    if let Some(value_start) = json_credential_value_at_start(text) {
        let (_, token_start, value_end) =
            credential_value_bounds(text, value_start, ValueTermination::Token);
        if value_end.max(token_start) == text.len() {
            return Some(0);
        }
    }
    let mut offset = 0;
    while let Some((relative_start, relative_value_start)) =
        next_json_credential_value(&text[offset..])
    {
        let start = offset + relative_start;
        let value_start = offset + relative_value_start;
        let (_, token_start, value_end) =
            credential_value_bounds(text, value_start, ValueTermination::Token);
        if value_end.max(token_start) == text.len() {
            return Some(start);
        }
        offset = value_end.max(token_start);
    }
    None
}

fn json_credential_value_at_start(text: &str) -> Option<usize> {
    let wrapped = format!("{{{text}");
    next_json_credential_value(&wrapped)
        .and_then(|(start, value_start)| (start == 1).then_some(value_start - 1))
}

fn unterminated_json_key_start(text: &str) -> Option<usize> {
    let mut offset = 0;
    while let Some(relative_start) = text[offset..].find('"') {
        let start = offset + relative_start;
        if (start == 0 || json_key_can_start_at(text, start))
            && quoted_value_end(text, start + 1, '"') == text.len()
        {
            return Some(start);
        }
        offset = start + 1;
    }
    None
}

fn unterminated_marker_start(
    text: &str,
    marker: &str,
    termination: ValueTermination,
) -> Option<usize> {
    let mut offset = 0;
    let mut last = None;
    while let Some(relative) = find_ascii_case_insensitive(&text[offset..], marker) {
        let start = offset + relative;
        let value_start = start + marker.len();
        let (_, token_start, value_end) = credential_value_bounds(text, value_start, termination);
        if value_end.max(token_start) == text.len() {
            last = Some(start);
        }
        offset = value_start;
    }
    last
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
    use signalbox_model_runtime::{Observation, ObservationFact, ObservationSink};

    use super::{
        MAX_PENDING_STREAM_BYTES, REDACTED, RedactingSink, redact_json, redact_text,
        stream_candidate_starts_at_zero, unsafe_stream_suffix_start,
    };

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
    const COMPOSITE_SECRET_VALUE: &str = "sensitive-composite-value";

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

    /// INV-035: composite credential keys embedded in arbitrary CLI text are
    /// redacted with the same contains-based key policy as decoded JSON.
    #[test]
    fn inv_035_redacts_composite_json_keys_in_text() {
        let fixture = format!(
            r#"provider detail: {{"client_secret":"{COMPOSITE_SECRET_VALUE}","bedrock_api_key":"{COMPOSITE_SECRET_VALUE}"}}"#
        );
        let output = redact_text(&fixture);

        assert!(!output.contains(COMPOSITE_SECRET_VALUE));
        assert!(output.contains("[redacted]"));
    }

    #[test]
    fn inv_035_redacts_a_bare_credential_member_at_fragment_start() {
        let fixture = format!(r#"  "client_secret":"{COMPOSITE_SECRET_VALUE}""#);
        let output = redact_text(&fixture);

        assert_eq!(output, r#"  "client_secret":"[redacted]""#);
        assert!(!output.contains(COMPOSITE_SECRET_VALUE));
    }

    #[test]
    fn harmless_tool_arguments_remain_byte_exact() {
        let input = r#"{ "city" : "Oslo", "limit": 3 }"#;

        assert_eq!(redact_json(input), input);
    }

    #[test]
    fn inv_035_stream_redaction_holds_a_split_token_prefix() {
        assert_eq!(unsafe_stream_suffix_start("safe s"), Some(5));
    }

    #[test]
    fn inv_035_stream_redaction_holds_an_unterminated_credential() {
        assert_eq!(unsafe_stream_suffix_start("sk-sensitive"), Some(0));
    }

    #[test]
    fn inv_035_stream_redaction_releases_a_terminated_credential() {
        assert_eq!(unsafe_stream_suffix_start("sk-sensitive done."), None);
    }

    #[test]
    fn inv_035_stream_redaction_holds_a_split_composite_json_key() {
        assert_eq!(unsafe_stream_suffix_start(r#"safe {"client_sec"#), Some(6));
        assert!(stream_candidate_starts_at_zero(r#""client_secret":"#));
    }

    /// INV-035: an unterminated streamed credential cannot grow retained
    /// redaction state without bound.
    #[test]
    fn inv_035_stream_redaction_bounds_an_unterminated_credential() {
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::ThinkingDelta {
                    index: 0,
                    text: format!("sk-{}", "x".repeat(MAX_PENDING_STREAM_BYTES)),
                },
            });
            assert!(sink.pending.is_none());
            assert!(sink.suppressing);
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 1,
                    text: "continued".to_string(),
                },
            });
            sink.finish();
        }

        assert_eq!(
            observed,
            vec![
                Observation {
                    correlation: 7_u8,
                    fact: ObservationFact::ThinkingDelta {
                        index: 0,
                        text: REDACTED.to_string(),
                    },
                },
                Observation {
                    correlation: 7_u8,
                    fact: ObservationFact::TextDelta {
                        index: 1,
                        text: REDACTED.to_string(),
                    },
                },
            ]
        );
    }

    /// Plumbing only: streams `count` empty reasoning items at ascending
    /// indexes after the caller's held fragment.
    fn observe_empty_reasoning_items(sink: &mut RedactingSink<'_, u8>, count: u32) {
        for index in 1..=count {
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::ThinkingDelta {
                    index,
                    text: String::new(),
                },
            });
        }
    }

    /// INV-035: empty streamed items behind a held credential candidate
    /// cannot grow retained fragment metadata without bound.
    #[test]
    fn inv_035_stream_redaction_bounds_held_fragments_across_empty_items() {
        let empty_items = 1024_u32;
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::ThinkingDelta {
                    index: 0,
                    text: "sk-".to_string(),
                },
            });
            observe_empty_reasoning_items(&mut sink, empty_items);
            assert_eq!(
                sink.pending.as_ref().map(|pending| pending.fragments.len()),
                Some(1),
                "empty deltas must not extend held-fragment retention"
            );
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: empty_items + 1,
                    text: "held-secret".to_string(),
                },
            });
            sink.finish();
        }

        assert_eq!(
            observed,
            vec![
                Observation {
                    correlation: 7_u8,
                    fact: ObservationFact::ThinkingDelta {
                        index: 0,
                        text: REDACTED.to_string(),
                    },
                },
                Observation {
                    correlation: 7_u8,
                    fact: ObservationFact::TextDelta {
                        index: empty_items + 1,
                        text: REDACTED.to_string(),
                    },
                },
            ]
        );
    }
}
