//! Scripted offline Claude Code executable used by integration tests.

use std::io::{Read, Write};

mod fixtures;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    record_spawn()?;
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    std::fs::write("fake-claude-argv", arguments.join("\n"))?;
    let mut prompt = String::new();
    std::io::stdin().read_to_string(&mut prompt)?;
    std::fs::write("fake-claude-prompt", &prompt)?;
    let scenario = scenario(&prompt)?;
    if scenario == "process_nonzero" {
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
    system_init(&arguments)?;
    match scenario.as_str() {
        "normal_completion" => {
            assistant_text(fixtures::ANSWER)?;
            success("end_turn", Some(fixtures::ANSWER))?;
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
        "success_without_stop_reason" => {
            assistant_text(fixtures::ANSWER)?;
            success_without_stop_reason()?;
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
        "truncated_stream" => assistant_text(fixtures::ANSWER)?,
        other => return Err(format!("unsupported synthetic scenario `{other}`").into()),
    }
    Ok(())
}

fn system_init(arguments: &[String]) -> std::io::Result<()> {
    system_init_with_identity(arguments, fixtures::SESSION_ID, fixtures::MODEL)
}

fn system_init_with_identity(
    arguments: &[String],
    session_id: &str,
    reported_model: &str,
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
        "claude_code_version": "2.1.220"
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
