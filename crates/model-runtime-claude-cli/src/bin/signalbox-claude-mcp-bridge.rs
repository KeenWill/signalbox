//! Compiled entrypoint for the `signalbox-claude-mcp-bridge` binary: includes
//! `bridge.rs` via `#[path]` and calls `bridge::run` (see that module's doc
//! comment for what the bridge does).

#[allow(dead_code)]
#[path = "../bridge.rs"]
mod bridge;

fn main() -> std::process::ExitCode {
    bridge::run(std::env::args_os())
}
