//! Adapter configuration.

use std::time::Duration;
use std::{collections::HashMap, path::PathBuf};

/// Configuration for [`crate::CodexCliRuntime`].
///
/// It carries model controls, paths, bounds, and a non-secret credential
/// references only. OAuth values arrive through the daemon delivery boundary.
#[derive(Clone)]
pub struct CodexCliConfig {
    /// Exact per-model reasoning, fast-mode, and service-tier capabilities.
    pub model_capabilities: signalbox_model_runtime::ModelCapabilityCatalog,
    /// Exact provider-model names whose Codex context window is overridden.
    pub model_context_window_overrides: HashMap<String, u32>,
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
    /// `docs/spec/configuration-and-credentials.md`.
    pub credential_homes: HashMap<signalbox_model_runtime::CredentialReference, PathBuf>,
    /// References whose authentication is supplied by daemon-owned OAuth delivery.
    pub oauth_profiles: std::collections::HashSet<signalbox_model_runtime::CredentialReference>,
    /// Optional positive whole-process timeout representable by the runtime clock.
    pub exchange_timeout: Option<Duration>,
    /// Grace after a cancellation interrupt before force-killing the process.
    pub interrupt_grace: Duration,
    /// Maximum post-kill wait, or unbounded when explicitly configured as
    /// `none`.
    pub post_kill_reap_bound: Option<Duration>,
    /// Maximum bytes admitted for one JSONL stdout event.
    pub event_limit: usize,
    /// Maximum stderr bytes retained as native failure evidence.
    pub stderr_limit: usize,
}

impl std::fmt::Debug for CodexCliConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexCliConfig")
            .field("model_capabilities", &self.model_capabilities)
            .field(
                "model_context_window_overrides",
                &self.model_context_window_overrides,
            )
            .field("executable", &"[redacted]")
            .field("working_directory", &"[redacted]")
            .field("credential_reference", &self.credential_reference)
            .field("credential_home_count", &self.credential_homes.len())
            .field("oauth_profiles", &self.oauth_profiles)
            .field("exchange_timeout", &self.exchange_timeout)
            .field("interrupt_grace", &self.interrupt_grace)
            .field("post_kill_reap_bound", &self.post_kill_reap_bound)
            .field("event_limit", &self.event_limit)
            .field("stderr_limit", &self.stderr_limit)
            .finish()
    }
}

impl CodexCliConfig {
    /// Builds configuration with the caller-supplied process-reap policy.
    pub fn new(
        executable: impl Into<PathBuf>,
        working_directory: impl Into<PathBuf>,
        credential_reference: signalbox_model_runtime::CredentialReference,
        post_kill_reap_bound: Option<Duration>,
    ) -> Self {
        Self {
            model_capabilities: signalbox_model_runtime::ModelCapabilityCatalog::empty(),
            model_context_window_overrides: HashMap::new(),
            executable: executable.into(),
            working_directory: working_directory.into(),
            credential_reference,
            credential_homes: HashMap::new(),
            oauth_profiles: std::collections::HashSet::new(),
            exchange_timeout: None,
            interrupt_grace: Duration::from_secs(2),
            post_kill_reap_bound,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_debug_omits_credential_and_host_paths() {
        let credential_home = PathBuf::from("/synthetic-private-account/login-home");
        let executable = PathBuf::from("/synthetic-private-install/codex");
        let workspace = PathBuf::from("/synthetic-private-workspace");
        let reference = signalbox_model_runtime::CredentialReference::new("fixture-profile");
        let config = CodexCliConfig::new(&executable, &workspace, reference.clone(), None)
            .with_credential_homes([(reference, credential_home.clone())]);

        let debug = format!("{config:?}");

        assert!(!debug.contains(credential_home.to_string_lossy().as_ref()));
        assert!(!debug.contains(executable.to_string_lossy().as_ref()));
        assert!(!debug.contains(workspace.to_string_lossy().as_ref()));
        assert!(debug.contains("fixture-profile"));
        assert!(debug.contains("credential_home_count: 1"));
    }

    #[test]
    fn constructed_runtime_debug_omits_host_paths() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        let executable = workspace.path().join("synthetic-private-codex");
        let reference = signalbox_model_runtime::CredentialReference::new("fixture-profile");
        let runtime = crate::CodexCliRuntime::new(CodexCliConfig::new(
            &executable,
            workspace.path(),
            reference,
            None,
        ))?;

        let debug = format!("{runtime:?}");

        assert!(!debug.contains(executable.to_string_lossy().as_ref()));
        assert!(!debug.contains(workspace.path().to_string_lossy().as_ref()));
        assert!(debug.contains("fixture-profile"));
        Ok(())
    }
}
