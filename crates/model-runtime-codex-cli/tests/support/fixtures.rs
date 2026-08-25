//! Values shared by the scripted executable and its process-level assertions.

pub const THREAD_ID: &str = "thread-offline-1";
pub const STREAM_ERROR_MESSAGE: &str = "quota exhausted for the active plan";
pub const BUFFERED_ANSWER: &str = "buffered answer";
pub const STREAMED_ANSWER: &str = "streamed answer";
pub const REASONING_TEXT: &str = "considering";
pub const PENDING_PROGRESS_TEXT: &str = "harmless";
pub const REFUSAL_TEXT: &str = "request refused";
pub const TOOL_NAME: &str = "lookup";
pub const OTHER_TOOL_NAME: &str = "timezone";
pub const TOOL_ARGUMENTS: &str = r#"{ "city" : "Oslo", "limit": 3 }"#;
pub const MALFORMED_TOOL_ARGUMENTS: &str = "not an argument object";
pub const NON_OBJECT_TOOL_ARGUMENTS: &str = "7";

pub fn deeply_nested_tool_arguments() -> String {
    let mut value = "{}".to_string();
    for _ in 0..130 {
        value = format!(r#"{{"nested":{value}}}"#);
    }
    value
}

pub const STRUCTURED_ACCEPTED: bool = true;
pub const INPUT_TOKENS: u64 = 11;
pub const CACHE_READ_INPUT_TOKENS: u64 = 2;
pub const CACHE_CREATION_INPUT_TOKENS: u64 = 1;
pub const OUTPUT_TOKENS: u64 = 7;
pub const SENSITIVE_OUTPUT_TOKEN: &str = "sk-sensitive-output";
pub const SENSITIVE_REFRESH_TOKEN: &str = "sensitive-refresh";
pub const SENSITIVE_STDERR_TOKEN: &str = "sensitive-stderr";
pub const SENSITIVE_TOOL_ID_ONE: &str = "sk-sensitive-call-one";
pub const SENSITIVE_TOOL_ID_TWO: &str = "API_KEY=sensitive-call-two";
pub const SENSITIVE_ENVELOPE_TOKEN: &str = "authorization=Bearer sensitive-envelope-token";
pub const SENSITIVE_SPLIT_STREAM_TOKEN: &str = "sk-sensitive-stream-token";
pub const SENSITIVE_SPLIT_AUTHORIZATION: &str = "sensitive-split-authorization";
pub const SENSITIVE_COMPOSITE_SECRET: &str = "sensitive-composite-secret";
pub const SENSITIVE_STRUCTURED_SECRET: &str = "sensitive-structured-container-value";
pub const SENSITIVE_STDERR_CONTINUATION: &str = "sensitive-stderr-continuation";
/// A thread id ending in a credential-marker prefix; clean on its own, but
/// later text beginning with the marker's remainder reconstructs `api_key=`
/// beside it.
pub const CREDENTIAL_PREFIX_THREAD_ID: &str = "api_";
/// The final-text continuation of `CREDENTIAL_PREFIX_THREAD_ID`; also clean
/// on its own (`key` matches no credential shape).
pub const SENSITIVE_THREAD_CONTINUATION: &str = "key=opaque-thread-continuation done";
pub const EARLY_STDIN_EXIT_MARKER: &str = "fake-codex-exit-before-stdin";
/// Makes the fake CLI hand its stdin to a surviving descendant (which never
/// reads it, keeping the upload blocked), write a classifiable stderr
/// failure, and exit nonzero before reading the prompt.
pub const EARLY_STDIN_HELD_EXIT_MARKER: &str = "fake-codex-exit-with-held-stdin";
pub const EARLY_STDIN_COMPLETION_MARKER: &str = "fake-codex-complete-before-stdin";
pub const EARLY_STDIN_FAILURE: &str = "unexpected status 400 Bad Request: context_length_exceeded";
#[allow(
    dead_code,
    reason = "process-only expected values share this module with the fake executable"
)]
pub const ADVISORY_MAX_OUTPUT_TOKENS: u32 = 17;
#[allow(
    dead_code,
    reason = "process-only expected values share this module with the fake executable"
)]
pub const ADVISORY_TEMPERATURE: f64 = 0.25;
#[allow(
    dead_code,
    reason = "process-only expected values share this module with the fake executable"
)]
pub const ADVISORY_TOP_P: f64 = 0.75;
#[allow(
    dead_code,
    reason = "process-only expected values share this module with the fake executable"
)]
pub const ADVISORY_STOP_SEQUENCE: &str = "offline-stop";
#[allow(
    dead_code,
    reason = "process-only expected values share this module with the fake executable"
)]
pub const PRECISE_JSON_NUMBER: &str = "123456789012345678901234567890";
#[allow(
    dead_code,
    reason = "process-only expected values share this module with the fake executable"
)]
pub const REDACTED_TOOL_ID_ONE: &str = "codex-redacted-call-1";
#[allow(
    dead_code,
    reason = "process-only expected values share this module with the fake executable"
)]
pub const REDACTED_TOOL_ID_TWO: &str = "codex-redacted-call-2";

/// Rejects what the live API's strict structured-output validation rejects,
/// for the schema shapes the adapter's output schema uses (`properties` and
/// array `items`): every object node must supply `additionalProperties:
/// false` and must require every property it declares. The gated
/// compatibility smoke observed the live rejection as `invalid_json_schema`
/// with exactly this complaint, so the fake CLI applies the same rules to
/// every spawn and this class of drift can no longer pass offline.
pub fn strict_schema_violation(schema: &serde_json::Value) -> Option<String> {
    violation_under(schema, &mut Vec::new())
}

fn violation_under(node: &serde_json::Value, context: &mut Vec<String>) -> Option<String> {
    let object = node.as_object()?;
    if object.get("type").and_then(serde_json::Value::as_str) == Some("object") {
        if object.get("additionalProperties") != Some(&serde_json::Value::Bool(false)) {
            return Some(format!(
                "In context=({}), 'additionalProperties' is required to be supplied and to be false.",
                rendered_context(context)
            ));
        }
        let required: Vec<&str> = object
            .get("required")
            .and_then(serde_json::Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect()
            })
            .unwrap_or_default();
        let properties = object
            .get("properties")
            .and_then(serde_json::Value::as_object);
        for name in properties
            .map(|entries| entries.keys())
            .into_iter()
            .flatten()
        {
            if !required.contains(&name.as_str()) {
                return Some(format!(
                    "In context=({}), 'required' is required to be supplied and to include every key in 'properties'. Missing '{name}'.",
                    rendered_context(context)
                ));
            }
        }
    }
    if let Some(properties) = object
        .get("properties")
        .and_then(serde_json::Value::as_object)
    {
        for (name, child) in properties {
            context.push("properties".to_string());
            context.push(name.clone());
            let violation = violation_under(child, context);
            context.pop();
            context.pop();
            if violation.is_some() {
                return violation;
            }
        }
    }
    if let Some(items) = object.get("items") {
        context.push("items".to_string());
        let violation = violation_under(items, context);
        context.pop();
        if violation.is_some() {
            return violation;
        }
    }
    None
}

fn rendered_context(context: &[String]) -> String {
    context
        .iter()
        .map(|segment| format!("'{segment}'"))
        .collect::<Vec<_>>()
        .join(",")
}
