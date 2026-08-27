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
    let scenario = scenario(&prompt)?;
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
    if scenario == "redacted_native_identity" {
        system_init_with_identity(
            &arguments,
            fixtures::CREDENTIAL_SHAPED_SESSION_ID,
            fixtures::CREDENTIAL_SHAPED_MODEL,
        )?;
        assistant_text_with_identity(
            fixtures::MESSAGE_ID,
            fixtures::CREDENTIAL_SHAPED_MODEL,
            fixtures::ANSWER,
        )?;
        success_with_session(
            fixtures::CREDENTIAL_SHAPED_SESSION_ID,
            "end_turn",
            Some(fixtures::ANSWER),
        )?;
        return Ok(());
    }
    if scenario == "suppressed_state_survives_the_usage_barrier" {
        // Two live identifier chains (the model prefix seeded at init, then a
        // message id ending in its own marker prefix) drive the sink into
        // fail-closed suppression, then a terminal error carries a value that
        // continues the suppressed marker.
        system_init_with_identity(
            &arguments,
            fixtures::SESSION_ID,
            fixtures::MODEL_CREDENTIAL_PREFIX,
        )?;
        assistant_text_with_identity(
            fixtures::CREDENTIAL_PREFIX_MESSAGE_ID,
            fixtures::MODEL_CREDENTIAL_PREFIX,
            fixtures::ANSWER,
        )?;
        error_result_with_message(fixtures::OPAQUE_CREDENTIAL_CONTINUATION)?;
        return Ok(());
    }
    if scenario == "model_prefix_redaction" {
        system_init_with_identity(
            &arguments,
            fixtures::SESSION_ID,
            fixtures::MODEL_CREDENTIAL_PREFIX,
        )?;
        assistant_text_with_identity(
            fixtures::MESSAGE_ID,
            fixtures::MODEL_CREDENTIAL_PREFIX,
            fixtures::MODEL_CREDENTIAL_CONTINUATION,
        )?;
        success("end_turn", Some(fixtures::MODEL_RECONSTRUCTED_CREDENTIAL))?;
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
    if scenario == "system_lifecycle_event_redaction" {
        system_init(&arguments)?;
        system_status(Some(fixtures::FRAGMENTED_SECRET_PREFIX))?;
        assistant_text(fixtures::FRAGMENTED_SECRET_CONTINUATION)?;
        success("end_turn", Some(fixtures::FRAGMENTED_SECRET_CONTINUATION))?;
        return Ok(());
    }
    if scenario == "lifecycle_session_contradicts_init" {
        // The contradicting identity carries the credential prefix, so a
        // decoder that discarded it as a repeated identity would drop the
        // lookbehind the continuation below needs.
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
    if scenario == "lifecycle_session_precedes_init" {
        // No init has correlated a session yet, so this identity is not a
        // repeated one and stays provider content that seeds the lookbehind.
        system_event_with_session("status", fixtures::FRAGMENTED_SECRET_PREFIX)?;
        system_init(&arguments)?;
        assistant_text(fixtures::FRAGMENTED_SECRET_CONTINUATION)?;
        success("end_turn", Some(fixtures::FRAGMENTED_SECRET_CONTINUATION))?;
        return Ok(());
    }
    system_init(&arguments)?;
    match scenario.as_str() {
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
        "resolved_model_prefix_redaction" => {
            // The init model is the clean selected alias, so nothing seeds the
            // lookbehind before the assistant event. The resolved model this
            // event newly accepts — stored only for the contradiction check and
            // otherwise discarded — ends in the credential marker its own first
            // text block then continues.
            assistant_text_with_identity(
                fixtures::MESSAGE_ID,
                fixtures::CREDENTIAL_PREFIX_RESOLVED_MODEL,
                fixtures::MODEL_CREDENTIAL_CONTINUATION,
            )?;
            success("end_turn", Some(fixtures::MODEL_CREDENTIAL_CONTINUATION))?;
        }
        "repeated_model_prefix_redaction" => {
            // Clean content in the first event spends the discarded model's
            // lookbehind. The second event must repeat that same model, and its
            // own text block continues the marker the model ends in.
            assistant_text_with_identity(
                fixtures::MESSAGE_ID,
                fixtures::CREDENTIAL_PREFIX_RESOLVED_MODEL,
                fixtures::ANSWER,
            )?;
            assistant_text_with_identity(
                fixtures::MESSAGE_ID,
                fixtures::CREDENTIAL_PREFIX_RESOLVED_MODEL,
                fixtures::MODEL_CREDENTIAL_CONTINUATION,
            )?;
            success("end_turn", Some(fixtures::MODEL_CREDENTIAL_CONTINUATION))?;
        }
        "repeated_model_prefix_release" => {
            // The first event's text continues the model's marker without
            // completing a credential, so the lookbehind is still live when the
            // second event repeats the same model. Reading that repeat as a
            // second independent field would fail the exchange closed and
            // destroy output the held marker eventually releases.
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
        "credential_redaction" => {
            assistant_text(fixtures::SENSITIVE_TEXT)?;
            success("end_turn", Some(fixtures::SENSITIVE_TEXT))?;
        }
        "fragmented_credential_redaction" => {
            assistant_text(fixtures::FRAGMENTED_SECRET_PREFIX)?;
            assistant_text(fixtures::FRAGMENTED_SECRET_CONTINUATION)?;
            success("end_turn", Some(fixtures::FRAGMENTED_SECRET))?;
        }
        "control_sequence_credential_redaction" => {
            assistant_text(fixtures::FRAGMENTED_SECRET_PREFIX)?;
            assistant_text(fixtures::CONTROL_SEQUENCE)?;
            assistant_text(fixtures::CONTROL_SEQUENCE_SECRET_CONTINUATION)?;
            success("end_turn", Some(fixtures::CONTROL_OBFUSCATED_SECRET))?;
        }
        "named_choice_extra_tool" => {
            assistant_tool(fixtures::TOOL_ID, fixtures::TOOL_NAME)?;
            assistant_tool(fixtures::OTHER_TOOL_ID, fixtures::OTHER_TOOL_NAME)?;
            tool_result(fixtures::TOOL_ID)?;
            tool_result(fixtures::OTHER_TOOL_ID)?;
            success("tool_use", None)?;
        }
        "named_choice_suppressed_extra_tool" => {
            assistant_tool(fixtures::TOOL_ID, fixtures::TOOL_NAME)?;
            assistant_tool_with_raw_arguments(
                fixtures::OTHER_TOOL_ID,
                fixtures::OTHER_TOOL_NAME,
                fixtures::SUPPRESSED_TOOL_ARGUMENTS,
            )?;
            tool_result(fixtures::TOOL_ID)?;
            tool_result(fixtures::OTHER_TOOL_ID)?;
            success("tool_use", None)?;
        }
        "redacted_tool_ids" => {
            assistant_tool(fixtures::CREDENTIAL_TOOL_ID_ONE, fixtures::TOOL_NAME)?;
            assistant_tool(fixtures::CREDENTIAL_TOOL_ID_TWO, fixtures::TOOL_NAME)?;
            tool_result(fixtures::CREDENTIAL_TOOL_ID_ONE)?;
            tool_result(fixtures::CREDENTIAL_TOOL_ID_TWO)?;
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
        "credential_finish_token" => {
            assistant_text(fixtures::ANSWER)?;
            success(fixtures::FINISH_TOKEN_SECRET, Some(fixtures::ANSWER))?;
        }
        "credential_error_token" => credential_error_token()?,
        "reasoning_metadata_credential" => reasoning_metadata_credential()?,
        "redacted_reasoning_metadata_credential" => redacted_reasoning_metadata_credential()?,
        "message_id_prefix_redaction" => {
            assistant_text_with_id(
                fixtures::CREDENTIAL_PREFIX_MESSAGE_ID,
                fixtures::OPAQUE_CREDENTIAL_CONTINUATION,
            )?;
            success("end_turn", Some(fixtures::OPAQUE_CREDENTIAL_CONTINUATION))?;
        }
        "tool_id_prefix_redaction" => {
            assistant_tool(fixtures::CREDENTIAL_PREFIX_TOOL_ID, fixtures::TOOL_NAME)?;
            assistant_text(fixtures::OPAQUE_CREDENTIAL_CONTINUATION)?;
            tool_result(fixtures::CREDENTIAL_PREFIX_TOOL_ID)?;
            success("tool_use", Some(fixtures::OPAQUE_CREDENTIAL_CONTINUATION))?;
        }
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

/// Emits a lifecycle event whose only retained member is its `session_id`, so
/// that identity is the trailing dropped context a later field must complete.
fn system_event_with_session(subtype: &str, session_id: &str) -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "system", "subtype": subtype, "session_id": session_id
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

fn reasoning_metadata_credential() -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "assistant", "parent_tool_use_id": null,
        "message": {"model": fixtures::MODEL, "id": fixtures::MESSAGE_ID, "role": "assistant",
            "content": [{"type": "thinking", "thinking": fixtures::REASONING_SECRET_PREFIX,
                "signature": fixtures::REASONING_SECRET_CONTINUATION}]}
    }))?;
    success("end_turn", Some(fixtures::REASONING_SECRET))
}

fn redacted_reasoning_metadata_credential() -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "assistant", "parent_tool_use_id": null,
        "message": {"model": fixtures::MODEL, "id": fixtures::MESSAGE_ID, "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": fixtures::REASONING_SECRET_PREFIX},
                {"type": "redacted_thinking", "data": fixtures::REASONING_SECRET_CONTINUATION}
            ]}
    }))?;
    success("end_turn", Some(fixtures::REASONING_SECRET))
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

/// A terminal error whose provider-controlled message is caller supplied, so a
/// test can drive a specific continuation into `NativeErrorFacts`.
fn error_result_with_message(message: &str) -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "result", "subtype": "error_during_execution", "is_error": true,
        "session_id": fixtures::SESSION_ID, "stop_reason": null,
        "terminal_reason": null, "result": message,
        "errors": [], "api_error_status": null,
        "usage": {"input_tokens": fixtures::INPUT_TOKENS, "output_tokens": fixtures::OUTPUT_TOKENS}
    }))
}

fn credential_error_token() -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "result", "subtype": fixtures::ERROR_TOKEN_SECRET, "is_error": true,
        "session_id": fixtures::SESSION_ID, "stop_reason": null,
        "terminal_reason": null, "result": "synthetic provider error",
        "errors": [], "api_error_status": 500,
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

fn scenario(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    let value: serde_json::Value = serde_json::from_str(
        prompt
            .split_once("\n\n")
            .map(|(_, json)| json.trim())
            .ok_or("missing prompt JSON")?,
    )?;
    Ok(value["messages"][0]["parts"][0]["text"]
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
