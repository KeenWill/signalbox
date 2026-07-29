//! Claude Code subscription adapter for the Layer-1 model runtime specified in
//! `docs/spec/runtime-substrate.md`.
//!
//! One prepared operation becomes one fresh `claude --print --verbose
//! --output-format=stream-json` process. Process spawn is this adapter's
//! irrevocable-dispatch boundary: preparation performs no spawn, execution
//! never respawns, and a process that ends without definitive typed Claude
//! terminal evidence is never completion.
//!
//! Claude Code owns subscription authentication. This crate invokes the binary
//! and neither locates nor reads its credential store. Provider-controlled
//! output is sanitized for credential-shaped material before it crosses the
//! adapter boundary.

#[allow(dead_code)]
mod bridge;
mod config;
mod event;
mod redaction;
mod runtime;
mod status;
mod translate;
mod wire;

pub use config::ClaudeCliConfig;
pub use runtime::{
    ClaudeCliConstructionError, ClaudeCliPreparedRequest, ClaudeCliRuntime,
    DISABLED_CLAUDE_CLI_BUILTIN_TOOLS, SUPPORTED_CLAUDE_CLI_VERSION,
};
