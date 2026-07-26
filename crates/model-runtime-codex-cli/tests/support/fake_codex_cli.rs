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
    let output_schema = validate_argv()?;
    if Path::new(fixtures::EARLY_STDIN_EXIT_MARKER).exists() {
        eprintln!("Codex rejected stdin");
        emit(&format!(
            r#"{{"type":"thread.started","thread_id":"{}"}}"#,
            fixtures::THREAD_ID
        ));
        emit(r#"{"type":"turn.started"}"#);
        failed(fixtures::EARLY_STDIN_FAILURE);
    }
    if Path::new(fixtures::EARLY_STDIN_COMPLETION_MARKER).exists() {
        emit(&format!(
            r#"{{"type":"thread.started","thread_id":"{}"}}"#,
            fixtures::THREAD_ID
        ));
        emit(r#"{"type":"turn.started"}"#);
        envelope(&format!(
            r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
            fixtures::BUFFERED_ANSWER
        ));
        completed();
        return Ok(());
    }
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
    if let Some(violation) = fixtures::strict_schema_violation(&output_schema) {
        // The live API rejects a non-strict output schema before producing
        // any model output, and the pinned CLI reports that rejection as a
        // stream-level `error` event followed by its `turn.failed` lifecycle
        // echo and a nonzero exit — the sequence the gated compatibility
        // smoke observed for `invalid_json_schema`. Enforcing the same rules
        // here keeps the offline corpus from accepting a schema the live API
        // refuses.
        unrecoverable(&format!(
            "unexpected status 400 Bad Request: {{\"error\":{{\"type\":\"invalid_request_error\",\"code\":\"invalid_json_schema\",\"message\":\"{violation}\"}}}}"
        ));
    }
    match scenario.as_str() {
        "buffered_completed" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "streamed_completed" => {
            reasoning("reason-1", fixtures::REASONING_TEXT);
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::STREAMED_ANSWER
            ));
            completed();
        }
        "split_stream_credential_between_reasoning_items" => {
            reasoning(
                "reason-split-1",
                &fixtures::SENSITIVE_SPLIT_STREAM_TOKEN[..3],
            );
            reasoning(
                "reason-split-2",
                &fixtures::SENSITIVE_SPLIT_STREAM_TOKEN[3..],
            );
            envelope(r#"{"outcome":"completed","text":"safe","tool_calls":[]}"#);
            completed();
        }
        "split_stream_credential_before_final_text" => {
            reasoning(
                "reason-split-final",
                &fixtures::SENSITIVE_SPLIT_STREAM_TOKEN[..3],
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                &fixtures::SENSITIVE_SPLIT_STREAM_TOKEN[3..]
            ));
            completed();
        }
        "split_stream_authorization_before_final_text" => {
            reasoning("reason-split-authorization", "Authorization:");
            envelope(&format!(
                r#"{{"outcome":"completed","text":" {}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "split_stream_authorization_before_failure" => {
            reasoning("reason-split-authorization-failed", "Authorization:");
            unrecoverable(&format!(" {}", fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
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
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"call-offline-1","name":"{}","arguments":"{}"}}]}}"#,
                fixtures::TOOL_NAME,
                json_escape(fixtures::TOOL_ARGUMENTS)
            ));
            completed();
        }
        "tool_call_bad_arguments" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"call-offline-bad","name":"{}","arguments":"not an argument object"}}]}}"#,
                fixtures::TOOL_NAME
            ));
            completed();
        }
        "tool_call_deep_arguments" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"call-offline-deep","name":"{}","arguments":"{}"}}]}}"#,
                fixtures::TOOL_NAME,
                json_escape(&deeply_nested_arguments())
            ));
            completed();
        }
        "structured_output" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"structured-offline-1","name":"verdict","arguments":"{}"}}]}}"#,
                json_escape(&format!(
                    r#"{{ "accepted" : {} }}"#,
                    fixtures::STRUCTURED_ACCEPTED
                ))
            ));
            completed();
        }
        "structured_output_missing" => {
            envelope(r#"{"outcome":"completed","text":"","tool_calls":[]}"#);
            completed();
        }
        "structured_output_multiple" => {
            let arguments = json_escape(&format!(
                r#"{{ "accepted" : {} }}"#,
                fixtures::STRUCTURED_ACCEPTED
            ));
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"structured-offline-1","name":"verdict","arguments":"{arguments}"}},{{"id":"structured-offline-2","name":"verdict","arguments":"{arguments}"}}]}}"#
            ));
            completed();
        }
        "named_choice_extra_tool" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"named-offline-1","name":"{}","arguments":"{}"}},{{"id":"named-offline-2","name":"{}","arguments":"{{}}"}}]}}"#,
                fixtures::TOOL_NAME,
                json_escape(fixtures::TOOL_ARGUMENTS),
                fixtures::OTHER_TOOL_NAME
            ));
            completed();
        }
        "bare_credential_text" => {
            let text = json_escape(&format!(
                r#"  "client_secret":"{}""#,
                fixtures::SENSITIVE_COMPOSITE_SECRET
            ));
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{text}","tool_calls":[]}}"#
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
        "error_quota_exhausted" => failed("insufficient_quota"),
        "error_overloaded" => failed("provider overloaded"),
        "error_provider_internal" => failed("internal server error"),
        "error_unrecognized" => failed("future failure shape"),
        "error_then_turn_failed" => unrecoverable(fixtures::STREAM_ERROR_MESSAGE),
        "error_then_turn_completed" => {
            emit_error(fixtures::STREAM_ERROR_MESSAGE);
            completed();
        }
        "no_terminal" => {
            envelope(r#"{"outcome":"completed","text":"not terminal","tool_calls":[]}"#);
        }
        "malformed_event" => emit("{not-json"),
        "reasoning_then_malformed_event" => {
            reasoning("reason-before-malformed", fixtures::PENDING_PROGRESS_TEXT);
            emit("{not-json");
        }
        "malformed_known_lifecycle" => {
            emit(r#"{"type":"item.started"}"#);
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "empty_completed_item_identity" => {
            agent_message(
                "",
                &format!(
                    r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                    fixtures::BUFFERED_ANSWER
                ),
            );
            completed();
        }
        "redaction" => {
            let text = format!(
                r#"Bearer {} and {{"client_secret":"{}"}}"#,
                fixtures::SENSITIVE_OUTPUT_TOKEN,
                fixtures::SENSITIVE_COMPOSITE_SECRET
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[{{"id":"call-redaction","name":"{}","arguments":"{}"}}]}}"#,
                json_escape(&text),
                fixtures::TOOL_NAME,
                json_escape(&format!(
                    r#"{{"access_token":"{}","city":"Oslo"}}"#,
                    fixtures::SENSITIVE_REFRESH_TOKEN
                ))
            ));
            completed();
        }
        "sensitive_tool_ids" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"{}","name":"{}","arguments":"{{}}"}},{{"id":"{}","name":"{}","arguments":"{{}}"}}]}}"#,
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
            record_process_group("fake-codex-inherited-stderr-process-group", descendant.id())?;
        }
        "completed_with_detached_descendant" => {
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
                .stderr(Stdio::null())
                .spawn()?;
            record_process_group(
                "fake-codex-detached-descendant-process-group",
                descendant.id(),
            )?;
        }
        "interrupt_with_descendant" => {
            let descendant = std::process::Command::new("sh")
                .arg("-c")
                .arg("trap '' INT; sleep 60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            record_process_group(
                "fake-codex-interrupt-descendant-process-group",
                descendant.id(),
            )?;
            std::thread::sleep(Duration::from_secs(60));
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
                emit(
                    r#"{"type":"item.updated","item":{"id":"busy-progress","type":"future_item"}}"#,
                );
            }
        }
        _ => failed("invalid request: unknown offline fixture"),
    }
    Ok(())
}

fn record_process_group(path: &str, descendant: u32) -> std::io::Result<()> {
    std::fs::write(
        path,
        format!(
            "process_group={}\ndescendant={descendant}\n",
            std::process::id()
        ),
    )
}

fn record_spawn() -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("fake-codex-spawns")?;
    writeln!(file, "spawn")
}

fn validate_argv() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let arguments: Vec<String> = std::env::args().collect();
    std::fs::write("fake-codex-argv", arguments.join("\n"))?;
    let schema_index = arguments
        .iter()
        .position(|argument| argument == "--output-schema")
        .ok_or("missing --output-schema")?;
    let schema_path = arguments
        .get(schema_index + 1)
        .ok_or("missing output schema path")?;
    let schema: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(schema_path)?)?;
    if schema["properties"]["outcome"].is_null() {
        return Err("unexpected output schema".into());
    }
    Ok(schema)
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
    let escaped = json_escape(value);
    emit(&format!(
        r#"{{"type":"item.completed","item":{{"id":"{id}","type":"agent_message","text":"{escaped}"}}}}"#
    ));
}

fn reasoning(id: &str, text: &str) {
    emit(&format!(
        r#"{{"type":"item.completed","item":{{"id":"{id}","type":"reasoning","text":"{}"}}}}"#,
        json_escape(text)
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
        r#"{{"type":"turn.failed","error":{{"message":"{}"}}}}"#,
        json_escape(message)
    ));
    std::process::exit(1);
}

/// The pinned CLI's observed failure sequencing: a stream-level `error`
/// event first, then the `turn.failed` lifecycle echo, then a nonzero exit.
fn unrecoverable(message: &str) -> ! {
    emit_error(message);
    failed(message)
}

fn emit_error(message: &str) {
    emit(&format!(
        r#"{{"type":"error","message":"{}"}}"#,
        json_escape(message)
    ));
}

fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn emit(line: &str) {
    println!("{line}");
}
