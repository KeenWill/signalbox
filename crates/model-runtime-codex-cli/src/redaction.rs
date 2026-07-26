//! Credential-shape redaction for CLI-controlled output.

use serde_json::Value;

const REDACTED: &str = "[redacted]";

pub(crate) fn redact_text(text: &str) -> String {
    let mut sanitized = text.to_string();
    for marker in ["authorization=", "authorization:", "cookie=", "cookie:"] {
        sanitized = redact_line_value(&sanitized, marker);
    }
    for marker in [
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
    ] {
        sanitized = redact_after_marker(&sanitized, marker);
    }
    let sanitized = redact_prefixed_token(&sanitized, "sk-");
    redact_prefixed_token(&sanitized, "eyJ")
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

#[cfg(test)]
mod tests {
    use super::{redact_json, redact_text};

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
}
