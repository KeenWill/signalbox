//! Credential-shape redaction for CLI-controlled output.

use serde_json::Value;
use signalbox_model_runtime::{Observation, ObservationFact, ObservationSink};

pub(crate) const REDACTED: &str = "[redacted]";
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
/// PEM armor opening a block, and the label substring that makes the block a
/// private key of any type (`PRIVATE KEY`, `RSA PRIVATE KEY`, `OPENSSH PRIVATE
/// KEY`, `ENCRYPTED PRIVATE KEY`). Such a block is an unambiguous credential on
/// its own, with no assignment marker in front of it for the marker,
/// spaced-name, or identifier scanners to key on.
const PEM_BEGIN: &str = "-----BEGIN";
const PEM_DASHES: &str = "-----";
const PEM_PRIVATE_KEY_LABEL: &str = "PRIVATE KEY";

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
    // Space-separated labels a diagnostic prints (`API key: …`): the JSON key
    // policy normalizes across punctuation, so the plaintext spellings must be
    // recognized here too or the fast path releases them unscanned.
    "api key",
    "auth token",
    "bearer token",
    "access token",
    "refresh token",
    "id token",
    "session token",
    "private key",
    // A standalone private-key PEM block carries no credential word at all;
    // its armor is what the PEM pass keys on.
    "-----begin",
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
    // A standalone private-key PEM block is consumed whole before the
    // assignment scanners run, so its body cannot be mistaken for ordinary
    // prose by them.
    let mut sanitized = redact_pem_private_keys(text);
    sanitized = redact_json_credential_values(&sanitized);
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
/// the identifier content for the `credential_key` contains-policy check, plus
/// the opening quote (`Some('"')` / `Some('\'')`) when the key was quoted so the
/// caller can distinguish a double-quoted JSON member from a single-quoted or
/// bare plaintext assignment.
fn trailing_identifier(before_separator: &str) -> Option<(&str, Option<char>)> {
    let trimmed = before_separator.trim_end_matches([' ', '\t']);
    for quote in ['"', '\''] {
        if let Some(without_close) = trimmed.strip_suffix(quote) {
            // Find the opening delimiter escape-aware: a TOML basic key (`"…"`)
            // can embed an escaped quote (`\"`), and `rfind` would select that
            // content quote as the opener and return only the tail after it,
            // hiding the credential shape carried by the full key.
            let start = last_unescaped_quote(without_close, quote)?;
            return Some((&without_close[start + 1..], Some(quote)));
        }
    }
    // Advance past the delimiter by its full UTF-8 width: `rfind` returns the
    // byte offset where the (possibly multibyte) delimiter char begins, so
    // `index + 1` could land inside its encoding (`éAWS_SECRET_ACCESS_KEY=`)
    // and panic the slice — aborting the executing task on provider-controlled
    // text.
    let start = trimmed
        .rfind(|character: char| {
            !(character.is_ascii_alphanumeric() || character == '_' || character == '-')
        })
        .map_or(0, |index| {
            index + trimmed[index..].chars().next().map_or(1, char::len_utf8)
        });
    (start < trimmed.len()).then_some((&trimmed[start..], None))
}

/// The byte index of the last `quote` in `s` that is not backslash-escaped,
/// scanning from the end. Basic strings (`"`) honor `\` escapes, so a `\"` is
/// content and skipped; literal strings (`'`) have no escapes, so every quote
/// counts. Bounded by the distance back to that quote (plus any backslash run
/// before an escaped one), so it stays a local lookbehind rather than a scan of
/// the whole prefix.
fn last_unescaped_quote(s: &str, quote: char) -> Option<usize> {
    let honors_escapes = quote == '"';
    let mut search_end = s.len();
    while let Some(index) = s[..search_end].rfind(quote) {
        if !honors_escapes {
            return Some(index);
        }
        let backslashes = s[..index]
            .bytes()
            .rev()
            .take_while(|&byte| byte == b'\\')
            .count();
        if backslashes % 2 == 0 {
            return Some(index);
        }
        search_end = index;
    }
    None
}

/// If the identifier content starting at byte `content` in `text` is immediately
/// preceded by an unescaped `"` or `'`, returns that quote's offset — the
/// opening delimiter of a quoted key whose closing quote has not yet arrived in
/// the stream. Holding from the quote keeps the rejoined `"api_key" = value`
/// recognizable once the close, separator, and value follow; holding from the
/// bare name would drop the opener and leak the value.
fn opening_quote_before(text: &str, content: usize) -> Option<usize> {
    let prefix = &text[..content];
    for quote in ['"', '\''] {
        if let Some(before_quote) = prefix.strip_suffix(quote) {
            let escaped = before_quote
                .bytes()
                .rev()
                .take_while(|&byte| byte == b'\\')
                .count()
                % 2
                == 1;
            if !escaped {
                return Some(content - quote.len_utf8());
            }
        }
    }
    None
}

/// Whether the value beginning at `value_start` (after leading spaces or tabs)
/// is one or two quote characters running to the text end — the split opening of
/// a `"""`/`'''` triple whose third quote and multiline body arrive in a later
/// delta. Such a suffix must be held: otherwise `credential_value_bounds` reads
/// the two quotes as a completed empty quoted value and the following secret
/// body is emitted unheld.
fn partial_triple_open(text: &str, value_start: usize) -> bool {
    let whitespace = text[value_start..]
        .bytes()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count();
    matches!(
        &text[value_start + whitespace..],
        "\"" | "\"\"" | "'" | "''"
    )
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
        // A *double*-quoted key before `:` is exempt only where the JSON
        // scanner actually owns it — after `{`, `,`, or at the scan start. A
        // quoted key embedded after prose (`detail: "client_secret":"v"`) is
        // one the JSON scanner rejects, so this scanner must take it or a
        // composite credential name leaks. A single-quoted key before `:`
        // (`'api_key': value`) is never JSON, and a `=` after any quoted key
        // (TOML) or any separator after a bare key is a plaintext credential
        // assignment this scanner owns outright.
        if let Some((identifier, quote)) = trailing_identifier(&remaining[..separator])
            && !(quote == Some('"')
                && is_colon
                && json_key_can_start_at(
                    text,
                    identifier.as_ptr() as usize - text.as_ptr() as usize - 1,
                ))
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
    // The space-separated label a diagnostic prints; `secret` alone does not
    // match it, because the separator does not follow the word.
    "secret key",
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
    // Space-separated spellings of the same names, which ordinary provider
    // diagnostics print (`API key: opaque-value`).
    "api key",
    "auth token",
    "bearer token",
    "access token",
    "refresh token",
    "id token",
    "session token",
    "private key",
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
    // unterminated. The terminator scan is escape-aware for basic strings
    // (`"""`), where an escaped quote (`\"`) followed by two quotes is content,
    // not a close; literal strings (`'''`) have no escapes.
    for triple in ["\"\"\"", "'''"] {
        if text[value_body..].starts_with(triple) {
            let body_start = value_body + triple.len();
            let value_end = find_unescaped_triple(&text[body_start..], triple)
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

/// Redacts every standalone private-key PEM block — `-----BEGIN … PRIVATE
/// KEY-----` through its matching `-----END …-----` — which the marker,
/// spaced-name, and identifier scanners all miss because no assignment
/// introduces it. An unterminated block is suppressed through the text end, so
/// a truncated key never surfaces its body.
fn redact_pem_private_keys(text: &str) -> String {
    let mut remaining = text;
    let mut output = String::with_capacity(text.len());
    while let Some(index) = private_key_pem_start(remaining) {
        output.push_str(&remaining[..index]);
        output.push_str(REDACTED);
        let end = pem_block_end(remaining, index).unwrap_or(remaining.len());
        remaining = &remaining[end..];
    }
    output.push_str(remaining);
    output
}

/// The byte offset of the first `-----BEGIN … PRIVATE KEY-----` armor in
/// `text`. The label is the span between `-----BEGIN` and the header's closing
/// dashes; an armor whose closing dashes never arrive names no complete label
/// and is left to the streaming lookbehind
/// ([`pem_private_key_unsafe_start`]) to hold.
fn private_key_pem_start(text: &str) -> Option<usize> {
    let mut offset = 0;
    while let Some(relative) = find_ascii_case_insensitive(&text[offset..], PEM_BEGIN) {
        let start = offset + relative;
        let after_begin = start + PEM_BEGIN.len();
        let label_end = text[after_begin..]
            .find(PEM_DASHES)
            .map(|length| after_begin + length)?;
        if find_ascii_case_insensitive(&text[after_begin..label_end], PEM_PRIVATE_KEY_LABEL)
            .is_some()
        {
            return Some(start);
        }
        offset = after_begin;
    }
    None
}

/// Byte offset where a private-key PEM block still in progress at the end of
/// `text` begins, so a block split across deltas is held rather than emitted
/// piecewise. Three shapes are in progress: a trailing partial `-----BEGIN`
/// prefix, an armor whose closing dashes have not arrived (its label may still
/// name a private key), and a private-key block whose matching `-----END …-----`
/// has not arrived. A block that closed inside the fragment is skipped past
/// rather than rescanned, so a fragment carrying many blocks stays linear.
fn pem_private_key_unsafe_start(text: &str) -> Option<usize> {
    let mut earliest: Option<usize> = None;
    let mut fold = |start: usize| {
        earliest = Some(earliest.map_or(start, |current: usize| current.min(start)));
    };
    let prefix_length = trailing_marker_prefix(text, PEM_BEGIN, true);
    if prefix_length > 0 {
        fold(text.len() - prefix_length);
    }
    let mut offset = 0;
    while let Some(relative) = find_ascii_case_insensitive(&text[offset..], PEM_BEGIN) {
        let start = offset + relative;
        let after_begin = start + PEM_BEGIN.len();
        let Some(label_end) = text[after_begin..]
            .find(PEM_DASHES)
            .map(|length| after_begin + length)
        else {
            fold(start);
            break;
        };
        if find_ascii_case_insensitive(&text[after_begin..label_end], PEM_PRIVATE_KEY_LABEL)
            .is_none()
        {
            offset = after_begin;
            continue;
        }
        let end_marker = format!(
            "-----END {}{PEM_DASHES}",
            text[after_begin..label_end].trim()
        );
        match find_ascii_case_insensitive(&text[label_end..], &end_marker) {
            Some(relative_end) => offset = label_end + relative_end + end_marker.len(),
            None => {
                fold(start);
                break;
            }
        }
    }
    earliest
}

/// Whether `text` begins a private-key PEM block — the armor, a prefix of it
/// whose remainder may still arrive, or an armor whose label has not finished
/// arriving — whether or not the block has terminated. Held text that already
/// reached its `-----END …-----` must still enter the redaction branch, so this
/// is broader than the in-progress-only [`pem_private_key_unsafe_start`],
/// exactly as `spaced_credential_starts_at_zero` is broader than
/// `spaced_credential_unsafe_start`.
fn pem_private_key_starts_at_zero(text: &str) -> bool {
    if text.len() <= PEM_BEGIN.len() {
        return PEM_BEGIN.as_bytes()[..text.len()].eq_ignore_ascii_case(text.as_bytes());
    }
    if !text.as_bytes()[..PEM_BEGIN.len()].eq_ignore_ascii_case(PEM_BEGIN.as_bytes()) {
        return false;
    }
    let label_end = text[PEM_BEGIN.len()..]
        .find(PEM_DASHES)
        .map_or(text.len(), |length| PEM_BEGIN.len() + length);
    label_end == text.len()
        || find_ascii_case_insensitive(&text[PEM_BEGIN.len()..label_end], PEM_PRIVATE_KEY_LABEL)
            .is_some()
}

/// If an unquoted value at `value_start` opens a PEM block, returns the byte
/// offset just past its matching `-----END …-----` marker, or the text end
/// when the block is unterminated. `None` when the value is not a PEM block.
fn pem_block_end(text: &str, value_start: usize) -> Option<usize> {
    const BEGIN: &str = PEM_BEGIN;
    const DASHES: &str = PEM_DASHES;
    // Case-insensitive like the matching END search below, so lowercase armor
    // cannot slip a key body past the scan.
    if !text.as_bytes()[value_start..]
        .get(..BEGIN.len())
        .is_some_and(|armor| armor.eq_ignore_ascii_case(BEGIN.as_bytes()))
    {
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

/// The byte offset within `body` where the closing `triple` delimiter begins.
/// For basic strings (`"""`), the scan honors backslash escapes so an escaped
/// quote (`\"`) is content and cannot start the closing run; the closing run is
/// three consecutive quotes whose first is unescaped. Literal strings (`'''`)
/// have no escapes, so every quote is literal.
fn find_unescaped_triple(body: &str, triple: &str) -> Option<usize> {
    let quote = triple.as_bytes()[0] as char;
    let honors_escapes = quote == '"';
    let mut escaped = false;
    for (offset, character) in body.char_indices() {
        if honors_escapes && escaped {
            escaped = false;
        } else if honors_escapes && character == '\\' {
            escaped = true;
        } else if character == quote && body[offset..].starts_with(triple) {
            return Some(offset);
        }
    }
    None
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
    /// The unsafe trailing suffix of a provider-controlled field already
    /// emitted in an out-of-band record (the thread id in
    /// `ExchangeEstablished`). Later provider text sits beside that record in
    /// observations and terminal evidence, so a credential split between the
    /// field's end and the text's start must be caught by joining this context
    /// into the lookbehind — it is match-state only and is never emitted.
    emitted_context: String,
    /// The rolling unsafe trailing suffix of provider text the adapter drops
    /// in buffered delivery (reasoning items never observed as deltas).
    /// Dropped bytes appear in no record and cannot be reconstructed — but a
    /// credential marker inside them still marks the value that follows in
    /// the final text as a secret, and the streamed path suppresses those
    /// same value bytes. Tracked separately from `emitted_context`: dropped
    /// bytes sit in no record between the emitted id and later text, so
    /// folding them into the emitted chain would break its adjacency
    /// matching. Match-state only, never emitted.
    dropped_context: String,
}

impl<'a, C: Clone> RedactingSink<'a, C> {
    pub(crate) fn new(inner: &'a mut (dyn ObservationSink<C> + Send)) -> Self {
        Self {
            inner,
            pending: None,
            suppressing: false,
            terminal_text_capture: None,
            emitted_context: String::new(),
            dropped_context: String::new(),
        }
    }

    /// Seeds the lookbehind with the unsafe trailing context of a field value
    /// that has already been emitted in an out-of-band record, so stream text
    /// that would extend a credential marker ending that field (`api_` in the
    /// id, `key=value` in the text) is suppressed rather than emitted as the
    /// marker's reconstructable continuation beside it.
    pub(crate) fn seed_emitted_context(&mut self, emitted: &str) {
        self.emitted_context = trailing_credential_context(emitted).to_string();
    }

    /// Extends the match-only lookbehind with provider text the adapter is
    /// dropping (a buffered-delivery reasoning item), so a later field or the
    /// final text completing a credential begun in the dropped bytes is
    /// suppressed exactly as the streamed path suppresses it. An unsafe
    /// suffix growing past the pending byte cap is an unresolved oversized
    /// credential candidate; matching against a truncation could miss it, so
    /// the sink fails closed into suppression instead.
    pub(crate) fn extend_dropped_context(&mut self, dropped: &str) {
        if self.suppressing {
            return;
        }
        // Two live match-only chains precede the held text, and the held text
        // must be judged against BOTH (a fragment safe against one can still be
        // unsafe against the other):
        //   * the emitted-adjacency chain `emitted_context` (the thread id,
        //     and suppressed held markers) — future *emitted* output sits
        //     beside it directly, dropped bytes being invisible; and
        //   * the full chronological chain `emitted_context ++ dropped_context
        //     ++ pending`, which also threads the dropped bytes.
        // The held text's clean prefix is the shorter of what each chain
        // allows; the rest is suppressed (a released prefix would reconstruct a
        // credential beside later output) and carried into both chains so a
        // marker completed by future emitted output, the dropped bytes, or a
        // later delta is caught.
        let mut chain = self.emitted_context.clone();
        chain.push_str(&self.dropped_context);
        let pre_pending_len = chain.len();
        if let Some(pending) = self.pending.take() {
            chain.push_str(&pending.text);
            let chrono_unsafe = trailing_credential_context(&chain);
            let chrono_clean = (chain.len() - chrono_unsafe.len()).saturating_sub(pre_pending_len);
            let mut adjacency = self.emitted_context.clone();
            let adjacency_prefix_len = adjacency.len();
            adjacency.push_str(&pending.text);
            let adjacency_unsafe = trailing_credential_context(&adjacency);
            let adjacency_clean =
                (adjacency.len() - adjacency_unsafe.len()).saturating_sub(adjacency_prefix_len);
            let clean_in_pending = chrono_clean.min(adjacency_clean).min(pending.text.len());
            let (safe, unsafe_fragments) =
                split_stream_fragments(pending.fragments, clean_in_pending);
            self.emit_original(safe);
            self.emit_redacted(unsafe_fragments);
            let held_unsafe = &pending.text[clean_in_pending..];
            if !held_unsafe.is_empty() {
                let mut merged = self.emitted_context.clone();
                merged.push_str(held_unsafe);
                let carried_emitted = trailing_credential_context(&merged);
                // Same 64-KiB lookbehind bound as the dropped chain: an
                // emitted marker that opens a line credential (`Authorization:`)
                // can grow this context by a full held delta each dropped
                // newline, so an unterminated candidate past the cap fails
                // closed instead of pinning unbounded provider-controlled bytes.
                if carried_emitted.len() > MAX_PENDING_STREAM_BYTES {
                    self.suppress_remaining();
                    return;
                }
                self.emitted_context = carried_emitted.to_string();
            }
            // The dropped chain carries the unsafe tail of the full
            // chronological chain (which already folds in any emitted/dropped
            // prefix that reached into the held text).
            let chrono_unsafe_start = chain.len() - chrono_unsafe.len();
            chain = chain[chrono_unsafe_start..].to_string();
        }
        chain.push_str(dropped);
        let context = trailing_credential_context(&chain);
        if context.len() > MAX_PENDING_STREAM_BYTES {
            self.suppressing = true;
            self.dropped_context = String::new();
            return;
        }
        self.dropped_context = context.to_string();
    }

    /// Fails closed: suppresses all subsequent emitted output. Used when the
    /// adapter cannot safely reason about content it does not model (an
    /// unsupported item carrying multiple independent credential markers).
    pub(crate) fn suppress_remaining(&mut self) {
        self.pending = None;
        self.dropped_context = String::new();
        self.emitted_context = String::new();
        self.suppressing = true;
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
        // Each context is its own adjacency chain (the emitted id's record,
        // the dropped reasoning's marker) and is judged separately: joining
        // both at once would insert one chain's bytes between the other's
        // marker and its continuation and miss the match.
        for context in [&self.emitted_context, &self.dropped_context] {
            if context.is_empty() && self.pending.is_none() {
                continue;
            }
            let mut joined = context.clone();
            if let Some(pending) = &self.pending {
                joined.push_str(&pending.text);
            }
            joined.push_str(message);
            if redact_text(&joined) != joined {
                return REDACTED.to_string();
            }
        }
        redact_text(message)
    }

    /// Redacts a provider-derived diagnostic the adapter will then wrap in its
    /// own prose. Wrapping inserts adapter text — and, for a serde detail, that
    /// library's own prose and quoting — between a held credential marker and
    /// the continuation the diagnostic quotes, so the joined-form scan
    /// [`Self::redact_terminal_failure_text`] performs cannot see the pair
    /// rejoin and reads the join as clean. While any lookbehind context is live
    /// the quoted provider bytes are therefore replaced whole rather than
    /// scanned; with nothing held there is no marker for them to complete, so
    /// the ordinary stateless redaction applies and the diagnostic keeps its
    /// content.
    pub(crate) fn redact_wrapped_provider_detail(&self, detail: &str) -> String {
        if self.suppressing
            || self.pending.is_some()
            || !self.emitted_context.is_empty()
            || !self.dropped_context.is_empty()
        {
            return REDACTED.to_string();
        }
        redact_text(detail)
    }

    /// Redacts the final envelope text like a terminal failure message, then
    /// resolves the dropped-reasoning chain through it: after this text, a
    /// dropped-marker candidate has either completed inside it (and was
    /// suppressed just now), been broken by it, or — only when the candidate
    /// is still in progress at the text's end — remains live. Consuming the
    /// resolved chain keeps it from misfiring on the clean provider ids that
    /// follow: with the broken chain still live, an id beginning `key=` would
    /// be reassembled as `api_key=` despite the intervening text.
    pub(crate) fn redact_final_envelope_text(&mut self, text: &str) -> String {
        let redacted = self.redact_terminal_failure_text(text);
        if !self.suppressing {
            // Both chains resolve through the final text: the emitted
            // thread-id chain because the text's bytes now sit between the id
            // record and every later field (a clean text breaks that
            // adjacency), and the dropped chain because its marker either
            // completed inside the text or was broken by it.
            let emitted = std::mem::take(&mut self.emitted_context);
            self.emitted_context = self.resolve_context_through(emitted, text);
            let dropped = std::mem::take(&mut self.dropped_context);
            self.dropped_context = self.resolve_context_through(dropped, text);
        }
        redacted
    }

    /// Resolves one lookbehind chain through the final envelope text: a
    /// candidate the text completed was suppressed with it, a candidate the
    /// text broke cannot be continued by later fields, and only a candidate
    /// still in progress at the text's end stays live (its updated suffix is
    /// returned). An unsafe suffix past the pending byte cap is an unresolved
    /// oversized candidate and fails closed into suppression.
    fn resolve_context_through(&mut self, context: String, text: &str) -> String {
        if context.is_empty() {
            return context;
        }
        let context_length = context.len();
        let mut joined = context;
        joined.push_str(text);
        if unsafe_stream_suffix_start(&joined).is_some_and(|start| start < context_length) {
            let live = trailing_credential_context(&joined);
            if live.len() > MAX_PENDING_STREAM_BYTES {
                self.suppressing = true;
                return String::new();
            }
            return live.to_string();
        }
        String::new()
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
        // Each context is its own adjacency chain, judged separately (see
        // `redact_terminal_failure_text`).
        for context in [&self.emitted_context, &self.dropped_context] {
            if context.is_empty() && held.is_empty() && preceding.is_empty() {
                continue;
            }
            let mut joined =
                String::with_capacity(context.len() + held.len() + preceding.len() + value.len());
            joined.push_str(context);
            joined.push_str(held);
            joined.push_str(preceding);
            joined.push_str(value);
            if redact_text(&joined) != joined {
                return true;
            }
        }
        false
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
            // A live lookbehind chain (emitted thread id, dropped provider
            // text) ties the held text to bytes outside the stream; the
            // boundary forces a decision, so a chained candidate is
            // suppressed whole and every chain is spent by the flush. Chains
            // with no held text survive the boundary — nothing was emitted,
            // so adjacency is unchanged.
            let chained = self.pending_extends_a_chain(&pending.text);
            self.emitted_context.clear();
            self.dropped_context.clear();
            if chained || stream_candidate_starts_at_zero(&pending.text) {
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

    /// Whether the held text is chained to any live lookbehind context — a
    /// candidate begins at the joined start or inside the context and runs
    /// into the held bytes, so the held text may be a credential
    /// continuation.
    fn pending_extends_a_chain(&self, pending_text: &str) -> bool {
        for context in [&self.emitted_context, &self.dropped_context] {
            if context.is_empty() {
                continue;
            }
            let mut joined = String::with_capacity(context.len() + pending_text.len());
            joined.push_str(context);
            joined.push_str(pending_text);
            if stream_candidate_starts_at_zero(&joined)
                || unsafe_stream_suffix_start(&joined).is_some_and(|start| start < context.len())
            {
                return true;
            }
        }
        false
    }

    /// Flushes already-decoded text when no later provider text can extend it.
    pub(crate) fn finish(&mut self) {
        self.suppressing = false;
        // Terminal: judged on each chain's joined form so held text
        // completing a credential begun in an already-emitted field (the
        // thread id) or in dropped provider text is suppressed; no chain
        // outlives the terminal either way.
        let emitted = std::mem::take(&mut self.emitted_context);
        let dropped = std::mem::take(&mut self.dropped_context);
        if let Some(pending) = self.pending.take() {
            let dirty = ["", emitted.as_str(), dropped.as_str()]
                .iter()
                .any(|context| {
                    let mut joined = String::with_capacity(context.len() + pending.text.len());
                    joined.push_str(context);
                    joined.push_str(&pending.text);
                    redact_text(&joined) != joined
                });
            if dirty {
                self.emit_redacted(pending.fragments);
            } else {
                self.emit_original(pending.fragments);
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
        // Each live chain is judged in turn; a chain that consumes the delta
        // (holding or suppressing it) protects the other implicitly, since
        // the held text is joined with every chain again on later deltas and
        // at flush points.
        let emitted = std::mem::take(&mut self.emitted_context);
        if !emitted.is_empty() {
            let (consumed, live) =
                self.delta_against_context(emitted, field, index, correlation.clone(), &text);
            self.emitted_context = live;
            if consumed {
                return;
            }
        }
        let dropped = std::mem::take(&mut self.dropped_context);
        if !dropped.is_empty() {
            let (consumed, live) =
                self.delta_against_context(dropped, field, index, correlation.clone(), &text);
            self.dropped_context = live;
            if consumed {
                return;
            }
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

    /// Processes one delta while a lookbehind chain (the emitted thread id's
    /// suffix or dropped provider text's suffix) is live. Decisions are made
    /// on the joined form `context + pending + delta` and emissions are
    /// mapped back into pending space (the context itself appears in no
    /// stream output and is never emitted). Returns whether the delta was
    /// consumed here (held or suppressed) plus the chain's surviving context;
    /// an unconsumed delta with a spent chain falls through to the next chain
    /// or to ordinary lookbehind processing.
    fn delta_against_context(
        &mut self,
        context: String,
        field: StreamField,
        index: u32,
        correlation: C,
        text: &str,
    ) -> (bool, String) {
        let context_length = context.len();
        let mut joined = context.clone();
        if let Some(pending) = &self.pending {
            joined.push_str(&pending.text);
        }
        joined.push_str(text);
        let unsafe_start = unsafe_stream_suffix_start(&joined);
        let candidate = stream_candidate_starts_at_zero(&joined);
        if !candidate && !unsafe_start.is_some_and(|start| start < context_length) {
            // The join resolved clean: no candidate begins inside the chain's
            // suffix anymore, so adjacency to it is no longer a hazard and
            // the context is spent.
            return (false, String::new());
        }
        let mut pending = self.pending.take().unwrap_or(PendingStreamText {
            fragments: Vec::new(),
            text: String::new(),
        });
        if !text.is_empty() {
            pending.fragments.push(StreamFragment {
                field,
                index,
                correlation,
                text: text.to_string(),
            });
            pending.text.push_str(text);
        }
        match unsafe_start {
            // A candidate begun in (or spanning) the context is still in
            // progress at the joined end; its value bytes may follow, so the
            // held text cannot be emitted or suppressed piecewise yet.
            Some(start) if start < context_length => {
                self.hold_or_suppress(pending);
                (true, context)
            }
            // A candidate begun in the context completed within the join and
            // a distinct unsafe suffix follows: suppress the completed
            // portion's pending bytes whole and hold the tail as a fresh
            // candidate of its own — the context is consumed by the
            // suppression, which also breaks reader adjacency.
            Some(start) => {
                let pending_split = start - context_length;
                let (completed, tail) = split_stream_fragments(pending.fragments, pending_split);
                self.emit_redacted(completed);
                self.hold_or_suppress(PendingStreamText {
                    fragments: tail,
                    text: pending.text[pending_split..].to_string(),
                });
                (true, String::new())
            }
            // The whole join is a completed candidate: every held byte is the
            // credential's continuation, suppressed whole.
            None => {
                self.emit_redacted(pending.fragments);
                (true, String::new())
            }
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
        || pem_private_key_starts_at_zero(text)
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
    if let Some(start) = pem_private_key_unsafe_start(text) {
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
            if separator == text.len() {
                // A bare trailing name; the separator and value may still arrive.
                // Earliest in-progress for this name, so stop scanning it.
                fold(start);
                break;
            } else if matches!(text.as_bytes().get(separator), Some(b'=' | b':')) {
                let (_, token_start, value_end) =
                    credential_value_bounds(text, separator + 1, termination);
                let consumed = value_end.max(token_start);
                if consumed == text.len() || partial_triple_open(text, separator + 1) {
                    fold(start);
                    break;
                }
                // The value terminated within the fragment; skip past it rather
                // than rescanning its interior. A `name=…` nested inside another
                // credential value cannot be an earlier unterminated candidate,
                // so this keeps a hostile fragment (many `name={` runs) linear.
                offset = consumed.max(after_name);
            } else {
                offset = after_name;
            }
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
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if !matches!(byte, b'=' | b':') {
            index += 1;
            continue;
        }
        let separator = index;
        let is_colon = byte == b':';
        // The double-quoted-colon exemption mirrors the stateless scanner:
        // only a position the JSON scanner owns is left to it.
        if let Some((identifier, quote)) = trailing_identifier(&text[..separator])
            && !(quote == Some('"')
                && is_colon
                && json_key_can_start_at(text, identifier.as_ptr() as usize - base - 1))
            && credential_key(identifier)
        {
            let termination = if credential_key_is_free_form(identifier) {
                ValueTermination::Line
            } else {
                ValueTermination::Token
            };
            let (_, token_start, value_end) =
                credential_value_bounds(text, separator + 1, termination);
            let consumed = value_end.max(token_start);
            if consumed == text.len() || partial_triple_open(text, separator + 1) {
                let content = identifier.as_ptr() as usize - base;
                fold(if quote.is_some() {
                    content - 1
                } else {
                    content
                });
            }
            // Skip past this value rather than rescanning its interior, so a
            // hostile fragment (many `secret={` runs) stays linear instead of
            // walking the remaining suffix once per separator.
            index = consumed.max(separator + 1);
        } else {
            index += 1;
        }
    }
    // A trailing identifier with no separator yet — a separator may arrive in
    // the next delta. When a bare trailing name is immediately preceded by an
    // unescaped opening quote (a quoted key whose closing quote has not arrived,
    // `…"api_key`), hold from that quote so the rejoined `"api_key" = value` is
    // recognized rather than emitted with the opener stripped.
    let end_trimmed = text.trim_end_matches([' ', '\t']);
    if let Some((identifier, quote)) = trailing_identifier(end_trimmed)
        && credential_key(identifier)
    {
        let content = identifier.as_ptr() as usize - base;
        let start = match quote {
            Some(_) => content - 1,
            None => opening_quote_before(text, content).unwrap_or(content),
        };
        fold(start);
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
    while let Some(relative) = find_ascii_case_insensitive(&text[offset..], marker) {
        let start = offset + relative;
        let value_start = start + marker.len();
        let (_, token_start, value_end) = credential_value_bounds(text, value_start, termination);
        let consumed = value_end.max(token_start);
        // The first unterminated occurrence begins the unsafe suffix: its value
        // spans to the text end, so every later occurrence sits inside it and
        // cannot start earlier. Returning here (rather than recording the last
        // match) also stops the scan, and skipping past a terminated value's end
        // avoids re-walking its interior — so a hostile fragment with many
        // unterminated containers (`secret={secret={…`) stays linear instead of
        // running a full balanced-container scan per occurrence.
        if consumed == text.len() {
            return Some(start);
        }
        offset = consumed.max(value_start);
    }
    None
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
