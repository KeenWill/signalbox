//! Credential-shape redaction for CLI-controlled output.

use serde_json::Value;
use signalbox_model_runtime::{Observation, ObservationFact, ObservationSink};

const REDACTED: &str = "[redacted]";
/// A valid-JSON stand-in for suppressed tool arguments and objects whose raw
/// form still carries a credential shape after structural redaction, so the
/// `arguments_json` raw-JSON contract is never broken by a bare sentinel.
const REDACTED_JSON_OBJECT: &str = r#"{"redacted":"[redacted]"}"#;
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
    let sanitized = redact_text_literal(text);
    // Detection also runs on the form a JSON consumer would reconstruct, so a
    // credential spelled with `\uXXXX` escapes cannot ride escaped bytes past
    // the literal scanners. The decoded form only decides: when it is
    // credential-clean the original bytes are returned verbatim, so a benign
    // literal escape is never rewritten; only when the decoded form reveals a
    // credential shape the literal scan missed is the redacted decoded form
    // returned, failing closed.
    let Some(decoded) = decode_unicode_escapes(text) else {
        // Pathological escape nesting exhausted the decode budget; a
        // credential could hide behind the unresolved escapes, so fail closed.
        return REDACTED.to_string();
    };
    if decoded == text {
        return sanitized;
    }
    let sanitized_decoded = redact_text_literal(&decoded);
    if sanitized_decoded == decoded {
        return sanitized;
    }
    sanitized_decoded
}

/// Substrings whose case-insensitive presence is necessary for any redaction
/// pass to change the text: every marker, credential name, and token prefix
/// begins with or contains one of these. A leaf lacking all of them is
/// credential-clean and can bypass the allocating passes. A JSON structural
/// character is included because a bare credential member can appear without a
/// separator keyword once inside an object.
const CREDENTIAL_INDICATORS: &[&str] = &[
    "authorization",
    "cookie",
    "api_key",
    "api-key",
    "apikey",
    "auth_token",
    "auth-token",
    "authtoken",
    "bearer",
    "bearertoken",
    "access_token",
    "accesstoken",
    "refresh_token",
    "refreshtoken",
    "id_token",
    "idtoken",
    "session_token",
    "sessiontoken",
    "private_key",
    "private-key",
    "privatekey",
    "password",
    "secret",
    "credential",
    "sk-",
    "eyJ",
];

fn text_might_contain_credential(text: &str) -> bool {
    // A quote can begin a bare credential member the JSON-value scanner reads
    // without an enclosing object; an escape can hide any indicator.
    if text.contains('"') || text.contains("\\u") {
        return true;
    }
    CREDENTIAL_INDICATORS
        .iter()
        .any(|indicator| find_ascii_case_insensitive(text, indicator).is_some())
}

fn redact_text_literal(text: &str) -> String {
    // No-match fast path: a leaf that contains none of the credential
    // indicators cannot be changed by any pass below, so it skips the six
    // full-string allocations those passes would each make. Bounded provider
    // input (a large array of short clean strings) therefore has bounded
    // practical work while decoding one event.
    if !text_might_contain_credential(text) {
        return text.to_string();
    }
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
    for name in LINE_CREDENTIAL_NAMES {
        sanitized = redact_spaced_credential(&sanitized, name, ValueTermination::Line);
    }
    for name in VALUE_CREDENTIAL_NAMES {
        sanitized = redact_spaced_credential(&sanitized, name, ValueTermination::Token);
    }
    sanitized = redact_identifier_assignment(&sanitized);
    sanitized
}

/// Whether the credential shape inside a key is a free-form secret whose
/// unquoted value can carry spaces, so its assignment consumes the whole line
/// rather than stopping at the first whitespace.
fn credential_key_is_free_form(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect::<String>();
    [
        "authorization",
        "password",
        "secret",
        "credential",
        "cookie",
    ]
    .iter()
    .any(|shape| normalized.contains(shape))
}

/// The credential identifier immediately before an assignment separator: a
/// bare `[A-Za-z0-9_-]+` run or a quoted key, whichever ends the text. Returns
/// the identifier content for the `credential_key` contains-policy check.
fn trailing_identifier(before_separator: &str) -> Option<(&str, bool)> {
    let trimmed = before_separator.trim_end_matches([' ', '\t']);
    for quote in ['"', '\''] {
        if let Some(without_close) = trimmed.strip_suffix(quote) {
            let start = without_close.rfind(quote)?;
            return Some((&without_close[start + 1..], true));
        }
    }
    let start = trimmed
        .rfind(|character: char| {
            !(character.is_ascii_alphanumeric() || character == '_' || character == '-')
        })
        .map_or(0, |index| index + 1);
    (start < trimmed.len()).then_some((&trimmed[start..], false))
}

/// Redacts `identifier = value` / `identifier: value` where the identifier is
/// a complete name (bare or quoted) whose contains-policy matches a credential
/// shape, catching composite names (`AWS_SECRET_ACCESS_KEY`, `client_secret`)
/// and TOML quoted keys (`"api_key" = …`) the exact-name scanners miss.
fn redact_identifier_assignment(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(separator) = remaining
        .bytes()
        .position(|byte| matches!(byte, b'=' | b':'))
    {
        let is_colon = remaining.as_bytes()[separator] == b':';
        // A quoted key before `:` is a JSON member the JSON scanner already
        // owns; only a `=` after a quoted key (TOML) or any separator after a
        // bare key is a plaintext credential assignment.
        if let Some((identifier, quoted)) = trailing_identifier(&remaining[..separator])
            && !(quoted && is_colon)
            && credential_key(identifier)
        {
            let termination = if credential_key_is_free_form(identifier) {
                ValueTermination::Line
            } else {
                ValueTermination::Token
            };
            let value_start = separator + 1;
            output.push_str(&remaining[..value_start]);
            let (prefix, token_start, value_end) =
                credential_value_bounds(remaining, value_start, termination);
            output.push_str(prefix);
            output.push_str(REDACTED);
            remaining = &remaining[value_end.max(token_start)..];
        } else {
            output.push_str(&remaining[..=separator]);
            remaining = &remaining[separator + 1..];
        }
    }
    output.push_str(remaining);
    output
}

/// Credential names whose spaced `name = value` / `name : value` form the
/// exact markers cannot catch; the separator may carry surrounding spaces or
/// tabs, as ordinary configuration and error output often print.
const LINE_CREDENTIAL_NAMES: &[&str] = &[
    "authorization",
    "cookie",
    // Free-form secrets whose unquoted value can carry spaces (a passphrase,
    // a PEM body) must consume the whole line, not stop at the first space.
    "password",
    "secret",
    "credential",
];
const VALUE_CREDENTIAL_NAMES: &[&str] = &[
    "api_key",
    "api-key",
    "auth_token",
    "auth-token",
    "bearer_token",
    "bearer-token",
    "access_token",
    "refresh_token",
    "id_token",
    "session_token",
    "private_key",
    "private-key",
    // Concatenated spellings also match their camel-case forms
    // case-insensitively (`apiKey`, `privateKey`), covering plaintext
    // assignments the separator-bearing names miss.
    "apikey",
    "authtoken",
    "bearertoken",
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "sessiontoken",
    "privatekey",
];

/// Redacts a `name` credential whose separator (`=` or `:`) carries optional
/// surrounding horizontal whitespace, so `api_key = opaque` is caught exactly
/// as `api_key=opaque` already is. Only a name immediately followed — after
/// spaces or tabs — by a separator matches, so prose mentioning the name
/// without assigning it is untouched.
fn redact_spaced_credential(text: &str, name: &str, termination: ValueTermination) -> String {
    let mut remaining = text;
    let mut output = String::with_capacity(text.len());
    while let Some(index) = find_ascii_case_insensitive(remaining, name) {
        let after_name = index + name.len();
        let whitespace = remaining[after_name..]
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        let separator = after_name + whitespace;
        if matches!(remaining.as_bytes().get(separator), Some(b'=' | b':')) {
            let value_start = separator + 1;
            output.push_str(&remaining[..value_start]);
            let (prefix, token_start, value_end) =
                credential_value_bounds(remaining, value_start, termination);
            output.push_str(prefix);
            output.push_str(REDACTED);
            remaining = &remaining[value_end.max(token_start)..];
        } else {
            output.push_str(&remaining[..after_name]);
            remaining = &remaining[after_name..];
        }
    }
    output.push_str(remaining);
    output
}

/// Rewrites `\uXXXX` escape sequences (including surrogate pairs) to the
/// characters they name, so a credential shape spelled with JSON escapes —
/// for example `sk\u002d…` — is scanned in the same form a JSON consumer
/// would reconstruct. Quote and backslash escapes are left alone: they carry
/// the quoted-value semantics the scanners already honor. Escapes can nest
/// (`\u005cu0073…` decodes into a new escape), so decoding repeats to a
/// fixed point; each changing pass replaces a six-byte escape with at most
/// four bytes, so the string strictly shrinks and the loop is bounded by the
/// input length rather than a fixed ceiling.
fn decode_unicode_escapes(text: &str) -> Option<String> {
    // Cumulative scan work is capped at a small multiple of the input length,
    // so a deeply nested escape spelling — which peels one level per whole-
    // string pass and would otherwise be quadratic — cannot pin the decode
    // (run synchronously while decoding a bounded-but-large provider event)
    // past the exchange deadline. Legitimate content converges in one or two
    // passes and stays well inside the budget; exhaustion means pathological
    // nesting, and the caller fails closed.
    let mut budget = text.len().saturating_mul(4).saturating_add(4096);
    let mut current = text.to_string();
    while current.contains("\\u") {
        budget = budget.checked_sub(current.len())?;
        let decoded = decode_unicode_escape_pass(&current);
        if decoded == current {
            break;
        }
        current = decoded;
    }
    Some(current)
}

fn decode_unicode_escape_pass(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(position) = remaining.find("\\u") {
        output.push_str(&remaining[..position]);
        match unicode_escape_at(&remaining[position..]) {
            Some((character, consumed)) => {
                output.push(character);
                remaining = &remaining[position + consumed..];
            }
            None => {
                output.push_str("\\u");
                remaining = &remaining[position + 2..];
            }
        }
    }
    output.push_str(remaining);
    output
}

/// Decodes one `\uXXXX` sequence at the start of `rest`, pairing a leading
/// high surrogate with an immediately following low one; an invalid or lone
/// sequence decodes to nothing and is left in place.
fn unicode_escape_at(rest: &str) -> Option<(char, usize)> {
    let high = u32::from_str_radix(rest.get(2..6)?, 16).ok()?;
    if (0xD800..0xDC00).contains(&high) {
        let low_rest = rest.get(6..12)?;
        if !low_rest.starts_with("\\u") {
            return None;
        }
        let low = u32::from_str_radix(low_rest.get(2..6)?, 16).ok()?;
        if !(0xDC00..0xE000).contains(&low) {
            return None;
        }
        let combined = 0x1_0000 + ((high - 0xD800) << 10) + (low - 0xDC00);
        return char::from_u32(combined).map(|character| (character, 12));
    }
    char::from_u32(high).map(|character| (character, 6))
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
    // Credential-clean raw bytes are returned verbatim. Cleanliness is judged
    // on the raw text, not the parsed tree, because a shadowed duplicate
    // member is invisible after parsing.
    if !changed && redact_text(raw) == raw {
        return raw.to_string();
    }
    // Every serialized result is rescanned: structural field-name redaction
    // reaches neither a shadowed duplicate value nor a token-shaped object
    // key, so a residual credential shape after serialization — whether or
    // not the tree changed — conservatively suppresses the whole object to a
    // valid redacted sentinel. Re-serialization also drops a shadowed
    // duplicate while keeping arbitrary-precision numeric lexemes.
    let serialized =
        serde_json::to_string(&value).unwrap_or_else(|_| REDACTED_JSON_OBJECT.to_string());
    if redact_text(&serialized) == serialized {
        return serialized;
    }
    REDACTED_JSON_OBJECT.to_string()
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
        "authtoken",
        "bearertoken",
        "privatekey",
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
    let value_body = value_start + whitespace;
    let opening = text[value_body..].chars().next();
    if matches!(opening, Some('{' | '[')) {
        let structural_end = structural_value_end(text, value_body);
        let value_end = match termination {
            ValueTermination::Token => structural_end,
            ValueTermination::Line => structural_end.max(
                text[value_body..]
                    .find(['\r', '\n'])
                    .map_or(text.len(), |length| value_body + length),
            ),
        };
        return (&text[value_start..value_body], value_body, value_end);
    }
    // A TOML multiline value opens with three quotes; a plain `quoted_value_end`
    // would treat the second quote as the close and emit the body. Consume
    // through the matching triple delimiter, or to the text end when
    // unterminated.
    for triple in ["\"\"\"", "'''"] {
        if text[value_body..].starts_with(triple) {
            let body_start = value_body + triple.len();
            let value_end = text[body_start..]
                .find(triple)
                .map_or(text.len(), |offset| body_start + offset + triple.len());
            return (&text[value_start..value_body], value_body, value_end);
        }
    }
    let opening_quote = opening.filter(|character| matches!(character, '"' | '\''));
    let prefix_end = value_body + usize::from(opening_quote.is_some());
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
    // A PEM block value spans lines and whitespace, so token/line termination
    // would stop at the first space after `-----BEGIN` and emit the key body.
    // When the value opens a PEM block, extend through its matching
    // `-----END …-----` marker (or to the text end if unterminated), so the
    // whole private key is suppressed.
    let value_end =
        pem_block_end(text, prefix_end).map_or(value_end, |pem_end| value_end.max(pem_end));
    (&text[value_start..prefix_end], prefix_end, value_end)
}

/// If an unquoted value at `value_start` opens a PEM block, returns the byte
/// offset just past its matching `-----END …-----` marker, or the text end
/// when the block is unterminated. `None` when the value is not a PEM block.
fn pem_block_end(text: &str, value_start: usize) -> Option<usize> {
    const BEGIN: &str = "-----BEGIN";
    const DASHES: &str = "-----";
    if !text[value_start..].starts_with(BEGIN) {
        return None;
    }
    // The BEGIN header is `-----BEGIN <label>-----`; the matching close is
    // `-----END <label>-----`. Requiring the same label prevents an intervening
    // mismatched marker (a `-----END CERTIFICATE-----` before the real
    // `-----END PRIVATE KEY-----`) from releasing the key body; a missing
    // matching marker suppresses through the input end.
    let after_begin = value_start + BEGIN.len();
    let Some(label_end) = text[after_begin..]
        .find(DASHES)
        .map(|offset| after_begin + offset)
    else {
        return Some(text.len());
    };
    let label = text[after_begin..label_end].trim();
    let end_marker = format!("-----END {label}{DASHES}");
    match find_ascii_case_insensitive(&text[label_end..], &end_marker) {
        Some(offset) => Some(label_end + offset + end_marker.len()),
        None => Some(text.len()),
    }
}

/// Consumes a `{`- or `[`-opened credential value through its balanced
/// structural close, treating `"`-quoted spans (with backslash escapes) as
/// opaque content. The scan is bounded by the text it is given: a container
/// still open at the end of the text reports the text's end, so the stateless
/// redactor suppresses the unterminated value whole and the stateful sink
/// holds it as an unterminated credential candidate; a mismatched structural
/// close is malformed and reports the text's end the same way.
fn structural_value_end(text: &str, value_start: usize) -> usize {
    let mut expected_closers = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for (offset, character) in text[value_start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
        } else {
            match character {
                '"' => in_string = true,
                '{' => expected_closers.push('}'),
                '[' => expected_closers.push(']'),
                '}' | ']' => {
                    // A close that does not match its opener is malformed;
                    // resuming after it could release text that still belongs
                    // to the credential value, so suppress through the end.
                    if expected_closers.pop() != Some(character) {
                        return text.len();
                    }
                    if expected_closers.is_empty() {
                        return value_start + offset + character.len_utf8();
                    }
                }
                _ => {}
            }
        }
    }
    text.len()
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

    /// Redacts a terminal failure message against the held lookbehind state:
    /// a message that extends a held credential candidate — or that arrives
    /// while the sink is suppressing an oversized one — is replaced whole,
    /// exactly as the streamed fragments it continues are, so a credential
    /// split between streamed text and a failure message cannot cross the
    /// adapter boundary inside terminal evidence.
    pub(crate) fn redact_terminal_failure_text(&self, message: &str) -> String {
        if self.suppressing {
            return REDACTED.to_string();
        }
        if let Some(pending) = &self.pending {
            let mut joined = pending.text.clone();
            joined.push_str(message);
            if redact_text(&joined) != joined {
                return REDACTED.to_string();
            }
        }
        redact_text(message)
    }

    /// Redacts tool-argument JSON against the held lookbehind state:
    /// arguments that extend a held credential candidate — or that arrive
    /// while the sink is suppressing an oversized one — are replaced whole,
    /// exactly as the streamed fragments they continue are; otherwise the
    /// stateless JSON-aware redaction applies and clean arguments stay
    /// byte-verbatim.
    /// True if `value`, read as a continuation of the held lookbehind followed
    /// by `preceding` provider text (the same-envelope final text emitted
    /// before this field), would be redacted — a credential shape spanning the
    /// held state, the final text, and the field itself.
    fn extends_held_credential(&self, preceding: &str, value: &str) -> bool {
        if self.suppressing {
            return true;
        }
        let held = self
            .pending
            .as_ref()
            .map_or("", |pending| pending.text.as_str());
        if held.is_empty() && preceding.is_empty() {
            return false;
        }
        let mut joined = String::with_capacity(held.len() + preceding.len() + value.len());
        joined.push_str(held);
        joined.push_str(preceding);
        joined.push_str(value);
        redact_text(&joined) != joined
    }

    pub(crate) fn redact_tool_arguments(&self, preceding: &str, arguments: &str) -> String {
        // Suppression yields a valid JSON object, not the bare `[redacted]`
        // sentinel, so the `arguments_json` raw-JSON contract holds and
        // `decode_tool_arguments` never reports a syntax error for a call the
        // provider actually supplied validly. The final text emitted before
        // this field is joined in, so an argument continuing a marker at the
        // end of that text is suppressed too.
        if self.extends_held_credential(preceding, arguments) {
            return REDACTED_JSON_OBJECT.to_string();
        }
        redact_json(arguments)
    }

    /// Sanitizes a provider-controlled identifier or name against the held
    /// state plus the same-envelope preceding text: an id continuing a
    /// credential marker in the final text is suppressed, not left verbatim.
    pub(crate) fn redact_provider_id(&self, preceding: &str, value: &str) -> String {
        if self.extends_held_credential(preceding, value) {
            return REDACTED.to_string();
        }
        redact_text(value)
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
        || json_credential_key_awaiting_colon(text) == Some(0)
        || match decode_unicode_escapes(text) {
            // A candidate spelled with `\uXXXX` escapes still starts at zero
            // in the form a JSON consumer reconstructs; an exhausted decode
            // budget is treated as a candidate so the fragment is held.
            Some(decoded) => decoded != text && stream_candidate_starts_at_zero(&decoded),
            None => true,
        }
        || trailing_partial_unicode_escape(text) == Some(0)
        || spaced_credential_starts_at_zero(text)
        || identifier_assignment_unsafe_start(text) == Some(0)
}

/// Whether `text` begins a spaced credential assignment — a recognized name
/// (or a prefix of one) at index zero, optionally followed by whitespace and a
/// separator — whether or not its value has terminated. A held candidate that
/// has already reached its separator must still enter the redaction branch, so
/// this is broader than the in-progress-only `spaced_credential_unsafe_start`.
fn spaced_credential_starts_at_zero(text: &str) -> bool {
    LINE_CREDENTIAL_NAMES
        .iter()
        .chain(VALUE_CREDENTIAL_NAMES)
        .any(|name| {
            if text.len() < name.len() {
                return name.as_bytes()[..text.len()].eq_ignore_ascii_case(text.as_bytes());
            }
            if !text.as_bytes()[..name.len()].eq_ignore_ascii_case(name.as_bytes()) {
                return false;
            }
            let whitespace = text[name.len()..]
                .bytes()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            let separator = name.len() + whitespace;
            separator == text.len() || matches!(text.as_bytes().get(separator), Some(b'=' | b':'))
        })
}

/// The trailing portion of `text` that could begin a credential extending into
/// text emitted after it — everything before it is credential-clean. Used to
/// give same-envelope tool fields the final text's relevant context without
/// rescanning the whole (possibly multi-megabyte) text per field.
pub(crate) fn trailing_credential_context(text: &str) -> &str {
    unsafe_stream_suffix_start(text).map_or("", |start| &text[start..])
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
    if let Some(start) = json_credential_key_awaiting_colon(text) {
        earliest = Some(earliest.map_or(start, |current| current.min(start)));
    }
    if let Some(start) = escaped_unsafe_suffix_start(text) {
        earliest = Some(earliest.map_or(start, |current| current.min(start)));
    }
    if let Some(start) = spaced_credential_unsafe_start(text) {
        earliest = Some(earliest.map_or(start, |current| current.min(start)));
    }
    if let Some(start) = identifier_assignment_unsafe_start(text) {
        earliest = Some(earliest.map_or(start, |current| current.min(start)));
    }
    earliest
}

/// Holds a spaced credential assignment (`api_key = value`, `authorization :
/// value`) that the exact no-whitespace markers miss, when it is still in
/// progress at the end of a fragment: a bare recognized name (its separator or
/// value may arrive next), a name with a separator but an unterminated value,
/// or a trailing partial name prefix. The contiguous form is redacted by the
/// literal scanner; this only governs cross-delta holding.
fn spaced_credential_unsafe_start(text: &str) -> Option<usize> {
    let mut earliest: Option<usize> = None;
    let mut fold = |start: usize| {
        earliest = Some(earliest.map_or(start, |current: usize| current.min(start)));
    };
    let named = LINE_CREDENTIAL_NAMES
        .iter()
        .map(|name| (name, ValueTermination::Line))
        .chain(
            VALUE_CREDENTIAL_NAMES
                .iter()
                .map(|name| (name, ValueTermination::Token)),
        );
    for (name, termination) in named {
        let prefix_length = trailing_marker_prefix(text, name, true);
        if prefix_length > 0 {
            fold(text.len() - prefix_length);
        }
        let mut offset = 0;
        while let Some(relative) = find_ascii_case_insensitive(&text[offset..], name) {
            let start = offset + relative;
            let after_name = start + name.len();
            let whitespace = text[after_name..]
                .bytes()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            let separator = after_name + whitespace;
            let in_progress = if separator == text.len() {
                true
            } else if matches!(text.as_bytes().get(separator), Some(b'=' | b':')) {
                let (_, token_start, value_end) =
                    credential_value_bounds(text, separator + 1, termination);
                value_end.max(token_start) == text.len()
            } else {
                false
            };
            if in_progress {
                fold(start);
            }
            offset = after_name;
        }
    }
    earliest
}

/// Byte offset of a trailing credential identifier assignment still in progress
/// at the end of a fragment — a composite (`AWS_SECRET_ACCESS_KEY`) or quoted
/// (`"api_key"`) key that the exact-name scanners miss, either awaiting its
/// separator or with an unterminated value — so a credential split across
/// deltas is held rather than emitted piecewise.
fn identifier_assignment_unsafe_start(text: &str) -> Option<usize> {
    let mut earliest: Option<usize> = None;
    let mut fold = |start: usize| {
        earliest = Some(earliest.map_or(start, |current: usize| current.min(start)));
    };
    let base = text.as_ptr() as usize;
    for (separator, byte) in text.bytes().enumerate() {
        if !matches!(byte, b'=' | b':') {
            continue;
        }
        let is_colon = byte == b':';
        if let Some((identifier, quoted)) = trailing_identifier(&text[..separator])
            && !(quoted && is_colon)
            && credential_key(identifier)
        {
            let termination = if credential_key_is_free_form(identifier) {
                ValueTermination::Line
            } else {
                ValueTermination::Token
            };
            let (_, token_start, value_end) =
                credential_value_bounds(text, separator + 1, termination);
            if value_end.max(token_start) == text.len() {
                let content = identifier.as_ptr() as usize - base;
                fold(if quoted { content - 1 } else { content });
            }
        }
    }
    // A trailing identifier with no separator yet — a separator may arrive in
    // the next delta.
    let end_trimmed = text.trim_end_matches([' ', '\t']);
    if let Some((identifier, quoted)) = trailing_identifier(end_trimmed)
        && credential_key(identifier)
    {
        let content = identifier.as_ptr() as usize - base;
        fold(if quoted { content - 1 } else { content });
    }
    earliest
}

/// Holds credential shapes that only appear once `\uXXXX` escapes are decoded,
/// and holds a trailing partial escape so a sequence split across deltas
/// (`sk\u00` then `2d…`) is not emitted before it can be completed. A
/// decoded form whose credential shape the literal suffix scan missed is held
/// from the first escape in the original — conservative but bounded by the
/// pending-byte cap, which suppresses an oversized hold.
fn escaped_unsafe_suffix_start(text: &str) -> Option<usize> {
    if let Some(start) = trailing_partial_unicode_escape(text) {
        return Some(start);
    }
    let Some(decoded) = decode_unicode_escapes(text) else {
        // An exhausted decode budget means unresolved nested escapes; hold the
        // whole fragment so a credential hidden behind them cannot be emitted.
        return Some(0);
    };
    if decoded == text {
        return None;
    }
    if unsafe_stream_suffix_start(&decoded).is_some() || stream_candidate_starts_at_zero(&decoded) {
        // The decoded credential's start cannot be mapped back through the
        // escapes precisely, so the whole fragment is held — fail closed,
        // bounded by the pending-byte cap that suppresses an oversized hold.
        return Some(0);
    }
    None
}

/// Byte offset from which a trailing, still-incomplete `\uXXXX` escape must be
/// held: the escape is a final lone backslash, or a `\u` followed only by
/// fewer than four hexadecimal digits with nothing after them. The held span
/// backs up over the contiguous non-separator run ending at the escape, so a
/// token split mid-escape (`sk\u00`) is held whole rather than emitting its
/// clean-looking head. A complete or clearly non-hex sequence is not partial
/// and returns `None`.
fn trailing_partial_unicode_escape(text: &str) -> Option<usize> {
    let escape_position = if text.ends_with('\\') {
        Some(text.len() - 1)
    } else if let Some(position) = text.rfind("\\u") {
        let digits = &text[position + 2..];
        (digits.len() < 4 && digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then_some(position)
    } else {
        None
    }?;
    let token_start = text[..escape_position]
        .char_indices()
        .rev()
        .take_while(|(_, character)| !is_stream_token_boundary(*character))
        .last()
        .map_or(escape_position, |(offset, _)| offset);
    Some(token_start)
}

fn is_stream_token_boundary(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            '"' | '\'' | ',' | '{' | '}' | '[' | ']' | ';' | ':' | '='
        )
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

/// Finds a complete credential-bearing JSON key whose member is still
/// awaiting its colon: the text ends with the quoted key followed only by
/// optional whitespace. The stateless member scan accepts whitespace between
/// key and colon, so streamed text split there could otherwise release the
/// key and let a later delta deliver the value unredacted.
fn json_credential_key_awaiting_colon(text: &str) -> Option<usize> {
    let mut offset = 0;
    while let Some(relative_start) = text[offset..].find('"') {
        let start = offset + relative_start;
        if !(start == 0 || json_key_can_start_at(text, start)) {
            offset = start + 1;
            continue;
        }
        let key_end = quoted_value_end(text, start + 1, '"');
        if key_end == text.len() {
            return None;
        }
        let encoded_key = &text[start..=key_end];
        if let Ok(key) = serde_json::from_str::<String>(encoded_key)
            && credential_key(&key)
            && text[key_end + 1..]
                .chars()
                .all(|character| character.is_whitespace())
        {
            return Some(start);
        }
        offset = key_end + 1;
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
        MAX_PENDING_STREAM_BYTES, REDACTED, REDACTED_JSON_OBJECT, RedactingSink,
        decode_unicode_escapes, redact_json, redact_text, stream_candidate_starts_at_zero,
        trailing_credential_context, unsafe_stream_suffix_start,
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

    fn observation_text(observation: Observation<u8>) -> String {
        match observation.fact {
            ObservationFact::TextDelta { text, .. }
            | ObservationFact::ThinkingDelta { text, .. } => text,
            _ => String::new(),
        }
    }
    const STRUCTURED_OBJECT_SECRET_VALUE: &str = "sensitive-structured-object-value";
    const STRUCTURED_ARRAY_SECRET_ONE: &str = "sensitive-structured-array-one";
    const STRUCTURED_ARRAY_SECRET_TWO: &str = "sensitive-structured-array-two";

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

    /// INV-035: a credential member whose value is a JSON object is consumed
    /// through its balanced structural close, never released piecewise.
    #[test]
    fn inv_035_redacts_an_object_valued_credential_member_whole() {
        let fixture = format!(r#"{{"credential":{{"value":"{STRUCTURED_OBJECT_SECRET_VALUE}"}}}}"#);
        let output = redact_text(&fixture);

        assert_eq!(output, r#"{"credential":[redacted]}"#);
        assert!(!output.contains(STRUCTURED_OBJECT_SECRET_VALUE));
    }

    /// INV-035: a credential member whose value is a JSON array is consumed
    /// through its balanced structural close.
    #[test]
    fn inv_035_redacts_an_array_valued_credential_member_whole() {
        let fixture = format!(
            r#"{{"password":["{STRUCTURED_ARRAY_SECRET_ONE}","{STRUCTURED_ARRAY_SECRET_TWO}"]}}"#
        );
        let output = redact_text(&fixture);

        assert_eq!(output, r#"{"password":[redacted]}"#);
        assert!(!output.contains(STRUCTURED_ARRAY_SECRET_ONE));
        assert!(!output.contains(STRUCTURED_ARRAY_SECRET_TWO));
    }

    /// INV-035: a structured value behind a key=value credential marker is
    /// consumed whole rather than to its first structural character.
    #[test]
    fn inv_035_redacts_a_marker_prefixed_structured_value_whole() {
        let fixture = format!(r#"api_key={{"nested":"{STRUCTURED_OBJECT_SECRET_VALUE}"}} tail"#);
        let output = redact_text(&fixture);

        assert_eq!(output, "api_key=[redacted] tail");
        assert!(!output.contains(STRUCTURED_OBJECT_SECRET_VALUE));
    }

    /// INV-035: a structured credential value still open at the end of the
    /// text is suppressed through the end rather than released piecewise.
    #[test]
    fn inv_035_suppresses_an_unterminated_structured_credential_value() {
        let fixture = format!(r#"{{"credential":{{"value":"{STRUCTURED_OBJECT_SECRET_VALUE}"#);
        let output = redact_text(&fixture);

        assert_eq!(output, r#"{"credential":[redacted]"#);
        assert!(!output.contains(STRUCTURED_OBJECT_SECRET_VALUE));
    }

    /// INV-035: a mismatched structural close is malformed and cannot release
    /// the remainder of a credential container as safe text.
    #[test]
    fn inv_035_suppresses_a_mismatched_structural_credential_close() {
        let fixture = format!(
            r#"{{"credential":{{"value":"{STRUCTURED_OBJECT_SECRET_VALUE}"],"tail":"{STRUCTURED_ARRAY_SECRET_ONE}"}}"#
        );
        let output = redact_text(&fixture);

        assert_eq!(output, r#"{"credential":[redacted]"#);
        assert!(!output.contains(STRUCTURED_OBJECT_SECRET_VALUE));
        assert!(!output.contains(STRUCTURED_ARRAY_SECRET_ONE));
    }

    /// INV-035: a structured authorization header value spanning lines is
    /// consumed through its structural close, not only to the line end.
    #[test]
    fn inv_035_redacts_a_multiline_structured_header_value_whole() {
        let fixture = format!(
            "authorization: {{\"token\":\n\"{STRUCTURED_OBJECT_SECRET_VALUE}\"}}\nsafe-after-value"
        );
        let output = redact_text(&fixture);

        assert_eq!(output, "authorization: [redacted]\nsafe-after-value");
        assert!(!output.contains(STRUCTURED_OBJECT_SECRET_VALUE));
    }

    /// INV-035: a duplicate member shadowed in the parsed tree cannot
    /// smuggle a credential token through the raw-bytes fast path.
    #[test]
    fn inv_035_shadowed_duplicate_members_cannot_leak_tokens() {
        let fixture = format!(r#"{{"value":"{CREDENTIAL_SHAPED_VALUE}","value":"safe"}}"#);
        let output = redact_json(&fixture);

        assert!(!output.contains(CREDENTIAL_SHAPED_VALUE));
        assert_eq!(output, r#"{"value":"safe"}"#);
    }

    /// INV-035: a token shape spelled with JSON unicode escapes is scanned
    /// in the reconstructed form a JSON consumer would produce.
    #[test]
    fn inv_035_redacts_json_escaped_token_shapes() {
        let fixture = r#"{"detail":"sk\u002dsensitive-escaped-value"}"#;
        let output = redact_text(fixture);

        assert!(!output.contains("sensitive-escaped-value"));
        assert!(output.contains("[redacted]"));
    }

    /// INV-035: a credential member key spelled with JSON unicode escapes is
    /// recognized in its reconstructed form.
    #[test]
    fn inv_035_redacts_json_escaped_credential_members() {
        let fixture = format!(r#"{{"api\u005fkey":"{QUOTED_CREDENTIAL_VALUE}"}}"#);
        let output = redact_text(&fixture);

        assert!(!output.contains(QUOTED_CREDENTIAL_VALUE));
        assert!(output.contains("[redacted]"));
    }

    /// INV-035: a benign literal `\uXXXX` escape in credential-clean text is
    /// preserved byte-for-byte, not rewritten to the character it names.
    #[test]
    fn inv_035_preserves_benign_unicode_escapes_in_clean_text() {
        let fixture = r#"{"note":"smile \u263A done"}"#;

        assert_eq!(redact_text(fixture), fixture);
        assert_eq!(redact_json(fixture), fixture);
    }

    /// INV-035: a token shape hidden behind more nesting than any fixed pass
    /// ceiling is still decoded to a fixed point and redacted.
    #[test]
    fn inv_035_redacts_deeply_nested_escaped_token_shapes() {
        let fixture = r"sk\u005cu005cu005cu005cu002dsensitive-nested-value";
        let output = redact_text(fixture);

        assert!(!output.contains("sensitive-nested-value"));
        assert!(output.contains("[redacted]"));
    }

    /// INV-035: a spaced `name = value` credential is redacted exactly as the
    /// separator-adjacent form is.
    #[test]
    fn inv_035_redacts_spaced_credential_separators() {
        let output = redact_text("api_key = spaced-secret-value and safe-tail");

        assert!(!output.contains("spaced-secret-value"));
        assert!(output.contains("[redacted]"));
        assert!(output.contains("safe-tail"));
    }

    /// INV-035: a spaced authorization header consumes its whole line value.
    #[test]
    fn inv_035_redacts_a_spaced_authorization_line() {
        let output = redact_text("authorization : opaque header value\nsafe-after");

        assert!(!output.contains("opaque header value"));
        assert!(output.contains("[redacted]"));
        assert!(output.contains("safe-after"));
    }

    /// INV-035: a trailing partial Unicode escape holds its whole token so a
    /// credential split mid-escape across deltas is never emitted piecewise.
    #[test]
    fn inv_035_stream_redaction_holds_a_partial_unicode_escape() {
        assert_eq!(unsafe_stream_suffix_start(r"safe sk\u00"), Some(5));
        assert!(stream_candidate_starts_at_zero(r"sk-"));
    }

    /// INV-035: a credential token split across a Unicode escape boundary is
    /// redacted whole once the escape completes.
    #[test]
    fn inv_035_stream_redaction_redacts_a_split_escaped_token() {
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: r"sk\u00".to_string(),
                },
            });
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 1,
                    text: "2dsensitive-escaped-stream".to_string(),
                },
            });
            sink.finish();
        }

        assert_eq!(
            observed,
            vec![
                Observation {
                    correlation: 7_u8,
                    fact: ObservationFact::TextDelta {
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

    /// INV-035: `auth_token` / `bearer_token` members are redacted even with an
    /// opaque value lacking any token prefix.
    #[test]
    fn inv_035_redacts_authentication_token_members() {
        let auth = format!(r#"{{"auth_token":"{QUOTED_CREDENTIAL_VALUE}"}}"#);
        let bearer = format!(r#"{{"bearer_token":"{JSON_CREDENTIAL_VALUE}"}}"#);

        assert_eq!(redact_json(&auth), r#"{"auth_token":"[redacted]"}"#);
        assert_eq!(redact_json(&bearer), r#"{"bearer_token":"[redacted]"}"#);
    }

    /// INV-035: private-key members and their plaintext assignments are
    /// redacted even with an opaque value lacking a token prefix.
    #[test]
    fn inv_035_redacts_private_key_credentials() {
        let member = format!(r#"{{"private_key":"{QUOTED_CREDENTIAL_VALUE}"}}"#);
        let camel = format!(r#"{{"privateKey":"{JSON_CREDENTIAL_VALUE}"}}"#);
        let plaintext = redact_text("private_key = opaque-private-key-value tail");
        let camel_plaintext = redact_text("privateKey = opaque-camel-key-value tail");

        assert_eq!(redact_json(&member), r#"{"private_key":"[redacted]"}"#);
        assert_eq!(redact_json(&camel), r#"{"privateKey":"[redacted]"}"#);
        assert!(!plaintext.contains("opaque-private-key-value"));
        assert!(plaintext.contains("[redacted]"));
        assert!(!camel_plaintext.contains("opaque-camel-key-value"));
        assert!(camel_plaintext.contains("[redacted]"));
    }

    /// INV-035: plaintext `auth_token` / `bearer_token` assignments are
    /// redacted, not only their quoted JSON-member forms.
    #[test]
    fn inv_035_redacts_plaintext_auth_token_assignments() {
        let auth = redact_text("auth_token = plaintext-auth-secret and safe-tail");
        let bearer = redact_text("bearer_token: plaintext-bearer-secret");

        assert!(!auth.contains("plaintext-auth-secret"));
        assert!(auth.contains("[redacted]"));
        assert!(auth.contains("safe-tail"));
        assert!(!bearer.contains("plaintext-bearer-secret"));
        assert!(bearer.contains("[redacted]"));
    }

    /// INV-035: a token-shaped JSON object key that the field-name traversal
    /// cannot reach is suppressed to a valid redacted object, not reserialized
    /// verbatim.
    #[test]
    fn inv_035_redacts_token_shaped_json_object_keys() {
        let fixture = r#"{"sk-opaque-token-key":"safe"}"#;
        let output = redact_json(fixture);

        assert!(!output.contains("sk-opaque-token-key"));
        assert_eq!(output, REDACTED_JSON_OBJECT);
        assert!(serde_json::from_str::<serde_json::Value>(&output).is_ok());
    }

    /// INV-035: a token-shaped object key alongside an ordinarily redacted
    /// value — which sets `changed` — is still suppressed, since every
    /// serialized result is rescanned.
    #[test]
    fn inv_035_redacts_token_key_beside_a_changed_value() {
        let fixture = r#"{"password":"opaque","sk-sensitive-mixed-key":"safe"}"#;
        let output = redact_json(fixture);

        assert!(!output.contains("sk-sensitive-mixed-key"));
        assert_eq!(output, REDACTED_JSON_OBJECT);
        assert!(serde_json::from_str::<serde_json::Value>(&output).is_ok());
    }

    /// INV-035: an unquoted PEM private-key assignment consumes the whole
    /// block through its END marker, not just the first token of the header.
    #[test]
    fn inv_035_redacts_unquoted_pem_private_key() {
        let fixture = "private_key = -----BEGIN PRIVATE KEY-----\nMIIBpem-body-secret\n-----END PRIVATE KEY-----\nsafe-after";
        let output = redact_text(fixture);

        assert!(!output.contains("MIIBpem-body-secret"));
        assert!(!output.contains("BEGIN PRIVATE KEY"));
        assert!(output.contains("[redacted]"));
        assert!(output.contains("safe-after"));
    }

    /// INV-035: a mismatched intervening `-----END …-----` marker does not
    /// release the PEM body; suppression continues to the matching END.
    #[test]
    fn inv_035_pem_requires_the_matching_end_label() {
        let fixture = "private_key = -----BEGIN PRIVATE KEY-----\nMIIBmismatch-body\n-----END CERTIFICATE-----\nMIIBmore-key-body\n-----END PRIVATE KEY-----\nsafe-tail";
        let output = redact_text(fixture);

        assert!(!output.contains("MIIBmismatch-body"));
        assert!(!output.contains("MIIBmore-key-body"));
        assert!(output.contains("[redacted]"));
        assert!(output.contains("safe-tail"));
    }

    /// INV-035: a TOML triple-quoted credential value is consumed through its
    /// matching triple delimiter, not just the empty span between the first
    /// two quotes.
    #[test]
    fn inv_035_redacts_triple_quoted_credential_values() {
        let fixture = "private_key = \"\"\"-----BEGIN PRIVATE KEY-----\nMIIBtriple-body\n-----END PRIVATE KEY-----\"\"\"\nsafe-tail";
        let output = redact_text(fixture);

        assert!(!output.contains("MIIBtriple-body"));
        assert!(output.contains("[redacted]"));
        assert!(output.contains("safe-tail"));
    }

    /// INV-035: a quoted TOML assignment key with `=` is redacted.
    #[test]
    fn inv_035_redacts_quoted_key_equals_assignment() {
        let output = redact_text("\"api_key\" = opaque-quoted-key-value tail");

        assert!(!output.contains("opaque-quoted-key-value"));
        assert!(output.contains("[redacted]"));
        assert!(output.contains("tail"));
    }

    /// INV-035: composite identifier assignments whose credential shape is
    /// embedded (not at the identifier end) are redacted in plaintext.
    #[test]
    fn inv_035_redacts_composite_identifier_assignments() {
        let client = redact_text("client_secret = opaque-client-secret and tail");
        let aws = redact_text("AWS_SECRET_ACCESS_KEY=opaque-aws-secret");

        assert!(!client.contains("opaque-client-secret"));
        assert!(client.contains("[redacted]"));
        assert!(!aws.contains("opaque-aws-secret"));
        assert!(aws.contains("[redacted]"));
    }

    /// The trailing credential context is the unsafe suffix; a clean-ending
    /// text yields nothing to rescan.
    #[test]
    fn trailing_credential_context_is_the_unsafe_suffix() {
        assert_eq!(trailing_credential_context("the quick brown fox"), "");
        assert_eq!(
            trailing_credential_context("some text Authorization:"),
            "Authorization:"
        );
    }

    /// INV-035: a multiword unquoted secret consumes its whole line, not just
    /// the first word.
    #[test]
    fn inv_035_redacts_multiword_unquoted_secrets() {
        let password = redact_text(
            "password: correct horse battery staple
safe-line",
        );
        let secret = redact_text("secret: multi word secret value");

        assert!(!password.contains("horse battery staple"));
        assert_eq!(
            password,
            "password: [redacted]
safe-line"
        );
        assert!(!secret.contains("word secret value"));
        assert_eq!(secret, "secret: [redacted]");
    }

    /// A non-secret usage field that merely contains "token" is not matched.
    #[test]
    fn input_tokens_usage_field_is_not_redacted() {
        let fixture = r#"{"input_tokens":11,"output_tokens":7}"#;

        assert_eq!(redact_json(fixture), fixture);
    }

    /// INV-035: pathologically nested escapes exhaust the decode budget and
    /// fail closed rather than leaking or running unbounded.
    #[test]
    fn inv_035_exhausted_escape_decode_fails_closed() {
        // Each `u005c` re-forms an escape after the prior one decodes to a
        // backslash, so the spelling peels one level per whole-string pass.
        let nested = format!(r"\u005c{}", "u005c".repeat(64));
        let fixture = format!("sk{nested}u002dbudget-buster-secret");

        assert!(decode_unicode_escapes(&fixture).is_none());
        assert_eq!(redact_text(&fixture), REDACTED);
    }

    /// INV-035: a spaced credential name is held across streamed deltas so the
    /// separated fragments cannot reconstruct the assignment.
    #[test]
    fn inv_035_stream_redaction_holds_a_spaced_credential_name() {
        assert_eq!(unsafe_stream_suffix_start("safe api_key "), Some(5));
        assert!(stream_candidate_starts_at_zero("api_key "));
    }

    /// INV-035: a spaced assignment split between name and separator/value is
    /// redacted whole once the fragments join.
    #[test]
    fn inv_035_stream_redaction_redacts_a_split_spaced_assignment() {
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: "api_key ".to_string(),
                },
            });
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 1,
                    text: "= spaced-split-secret done".to_string(),
                },
            });
            sink.finish();
        }
        let emitted: Vec<String> = observed.into_iter().map(observation_text).collect();

        assert!(
            !emitted
                .iter()
                .any(|text| text.contains("spaced-split-secret"))
        );
        assert!(emitted.iter().any(|text| text.contains("[redacted]")));
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

    /// INV-035: a structured credential value still open at the end of
    /// streamed text is held as an unterminated credential candidate.
    #[test]
    fn inv_035_stream_redaction_holds_an_unterminated_structured_value() {
        assert_eq!(
            unsafe_stream_suffix_start(r#"{"credential":{"value":"#),
            Some(1)
        );
    }

    /// INV-035: a complete credential key still awaiting its colon is held
    /// as a candidate rather than released before its value arrives.
    #[test]
    fn inv_035_stream_redaction_holds_a_credential_key_awaiting_its_colon() {
        assert_eq!(
            unsafe_stream_suffix_start(r#"safe {"credential" "#),
            Some(6)
        );
        assert!(stream_candidate_starts_at_zero(r#""credential" "#));
    }

    /// INV-035: a JSON member split between its key and a whitespace-led
    /// colon cannot be reconstructed from the emitted deltas.
    #[test]
    fn inv_035_stream_redaction_redacts_a_member_split_before_its_colon() {
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: r#"{"credential" "#.to_string(),
                },
            });
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 1,
                    text: format!(r#": "{STRUCTURED_OBJECT_SECRET_VALUE}"}}"#),
                },
            });
            sink.finish();
        }

        assert_eq!(
            observed,
            vec![
                Observation {
                    correlation: 7_u8,
                    fact: ObservationFact::TextDelta {
                        index: 0,
                        text: "{".to_string(),
                    },
                },
                Observation {
                    correlation: 7_u8,
                    fact: ObservationFact::TextDelta {
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

    /// INV-035: a structured credential value split across streamed deltas
    /// is redacted whole once its structural close arrives.
    #[test]
    fn inv_035_stream_redaction_redacts_a_structured_value_split_across_deltas() {
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: r#"{"credential":{"value":"#.to_string(),
                },
            });
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 1,
                    text: format!(r#""{STRUCTURED_OBJECT_SECRET_VALUE}"}}}}"#),
                },
            });
            sink.finish();
        }

        assert_eq!(
            observed,
            vec![
                Observation {
                    correlation: 7_u8,
                    fact: ObservationFact::TextDelta {
                        index: 0,
                        text: "{".to_string(),
                    },
                },
                Observation {
                    correlation: 7_u8,
                    fact: ObservationFact::TextDelta {
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

    /// INV-035: a failure message that extends a credential marker held
    /// from streamed text is suppressed whole, never returned verbatim.
    #[test]
    fn inv_035_terminal_failure_text_consults_held_redaction_state() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);
        sink.observe(Observation {
            correlation: 7_u8,
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "Authorization:".to_string(),
            },
        });

        assert_eq!(
            sink.redact_terminal_failure_text(AUTHORIZATION_VALUE),
            REDACTED
        );
    }

    /// INV-035: a failure message that arrives while the sink suppresses an
    /// oversized unterminated credential is suppressed with it.
    #[test]
    fn inv_035_terminal_failure_text_stays_suppressed_after_an_oversized_credential() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);
        sink.observe(Observation {
            correlation: 7_u8,
            fact: ObservationFact::TextDelta {
                index: 0,
                text: format!("sk-{}", "x".repeat(MAX_PENDING_STREAM_BYTES)),
            },
        });

        assert_eq!(
            sink.redact_terminal_failure_text("harmless failure detail"),
            REDACTED
        );
    }

    /// INV-035: tool arguments that extend a credential marker held from
    /// streamed text are suppressed whole, never returned piecewise.
    #[test]
    fn inv_035_tool_arguments_consult_held_redaction_state() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);
        sink.observe(Observation {
            correlation: 7_u8,
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "Authorization:".to_string(),
            },
        });

        let redacted =
            sink.redact_tool_arguments("", &format!(r#"{{"city":" {AUTHORIZATION_VALUE}"}}"#));

        assert_eq!(redacted, REDACTED_JSON_OBJECT);
        assert!(!redacted.contains(AUTHORIZATION_VALUE));
        assert!(
            serde_json::from_str::<serde_json::Value>(&redacted)
                .expect("suppressed tool arguments are valid JSON")
                .is_object()
        );
    }

    /// INV-035: a tool argument continuing a marker at the end of the
    /// same-envelope final text is suppressed, not left verbatim.
    #[test]
    fn inv_035_tool_arguments_consult_same_envelope_final_text() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let sink = RedactingSink::new(&mut observed);

        let redacted = sink.redact_tool_arguments(
            "Authorization:",
            &format!(r#"{{"value":" {AUTHORIZATION_VALUE}"}}"#),
        );

        assert_eq!(redacted, REDACTED_JSON_OBJECT);
        assert!(!redacted.contains(AUTHORIZATION_VALUE));
    }

    /// Harmless tool arguments stay byte-exact with no held redaction state.
    #[test]
    fn tool_arguments_without_held_state_stay_byte_exact() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let sink = RedactingSink::new(&mut observed);
        let arguments = r#"{ "city" : "Oslo", "limit": 3 }"#;

        assert_eq!(sink.redact_tool_arguments("", arguments), arguments);
    }

    /// A failure message with no held redaction state keeps its stateless
    /// redaction, so harmless provider errors stay legible.
    #[test]
    fn terminal_failure_text_without_held_state_is_unchanged() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let sink = RedactingSink::new(&mut observed);
        let message = "quota exhausted for the active plan";

        assert_eq!(sink.redact_terminal_failure_text(message), message);
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
