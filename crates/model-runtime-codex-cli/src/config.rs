//! Adapter configuration.

use std::time::Duration;
use std::{collections::HashMap, path::PathBuf};

/// Configuration for [`crate::CodexCliRuntime`].
///
/// It carries paths, bounds, and a non-secret credential reference only. The
/// CLI resolves its own subscription login; the adapter never receives a
/// credential value.
#[derive(Debug, Clone)]
pub struct CodexCliConfig {
    /// Exact per-model reasoning, fast-mode, and service-tier capabilities.
    pub model_capabilities: signalbox_model_runtime::ModelCapabilityCatalog,
    /// Absolute path to the locally installed Codex executable.
    pub executable: PathBuf,
    /// Absolute existing directory used as the CLI's working root.
    pub working_directory: PathBuf,
    /// Non-secret durable reference that names the operator-selected ambient
    /// Codex login. Operations prepared by this runtime must carry this exact
    /// reference.
    pub credential_reference: signalbox_model_runtime::CredentialReference,
    /// Per-profile login homes. Values are path references only; the adapter
    /// never reads their auth material. See
    /// `docs/spec/configuration-and-credentials.md#the-codex_home-delivery`.
    pub credential_homes: HashMap<signalbox_model_runtime::CredentialReference, PathBuf>,
    /// Positive whole-process timeout representable by the runtime clock.
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
            model_capabilities: signalbox_model_runtime::ModelCapabilityCatalog::empty(),
            executable: executable.into(),
            working_directory: working_directory.into(),
            credential_reference,
            credential_homes: HashMap::new(),
            exchange_timeout: Duration::from_secs(10 * 60),
            interrupt_grace: Duration::from_secs(2),
            event_limit: 8 * 1024 * 1024,
            stderr_limit: 64 * 1024,
        }
    }

    /// Supplies admitted per-profile `CODEX_HOME` paths under the delivery
    /// contract in `docs/spec/configuration-and-credentials.md`.
    pub fn with_credential_homes(
        mut self,
        homes: impl IntoIterator<Item = (signalbox_model_runtime::CredentialReference, PathBuf)>,
    ) -> Self {
        self.credential_homes = homes.into_iter().collect();
        self
    }
}
