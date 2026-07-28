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
const SPACE_SEPARATED_CREDENTIAL_FLAGS: &[&str] = &["--password", "--api-key", "--passphrase"];
const CURL_USER_FLAGS: &[&str] = &["-u", "--user"];
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
    "passphrase",
    "passwd",
    "_pwd",
    "secret",
    "credential",
    "token",
    "signing_key",
    "encryption_key",
    "ssh_key",
    "hmac_key",
    "license_key",
    "sk-",
    "eyJ",
    "://",
    "-u ",
    "--user",
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
    sanitized = redact_space_separated_flags(&sanitized);
    sanitized = redact_url_userinfo_passwords(&sanitized);
    sanitized = redact_curl_userinfo_passwords(&sanitized);
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
        "passphrase",
        "passwd",
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
/// the opening quote (`Some('"')` / `Some('\'')`) when the key was quoted.
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
        // The JSON scanner runs first, but malformed quote pairing or an
        // invalid encoded key can make it decline a position whose raw
        // identifier still carries a credential name. Rechecking the already
        // redacted well-formed case is idempotent; exempting it would create a
        // gap where both scanners decline the same key.
        if let Some((identifier, quote)) = trailing_identifier(&remaining[..separator])
            && credential_key(identifier)
        {
            let termination = if quote == Some('"') {
                ValueTermination::Token
            } else if credential_key_is_free_form(identifier) {
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

/// Redacts the value of a credential-bearing long option whose argument is
/// separated by horizontal whitespace (`--password value`). Assignment forms
/// with `=` remain owned by the ordinary marker and identifier scanners.
fn redact_space_separated_flags(text: &str) -> String {
    let mut sanitized = text.to_string();
    for flag in SPACE_SEPARATED_CREDENTIAL_FLAGS {
        sanitized = redact_space_separated_flag(&sanitized, flag);
    }
    sanitized
}

fn redact_space_separated_flag(text: &str, flag: &str) -> String {
    let mut remaining = text;
    let mut output = String::with_capacity(text.len());
    while let Some(index) = find_ascii_case_insensitive(remaining, flag) {
        let after_flag = index + flag.len();
        let boundary_before = index == 0
            || remaining[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        let whitespace = remaining[after_flag..]
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        if boundary_before && whitespace > 0 {
            output.push_str(&remaining[..after_flag]);
            let (prefix, token_start, value_end) =
                credential_value_bounds(remaining, after_flag, ValueTermination::Token);
            output.push_str(prefix);
            output.push_str(REDACTED);
            remaining = &remaining[value_end.max(token_start)..];
        } else {
            output.push_str(&remaining[..after_flag]);
            remaining = &remaining[after_flag..];
        }
    }
    output.push_str(remaining);
    output
}

/// Redacts the password component of URL userinfo while retaining the scheme,
/// username, authority delimiter, host, and path byte-for-byte.
fn redact_url_userinfo_passwords(text: &str) -> String {
    let mut remaining = text;
    let mut output = String::with_capacity(text.len());
    while let Some(separator) = remaining.find("://") {
        let authority_start = separator + 3;
        let authority_end = remaining[authority_start..]
            .find(is_url_authority_boundary)
            .map_or(remaining.len(), |length| authority_start + length);
        let authority = &remaining[authority_start..authority_end];
        let Some(at) = authority.rfind('@') else {
            output.push_str(&remaining[..authority_start]);
            remaining = &remaining[authority_start..];
            continue;
        };
        let Some(colon) = authority[..at].find(':') else {
            output.push_str(&remaining[..authority_start + at + 1]);
            remaining = &remaining[authority_start + at + 1..];
            continue;
        };
        let password_start = authority_start + colon + 1;
        let password_end = authority_start + at;
        output.push_str(&remaining[..password_start]);
        output.push_str(REDACTED);
        remaining = &remaining[password_end..];
    }
    output.push_str(remaining);
    output
}

fn is_url_authority_boundary(character: char) -> bool {
    character.is_whitespace() || matches!(character, '/' | '?' | '#' | '"' | '\'' | ',' | ';')
}

/// Redacts the password inside curl's `-u user:password` and
/// `--user user:password` arguments while retaining the user name and option
/// spelling. Quoted arguments consume through their matching quote.
fn redact_curl_userinfo_passwords(text: &str) -> String {
    let mut sanitized = text.to_string();
    for flag in CURL_USER_FLAGS {
        sanitized = redact_curl_userinfo_password(&sanitized, flag);
    }
    sanitized
}

fn redact_curl_userinfo_password(text: &str, flag: &str) -> String {
    let mut remaining = text;
    let mut output = String::with_capacity(text.len());
    while let Some(index) = remaining.find(flag) {
        let after_flag = index + flag.len();
        let boundary_before = index == 0
            || remaining[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        let whitespace = remaining[after_flag..]
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        if !boundary_before || whitespace == 0 {
            output.push_str(&remaining[..after_flag]);
            remaining = &remaining[after_flag..];
            continue;
        }
        let argument_start = after_flag + whitespace;
        let opening_quote = remaining[argument_start..]
            .chars()
            .next()
            .filter(|character| matches!(character, '"' | '\''));
        let body_start = argument_start + opening_quote.map_or(0, char::len_utf8);
        let argument_end = opening_quote.map_or_else(
            || {
                remaining[body_start..]
                    .find(char::is_whitespace)
                    .map_or(remaining.len(), |length| body_start + length)
            },
            |quote| quoted_value_end(remaining, body_start, quote),
        );
        let Some(colon) = remaining[body_start..argument_end].find(':') else {
            output.push_str(&remaining[..argument_end]);
            remaining = &remaining[argument_end..];
            continue;
        };
        let password_start = body_start + colon + 1;
        output.push_str(&remaining[..password_start]);
        output.push_str(REDACTED);
        remaining = &remaining[argument_end..];
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

fn json_scanner_claims_credential_key_at(text: &str, target: usize) -> bool {
    let mut offset = 0;
    while let Some(relative_start) = text[offset..].find('"') {
        let key_start = offset + relative_start;
        if key_start > target {
            return false;
        }
        if !json_key_can_start_at(text, key_start) {
            offset = key_start + 1;
            continue;
        }
        let key_end = quoted_value_end(text, key_start + 1, '"');
        if key_end == text.len() {
            return false;
        }
        let encoded_key = &text[key_start..=key_end];
        let Ok(key) = serde_json::from_str::<String>(encoded_key) else {
            if key_start == target {
                return false;
            }
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
        if key_start == target {
            return text.as_bytes().get(whitespace_end) == Some(&b':') && credential_key(&key);
        }
        offset = key_end + 1;
    }
    false
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
    let normalized = key
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
        "passphrase",
        "passwd",
        "secret",
        "cookie",
    ]
    .iter()
    .any(|shape| normalized.contains(shape))
        || normalized == "token"
        || normalized.ends_with("token")
        || normalized == "pwd"
        || normalized.ends_with("pwd")
        || [
            "signingkey",
            "encryptionkey",
            "sshkey",
            "hmackey",
            "licensekey",
        ]
        .iter()
        .any(|shape| normalized.contains(shape))
}

fn credential_identifier_could_extend_to_credential(identifier: &str) -> bool {
    let lower = identifier.to_ascii_lowercase();
    let (qualified, tail) = lower
        .rfind(['_', '-'])
        .map_or((false, lower.as_str()), |separator| {
            (true, &lower[separator + 1..])
        });
    if tail.is_empty() {
        return false;
    }
    ["token", "passwd", "pwd", "passphrase"]
        .iter()
        .any(|shape| tail.len() < shape.len() && shape.starts_with(tail))
        || (qualified && tail.len() < "key".len() && "key".starts_with(tail))
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

enum FlushContinuation {
    None,
    One(String),
    Ambiguous,
}

struct StreamFragment<C> {
    field: StreamField,
    index: u32,
    correlation: C,
    text: String,
}

#[derive(Clone, Copy)]
enum PendingBasis {
    RecomputableCandidate,
    OpaqueCandidateAtZero,
    ContextContinuation,
}

struct PendingStreamText<C> {
    fragments: Vec<StreamFragment<C>>,
    text: String,
    next_rescan_len: usize,
    basis: PendingBasis,
}

impl<C> PendingStreamText<C> {
    fn candidate(fragments: Vec<StreamFragment<C>>, text: String) -> Self {
        Self::after_scan(fragments, text, PendingBasis::RecomputableCandidate)
    }

    fn opaque_candidate(fragments: Vec<StreamFragment<C>>, text: String) -> Self {
        Self::after_scan(fragments, text, PendingBasis::OpaqueCandidateAtZero)
    }

    fn context_continuation(fragments: Vec<StreamFragment<C>>, text: String) -> Self {
        Self::after_scan(fragments, text, PendingBasis::ContextContinuation)
    }

    /// Builds held state immediately after `text` received a full
    /// classification. A still-unresolved candidate is classified again only
    /// after its byte length doubles; the geometric series bounds aggregate
    /// rescanned bytes while every intervening delta remains held. `basis`
    /// records whether offset zero is already known to begin the candidate:
    /// recomputing that fact after the candidate terminates before a clean tail
    /// would otherwise forget why the bytes were held and release its value.
    fn after_scan(fragments: Vec<StreamFragment<C>>, text: String, basis: PendingBasis) -> Self {
        let next_rescan_len = text.len().saturating_mul(2).max(text.len() + 1);
        Self {
            fragments,
            text,
            next_rescan_len,
            basis,
        }
    }

    fn mark_scanned(&mut self) {
        self.next_rescan_len = self.text.len().saturating_mul(2).max(self.text.len() + 1);
    }

    fn push(&mut self, fragment: StreamFragment<C>) {
        self.text.push_str(&fragment.text);
        self.fragments.push(fragment);
    }
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
struct PendingRescanWork {
    classifications: usize,
    bytes: usize,
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
    #[cfg(test)]
    pending_rescan_work: PendingRescanWork,
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
            #[cfg(test)]
            pending_rescan_work: PendingRescanWork {
                classifications: 0,
                bytes: 0,
            },
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

    /// Whether the sink has entered fail-closed suppression (every subsequent
    /// emission becomes `[redacted]` and the match-only contexts are cleared).
    #[cfg(test)]
    pub(crate) fn is_suppressing(&self) -> bool {
        self.suppressing
    }

    #[cfg(test)]
    fn pending_rescan_work(&self) -> &PendingRescanWork {
        &self.pending_rescan_work
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
            // suppressed whole. Its still-live candidate suffix is retained as
            // match-only context, so a value following the boundary remains a
            // continuation instead of being released after its marker was
            // destroyed. Chains with no held text survive the boundary —
            // nothing was emitted, so adjacency is unchanged.
            let chained = self.pending_extends_a_chain(&pending.text);
            let continuation = self.flush_continuation(&pending.text);
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
            match continuation {
                FlushContinuation::None => {}
                FlushContinuation::One(context) => self.emitted_context = context,
                FlushContinuation::Ambiguous => self.suppress_remaining(),
            }
        }
    }

    fn flush_continuation(&self, pending_text: &str) -> FlushContinuation {
        let mut live = Vec::new();
        let standalone = trailing_credential_context(pending_text);
        if !standalone.is_empty() {
            live.push(standalone.to_string());
        }
        for context in [&self.emitted_context, &self.dropped_context] {
            if context.is_empty() {
                continue;
            }
            let mut joined = String::with_capacity(context.len() + pending_text.len());
            joined.push_str(context);
            joined.push_str(pending_text);
            let continuation = trailing_credential_context(&joined);
            if !continuation.is_empty() && !live.iter().any(|known| known == continuation) {
                live.push(continuation.to_string());
            }
        }
        match live.as_slice() {
            [] => FlushContinuation::None,
            [only] => FlushContinuation::One(only.clone()),
            _ => FlushContinuation::Ambiguous,
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
        if self.defer_pending_rescan(field, index, correlation.clone(), &text) {
            return;
        }
        // Each live chain is judged in turn; a chain that consumes the delta
        // (holding or suppressing it) protects the other implicitly, since
        // the held text is joined with every chain again at its next geometric
        // checkpoint and at flush points.
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
            if !text.is_empty() {
                pending.push(StreamFragment {
                    field,
                    index,
                    correlation,
                    text,
                });
            }
            self.resolve_scanned_pending(pending);
            return;
        }

        if let Some(unsafe_start) = unsafe_stream_suffix_start(&text) {
            let opaque_origin = unsafe_start == 0 && escaped_candidate_origin_is_opaque(&text);
            let fragment = StreamFragment {
                field,
                index,
                correlation,
                text: text.clone(),
            };
            let (safe, unsafe_fragments) = split_stream_fragments(vec![fragment], unsafe_start);
            self.emit_original(safe);
            let held_text = text[unsafe_start..].to_string();
            let pending = if opaque_origin {
                PendingStreamText::opaque_candidate(unsafe_fragments, held_text)
            } else {
                PendingStreamText::candidate(unsafe_fragments, held_text)
            };
            self.hold_or_suppress(pending);
        } else {
            self.emit(field, index, correlation, redact_text(&text));
        }
    }

    /// Appends an intervening delta without reclassifying the whole held
    /// candidate. The bytes remain unavailable to every output channel; a
    /// full decision is deferred until the held length doubles, while crossing
    /// the hard cap always forces a final classification before suppression.
    fn defer_pending_rescan(
        &mut self,
        field: StreamField,
        index: u32,
        correlation: C,
        text: &str,
    ) -> bool {
        let Some(pending) = &mut self.pending else {
            return false;
        };
        let combined_len = pending.text.len().saturating_add(text.len());
        if combined_len >= pending.next_rescan_len || combined_len > MAX_PENDING_STREAM_BYTES {
            return false;
        }
        if !text.is_empty() {
            pending.push(StreamFragment {
                field,
                index,
                correlation,
                text: text.to_string(),
            });
        }
        true
    }

    fn resolve_scanned_pending(&mut self, pending: PendingStreamText<C>) {
        self.record_pending_rescan(pending.text.len());
        let candidate = matches!(pending.basis, PendingBasis::OpaqueCandidateAtZero)
            || stream_candidate_starts_at_zero(&pending.text);
        let unsafe_start = unsafe_stream_suffix_start(&pending.text);
        match (candidate, unsafe_start) {
            (true, Some(0)) => {
                let mut pending = pending;
                pending.mark_scanned();
                self.hold_or_suppress(pending);
            }
            (true, Some(unsafe_start)) => {
                let (redacted, unsafe_fragments) =
                    split_stream_fragments(pending.fragments, unsafe_start);
                self.emit_redacted(redacted);
                self.hold_or_suppress(PendingStreamText::candidate(
                    unsafe_fragments,
                    pending.text[unsafe_start..].to_string(),
                ));
            }
            (true, None) => self.emit_redacted(pending.fragments),
            (false, Some(unsafe_start)) => {
                let (safe, unsafe_fragments) =
                    split_stream_fragments(pending.fragments, unsafe_start);
                self.emit_original(safe);
                self.hold_or_suppress(PendingStreamText::candidate(
                    unsafe_fragments,
                    pending.text[unsafe_start..].to_string(),
                ));
            }
            (false, None) => self.emit_original(pending.fragments),
        }
    }

    /// Charges the two top-level whole-buffer predicates used by one pending
    /// classification. Internal fixed scanner passes are an implementation
    /// detail; the regression bounds how many held bytes reach each classifier.
    fn record_pending_rescan(&mut self, bytes: usize) {
        #[cfg(test)]
        {
            self.pending_rescan_work.classifications += 1;
            self.pending_rescan_work.bytes += bytes.saturating_mul(2);
        }
        #[cfg(not(test))]
        let _ = bytes;
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
        let rescans_pending = self.pending.is_some();
        let mut joined = context.clone();
        if let Some(pending) = &self.pending {
            joined.push_str(&pending.text);
        }
        joined.push_str(text);
        if rescans_pending {
            self.record_pending_rescan(joined.len() - context_length);
        }
        let unsafe_start = unsafe_stream_suffix_start(&joined);
        let candidate = stream_candidate_starts_at_zero(&joined);
        if !candidate && !unsafe_start.is_some_and(|start| start < context_length) {
            // The join resolved clean: no candidate begins inside the chain's
            // suffix anymore, so adjacency to it is no longer a hazard and
            // the context is spent.
            return (false, String::new());
        }
        let mut pending = self
            .pending
            .take()
            .unwrap_or_else(|| PendingStreamText::context_continuation(Vec::new(), String::new()));
        if !text.is_empty() {
            pending.push(StreamFragment {
                field,
                index,
                correlation,
                text: text.to_string(),
            });
        }
        match unsafe_start {
            // A candidate begun in (or spanning) the context is still in
            // progress at the joined end; its value bytes may follow, so the
            // held text cannot be emitted or suppressed piecewise yet.
            Some(start) if start < context_length => {
                pending.mark_scanned();
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
                self.hold_or_suppress(PendingStreamText::candidate(
                    tail,
                    pending.text[pending_split..].to_string(),
                ));
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
            Some(decoded) => {
                decoded != text
                    && (stream_candidate_starts_at_zero(&decoded)
                        || unsafe_stream_suffix_start(&decoded).is_some())
            }
            None => true,
        }
        || trailing_partial_unicode_escape(text) == Some(0)
        || spaced_credential_starts_at_zero(text)
        || space_separated_flag_candidate(text)
        || url_userinfo_candidate(text)
        || curl_userinfo_candidate(text)
        || identifier_assignment_candidate(text)
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

fn space_separated_flag_candidate(text: &str) -> bool {
    space_separated_flag_unsafe_start(text) == Some(0) || redact_space_separated_flags(text) != text
}

fn space_separated_flag_unsafe_start(text: &str) -> Option<usize> {
    let mut earliest: Option<usize> = None;
    for flag in SPACE_SEPARATED_CREDENTIAL_FLAGS {
        let prefix_length = trailing_marker_prefix(text, flag, true);
        if prefix_length > 0 {
            let start = text.len() - prefix_length;
            earliest = Some(earliest.map_or(start, |current| current.min(start)));
        }
        let mut offset = 0;
        while let Some(relative) = find_ascii_case_insensitive(&text[offset..], flag) {
            let start = offset + relative;
            let after_flag = start + flag.len();
            let boundary_before = start == 0
                || text[..start]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace);
            let whitespace = text[after_flag..]
                .bytes()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            if boundary_before && after_flag + whitespace == text.len() {
                earliest = Some(earliest.map_or(start, |current| current.min(start)));
                break;
            }
            if boundary_before && whitespace > 0 {
                let (_, token_start, value_end) =
                    credential_value_bounds(text, after_flag, ValueTermination::Token);
                if value_end.max(token_start) == text.len() {
                    earliest = Some(earliest.map_or(start, |current| current.min(start)));
                    break;
                }
            }
            offset = after_flag;
        }
    }
    earliest
}

fn url_userinfo_candidate(text: &str) -> bool {
    url_userinfo_unsafe_start(text) == Some(0) || redact_url_userinfo_passwords(text) != text
}

fn url_userinfo_unsafe_start(text: &str) -> Option<usize> {
    let prefix_length = trailing_marker_prefix(text, "://", false);
    let mut earliest = (prefix_length > 0).then_some(text.len() - prefix_length);
    let mut offset = 0;
    while let Some(relative) = text[offset..].find("://") {
        let separator = offset + relative;
        let authority_start = separator + 3;
        let authority_end = text[authority_start..]
            .find(is_url_authority_boundary)
            .map_or(text.len(), |length| authority_start + length);
        let authority = &text[authority_start..authority_end];
        let has_password = authority
            .rfind('@')
            .is_some_and(|at| authority[..at].contains(':'));
        if authority_end == text.len() && !has_password {
            let start = text[..separator]
                .char_indices()
                .rev()
                .take_while(|(_, character)| {
                    character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
                })
                .last()
                .map_or(separator, |(start, _)| start);
            earliest = Some(earliest.map_or(start, |current| current.min(start)));
        }
        offset = authority_end.max(authority_start);
    }
    earliest
}

fn curl_userinfo_candidate(text: &str) -> bool {
    curl_userinfo_unsafe_start(text) == Some(0) || redact_curl_userinfo_passwords(text) != text
}

fn curl_userinfo_unsafe_start(text: &str) -> Option<usize> {
    let mut earliest: Option<usize> = None;
    for flag in CURL_USER_FLAGS {
        let prefix_length = trailing_marker_prefix(text, flag, false);
        if prefix_length > 0 {
            let start = text.len() - prefix_length;
            earliest = Some(earliest.map_or(start, |current| current.min(start)));
        }
        let mut offset = 0;
        while let Some(relative) = text[offset..].find(flag) {
            let start = offset + relative;
            let after_flag = start + flag.len();
            let boundary_before = start == 0
                || text[..start]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace);
            let whitespace = text[after_flag..]
                .bytes()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            if boundary_before && after_flag == text.len() {
                earliest = Some(earliest.map_or(start, |current| current.min(start)));
                break;
            }
            if !boundary_before || whitespace == 0 {
                offset = after_flag;
                continue;
            }
            let argument_start = after_flag + whitespace;
            let opening_quote = text[argument_start..]
                .chars()
                .next()
                .filter(|character| matches!(character, '"' | '\''));
            let body_start = argument_start + opening_quote.map_or(0, char::len_utf8);
            let argument_end = opening_quote.map_or_else(
                || {
                    text[body_start..]
                        .find(char::is_whitespace)
                        .map_or(text.len(), |length| body_start + length)
                },
                |quote| quoted_value_end(text, body_start, quote),
            );
            if argument_end == text.len() {
                earliest = Some(earliest.map_or(start, |current| current.min(start)));
                break;
            }
            offset = argument_end.max(after_flag);
        }
    }
    earliest
}

/// Whether any complete credential identifier assignment occurs in `text`.
/// The held fragment can begin before the identifier when another conservative
/// lookbehind rule retained the prefix, so candidate recognition cannot assume
/// the credential itself starts at byte zero.
fn identifier_assignment_candidate(text: &str) -> bool {
    if identifier_assignment_unsafe_start(text) == Some(0) {
        return true;
    }
    let base = text.as_ptr() as usize;
    let mut offset = 0;
    while let Some(relative) = text[offset..]
        .bytes()
        .position(|byte| matches!(byte, b'=' | b':'))
    {
        let separator = offset + relative;
        if let Some((identifier, quote)) = trailing_identifier(&text[..separator])
            && credential_key(identifier)
        {
            let content = identifier.as_ptr() as usize - base;
            let start = if quote.is_some() {
                content - 1
            } else {
                content
            };
            let json_scanner_declined =
                quote == Some('"') && !json_scanner_claims_credential_key_at(text, start);
            if start == 0 || json_scanner_declined {
                return true;
            }
        }
        offset = separator + 1;
    }
    false
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
    if let Some(start) = space_separated_flag_unsafe_start(text) {
        earliest = Some(earliest.map_or(start, |current| current.min(start)));
    }
    if let Some(start) = url_userinfo_unsafe_start(text) {
        earliest = Some(earliest.map_or(start, |current| current.min(start)));
    }
    if let Some(start) = curl_userinfo_unsafe_start(text) {
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
        if let Some((identifier, quote)) = trailing_identifier(&text[..separator])
            && credential_key(identifier)
        {
            let termination = if quote == Some('"') {
                ValueTermination::Token
            } else if credential_key_is_free_form(identifier) {
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
        && (credential_key(identifier)
            || credential_identifier_could_extend_to_credential(identifier))
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

/// Whether escape decoding exposed an unsafe suffix away from original offset
/// zero, so the held origin cannot be recomputed after that candidate closes.
/// This provenance stays sticky until the candidate is suppressed; ordinary
/// marker prefixes remain recomputable and can still release when later bytes
/// prove they were harmless text.
fn escaped_candidate_origin_is_opaque(text: &str) -> bool {
    let Some(decoded) = decode_unicode_escapes(text) else {
        return true;
    };
    decoded != text
        && !stream_candidate_starts_at_zero(&decoded)
        && unsafe_stream_suffix_start(&decoded).is_some()
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

#[cfg(test)]
mod tests {

    use signalbox_model_runtime::{Observation, ObservationFact, ObservationSink, TokenUsage};

    use super::{
        MAX_PENDING_STREAM_BYTES, PendingRescanWork, REDACTED, REDACTED_JSON_OBJECT, RedactingSink,
        decode_unicode_escapes, redact_json, redact_text, stream_candidate_starts_at_zero,
        trailing_credential_context, unsafe_stream_suffix_start, unterminated_json_key_start,
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
    const PLANTED_SYNTHETIC_SECRET: &str = "SYNTHETIC-SECRET-SHAPE-COVERAGE";

    fn observation_text(observation: Observation<u8>) -> String {
        match observation.fact {
            ObservationFact::TextDelta { text, .. }
            | ObservationFact::ThinkingDelta { text, .. } => text,
            _ => String::new(),
        }
    }

    #[track_caller]
    fn assert_two_delta_split_redacts(first: &str, second: &str) {
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: first.to_string(),
                },
            });
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 1,
                    text: second.to_string(),
                },
            });
            sink.finish();
        }
        let emitted = observed
            .into_iter()
            .map(observation_text)
            .collect::<String>();

        assert!(
            !emitted.contains(PLANTED_SYNTHETIC_SECRET),
            "split credential value must not survive stateful redaction: {emitted}"
        );
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

    /// A non-ASCII delimiter immediately before a composite credential
    /// identifier must be advanced by its full UTF-8 width; a one-byte advance
    /// would slice mid-character and panic the executing task.
    #[test]
    fn inv_035_multibyte_delimiter_before_credential_does_not_panic() {
        let output = redact_text("éAWS_SECRET_ACCESS_KEY=opaque-multibyte-value done");

        assert!(!output.contains("opaque-multibyte-value"));
        assert!(output.contains(REDACTED));
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

    /// The JSON-key suffix scan stays linear on text ending in many
    /// backslash-escaped quotes: an escaped quote cannot begin a JSON key, and
    /// the eligibility check short-circuits before the quoted-value walk, so no
    /// position rescans the remaining suffix. A quadratic scan would not finish.
    #[test]
    fn json_key_suffix_scan_is_linear_on_repeated_escaped_quotes() {
        let hostile = format!("prose {}", "\\\"".repeat(500_000));

        assert_eq!(unterminated_json_key_start(&hostile), None);
    }

    /// INV-035: a private-key PEM block standing on its own — no `private_key=`
    /// member in front of it — is consumed whole through its matching END
    /// marker. No assignment introduces it, so the marker, spaced-name, and
    /// identifier scanners never see it.
    #[test]
    fn inv_035_redacts_a_standalone_pem_private_key_block() {
        let fixture = "note\n-----BEGIN PRIVATE KEY-----\nMIIBstandalone-body-secret\n-----END PRIVATE KEY-----\nsafe-after";
        let output = redact_text(fixture);

        assert!(!output.contains("MIIBstandalone-body-secret"));
        assert!(!output.contains("BEGIN PRIVATE KEY"));
        assert!(output.contains("[redacted]"));
        assert!(output.contains("safe-after"));
    }

    /// INV-035: the labelled private-key variants share the standalone rule,
    /// and an unterminated block is suppressed through the text end rather
    /// than releasing the body it was still emitting.
    #[test]
    fn inv_035_redacts_an_unterminated_openssh_private_key_block() {
        let fixture = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC-openssh-body-secret";
        let output = redact_text(fixture);

        assert_eq!(output, "[redacted]");
    }

    /// A PEM block that is not a private key is ordinary provider output and
    /// is left verbatim: the rule names the credential shape, not all armor.
    #[test]
    fn certificate_pem_block_is_not_redacted() {
        let fixture = "-----BEGIN CERTIFICATE-----\nMIICertificate-body\n-----END CERTIFICATE-----";
        let output = redact_text(fixture);

        assert_eq!(output, fixture);
    }

    /// INV-035: a private-key PEM block arriving across streamed deltas is
    /// held from its armor and suppressed, rather than emitting the header
    /// delta and then the body that follows it.
    #[test]
    fn inv_035_holds_a_pem_private_key_split_across_deltas() {
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.observe(Observation {
                correlation: 3_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: "-----BEGIN PRIVATE KEY".to_string(),
                },
            });
            sink.observe(Observation {
                correlation: 3_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: "-----\nMIIBsplit-delta-body-secret\n".to_string(),
                },
            });
            sink.finish();
        }
        let emitted: Vec<String> = observed.into_iter().map(observation_text).collect();

        assert!(
            !emitted
                .iter()
                .any(|text| text.contains("MIIBsplit-delta-body-secret"))
        );
        assert_eq!(emitted, vec![REDACTED.to_string(), REDACTED.to_string()]);
    }

    /// INV-035: the space-separated label a provider diagnostic prints
    /// (`API key: …`) is recognized like the underscore, hyphenated, and
    /// concatenated spellings the exact-name scanners already carry.
    #[test]
    fn inv_035_redacts_a_space_separated_api_key_label() {
        let output = redact_text("API key: opaque-spaced-label-value tail");

        assert!(!output.contains("opaque-spaced-label-value"));
        assert!(output.contains("[redacted]"));
        assert!(output.contains("tail"));
    }

    /// INV-035: the free-form space-separated `secret key` label consumes its
    /// whole line, since an unquoted passphrase can carry spaces.
    #[test]
    fn inv_035_redacts_a_space_separated_secret_key_label() {
        let output = redact_text("secret key = opaque spaced passphrase\nsafe-line");

        assert!(!output.contains("opaque spaced passphrase"));
        assert!(output.contains("[redacted]"));
        assert!(output.contains("safe-line"));
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

    /// INV-035: malformed JSON quote pairing cannot make the JSON member
    /// scanner and identifier-assignment scanner both decline a covered key.
    #[test]
    fn inv_035_malformed_quoted_credential_keys_are_redacted_on_every_surface() {
        let fixture = format!(r#"{{"x,"client_secret":"{PLANTED_SYNTHETIC_SECRET}"}}"#);
        let text_output = redact_text(&fixture);
        let json_output = redact_json(&fixture);
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let sink = RedactingSink::new(&mut observed);
        let tool_output = sink.redact_tool_arguments("", &fixture);

        assert!(!text_output.contains(PLANTED_SYNTHETIC_SECRET));
        assert!(!json_output.contains(PLANTED_SYNTHETIC_SECRET));
        assert!(!tool_output.contains(PLANTED_SYNTHETIC_SECRET));
    }

    /// INV-035: bare `token` and every singular `*_TOKEN` assignment are
    /// credential-bearing, while plural usage counters remain ordinary data.
    #[test]
    fn inv_035_redacts_bare_and_suffix_token_assignments() {
        let github = redact_text(&format!("GITHUB_TOKEN={PLANTED_SYNTHETIC_SECRET}"));
        let bare = redact_text(&format!("token={PLANTED_SYNTHETIC_SECRET}"));
        let member = redact_json(&format!(
            r#"{{"CI_JOB_TOKEN":"{PLANTED_SYNTHETIC_SECRET}"}}"#
        ));
        let usage = r#"{"input_tokens":11,"output_tokens":7}"#;

        assert_eq!(github, "GITHUB_TOKEN=[redacted]");
        assert_eq!(bare, "token=[redacted]");
        assert_eq!(member, r#"{"CI_JOB_TOKEN":"[redacted]"}"#);
        assert_eq!(redact_json(usage), usage);
    }

    /// INV-035: credential-bearing long options consume a whitespace-separated
    /// argument exactly as their `=` assignment forms do.
    #[test]
    fn inv_035_redacts_space_separated_credential_flags() {
        let password = redact_text(&format!("codex --password {PLANTED_SYNTHETIC_SECRET}"));
        let api_key = redact_text(&format!("codex --api-key {PLANTED_SYNTHETIC_SECRET}"));
        let passphrase = redact_text(&format!("gpg --passphrase {PLANTED_SYNTHETIC_SECRET}"));

        assert_eq!(password, "codex --password [redacted]");
        assert_eq!(api_key, "codex --api-key [redacted]");
        assert_eq!(passphrase, "gpg --passphrase [redacted]");
    }

    /// INV-035: URL userinfo passwords are redacted without rewriting their
    /// scheme, username, host, or path.
    #[test]
    fn inv_035_redacts_scheme_url_userinfo_passwords() {
        let fixture = format!("psql postgres://admin:{PLANTED_SYNTHETIC_SECRET}@db.internal/app");

        assert_eq!(
            redact_text(&fixture),
            "psql postgres://admin:[redacted]@db.internal/app"
        );
    }

    /// INV-035: curl's userinfo option redacts the password while retaining
    /// the option, user name, and destination URL.
    #[test]
    fn inv_035_redacts_curl_userinfo_passwords() {
        let fixture = format!("curl -u admin:{PLANTED_SYNTHETIC_SECRET} https://api.example.test");

        assert_eq!(
            redact_text(&fixture),
            "curl -u admin:[redacted] https://api.example.test"
        );
    }

    /// Ordinary URLs and curl user arguments without a password remain
    /// byte-exact; userinfo redaction is not a generic URL rewrite.
    #[test]
    fn credential_clean_urls_and_curl_usernames_remain_byte_exact() {
        let url = "https://example.test/path?q=one";
        let curl = "curl -u admin https://api.example.test";

        assert_eq!(redact_text(url), url);
        assert_eq!(redact_text(curl), curl);
    }

    /// INV-035: adjacent high-confidence password and cryptographic-key names
    /// use the same assignment policy as the pre-existing credential names.
    #[test]
    fn inv_035_redacts_passwd_pwd_passphrase_and_security_key_assignments() {
        let passwd = redact_text(&format!("passwd={PLANTED_SYNTHETIC_SECRET}"));
        let mysql_pwd = redact_text(&format!("MYSQL_PWD={PLANTED_SYNTHETIC_SECRET}"));
        let passphrase = redact_text(&format!("passphrase={PLANTED_SYNTHETIC_SECRET}"));
        let signing = redact_text(&format!("signing_key={PLANTED_SYNTHETIC_SECRET}"));
        let encryption = redact_text(&format!("encryption_key={PLANTED_SYNTHETIC_SECRET}"));
        let ssh = redact_text(&format!("ssh_key={PLANTED_SYNTHETIC_SECRET}"));
        let hmac = redact_text(&format!("hmac_key={PLANTED_SYNTHETIC_SECRET}"));
        let license = redact_text(&format!("license_key={PLANTED_SYNTHETIC_SECRET}"));

        assert!(!passwd.contains(PLANTED_SYNTHETIC_SECRET));
        assert!(!mysql_pwd.contains(PLANTED_SYNTHETIC_SECRET));
        assert!(!passphrase.contains(PLANTED_SYNTHETIC_SECRET));
        assert!(!signing.contains(PLANTED_SYNTHETIC_SECRET));
        assert!(!encryption.contains(PLANTED_SYNTHETIC_SECRET));
        assert!(!ssh.contains(PLANTED_SYNTHETIC_SECRET));
        assert!(!hmac.contains(PLANTED_SYNTHETIC_SECRET));
        assert!(!license.contains(PLANTED_SYNTHETIC_SECRET));
    }

    /// INV-035: splitting a credential flag inside its name cannot release the
    /// later whitespace-separated value.
    #[test]
    fn inv_035_stream_redacts_a_flag_split_inside_its_name() {
        assert_two_delta_split_redacts(
            "codex --pass",
            &format!("word {PLANTED_SYNTHETIC_SECRET} tail"),
        );
    }

    /// INV-035: splitting URL userinfo immediately before its password cannot
    /// release that password as a standalone clean-looking delta.
    #[test]
    fn inv_035_stream_redacts_a_url_userinfo_password_split() {
        assert_two_delta_split_redacts(
            "postgres://admin:",
            &format!("{PLANTED_SYNTHETIC_SECRET}@db.internal/app"),
        );
    }

    /// INV-035: splitting a suffix-token name cannot separate the recognized
    /// name from the value it marks as a credential.
    #[test]
    fn inv_035_stream_redacts_a_suffix_token_assignment_split_inside_its_name() {
        assert_two_delta_split_redacts(
            "GITHUB_TO",
            &format!("KEN={PLANTED_SYNTHETIC_SECRET} tail"),
        );
    }

    /// INV-035: malformed quoted-key ownership remains consistent across a
    /// delta split instead of releasing the value after a conservative hold.
    #[test]
    fn inv_035_stream_redacts_a_split_malformed_quoted_credential_key() {
        assert_two_delta_split_redacts(
            r#"{"x,"client_sec"#,
            &format!(r#"ret":"{PLANTED_SYNTHETIC_SECRET}"}}"#),
        );
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

    /// INV-035: a TOML basic multiline value whose body embeds three literal
    /// quotes via an escaped quote (`\"` then `""`) is not closed there; the
    /// terminator scan honors the escape and consumes through the real `"""`,
    /// so the body between the escaped run and the real close is suppressed.
    #[test]
    fn inv_035_triple_quote_terminator_honors_escaped_quotes() {
        let fixture = "private_key = \"\"\"pre \\\"\"\" still-secret-body \"\"\"\nsafe-tail";
        let output = redact_text(fixture);

        assert!(!output.contains("still-secret-body"));
        assert!(output.contains("[redacted]"));
        assert!(output.contains("safe-tail"));
    }

    /// INV-035: a quoted TOML key that embeds an escaped quote
    /// (`"client_secret\"suffix"`) has its opening delimiter found escape-aware,
    /// so the full key content is checked against the contains policy and the
    /// credential value is redacted rather than emitted.
    #[test]
    fn inv_035_quoted_key_opening_quote_is_escape_aware() {
        let fixture = "\"client_secret\\\"suffix\" = opaque-embedded-quote-value\ntail";
        let output = redact_text(fixture);

        assert!(!output.contains("opaque-embedded-quote-value"));
        assert!(output.contains("[redacted]"));
        assert!(output.contains("tail"));
    }

    /// INV-035: a single-quoted plaintext key before a colon
    /// (`'api_key': value`) is not a JSON member the double-quoted JSON scanner
    /// can own, so the identifier scanner redacts it instead of exempting it.
    #[test]
    fn inv_035_redacts_single_quoted_colon_assignment() {
        let output = redact_text("'api_key': opaque-single-quote-colon-value tail");

        assert!(!output.contains("opaque-single-quote-colon-value"));
        assert!(output.contains("[redacted]"));
        assert!(output.contains("tail"));
    }

    /// INV-035: a quoted TOML key split before its closing quote, preceded by
    /// text that keeps both the JSON-key heuristic and the `"<name>":` marker
    /// prefixes from recognizing it, is held from its opening quote so the
    /// rejoined `"client_secret" = value` is recognized rather than emitted with
    /// the opener stripped (which left the value unredactable). A bare composite
    /// key is caught only by the identifier scanner, which previously folded
    /// from the name and dropped the opening quote.
    #[test]
    fn inv_035_stream_retains_opening_quote_for_split_toml_key() {
        assert_eq!(unsafe_stream_suffix_start("data \"client_secret"), Some(5));
    }

    /// INV-035: a credential value whose opening `"""` is split after its first
    /// two quotes is held as an in-progress triple opener, not read as a
    /// completed empty quoted value that would release the following body.
    #[test]
    fn inv_035_stream_holds_split_triple_quote_opener() {
        assert_eq!(unsafe_stream_suffix_start("private_key = \"\""), Some(0));
    }

    /// INV-035: unsafe-suffix scanning stays linear on a hostile fragment of
    /// many unterminated credential containers — it must still report the
    /// earliest unsafe byte without a per-occurrence balanced-container rescan
    /// (which was quadratic and could pin the adapter past its deadline).
    #[test]
    fn inv_035_stream_suffix_scan_is_linear_on_repeated_containers() {
        let hostile = "secret={".repeat(20_000);

        assert_eq!(unsafe_stream_suffix_start(&hostile), Some(0));
    }

    /// INV-035: text completing a credential marker begun by an emitted
    /// field's trailing suffix (`api_` in the thread id, `key=value` in the
    /// stream) is suppressed — emitted beside the id it would reconstruct the
    /// credential — while trailing clean text past the completed candidate is
    /// released.
    #[test]
    fn inv_035_emitted_context_suppresses_the_marker_continuation() {
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.seed_emitted_context("api_");
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: "key=opaque-context-value done".to_string(),
                },
            });
            sink.finish();
        }
        let emitted: Vec<String> = observed.into_iter().map(observation_text).collect();

        assert!(
            !emitted
                .iter()
                .any(|text| text.contains("opaque-context-value"))
        );
        assert!(emitted.iter().any(|text| text.contains(REDACTED)));
    }

    /// INV-035: the emitted-field context also governs a terminal failure
    /// message, so a failure text continuing the id's marker suffix is
    /// suppressed whole.
    #[test]
    fn inv_035_emitted_context_suppresses_a_failure_continuation() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);
        sink.seed_emitted_context("api_");

        assert_eq!(
            sink.redact_terminal_failure_text("key=opaque-context-value refused"),
            REDACTED
        );
    }

    /// INV-035: a double-quoted composite credential member embedded after
    /// prose — where the JSON scanner's key predicate rejects it — is taken
    /// by the identifier scanner instead of leaking, while a member in real
    /// JSON position stays owned by the JSON scanner.
    #[test]
    fn inv_035_redacts_quoted_credential_members_embedded_after_prose() {
        let fixture = r#"provider detail: "client_secret":"opaque-embedded-value" tail"#;
        let output = redact_text(fixture);

        assert!(!output.contains("opaque-embedded-value"));
        assert!(output.contains("[redacted]"));
        assert!(output.contains("provider detail:"));
    }

    /// INV-035: a credential marker inside dropped (buffered-delivery
    /// reasoning) bytes governs the final text, so its value continuation is
    /// suppressed whole rather than surfacing as an opaque-but-real secret.
    #[test]
    fn inv_035_dropped_context_suppresses_a_final_text_continuation() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);
        sink.extend_dropped_context("Authorization:");

        assert_eq!(
            sink.redact_terminal_failure_text(" opaque-dropped-value"),
            REDACTED
        );
    }

    /// INV-035: held stream bytes must be judged against the emitted
    /// (thread-id) chain too, not only the dropped chain. A thread id ending
    /// `api_` seeds the emitted chain; a streamed `key` is held because it
    /// continues it; a dropped `=` then arrives. Evaluating `key` against only
    /// the (empty) dropped chain would emit it and later release the value.
    #[test]
    fn inv_035_pending_is_judged_against_the_emitted_chain() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);
        sink.seed_emitted_context("api_");
        // `key` alone is clean, but continues the seeded `api_`; held.
        sink.observe(Observation {
            correlation: 7_u8,
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "key".to_string(),
            },
        });
        sink.extend_dropped_context("=");

        assert_eq!(
            sink.redact_terminal_failure_text("opaque-thread-continuation"),
            REDACTED
        );
    }

    /// INV-035: an emitted marker (`Authorization:`) whose held continuation
    /// plus dropped newlines would grow the carried emitted context past the
    /// lookbehind bound fails closed into suppression rather than pinning
    /// unbounded provider-controlled bytes.
    #[test]
    fn inv_035_oversized_carried_emitted_context_fails_closed() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);
        sink.seed_emitted_context("Authorization:");
        // A held line-credential body larger than the lookbehind bound; a
        // dropped newline then forces the carried emitted context to resolve.
        let body = "a".repeat(MAX_PENDING_STREAM_BYTES + 1);
        sink.observe(Observation {
            correlation: 7_u8,
            fact: ObservationFact::TextDelta {
                index: 0,
                text: body,
            },
        });
        sink.extend_dropped_context("\n");

        // Fail-closed suppression, not an unbounded held context.
        assert!(sink.is_suppressing());
    }

    /// INV-035: a dropped-text marker (an error item's message) governs
    /// streamed deltas too: the value continuation flowing through the delta
    /// machinery is suppressed rather than emitted beside nothing the reader
    /// can see but the provider controls.
    #[test]
    fn inv_035_dropped_context_suppresses_a_streamed_continuation() {
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.extend_dropped_context("api_");
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: "key=opaque-error-continuation done".to_string(),
                },
            });
            sink.finish();
        }
        let emitted: Vec<String> = observed.into_iter().map(observation_text).collect();

        assert!(
            !emitted
                .iter()
                .any(|text| text.contains("opaque-error-continuation"))
        );
        assert!(emitted.iter().any(|text| text.contains(REDACTED)));
    }

    /// INV-035: the dropped chain and the emitted-id chain are judged
    /// separately — clean dropped bytes must not sit between the emitted id's
    /// marker suffix and its continuation and break that match.
    #[test]
    fn inv_035_dropped_text_does_not_break_the_emitted_chain() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);
        sink.seed_emitted_context("api_");
        sink.extend_dropped_context("thinking about the request");

        assert_eq!(
            sink.redact_terminal_failure_text("key=opaque-context-value done"),
            REDACTED
        );
    }

    /// The emitted thread-id chain a clean final text breaks is consumed by
    /// that text: its bytes now sit between the id record and every later
    /// field, so a clean provider id beginning `key=` is NOT rejoined to
    /// `api_` across them.
    #[test]
    fn emitted_chain_broken_by_final_text_releases_later_ids() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);
        sink.seed_emitted_context("api_");

        assert_eq!(
            sink.redact_final_envelope_text("hello there."),
            "hello there."
        );
        assert_eq!(sink.redact_provider_id("", "key=call-7"), "key=call-7");
    }

    /// INV-035: the emitted chain stays live through an empty final text —
    /// nothing intervened between the id record and the fields that follow.
    #[test]
    fn inv_035_emitted_chain_survives_an_empty_final_text() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);
        sink.seed_emitted_context("api_");

        assert_eq!(sink.redact_final_envelope_text(""), "");
        assert_eq!(sink.redact_provider_id("", "key=call-7"), REDACTED);
    }

    /// A dropped-marker chain the final text breaks (`api_` then `hello`)
    /// is consumed by that text: a later provider id beginning `key=` is NOT
    /// reassembled as `api_key=` across the intervening text, so clean ids
    /// keep their fidelity.
    #[test]
    fn dropped_chain_broken_by_final_text_releases_later_ids() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);
        sink.extend_dropped_context("api_");

        assert_eq!(
            sink.redact_final_envelope_text("hello there."),
            "hello there."
        );
        assert_eq!(sink.redact_provider_id("", "key=call-7"), "key=call-7");
    }

    /// INV-035: a dropped-marker chain still in progress at the final text's
    /// end (an empty text resolves nothing; the bare marker alone already
    /// redacts, so even the empty text is suppressed) stays live and governs
    /// the fields that follow.
    #[test]
    fn inv_035_dropped_chain_survives_an_empty_final_text() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);
        sink.extend_dropped_context("Authorization:");

        assert_eq!(sink.redact_final_envelope_text(""), REDACTED);
        assert_eq!(
            sink.redact_provider_id("", " opaque-continuation"),
            REDACTED
        );
    }

    /// INV-035: a dropped-marker candidate completing inside the final text
    /// suppresses that text whole and is consumed by the suppression.
    #[test]
    fn inv_035_dropped_chain_completing_in_final_text_is_consumed() {
        let mut observed: Vec<Observation<u8>> = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);
        sink.extend_dropped_context("api_");

        assert_eq!(
            sink.redact_final_envelope_text("key=opaque-context-value done."),
            REDACTED
        );
        assert_eq!(sink.redact_provider_id("", "call-7"), "call-7");
    }

    /// An id with a credential-clean trailing suffix seeds nothing, and the
    /// following stream stays byte-exact once flushed.
    #[test]
    fn clean_emitted_context_leaves_the_stream_byte_exact() {
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.seed_emitted_context("thread-offline-1");
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: "key=ordinary value.".to_string(),
                },
            });
            sink.finish();
        }
        let emitted: Vec<String> = observed.into_iter().map(observation_text).collect();

        assert_eq!(emitted, vec!["key=ordinary value.".to_string()]);
    }

    /// INV-035: a decoded unsafe suffix held from escaped reasoning remains a
    /// candidate at offset zero when final text completes it. Neither streamed
    /// observations nor terminal capture may retain the planted value.
    #[test]
    fn inv_035_escaped_held_suffix_is_redacted_in_stream_and_terminal_capture() {
        const PLANTED_VALUE: &str =
            "AAAA-SYNTHETIC-SECRET-stream-BBBB safe-tail-that-crosses-checkpoint";
        let raw = format!(r"thinking about \u0063afé and api_key={PLANTED_VALUE}");
        let stateless = redact_text(&raw);
        let mut observed = Vec::new();
        let captured;
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::ThinkingDelta {
                    index: 0,
                    text: r"thinking about \u0063afé and api_key=".to_string(),
                },
            });
            sink.begin_terminal_text_capture();
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 1,
                    text: PLANTED_VALUE.to_string(),
                },
            });
            sink.finish();
            captured = sink.take_terminal_text_capture();
        }
        let streamed = observed
            .into_iter()
            .map(observation_text)
            .collect::<String>();

        assert!(!stateless.contains("SYNTHETIC-SECRET"));
        assert!(!streamed.contains("SYNTHETIC-SECRET"));
        assert!(!captured.contains("SYNTHETIC-SECRET"));
    }

    /// INV-035: once the sink fails closed, terminal flushes and repeatable
    /// usage reports cannot re-arm provider-byte emission.
    #[test]
    fn inv_035_suppression_is_absorbing_across_finish_and_usage() {
        const PLANTED_VALUE: &str = "SYNTHETIC-SECRET-after-suppression";
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.suppress_remaining();
            sink.finish();
            assert!(sink.is_suppressing());
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::UsageReported(TokenUsage::unreported()),
            });
            assert!(sink.is_suppressing());
            sink.finish();
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::UsageReported(TokenUsage::unreported()),
            });
            assert!(sink.is_suppressing());
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: PLANTED_VALUE.to_string(),
                },
            });
            sink.finish();
        }
        let streamed = observed
            .into_iter()
            .map(observation_text)
            .collect::<String>();

        assert!(!streamed.contains(PLANTED_VALUE));
        assert!(streamed.contains(REDACTED));
    }

    /// INV-035: forcing a held private-key marker through a non-delta boundary
    /// retains its continuation state, so the following body is destroyed too.
    #[test]
    fn inv_035_flush_boundary_does_not_release_a_credential_continuation() {
        const PLANTED_VALUE: &str = "MIIB-SYNTHETIC-SECRET-flush";
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: "-----BEGIN PRIVATE KEY-----\n".to_string(),
                },
            });
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::ToolArgumentsDelta {
                    index: 1,
                    fragment: String::new(),
                },
            });
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 2,
                    text: format!("{PLANTED_VALUE}\n-----END PRIVATE KEY-----"),
                },
            });
            sink.finish();
        }
        let streamed = observed
            .into_iter()
            .map(observation_text)
            .collect::<String>();

        assert!(!streamed.contains(PLANTED_VALUE));
        assert!(streamed.contains(REDACTED));
    }

    #[track_caller]
    fn assert_cap_edge_destroys_planted_value(total_held_bytes: usize) {
        const MARKER: &str = "authorization: ";
        const PLANTED_VALUE: &str = "SYNTHETIC-SECRET-cap-edge";
        let padding = total_held_bytes - MARKER.len() - PLANTED_VALUE.len();
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: MARKER.to_string(),
                },
            });
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 1,
                    text: format!("{PLANTED_VALUE}{}", "a".repeat(padding)),
                },
            });
            sink.finish();
        }
        let streamed = observed
            .into_iter()
            .map(observation_text)
            .collect::<String>();

        assert!(!streamed.contains(PLANTED_VALUE));
    }

    /// INV-035: every held-size permutation immediately around the 64-KiB
    /// boundary destroys the planted value; none creates a release seam.
    #[test]
    fn inv_035_stream_redaction_covers_cap_edge_permutations() {
        assert_cap_edge_destroys_planted_value(MAX_PENDING_STREAM_BYTES - 2);
        assert_cap_edge_destroys_planted_value(MAX_PENDING_STREAM_BYTES - 1);
        assert_cap_edge_destroys_planted_value(MAX_PENDING_STREAM_BYTES);
        assert_cap_edge_destroys_planted_value(MAX_PENDING_STREAM_BYTES + 1);
        assert_cap_edge_destroys_planted_value(MAX_PENDING_STREAM_BYTES + 2);
    }

    /// A recomputable marker prefix that later proves to be ordinary text is
    /// released byte-exact; only an unmappable escaped origin stays sticky.
    #[test]
    fn broken_stream_marker_prefix_remains_byte_exact() {
        let mut observed = Vec::new();
        {
            let mut sink = RedactingSink::new(&mut observed);
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: "s".to_string(),
                },
            });
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index: 0,
                    text: "afe text".to_string(),
                },
            });
            sink.finish();
        }
        let streamed = observed
            .into_iter()
            .map(observation_text)
            .collect::<String>();

        assert_eq!(streamed, "safe text");
    }

    /// Plumbing for the ignored stream stress soak: starts an
    /// unterminated line credential, then extends it one byte per delta.
    fn observe_one_byte_credential_deltas(sink: &mut RedactingSink<'_, u8>, delta_count: u32) {
        sink.observe(Observation {
            correlation: 7_u8,
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "authorization: ".to_string(),
            },
        });
        for index in 1..=delta_count {
            sink.observe(Observation {
                correlation: 7_u8,
                fact: ObservationFact::TextDelta {
                    index,
                    text: "a".to_string(),
                },
            });
        }
    }

    /// INV-035: the 66,000 one-byte-delta stress shape receives thirteen
    /// geometric classification rounds. Charging each round's candidate and
    /// unsafe-suffix predicates separately stays below six times the 64-KiB
    /// held-byte cap instead of growing with the square of the delta count.
    #[test]
    fn inv_035_stream_redaction_bounds_66000_delta_rescan_work() {
        const DELTA_COUNT: u32 = 66_000;
        const EXPECTED_WORK: PendingRescanWork = PendingRescanWork {
            classifications: 13,
            bytes: 376_774,
        };
        let mut observed = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);

        observe_one_byte_credential_deltas(&mut sink, DELTA_COUNT);

        assert_eq!(sink.pending_rescan_work(), &EXPECTED_WORK);
        assert!(sink.pending_rescan_work().bytes <= 6 * MAX_PENDING_STREAM_BYTES);
        assert!(sink.is_suppressing());
    }

    /// Manual 66,000-delta stress soak. The ordinary
    /// deterministic regression asserts scan work rather than elapsed time.
    #[test]
    #[ignore = "manual 66,000-delta stream stress soak"]
    fn inv_035_stream_redaction_soaks_66000_one_byte_deltas() {
        const DELTA_COUNT: u32 = 66_000;
        let mut observed = Vec::new();
        let mut sink = RedactingSink::new(&mut observed);

        observe_one_byte_credential_deltas(&mut sink, DELTA_COUNT);

        assert!(sink.is_suppressing());
    }
}

#[cfg(test)]
#[path = "redaction_corpus_tests.rs"]
mod redaction_corpus_tests;
