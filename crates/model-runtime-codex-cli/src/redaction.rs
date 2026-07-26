//! Credential-shape redaction for CLI-controlled output.

use serde_json::Value;

const REDACTED: &str = "[redacted]";

pub(crate) fn redact_text(text: &str) -> String {
    let mut sanitized = text.to_string();
    for marker in [
        "bearer ",
        "authorization=",
        "authorization:",
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
        "cookie=",
        "cookie:",
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
        let quoted = remaining[value_start + whitespace..].starts_with(['"', '\'']);
        if quoted {
            output.push_str(&remaining[value_start + whitespace..value_start + whitespace + 1]);
        }
        output.push_str(REDACTED);
        let token_start = value_start + whitespace + usize::from(quoted);
        let value_end = remaining[token_start..]
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '"' | '\'' | ',' | '}' | ']' | ';')
            })
            .map_or(remaining.len(), |length| token_start + length);
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
    const JWT_SHAPED_VALUE: &str = "eyJsensitive.jwt.value";

    /// INV-035: credential-shaped text never leaves the CLI adapter.
    #[test]
    fn inv_035_redacts_bearer_and_prefixed_credentials() {
        let fixture = format!(
            "Authorization BEARER {CREDENTIAL_SHAPED_VALUE} and \
             API_KEY=\"{QUOTED_CREDENTIAL_VALUE}\" with \
             {{\"REFRESH_TOKEN\":\"{JSON_CREDENTIAL_VALUE}\"}}, \
             Authorization: {AUTHORIZATION_VALUE}, and {JWT_SHAPED_VALUE}"
        );
        let output = redact_text(&fixture);

        assert!(!output.contains(CREDENTIAL_SHAPED_VALUE));
        assert!(!output.contains(QUOTED_CREDENTIAL_VALUE));
        assert!(!output.contains(JSON_CREDENTIAL_VALUE));
        assert!(!output.contains(AUTHORIZATION_VALUE));
        assert!(!output.contains(JWT_SHAPED_VALUE));
        assert!(output.contains("[redacted]"));
    }

    /// INV-035: credential-shaped JSON members are redacted recursively.
    #[test]
    fn inv_035_redacts_nested_credential_members() {
        let fixture = format!(
            r#"{{"safe":"kept","nested":{{"API_KEY":"{NESTED_CREDENTIAL_VALUE}","apiKey":"{NESTED_CREDENTIAL_VALUE}","accessToken":"{NESTED_CREDENTIAL_VALUE}","refreshToken":"{NESTED_CREDENTIAL_VALUE}"}}}}"#
        );
        let output = redact_json(&fixture);

        assert!(!output.contains(NESTED_CREDENTIAL_VALUE));
        assert!(output.contains(r#""safe":"kept""#));
    }

    #[test]
    fn harmless_tool_arguments_remain_byte_exact() {
        let input = r#"{ "city" : "Oslo", "limit": 3 }"#;

        assert_eq!(redact_json(input), input);
    }
}
