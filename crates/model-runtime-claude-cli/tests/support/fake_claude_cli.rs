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
        emit(br#"{"type":"system","type":"result","subtype":"success","is_error":false}\n"#)?;
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
            success("end_turn", Some(fixtures::TOOL_ARGUMENTS))?;
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
        "named_choice_extra_tool" => {
            assistant_tool(fixtures::TOOL_ID, fixtures::TOOL_NAME)?;
            assistant_tool(fixtures::OTHER_TOOL_ID, fixtures::OTHER_TOOL_NAME)?;
            tool_result(fixtures::TOOL_ID)?;
            tool_result(fixtures::OTHER_TOOL_ID)?;
            success("end_turn", None)?;
        }
        "api_status_error" => api_status_error()?,
        "truncated_stream" => assistant_text(fixtures::ANSWER)?,
        other => return Err(format!("unsupported synthetic scenario `{other}`").into()),
    }
    Ok(())
}

fn system_init(arguments: &[String]) -> std::io::Result<()> {
    let allowed = argument_after(arguments, "--allowedTools").unwrap_or_default();
    let tools = if allowed.is_empty() {
        Vec::new()
    } else {
        allowed.split(',').collect::<Vec<_>>()
    };
    let model = argument_after(arguments, "--model").unwrap_or(fixtures::MODEL);
    emit_json(&serde_json::json!({
        "type": "system", "subtype": "init", "session_id": fixtures::SESSION_ID,
        "tools": tools, "mcp_servers": [{"name": "signalbox_tools", "status": "connected"}],
        "model": model, "slash_commands": [], "skills": [], "plugins": [],
        "claude_code_version": "2.1.220"
    }))
}

fn assistant_text(text: &str) -> std::io::Result<()> {
    assistant_text_with_id(fixtures::MESSAGE_ID, text)
}

fn assistant_text_with_id(id: &str, text: &str) -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "assistant", "parent_tool_use_id": null,
        "message": {"model": fixtures::MODEL, "id": id, "role": "assistant",
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

fn success_without_stop_reason() -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false,
        "session_id": fixtures::SESSION_ID, "stop_reason": null,
        "terminal_reason": "completed", "result": fixtures::ANSWER, "errors": [],
        "usage": {"input_tokens": fixtures::INPUT_TOKENS, "output_tokens": fixtures::OUTPUT_TOKENS}
    }))
}

fn success(stop_reason: &str, result: Option<&str>) -> std::io::Result<()> {
    emit_json(&serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false,
        "session_id": fixtures::SESSION_ID, "stop_reason": stop_reason,
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
