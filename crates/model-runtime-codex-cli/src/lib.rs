//! Codex CLI subscription adapter for the Layer-1 model runtime specified in
//! `docs/spec/runtime-substrate.md`.
//!
//! One prepared operation becomes one fresh `codex app-server --stdio`
//! process. Process spawn is this adapter's irrevocable-dispatch boundary:
//! preparation performs no spawn, execution never respawns, and a process
//! that ends without definitive Codex terminal evidence is never completion.
//!
//! Ambient and credential-home profiles use the CLI's login and pass output
//! through unmodified. OAuth profiles receive daemon-minted tokens in isolated
//! homes, with exact-value redaction before output crosses the adapter boundary.

mod app_server;
mod config;
mod image;
pub use image::image_presentation_capability;
mod event;
mod executable_pin;
mod oauth;
mod runtime;
mod translate;

pub use translate::serialized_message_bytes;
mod wire;

pub use config::CodexCliConfig;
pub use oauth::{
    OauthCredentialInstaller, OauthCredentialMaterial, OauthCredentialProvider,
    OauthCredentialRoot, OauthDeliveryFuture, OauthDeliveryOutcome, OauthRecoveryFuture,
};
pub use runtime::{
    CodexCliConstructionError, CodexCliPreparedRequest, CodexCliRuntime, CodexCliVersionProbe,
    CodexCliVersionProbeError, DISABLED_CODEX_CLI_CAPABILITY_FEATURES, SUPPORTED_CODEX_CLI_VERSION,
    probe_pinned_codex_cli_version, validate_model_settings, verify_pinned_codex_cli_version,
};
