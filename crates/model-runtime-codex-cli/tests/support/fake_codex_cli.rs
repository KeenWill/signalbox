//! Scripted offline Codex executable used only by this crate's integration
//! tests.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

mod fixtures;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    record_spawn()?;
    validate_argv()?;
    if Path::new("fake-codex-block-stdin").exists() {
        std::fs::write("fake-codex-block-stdin-ready", "ready\n")?;
        std::thread::sleep(Duration::from_secs(60));
    }
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
            emit(&format!(
                r#"{{"type":"item.completed","item":{{"id":"reason-1","type":"reasoning","text":"{}"}}}}"#,
                fixtures::REASONING_TEXT
            ));
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::STREAMED_ANSWER
            ));
            completed();
        }
        "last_agent_message" => {
            agent_message("message-intermediate", "not a response envelope");
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "malformed_last_agent_message" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            agent_message("message-last", "not a response envelope");
            completed();
        }
        "credential_envelope_error" => {
            envelope(&format!(
                r#"{{"outcome":"{}","text":"","tool_calls":[]}}"#,
                fixtures::SENSITIVE_ENVELOPE_TOKEN
            ));
            completed();
        }
        "deep_agent_message" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"call-deep","name":"{}","arguments":{}}}]}}"#,
                fixtures::TOOL_NAME,
                deeply_nested_arguments()
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
        "structured_refused" => {
            envelope(&format!(
                r#"{{"outcome":"refused","text":"{}","tool_calls":[]}}"#,
                fixtures::REFUSAL_TEXT
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
        "sensitive_tool_ids" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"{}","name":"{}","arguments":{{}}}},{{"id":"{}","name":"{}","arguments":{{}}}}]}}"#,
                fixtures::SENSITIVE_TOOL_ID_ONE,
                fixtures::TOOL_NAME,
                fixtures::SENSITIVE_TOOL_ID_TWO,
                fixtures::TOOL_NAME
            ));
            completed();
        }
        "usage_without_cache" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed_without_cache();
        }
        "completion_before_cancellation" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
            std::fs::write("fake-codex-completion-ready", "ready\n")?;
        }
        "inherited_stderr" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
            let descendant = std::process::Command::new("sh")
                .arg("-c")
                .arg("sleep 60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?;
            std::fs::write(
                "fake-codex-inherited-stderr-pid",
                descendant.id().to_string(),
            )?;
        }
        "filtered_environment" => {
            if std::env::var_os("PWD").is_some() || std::env::var_os("PATH").is_none() {
                failed("subprocess environment was not filtered");
            } else {
                envelope(&format!(
                    r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                    fixtures::BUFFERED_ANSWER
                ));
                completed();
            }
        }
        "stderr_redaction" => {
            eprintln!(
                "authentication failed API_KEY=\"{}\"",
                fixtures::SENSITIVE_STDERR_TOKEN
            );
            std::process::exit(7);
        }
        "killed_process" => std::process::abort(),
        "hang" => std::thread::sleep(Duration::from_secs(60)),
        "busy_stdout" => {
            std::fs::write("fake-codex-busy-stdout", "ready\n")?;
            loop {
                emit(r#"{"type":"item.updated"}"#);
            }
        }
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
    agent_message("message-offline-1", value);
}

fn agent_message(id: &str, value: &str) {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    emit(&format!(
        r#"{{"type":"item.completed","item":{{"id":"{id}","type":"agent_message","text":"{escaped}"}}}}"#
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

fn completed_without_cache() {
    emit(&format!(
        r#"{{"type":"turn.completed","usage":{{"input_tokens":{},"output_tokens":{},"reasoning_output_tokens":3}}}}"#,
        fixtures::INPUT_TOKENS,
        fixtures::OUTPUT_TOKENS
    ));
}

fn deeply_nested_arguments() -> String {
    let mut value = "{}".to_string();
    for _ in 0..130 {
        value = format!(r#"{{"nested":{value}}}"#);
    }
    value
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
