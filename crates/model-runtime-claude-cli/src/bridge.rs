//! Adapter-owned MCP bridge exposing declared tool schemas to Claude Code.
//!
//! The bridge never executes a caller tool. It acknowledges each call so
//! Claude Code carries a typed `tool_result` back through its stream; the
//! adapter returns the preceding typed `tool_use` as the external proposal.

use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

pub(crate) const SERVER_NAME: &str = "signalbox_tools";
pub(crate) const TOOL_PREFIX: &str = "mcp__signalbox_tools__";
pub(crate) const TOOL_ACKNOWLEDGEMENT: &str =
    "Signalbox recorded this tool proposal for external execution.";
const BRIDGE_LINE_LIMIT: usize = 8 * 1024 * 1024;
const READY_WAIT_LIMIT: Duration = Duration::from_secs(10);
const READY_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Catalog {
    pub(crate) tools: Vec<CatalogTool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CatalogTool {
    pub(crate) name: String,
    pub(crate) description: String,
    #[serde(rename = "inputSchema")]
    pub(crate) input_schema: Box<RawValue>,
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

/// Runs the private bridge binary command.
///
/// Failures are exit-status only. The bridge deliberately writes no diagnostic
/// text because its inputs can contain caller tool descriptions and arguments.
pub(crate) fn run(arguments: impl IntoIterator<Item = OsString>) -> ExitCode {
    let mut arguments = arguments.into_iter();
    let _program = arguments.next();
    match (
        arguments.next().and_then(|value| value.into_string().ok()),
        arguments.next(),
        arguments.next(),
        arguments.next(),
    ) {
        (Some(mode), Some(catalog), Some(ready), None) if mode == "--serve" => {
            serve(PathBuf::from(catalog), PathBuf::from(ready))
        }
        (Some(mode), Some(ready), None, None) if mode == "--wait-ready" => {
            wait_ready(PathBuf::from(ready))
        }
        _ => ExitCode::FAILURE,
    }
}

fn wait_ready(path: PathBuf) -> ExitCode {
    let deadline = Instant::now() + READY_WAIT_LIMIT;
    while Instant::now() < deadline {
        if path.is_file() {
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(READY_POLL_INTERVAL);
    }
    ExitCode::FAILURE
}

fn serve(catalog_path: PathBuf, ready_path: PathBuf) -> ExitCode {
    let Ok(catalog_bytes) = std::fs::read(catalog_path) else {
        return ExitCode::FAILURE;
    };
    let Ok(catalog) = serde_json::from_slice::<Catalog>(&catalog_bytes) else {
        return ExitCode::FAILURE;
    };
    if !catalog_is_valid(&catalog) {
        return ExitCode::FAILURE;
    }
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        let Ok(read) = input.read_until(b'\n', &mut line) else {
            return ExitCode::FAILURE;
        };
        if read == 0 {
            return ExitCode::SUCCESS;
        }
        if line.len() > BRIDGE_LINE_LIMIT {
            return ExitCode::FAILURE;
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        let Ok(request) = serde_json::from_slice::<Request>(&line) else {
            return ExitCode::FAILURE;
        };
        let response = match request.method.as_str() {
            "initialize" => initialize_response(request.id, &request.params),
            "tools/list" => {
                let response = result_response(
                    request.id,
                    serde_json::json!({
                        "tools": &catalog.tools,
                    }),
                );
                if write_response(&mut output, &response).is_err()
                    || create_ready_marker(&ready_path).is_err()
                {
                    return ExitCode::FAILURE;
                }
                continue;
            }
            "tools/call" => tool_call_response(request.id, &request.params, &catalog),
            _ if request.id.is_none() => continue,
            _ => error_response(request.id, -32601, "method not implemented"),
        };
        if write_response(&mut output, &response).is_err() {
            return ExitCode::FAILURE;
        }
    }
}

fn catalog_is_valid(catalog: &Catalog) -> bool {
    let mut names = std::collections::HashSet::with_capacity(catalog.tools.len());
    catalog.tools.iter().all(|tool| {
        valid_mcp_tool_name(&tool.name)
            && names.insert(tool.name.as_str())
            && serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                tool.input_schema.get(),
            )
            .is_ok()
    })
}

pub(crate) fn valid_mcp_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn initialize_response(
    id: Option<serde_json::Value>,
    params: &serde_json::Value,
) -> serde_json::Value {
    let protocol_version = params
        .get("protocolVersion")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("2025-11-25");
    result_response(
        id,
        serde_json::json!({
            "protocolVersion": protocol_version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {
                "name": "signalbox-claude-cli-bridge",
                "version": env!("CARGO_PKG_VERSION"),
            },
        }),
    )
}

fn tool_call_response(
    id: Option<serde_json::Value>,
    params: &serde_json::Value,
    catalog: &Catalog,
) -> serde_json::Value {
    let name = params.get("name").and_then(serde_json::Value::as_str);
    let arguments_are_object = params
        .get("arguments")
        .is_some_and(serde_json::Value::is_object);
    if !arguments_are_object
        || !name.is_some_and(|name| catalog.tools.iter().any(|tool| tool.name == name))
    {
        return error_response(id, -32602, "undeclared tool or non-object arguments");
    }
    result_response(
        id,
        serde_json::json!({
            "content": [{"type": "text", "text": TOOL_ACKNOWLEDGEMENT}],
            "isError": false,
        }),
    )
}

fn result_response(id: Option<serde_json::Value>, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(serde_json::Value::Null),
        "result": result,
    })
}

fn error_response(id: Option<serde_json::Value>, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(serde_json::Value::Null),
        "error": {"code": code, "message": message},
    })
}

fn write_response(output: &mut impl Write, response: &serde_json::Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *output, response).map_err(std::io::Error::other)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn create_ready_marker(path: &Path) -> std::io::Result<()> {
    let mut marker = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    marker.write_all(b"ready\n")?;
    marker.sync_all()
}

#[cfg(test)]
mod tests {
    use super::valid_mcp_tool_name;

    #[test]
    fn mcp_tool_name_accepts_the_supported_punctuation() {
        assert!(valid_mcp_tool_name("lookup-city.v2"));
    }

    #[test]
    fn mcp_tool_name_rejects_spaces() {
        assert!(!valid_mcp_tool_name("lookup city"));
    }

    #[test]
    fn mcp_tool_name_rejects_an_empty_name() {
        assert!(!valid_mcp_tool_name(""));
    }
}
