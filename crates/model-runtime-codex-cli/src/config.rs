//! Adapter configuration.

use std::path::PathBuf;
use std::time::Duration;

/// Configuration for [`crate::CodexCliRuntime`].
///
/// It carries paths, bounds, and a non-secret credential reference only. The
/// CLI resolves its own subscription login; the adapter never receives a
/// credential value.
#[derive(Debug, Clone)]
pub struct CodexCliConfig {
    /// Locally installed Codex executable.
    pub executable: PathBuf,
    /// Existing directory used as the CLI's working root.
    pub working_directory: PathBuf,
    /// Non-secret durable reference that names the operator-selected ambient
    /// Codex login. Operations prepared by this runtime must carry this exact
    /// reference.
    pub credential_reference: signalbox_model_runtime::CredentialReference,
    /// Positive whole-process timeout.
    pub exchange_timeout: Duration,
    /// Grace after a cancellation interrupt before force-killing the process.
    pub interrupt_grace: Duration,
    /// Maximum bytes admitted for one JSONL stdout event.
    pub event_limit: usize,
    /// Maximum stderr bytes retained as native failure evidence.
    pub stderr_limit: usize,
}

impl CodexCliConfig {
    /// Builds configuration with conservative process and evidence bounds.
    pub fn new(
        executable: impl Into<PathBuf>,
        working_directory: impl Into<PathBuf>,
        credential_reference: signalbox_model_runtime::CredentialReference,
    ) -> Self {
        Self {
            executable: executable.into(),
            working_directory: working_directory.into(),
            credential_reference,
            exchange_timeout: Duration::from_secs(10 * 60),
            interrupt_grace: Duration::from_secs(2),
            event_limit: 8 * 1024 * 1024,
            stderr_limit: 64 * 1024,
        }
    }
}
