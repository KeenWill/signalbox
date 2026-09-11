//! Scripted offline Claude Code executable used by integration tests.

use std::io::{Read, Write};

mod fixtures;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    record_spawn()?;
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    std::fs::write("fake-claude-argv", arguments.join("\n"))?;
    record_credential_delivery(&arguments)?;
    let mut prompt = String::new();
    std::io::stdin().read_to_string(&mut prompt)?;
    std::fs::write("fake-claude-prompt", &prompt)?;
    let scenario = if let Some(pair) = arguments.windows(2).find(|pair| pair[0] == "--resume") {
        let history_path = &pair[1];
        let history = std::fs::read_to_string(history_path)?;
        std::fs::write("fake-claude-history", &history)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(history_path)?.permissions().mode() & 0o777;
            std::fs::write("fake-claude-history-mode", format!("{mode:o}"))?;
        }
        scenario(&history)?
    } else {
        let controls: serde_json::Value = serde_json::from_str(
            prompt
                .split_once("\n\n")
                .ok_or("missing request controls")?
                .1,
        )?;
        controls["system"]
            .as_str()
            .ok_or("missing system-only scenario")?
            .to_string()
    };
    if scenario == "piped_stdin_too_large" {
        std::io::stderr().write_all(b"Error: piped stdin input exceeds a synthetic limit.\n")?;
        std::process::exit(1);
    }
    if scenario == "process_nonzero" {
        system_status(None)?;
        std::io::stderr().write_all(b"authentication failed for synthetic login\n")?;
        std::process::exit(7);
    }
    if scenario == "malformed_stream" {
        emit(b"{not-json\n")?;
        return Ok(());
    }
    if scenario == "duplicate_stream_member" {
        emit(b"{\"type\":\"system\",\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}\n")?;
        return Ok(());
    }

    if scenario == "version_drift" {
        // The handshake is rejected on this first event, so nothing after it
        // would be read.
        system_init_with_version(
            &arguments,
            fixtures::SESSION_ID,
            fixtures::MODEL,
            fixtures::DRIFTED_VERSION,
        )?;
        return Ok(());
    }
    if scenario.starts_with("native_compaction") {
        system_init(&arguments)?;
        let session = if scenario == "native_compaction_wrong_session" {
            fixtures::OTHER_SESSION_ID
        } else {
            fixtures::SESSION_ID
        };
        // The SDK reports the input occupancy before its native compaction.
        const PRE_COMPACTION_TOKENS: u64 = 170_000;
        emit_json(&serde_json::json!({
            "type": "system",
            "subtype": "compact_boundary",
            "session_id": session,
            "compact_metadata": { "trigger": "auto", "pre_tokens": PRE_COMPACTION_TOKENS }
        }))?;
        assistant_text(fixtures::ANSWER)?;
        success("end_turn", Some(fixtures::ANSWER))?;
        return Ok(());
    }

    if scenario == "nonterminal_system_events" {
        system_event("hook_started")?;
        system_status(None)?;
        system_init(&arguments)?;
        system_event("hook_progress")?;
        system_event("hook_response")?;
        system_status(Some("requesting"))?;
        system_event("api_retry")?;
        system_event("thinking_tokens")?;
        assistant_text(fixtures::ANSWER)?;
        success("end_turn", Some(fixtures::ANSWER))?;
        return Ok(());
    }

    if scenario == "lifecycle_session_contradicts_init" {
        system_init(&arguments)?;
        system_status_with_session(
            Some("running"),
            &format!(
                "{}{}",
                fixtures::OTHER_SESSION_ID,
                fixtures::FRAGMENTED_SECRET_PREFIX
            ),
        )?;
        assistant_text(fixtures::FRAGMENTED_SECRET_CONTINUATION)?;
        success("end_turn", Some(fixtures::FRAGMENTED_SECRET_CONTINUATION))?;
        return Ok(());
    }

    system_init(&arguments)?;
    match scenario.as_str() {
        "api_error_diagnostic"
        | "api_error_diagnostic_without_result"
        | "api_error_diagnostic_wrong_session" => {
            assistant_text(fixtures::REFUSAL)?;
            let session = if scenario == "api_error_diagnostic_wrong_session" {
                fixtures::OTHER_SESSION_ID
            } else {
                fixtures::SESSION_ID
            };
            emit_json(&serde_json::json!({
                "type": "assistant", "session_id": session,
                "is_api_error_message": true, "error": "invalid_request",
                "message": { "id": fixtures::OTHER_MESSAGE_ID, "model": "<synthetic>",
                    "role": "assistant", "stop_reason": "refusal",
                    "content": [{ "type": "text", "text": fixtures::ANSWER }] }
            }))?;
            if scenario != "api_error_diagnostic_without_result" {
                success("refusal", Some(fixtures::REFUSAL))?;
            }
        }
        "refusal_notice" | "refusal_notice_without_result" | "refusal_notice_wrong_session" => {
            let session = if scenario == "refusal_notice_wrong_session" {
                fixtures::OTHER_SESSION_ID
            } else {
                fixtures::SESSION_ID
            };
            emit_json(&serde_json::json!({
                "type": "system", "subtype": "model_refusal_no_fallback",
                "session_id": session, "original_model": fixtures::MODEL,
                "request_id": null, "content": "",
                "api_refusal_category": "synthetic_refusal"
            }))?;
            if scenario != "refusal_notice_without_result" {
                assistant_text(fixtures::REFUSAL)?;
                success("refusal", Some(fixtures::REFUSAL))?;
            }
        }
        "session_window_status"
        | "session_window_without_status"
        | "request_size_status"
        | "native_budget_subtype"
        | "unknown_error_subtype" => {
            let (subtype, status, message) = match scenario.as_str() {
                "session_window_status" => ("success", Some(429), "You've hit your session limit"),
                "session_window_without_status" => {
                    ("success", None, "You've hit your session limit")
                }
                "request_size_status" => ("success", Some(413), "Prompt is too long"),
                "native_budget_subtype" => {
                    ("error_max_budget_usd", None, "synthetic budget detail")
                }
                _ => (
                    "synthetic_unknown_error",
                    None,
                    "authentication failed; rate limit; request too large",
                ),
            };
            emit_json(&serde_json::json!({
                "type": "result", "subtype": subtype, "is_error": true,
                "session_id": fixtures::SESSION_ID, "api_error_status": status,
                "result": message
            }))?;
        }
        "native_prompt_too_large" => {
            emit_json(&serde_json::json!({
                "type": "result", "subtype": "success", "is_error": true,
                "session_id": fixtures::SESSION_ID, "stop_reason": "stop_sequence",
                "result": "Prompt is too long",
                "usage": {"input_tokens": 0, "output_tokens": 0,
                    "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
            }))?;
        }
        "normal_completion" => {
            assistant_text(fixtures::ANSWER)?;
            success("end_turn", Some(fixtures::ANSWER))?;
        }
        "resolved_assistant_model" => {
            assistant_text_with_identity(
                fixtures::MESSAGE_ID,
                fixtures::RESOLVED_MODEL,
                fixtures::ANSWER,
            )?;
            success("end_turn", Some(fixtures::ANSWER))?;
        }
        "repeated_model_prefix_release" => {
            assistant_text_with_identity(
                fixtures::MESSAGE_ID,
                fixtures::CREDENTIAL_PREFIX_RESOLVED_MODEL,
                fixtures::MODEL_MARKER_HELD_WORD,
            )?;
            assistant_text_with_identity(
                fixtures::MESSAGE_ID,
                fixtures::CREDENTIAL_PREFIX_RESOLVED_MODEL,
                fixtures::MODEL_MARKER_RELEASED_TAIL,
            )?;
            success(
                "end_turn",
                Some(&format!(
                    "{}{}",
                    fixtures::MODEL_MARKER_HELD_WORD,
                    fixtures::MODEL_MARKER_RELEASED_TAIL
                )),
            )?;
        }
        "conflicting_assistant_model" => {
            assistant_text_with_identity(
                fixtures::MESSAGE_ID,
                fixtures::RESOLVED_MODEL,
                fixtures::ANSWER,
            )?;
            assistant_text_with_identity(
                fixtures::MESSAGE_ID,
                fixtures::OTHER_RESOLVED_MODEL,
                fixtures::ANSWER,
            )?;
            success("end_turn", Some(fixtures::ANSWER))?;
        }
        "file_credential_redaction" => {
            assistant_text(fixtures::FILE_DELIVERED_CREDENTIAL)?;
            success("end_turn", Some(fixtures::FILE_DELIVERED_CREDENTIAL))?;
        }
        "safe_terminal_prefix" => {
            assistant_text(fixtures::SAFE_CREDENTIAL_PREFIX)?;
            success("end_turn", Some(fixtures::SAFE_CREDENTIAL_PREFIX))?;
        }
        "conflicting_message_id" => {
            assistant_text(fixtures::ANSWER)?;
            assistant_text_with_id(fixtures::OTHER_MESSAGE_ID, fixtures::ANSWER)?;
            success("end_turn", Some(fixtures::ANSWER))?;
        }
        // The same semantic rejection, but as the stream's final line. Nothing
        // follows it, so the reader holds no undelivered suffix when the failure
        // is raised and the tool fact rests only on the rejected event's own
        // examination.
        "conflicting_message_id_at_end_of_stream" => {
            assistant_text(fixtures::ANSWER)?;
            assistant_text_with_id(fixtures::OTHER_MESSAGE_ID, fixtures::ANSWER)?;
        }
        // An assistant event that both announces a tool call and contradicts
        // the established message id. The identity check rejects it, so the
        // tool fact must come from the pre-scan of its decoded content.
        "tool_use_with_conflicting_message_id" => {
            assistant_text(fixtures::ANSWER)?;
            assistant_tool_with_message_id(fixtures::OTHER_MESSAGE_ID)?;
            success("tool_use", None)?;
        }
        "success_without_stop_reason" => {
            assistant_text(fixtures::ANSWER)?;
            success_without_stop_reason()?;
        }
        // A tool call the request never declared. The decoder rejects it
        // before it becomes a proposal, so no observation and no proposal
        // index record that the CLI opened one.
        "undeclared_tool_use" => {
            assistant_tool(fixtures::TOOL_ID, fixtures::TOOL_NAME)?;
            success("tool_use", None)?;
        }
        "tool_round_trip" => {
            assistant_tool(fixtures::TOOL_ID, fixtures::TOOL_NAME)?;
            tool_result(fixtures::TOOL_ID)?;
            success("tool_use", Some(fixtures::TOOL_ARGUMENTS))?;
        }
        "tool_acknowledgement_tail" => {
            assistant_tool(fixtures::TOOL_ID, fixtures::TOOL_NAME)?;
            tool_result(fixtures::TOOL_ID)?;
            assistant_text_with_id(fixtures::OTHER_MESSAGE_ID, fixtures::ANSWER)?;
            assistant_text_with_id(fixtures::OTHER_MESSAGE_ID, fixtures::ANSWER)?;
            success("end_turn", Some(fixtures::ANSWER))?;
        }
        "tool_acknowledgement_thinking_then_text" | "tool_acknowledgement_thinking_only" => {
            assistant_tool(fixtures::TOOL_ID, fixtures::TOOL_NAME)?;
            tool_result(fixtures::TOOL_ID)?;
            emit_json(&serde_json::json!({
                "type": "assistant", "parent_tool_use_id": null,
                "message": {
                    "id": fixtures::OTHER_MESSAGE_ID, "model": fixtures::MODEL,
                    "role": "assistant", "content": [{
                        "type": "thinking", "thinking": "synthetic acknowledgement reasoning",
                        "signature": "synthetic-signature",
                    }],
                },
            }))?;
            if scenario == "tool_acknowledgement_thinking_then_text" {
                assistant_text_with_id(fixtures::OTHER_MESSAGE_ID, fixtures::ANSWER)?;
            }
            success("end_turn", Some(fixtures::ANSWER))?;
        }
        "tool_acknowledgement_tail_reports_tool_use" => {
            assistant_tool(fixtures::TOOL_ID, fixtures::TOOL_NAME)?;
            tool_result(fixtures::TOOL_ID)?;
            assistant_text_with_id(fixtures::OTHER_MESSAGE_ID, fixtures::ANSWER)?;
            success("tool_use", Some(fixtures::ANSWER))?;
        }
        "tool_acknowledgement_tail_is_empty" => {
            assistant_tool(fixtures::TOOL_ID, fixtures::TOOL_NAME)?;
            tool_result(fixtures::TOOL_ID)?;
            emit_json(&serde_json::json!({
                "type": "assistant", "parent_tool_use_id": null,
                "message": {
                    "id": fixtures::OTHER_MESSAGE_ID, "model": fixtures::MODEL,
                    "role": "assistant", "content": [],
                },
            }))?;
            success("end_turn", None)?;
        }
        "tool_acknowledgement_tail_without_result" => {
            assistant_tool(fixtures::TOOL_ID, fixtures::TOOL_NAME)?;
            assistant_text_with_id(fixtures::OTHER_MESSAGE_ID, fixtures::ANSWER)?;
            success("end_turn", Some(fixtures::ANSWER))?;
        }
        "tool_acknowledgement_tail_changes_model" => {
            assistant_tool(fixtures::TOOL_ID, fixtures::TOOL_NAME)?;
            tool_result(fixtures::TOOL_ID)?;
            assistant_text_with_identity(
                fixtures::OTHER_MESSAGE_ID,
                fixtures::OTHER_RESOLVED_MODEL,
                fixtures::ANSWER,
            )?;
            success("end_turn", Some(fixtures::ANSWER))?;
        }
        "tool_acknowledgement_tail_proposes_tool" => {
            assistant_tool(fixtures::TOOL_ID, fixtures::TOOL_NAME)?;
            tool_result(fixtures::TOOL_ID)?;
            assistant_tool_with_message_id(fixtures::OTHER_MESSAGE_ID)?;
            success("tool_use", None)?;
        }
        "reserved_key_tool_arguments" => {
            assistant_tool_with_raw_arguments(
                fixtures::TOOL_ID,
                fixtures::TOOL_NAME,
                fixtures::RESERVED_KEY_TOOL_ARGUMENTS,
            )?;
            tool_result(fixtures::TOOL_ID)?;
            success("tool_use", Some(fixtures::RESERVED_KEY_TOOL_ARGUMENTS))?;
        }
        "noncanonical_tool_arguments" => {
            assistant_tool_with_raw_arguments(
                fixtures::TOOL_ID,
                fixtures::TOOL_NAME,
                fixtures::NONCANONICAL_TOOL_ARGUMENTS,
            )?;
            tool_result(fixtures::TOOL_ID)?;
            success("tool_use", Some(fixtures::NONCANONICAL_TOOL_ARGUMENTS))?;
        }
        "suppressed_tool_arguments" => {
            assistant_tool_with_raw_arguments(
                fixtures::TOOL_ID,
                fixtures::TOOL_NAME,
                fixtures::SUPPRESSED_TOOL_ARGUMENTS,
            )?;
            tool_result(fixtures::TOOL_ID)?;
            success("tool_use", Some(fixtures::SUPPRESSED_TOOL_ARGUMENTS))?;
        }
        "refusal" => {
            assistant_text(fixtures::REFUSAL)?;
            success("refusal", Some(fixtures::REFUSAL))?;
        }
        "fragmented_credential_redaction" => {
            assistant_text(fixtures::FRAGMENTED_SECRET_PREFIX)?;
            assistant_text(fixtures::FRAGMENTED_SECRET_CONTINUATION)?;
            success("end_turn", Some(fixtures::FRAGMENTED_SECRET))?;
        }
        "named_choice_extra_tool" => {
            assistant_tool(fixtures::TOOL_ID, fixtures::TOOL_NAME)?;
            assistant_tool(fixtures::OTHER_TOOL_ID, fixtures::OTHER_TOOL_NAME)?;
            tool_result(fixtures::TOOL_ID)?;
            tool_result(fixtures::OTHER_TOOL_ID)?;
            success("tool_use", None)?;
        }
        "tool_with_end_turn" => {
            assistant_tool(fixtures::TOOL_ID, fixtures::TOOL_NAME)?;
            tool_result(fixtures::TOOL_ID)?;
            success("end_turn", None)?;
        }
        "text_with_tool_use" => {
            assistant_text(fixtures::ANSWER)?;
            success("tool_use", Some(fixtures::ANSWER))?;
        }
        "success_with_errors" => {
            assistant_text(fixtures::ANSWER)?;
            contradictory_success(&[fixtures::ANSWER], None)?;
        }
        "success_with_api_status" => {
            assistant_text(fixtures::ANSWER)?;
            contradictory_success(&[], Some(500))?;
        }
        "api_status_error" => api_status_error()?,
        // A complete, fully decodable event and then silence at a line
        // boundary: the deadline fires with the reader holding nothing.
        "complete_event_then_hang" => {
            assistant_text(fixtures::ANSWER)?;
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
        // A prefix of an `assistant` event, then silence. The exchange deadline
        // fires while `read_bounded_line` holds bytes it will never deliver, so
        // the suffix that would have said whether a tool call opened is lost.
        "partial_assistant_then_hang" => {
            emit(b"{\"type\":\"assistant\",\"parent_tool_use_id\":null,\"message\":{")?;
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
        // An undecodable event and a `tool_use` event delivered as one write, so
        // both land in a single `fill_buf` batch. The runner delivers the first
        // line, that line fails to decode, and the reader is still holding the
        // second — the one that says a tool call opened — when the exchange ends.
        "undecodable_event_then_buffered_tool_use" => {
            let tool_use = serde_json::json!({
                "type": "assistant", "parent_tool_use_id": null,
                "message": {"model": fixtures::MODEL, "id": fixtures::MESSAGE_ID,
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": fixtures::TOOL_ID,
                        "name": format!("mcp__signalbox_tools__{}", fixtures::TOOL_NAME),
                        "input": {"subject": "synthetic"}, "caller": {"type": "direct"}}]}
            });
            let mut batch = Vec::from(&b"{\"type\":\"synthetic_unrecognized\"}\n"[..]);
            batch.extend_from_slice(&serde_json::to_vec(&tool_use).map_err(std::io::Error::other)?);
            batch.push(b'\n');
            emit(&batch)?;
        }
        "generic_error_then_definitive_stderr_exit" => {
            generic_error_result()?;
            std::io::stderr().write_all(b"authentication failed for synthetic login\n")?;
            std::process::exit(7);
        }
        "truncated_stream" => assistant_text(fixtures::ANSWER)?,
        other => return Err(format!("unsupported synthetic scenario `{other}`").into()),
    }
    Ok(())
}

fn system_init(arguments: &[String]) -> std::io::Result<()> {
    system_init_with_identity(arguments, fixtures::SESSION_ID, fixtures::MODEL)
}

fn system_status(status: Option<&str>) -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "system", "subtype": "status", "status": status,
        "session_id": fixtures::SESSION_ID
    }))
}

fn system_status_with_session(status: Option<&str>, session_id: &str) -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "system", "subtype": "status", "status": status,
        "session_id": session_id
    }))
}

fn system_event(subtype: &str) -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "system", "subtype": subtype, "session_id": fixtures::SESSION_ID
    }))
}

fn system_init_with_identity(
    arguments: &[String],
    session_id: &str,
    reported_model: &str,
) -> std::io::Result<()> {
    system_init_with_version(
        arguments,
        session_id,
        reported_model,
        fixtures::SUPPORTED_VERSION,
    )
}

fn system_init_with_version(
    arguments: &[String],
    session_id: &str,
    reported_model: &str,
    claude_code_version: &str,
) -> std::io::Result<()> {
    let allowed = argument_after(arguments, "--allowedTools").unwrap_or_default();
    let tools = if allowed.is_empty() {
        Vec::new()
    } else {
        allowed.split(',').collect::<Vec<_>>()
    };
    let model = if reported_model == fixtures::MODEL {
        argument_after(arguments, "--model").unwrap_or(reported_model)
    } else {
        reported_model
    };
    emit_json(&serde_json::json!({
        "type": "system", "subtype": "init", "session_id": session_id,
        "tools": tools, "mcp_servers": [{"name": "signalbox_tools", "status": "connected"}],
        "model": model, "slash_commands": [], "skills": [], "plugins": [],
        "claude_code_version": claude_code_version
    }))
}

fn assistant_text(text: &str) -> std::io::Result<()> {
    assistant_text_with_id(fixtures::MESSAGE_ID, text)
}

fn assistant_text_with_id(id: &str, text: &str) -> std::io::Result<()> {
    assistant_text_with_identity(id, fixtures::MODEL, text)
}

fn assistant_text_with_identity(id: &str, model: &str, text: &str) -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "assistant", "parent_tool_use_id": null,
        "message": {"model": model, "id": id, "role": "assistant",
            "content": [{"type": "text", "text": text}],
            "usage": {"input_tokens": fixtures::INPUT_TOKENS, "output_tokens": fixtures::OUTPUT_TOKENS}}
    }))
}

/// The standard tool-call event with the message id as its only knob: the tool
/// identity is the usual fixture, since what varies here is whose message the
/// event claims to belong to.
fn assistant_tool_with_message_id(message_id: &str) -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "assistant", "parent_tool_use_id": null,
        "message": {"model": fixtures::MODEL, "id": message_id, "role": "assistant",
            "content": [{"type": "tool_use", "id": fixtures::TOOL_ID,
                "name": format!("mcp__signalbox_tools__{}", fixtures::TOOL_NAME),
                "input": {"subject": "synthetic"}, "caller": {"type": "direct"}}]}
    }))
}

fn assistant_tool(tool_id: &str, name: &str) -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "assistant", "parent_tool_use_id": null,
        "message": {"model": fixtures::MODEL, "id": fixtures::MESSAGE_ID, "role": "assistant",
            "content": [{"type": "tool_use", "id": tool_id,
                "name": format!("mcp__signalbox_tools__{name}"),
                "input": {"subject": "synthetic"}, "caller": {"type": "direct"}}]}
    }))
}

fn assistant_tool_with_raw_arguments(
    tool_id: &str,
    name: &str,
    arguments: &str,
) -> std::io::Result<()> {
    emit(
        format!(
            "{{\"type\":\"assistant\",\"parent_tool_use_id\":null,\"message\":{{\"model\":\"{}\",\"id\":\"{}\",\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"{tool_id}\",\"name\":\"mcp__signalbox_tools__{name}\",\"input\":{arguments},\"caller\":{{\"type\":\"direct\"}}}}]}}}}\n",
            fixtures::MODEL,
            fixtures::MESSAGE_ID,
        )
        .as_bytes(),
    )
}

fn tool_result(id: &str) -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "user", "message": {"role": "user", "content": [{
            "tool_use_id": id, "type": "tool_result",
            "content": [{"type": "text", "text": "Signalbox recorded this tool proposal for external execution."}]
        }]},
        "tool_use_result": [{"type": "text", "text": "Signalbox recorded this tool proposal for external execution."}]
    }))
}

fn api_status_error() -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "result", "subtype": "error_during_execution", "is_error": true,
        "session_id": fixtures::SESSION_ID, "stop_reason": null,
        "terminal_reason": null, "result": "synthetic provider error",
        "errors": [], "api_error_status": 429,
        "usage": {"input_tokens": fixtures::INPUT_TOKENS, "output_tokens": fixtures::OUTPUT_TOKENS}
    }))
}

/// A structured error that determines no kind on its own: generic subtype, no
/// API status, and a message naming nothing. Usage is stated so the terminal
/// path has a progress fact it must not drop.
fn generic_error_result() -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "result", "subtype": "error_during_execution", "is_error": true,
        "session_id": fixtures::SESSION_ID, "stop_reason": null,
        "terminal_reason": null, "result": "synthetic provider error",
        "errors": [], "api_error_status": null,
        "usage": {"input_tokens": fixtures::INPUT_TOKENS, "output_tokens": fixtures::OUTPUT_TOKENS}
    }))
}

fn contradictory_success(errors: &[&str], api_error_status: Option<u16>) -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false,
        "session_id": fixtures::SESSION_ID, "stop_reason": "end_turn",
        "terminal_reason": "completed", "result": fixtures::ANSWER,
        "errors": errors, "api_error_status": api_error_status,
        "usage": {"input_tokens": fixtures::INPUT_TOKENS, "output_tokens": fixtures::OUTPUT_TOKENS}
    }))
}

fn success_without_stop_reason() -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false,
        "session_id": fixtures::SESSION_ID, "stop_reason": null,
        "terminal_reason": "completed", "result": fixtures::ANSWER, "errors": [],
        "usage": {"input_tokens": fixtures::INPUT_TOKENS, "output_tokens": fixtures::OUTPUT_TOKENS}
    }))
}

fn success(stop_reason: &str, result: Option<&str>) -> std::io::Result<()> {
    success_with_session(fixtures::SESSION_ID, stop_reason, result)
}

fn success_with_session(
    session_id: &str,
    stop_reason: &str,
    result: Option<&str>,
) -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false,
        "session_id": session_id, "stop_reason": stop_reason,
        "terminal_reason": "completed", "result": result, "errors": [],
        "usage": {"input_tokens": fixtures::INPUT_TOKENS, "output_tokens": fixtures::OUTPUT_TOKENS,
            "cache_creation_input_tokens": fixtures::CACHE_CREATION_TOKENS,
            "cache_read_input_tokens": fixtures::CACHE_READ_TOKENS}
    }))
}

fn argument_after<'a>(arguments: &'a [String], name: &str) -> Option<&'a str> {
    arguments
        .windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].as_str())
}

fn record_credential_delivery(arguments: &[String]) -> std::io::Result<()> {
    let settings = argument_after(arguments, "--settings").unwrap_or_default();
    let settings_contents = std::fs::read_to_string(settings).unwrap_or_default();
    std::fs::write("fake-claude-settings", settings_contents)?;
    record_settings_mode(settings)?;
    record_helper_delivery(settings)?;
    std::fs::write(
        "fake-claude-config-dir",
        std::env::var_os("CLAUDE_CONFIG_DIR")
            .unwrap_or_default()
            .to_string_lossy()
            .as_bytes(),
    )?;
    std::fs::write(
        "fake-claude-direct-credential-present",
        std::env::var_os("ANTHROPIC_API_KEY").is_some().to_string(),
    )
}

fn record_helper_delivery(settings: &str) -> std::io::Result<()> {
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(settings).unwrap_or_default())
            .unwrap_or_default();
    let Some(helper) = settings["apiKeyHelper"].as_str() else {
        return Ok(());
    };
    let output = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(helper)
        .output()?;
    std::fs::write("fake-claude-helper-credential", output.stdout)?;
    record_credential_mode()
}

#[cfg(unix)]
fn record_credential_mode() -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let store = std::path::PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR").unwrap_or_default());
    let credential = store.join("credential");
    let mode = std::fs::metadata(credential)?.permissions().mode() & 0o777;
    std::fs::write("fake-claude-credential-mode", format!("{mode:o}"))?;
    let helper = store.join("credential-helper");
    let mode = std::fs::metadata(helper)?.permissions().mode() & 0o777;
    std::fs::write("fake-claude-helper-mode", format!("{mode:o}"))
}

#[cfg(not(unix))]
fn record_credential_mode() -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn record_settings_mode(settings: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = std::fs::metadata(settings)?.permissions().mode() & 0o777;
    std::fs::write("fake-claude-settings-mode", format!("{mode:o}"))
}

#[cfg(not(unix))]
fn record_settings_mode(_settings: &str) -> std::io::Result<()> {
    Ok(())
}

fn scenario(history: &str) -> Result<String, Box<dyn std::error::Error>> {
    let row: serde_json::Value =
        serde_json::from_str(history.lines().next().ok_or("empty native history")?)?;
    let value: serde_json::Value = serde_json::from_str(
        row["message"]["content"][0]["text"]
            .as_str()
            .ok_or("missing canonical message")?,
    )?;
    Ok(value["parts"][0]["text"]
        .as_str()
        .ok_or("missing scenario")?
        .to_string())
}

fn emit_json(value: &serde_json::Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    line.push(b'\n');
    emit(&line)
}

fn emit(line: &[u8]) -> std::io::Result<()> {
    std::io::stdout().write_all(line)?;
    std::io::stdout().flush()
}

fn record_spawn() -> std::io::Result<()> {
    let count = std::fs::read_to_string("fake-claude-spawns")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_default()
        + 1;
    std::fs::write("fake-claude-spawns", count.to_string())
}
