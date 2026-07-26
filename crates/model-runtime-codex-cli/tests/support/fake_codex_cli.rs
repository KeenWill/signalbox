//! Scripted offline Codex executable used only by this crate's integration
//! tests.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::time::Duration;

mod fixtures;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    record_spawn()?;
    validate_argv()?;
    let mut prompt = String::new();
    std::io::stdin().read_to_string(&mut prompt)?;
    std::fs::write("fake-codex-prompt", &prompt)?;

    let scenario = scenario(&prompt)?;
    emit(&format!(
        r#"{{"type":"thread.started","thread_id":"{}"}}"#,
        fixtures::THREAD_ID
    ));
    emit(r#"{"type":"turn.started"}"#);
    match scenario.as_str() {
        "buffered_completed" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "streamed_completed" => {
            emit(
                r#"{"type":"item.completed","item":{"id":"reason-1","type":"reasoning","text":"considering"}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::STREAMED_ANSWER
            ));
            completed();
        }
        "tool_call" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"call-offline-1","name":"{}","arguments":{}}}]}}"#,
                fixtures::TOOL_NAME,
                fixtures::TOOL_ARGUMENTS
            ));
            completed();
        }
        "structured_output" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"structured-offline-1","name":"verdict","arguments":{{ "accepted" : {} }}}}]}}"#,
                fixtures::STRUCTURED_ACCEPTED
            ));
            completed();
        }
        "refused" => {
            envelope(&format!(
                r#"{{"outcome":"refused","text":"{}","tool_calls":[]}}"#,
                fixtures::REFUSAL_TEXT
            ));
            completed();
        }
        "credential_precedence" => {
            envelope(&format!(
                r#"{{"outcome":"refused","text":"{}","tool_calls":[]}}"#,
                fixtures::REFUSAL_TEXT
            ));
            failed("authentication failed after refusal");
        }
        "error_permission" => failed("permission denied"),
        "error_invalid_request" => failed("invalid request"),
        "error_target_not_found" => failed("model not found"),
        "error_request_too_large" => failed("request too large"),
        "error_rate_limited" => failed("rate limit exceeded"),
        "error_quota_exhausted" => failed("quota exhausted"),
        "error_overloaded" => failed("provider overloaded"),
        "error_provider_internal" => failed("internal server error"),
        "error_unrecognized" => failed("future failure shape"),
        "no_terminal" => {
            envelope(r#"{"outcome":"completed","text":"not terminal","tool_calls":[]}"#);
        }
        "malformed_event" => emit("{not-json"),
        "redaction" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"Bearer {}","tool_calls":[{{"id":"call-redaction","name":"{}","arguments":{{"access_token":"{}","city":"Oslo"}}}}]}}"#,
                fixtures::SENSITIVE_OUTPUT_TOKEN,
                fixtures::TOOL_NAME,
                fixtures::SENSITIVE_REFRESH_TOKEN
            ));
            completed();
        }
        "stderr_redaction" => {
            eprintln!(
                "authentication failed api_key=\"{}\"",
                fixtures::SENSITIVE_STDERR_TOKEN
            );
            std::process::exit(7);
        }
        "killed_process" => std::process::abort(),
        "hang" => std::thread::sleep(Duration::from_secs(60)),
        _ => failed("invalid request: unknown offline fixture"),
    }
    Ok(())
}

fn record_spawn() -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("fake-codex-spawns")?;
    writeln!(file, "spawn")
}

fn validate_argv() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = std::env::args().collect();
    std::fs::write("fake-codex-argv", arguments.join("\n"))?;
    let schema_index = arguments
        .iter()
        .position(|argument| argument == "--output-schema")
        .ok_or("missing --output-schema")?;
    let schema_path = arguments
        .get(schema_index + 1)
        .ok_or("missing output schema path")?;
    let schema = std::fs::read_to_string(schema_path)?;
    if !schema.contains(r#""outcome""#) {
        return Err("unexpected output schema".into());
    }
    Ok(())
}

fn scenario(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    let request = prompt
        .rsplit_once("\n\n")
        .map(|(_, request)| request.trim())
        .ok_or("missing rendered request")?;
    let value: serde_json::Value = serde_json::from_str(request)?;
    value["messages"][0]["parts"][0]["text"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "missing fixture scenario".into())
}

fn envelope(value: &str) {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    emit(&format!(
        r#"{{"type":"item.completed","item":{{"id":"message-offline-1","type":"agent_message","text":"{escaped}"}}}}"#
    ));
}

fn completed() {
    emit(&format!(
        r#"{{"type":"turn.completed","usage":{{"input_tokens":{},"cached_input_tokens":{},"cache_write_input_tokens":{},"output_tokens":{},"reasoning_output_tokens":3}}}}"#,
        fixtures::INPUT_TOKENS,
        fixtures::CACHE_READ_INPUT_TOKENS,
        fixtures::CACHE_CREATION_INPUT_TOKENS,
        fixtures::OUTPUT_TOKENS
    ));
}

fn failed(message: &str) -> ! {
    emit(&format!(
        r#"{{"type":"turn.failed","error":{{"message":"{message}"}}}}"#
    ));
    std::process::exit(1);
}

fn emit(line: &str) {
    println!("{line}");
}
