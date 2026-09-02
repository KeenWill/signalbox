//! Claude Code subscription adapter for the Layer-1 model runtime specified in
//! `docs/spec/runtime-substrate.md`.
//!
//! One prepared operation becomes one fresh `claude --print --verbose
//! --output-format=stream-json` process. Process spawn is this adapter's
//! irrevocable-dispatch boundary: preparation performs no spawn, execution
//! never respawns, and a process that ends without definitive typed Claude
//! terminal evidence is never completion.
//!
//! Ambient delivery leaves subscription authentication inside Claude Code.
//! File delivery resolves one credential during preparation and materializes it
//! only in a private request-scoped settings store beneath the child-selected
//! `CLAUDE_CONFIG_DIR`; the direct key is never part of the adapter-assembled
//! child environment. Provider-controlled output is sanitized for
//! credential-shaped material and, for file delivery, the exact request value
//! before it crosses the adapter boundary.

#[allow(dead_code)]
mod bridge;
mod config;
mod event;
mod runtime;
mod status;
mod translate;
mod wire;

pub use config::ClaudeCliConfig;
pub use runtime::{
    CLAUDE_CLI_FILE_CREDENTIAL_ENV_KEY, ClaudeCliConstructionError, ClaudeCliPreparedRequest,
    ClaudeCliRuntime, DISABLED_CLAUDE_CLI_BUILTIN_TOOLS, SUPPORTED_CLAUDE_CLI_VERSION,
    validate_model_settings,
};
