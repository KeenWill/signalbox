//! Codex CLI subscription adapter for the Layer-1 model runtime specified in
//! `docs/spec/runtime-substrate.md`.
//!
//! One prepared operation becomes one fresh `codex exec --json --ephemeral`
//! process. Process spawn is this adapter's irrevocable-dispatch boundary:
//! preparation performs no spawn, execution never respawns, and a process
//! that ends without definitive Codex terminal evidence is never completion.
//!
//! The CLI owns subscription authentication. This crate invokes the binary
//! and neither locates nor reads its credential store. Provider-controlled
//! output is sanitized for credential-shaped material before it crosses the
//! adapter boundary.

mod config;
mod event;
#[cfg(test)]
mod redaction;
mod runtime;
mod translate;
mod wire;

pub use config::CodexCliConfig;
pub use runtime::{
    CodexCliConstructionError, CodexCliPreparedRequest, CodexCliRuntime, CodexCliVersionProbeError,
    DISABLED_CODEX_CLI_CAPABILITY_FEATURES, SUPPORTED_CODEX_CLI_VERSION, validate_model_settings,
    verify_pinned_codex_cli_version,
};
