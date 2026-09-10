//! One operation, one Codex CLI process spawn.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use std::{collections::HashMap, path::PathBuf};

use signalbox_model_runtime::{
    AnthropicServiceTier, CLI_PROCESS_GROUP_SUPERVISION_SUPPORTED, CancellationSignal,
    CliEnvironmentOverride, CliEnvironmentVariable, CliProcessRequest, CodexCliServiceTier,
    DeliveryMode, FastMode, ModelCapabilityCatalog, ModelOperation, ModelRuntime, ModelSettings,
    ObservationSink, OpenAiServiceTier, PreparationDefect, PreparationFailure, PreparationOutcome,
    ProvenUnsentEvidence, ReasoningLevel, ServiceTier, TerminalEvidence, TerminalReport,
    UnsentCause, execute_cli_process,
};
use tempfile::TempDir;

use crate::config::CodexCliConfig;
use crate::event::EventDecoder;
use crate::translate::{TranslationError, translate};
use crate::wire::OUTPUT_SCHEMA;

const CODEX_CREDENTIAL_HOME: &str = "CODEX_HOME";
#[cfg(test)]
const FORBIDDEN_DIRECT_CREDENTIAL_ENVIRONMENT: &str = "OPENAI_API_KEY";
const CODEX_ENVIRONMENT: &[CliEnvironmentVariable] = &[
    CliEnvironmentVariable::inherited("ALL_PROXY"),
    CliEnvironmentVariable::credential_home(CODEX_CREDENTIAL_HOME),
    CliEnvironmentVariable::inherited("COLORTERM"),
    CliEnvironmentVariable::credential_home("HOME"),
    CliEnvironmentVariable::inherited("HTTP_PROXY"),
    CliEnvironmentVariable::inherited("HTTPS_PROXY"),
    CliEnvironmentVariable::inherited("LANG"),
    CliEnvironmentVariable::inherited("LC_ALL"),
    CliEnvironmentVariable::inherited("LC_CTYPE"),
    CliEnvironmentVariable::inherited("NO_PROXY"),
    CliEnvironmentVariable::inherited("PATH"),
    CliEnvironmentVariable::inherited("SSL_CERT_DIR"),
    CliEnvironmentVariable::inherited("SSL_CERT_FILE"),
    CliEnvironmentVariable::inherited("TEMP"),
    CliEnvironmentVariable::inherited("TERM"),
    CliEnvironmentVariable::inherited("TMP"),
    CliEnvironmentVariable::inherited("TMPDIR"),
    CliEnvironmentVariable::inherited("XDG_CACHE_HOME"),
    CliEnvironmentVariable::inherited("XDG_CONFIG_HOME"),
    CliEnvironmentVariable::inherited("XDG_DATA_HOME"),
    CliEnvironmentVariable::inherited("all_proxy"),
    CliEnvironmentVariable::inherited("http_proxy"),
    CliEnvironmentVariable::inherited("https_proxy"),
    CliEnvironmentVariable::inherited("no_proxy"),
];

/// Every pinned-CLI feature that can add a model-visible tool, external
/// interaction, instruction source, or delegated execution surface outside
/// the declared `ModelOperation` tools, or that can replace the pinned
/// executable this adapter's version contract names. The version-bump smoke
/// classifies the CLI's complete feature inventory so a new unclassified
/// feature fails that gate before this list can silently become incomplete.
///
/// The invocation passes `--disable` for every name here. The pinned CLI
/// accepts a name its registry still declares — including one whose stage is
/// `removed` — and rejects an unknown name outright, so a release that
/// *deletes* a name would fail every model call. The smoke's exact inventory
/// comparison is what catches such a deletion before the pin moves.
///
/// Exported so the compatibility smoke can prove that its pinned feature
/// classification and the invocation's hard disables remain the same set.
pub const DISABLED_CODEX_CLI_CAPABILITY_FEATURES: &[&str] = &[
    "apps",
    "artifact",
    "auth_elicitation",
    "browser_use",
    "browser_use_external",
    "browser_use_full_cdp_access",
    "code_mode",
    "code_mode_buffered_exec",
    "code_mode_host",
    "code_mode_only",
    "computer_use",
    "context_management",
    "current_time_reminder",
    "default_mode_request_user_input",
    "deferred_executor",
    "deferred_tool_world_state",
    "enable_mcp_apps",
    "exec_permission_approvals",
    "executor_capability_discovery",
    "external_agent_memory_import",
    "goals",
    "guardian_approval",
    // The registry gate for the CLI's guardian extension — the subsystem whose
    // two wired gates, immediately above and below, are already disabled: it
    // installs review contributors that spawn their own model exchanges. The
    // pinned release reads this name nowhere, so disabling it changes nothing
    // today; classifying an unwired name in that subsystem as behavior would
    // instead make the release that wires it a silent capability gain.
    "guardian_ext",
    "guardianv2",
    "hooks",
    "image_generation",
    "in_app_browser",
    "in_app_chat",
    "in_app_dictation",
    "in_app_local_automation",
    // The CLI update flow can replace the pinned executable.
    "in_app_updates",
    "mcp_2026_07_28",
    "mcp_oauth_refresh_coordination",
    "memories",
    "multi_agent",
    "multi_agent_v2",
    "plugin_sharing",
    "plugins",
    "powershell_shell_version",
    "realtime_conversation",
    "recommended_plugins",
    "remote_plugin",
    "request_permissions_tool",
    "shell_snapshot",
    "shell_snapshot_v2",
    "shell_tool",
    "skill_mcp_dependency_install",
    "skill_search",
    "sleep_tool",
    "standalone_web_search",
    "step_model_switching",
    "token_budget",
    "tool_call_mcp_elicitation",
    "tool_suggest",
    "unified_exec",
    "view_image",
    "workspace_dependencies",
];

/// Codex CLI protocol snapshot covered by this adapter's offline fixtures.
///
/// The build derives this marker from the exact pin in
/// `tooling/codex-cli/release.json`, so a Renovate change is mechanically
/// complete and the binding smoke tests that same version. That live exchange
/// is accompanied by a comparison of the pin's checked-in app-server schemas
/// with the adapter's consumed fields, enum members, and required fields.
/// Consumed fields must remain decoder-compatible, adapter-required fields must
/// remain required, and turn statuses must match. Tagged error objects contain
/// only their tag. Compatible additions are reported; consumed fields and error
/// members must remain present. The runtime does not add a version-probe process
/// to a model dispatch.
pub const SUPPORTED_CODEX_CLI_VERSION: &str = env!("SIGNALBOX_CODEX_CLI_VERSION");

/// Structural bound on retained version-banner bytes.
const MAX_VERSION_BANNER_BYTES: usize = 4096;

/// Why the configured Codex executable could not prove the adapter's pin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexCliVersionProbeError {
    /// The configured probe bound was zero.
    InvalidBound,
    /// The executable file could not be read for SHA-256 verification.
    ExecutableReadFailed,
    /// The executable could not be started or its path was not absolute.
    SpawnFailed,
    /// The executable did not finish within the deployment-owned bound.
    TimedOut,
    /// The executable's bounded output could not be collected.
    OutputFailed,
    /// The executable rejected the version request.
    Unsuccessful,
    /// The version banner was not bounded UTF-8 with a version token.
    InvalidBanner,
    /// The invoked executable does not match the adapter's exact pin.
    VersionMismatch,
}

impl std::fmt::Display for CodexCliVersionProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidBound => "Codex CLI version probe bound is invalid",
            Self::ExecutableReadFailed => "Codex CLI executable could not be read",
            Self::SpawnFailed => "Codex CLI version probe could not start",
            Self::TimedOut => "Codex CLI version probe exceeded its bound",
            Self::OutputFailed => "Codex CLI version probe output could not be collected",
            Self::Unsuccessful => "Codex CLI version probe exited unsuccessfully",
            Self::InvalidBanner => "Codex CLI version banner is invalid",
            Self::VersionMismatch => "Codex CLI executable does not match the adapter pin",
        })
    }
}

impl std::error::Error for CodexCliVersionProbeError {}

impl CodexCliVersionProbeError {
    /// Stable cause code for adapter availability and operator status.
    pub const fn cause_code(self) -> &'static str {
        match self {
            Self::InvalidBound => "codex_cli_probe_invalid_bound",
            Self::ExecutableReadFailed => "codex_cli_executable_read_failed",
            Self::SpawnFailed => "codex_cli_probe_spawn_failed",
            Self::TimedOut => "codex_cli_probe_timed_out",
            Self::OutputFailed => "codex_cli_probe_output_failed",
            Self::Unsuccessful => "codex_cli_probe_unsuccessful",
            Self::InvalidBanner => "codex_cli_version_invalid",
            Self::VersionMismatch => "codex_cli_pin_mismatch",
        }
    }
}

/// Installed Codex executable facts observed by the bounded startup probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexCliVersionProbe {
    version: semver::Version,
    digest: String,
    matches_pin: bool,
}

impl CodexCliVersionProbe {
    /// Installed semantic version reported by the executable.
    pub fn version(&self) -> &semver::Version {
        &self.version
    }

    /// Installed executable SHA-256 in lowercase hexadecimal.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Whether both installed facts match the adapter pin.
    pub const fn matches_pin(&self) -> bool {
        self.matches_pin
    }
}

/// Observes the installed Codex version and digest without admitting the adapter.
pub async fn probe_pinned_codex_cli_version(
    executable: &Path,
    bound: Duration,
) -> Result<CodexCliVersionProbe, CodexCliVersionProbeError> {
    let (version, deadline) = probe_codex_cli_version(executable, bound).await?;
    let digest = crate::executable_pin::executable_digest(executable, deadline).await?;
    let supported = semver::Version::parse(SUPPORTED_CODEX_CLI_VERSION)
        .map_err(|_| CodexCliVersionProbeError::InvalidBanner)?;
    let matches_pin = version == supported && digest == env!("SIGNALBOX_CODEX_CLI_SHA256");
    Ok(CodexCliVersionProbe {
        version,
        digest,
        matches_pin,
    })
}

/// Proves that the executable invoked by the composition matches this
/// adapter's upstream version and executable SHA-256 pin before the composition
/// admits model work. The path must be absolute; both checks share `bound`.
pub async fn verify_pinned_codex_cli_version(
    executable: &Path,
    bound: Duration,
) -> Result<(), CodexCliVersionProbeError> {
    let probe = probe_pinned_codex_cli_version(executable, bound).await?;
    if probe.matches_pin() {
        Ok(())
    } else {
        Err(CodexCliVersionProbeError::VersionMismatch)
    }
}

async fn probe_codex_cli_version(
    executable: &Path,
    bound: Duration,
) -> Result<(semver::Version, tokio::time::Instant), CodexCliVersionProbeError> {
    if bound.is_zero() {
        return Err(CodexCliVersionProbeError::InvalidBound);
    }
    if !executable.is_absolute() {
        return Err(CodexCliVersionProbeError::SpawnFailed);
    }
    let deadline = tokio::time::Instant::now()
        .checked_add(bound)
        .ok_or(CodexCliVersionProbeError::InvalidBound)?;
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    #[cfg(unix)]
    let exits = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
        .map_err(|_| CodexCliVersionProbeError::OutputFailed)?;
    let mut child = command
        .spawn()
        .map_err(|_| CodexCliVersionProbeError::SpawnFailed)?;
    let mut process_group = VersionProbeProcessGroup {
        id: child.id(),
        #[cfg(unix)]
        exits,
    };
    let output = match tokio::time::timeout_at(
        deadline,
        collect_version_output(&mut child, &mut process_group),
    )
    .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            process_group.kill();
            let _ = tokio::time::timeout_at(deadline, child.wait()).await;
            return Err(error);
        }
        Err(_) => {
            process_group.kill();
            let _ = tokio::time::timeout_at(deadline, child.wait()).await;
            return Err(CodexCliVersionProbeError::TimedOut);
        }
    };
    if !output.status.success() {
        return Err(CodexCliVersionProbeError::Unsuccessful);
    }
    let banner = std::str::from_utf8(&output.stdout)
        .map_err(|_| CodexCliVersionProbeError::InvalidBanner)?;
    let version = banner
        .lines()
        .next()
        .and_then(|line| {
            line.split_whitespace()
                .find_map(|token| semver::Version::parse(token).ok())
        })
        .ok_or(CodexCliVersionProbeError::InvalidBanner)?;
    Ok((version, deadline))
}

struct VersionProbeProcessGroup {
    id: Option<u32>,
    #[cfg(unix)]
    exits: tokio::signal::unix::Signal,
}

impl VersionProbeProcessGroup {
    async fn wait_for_exit(&mut self) -> Result<(), CodexCliVersionProbeError> {
        #[cfg(all(
            unix,
            not(any(
                target_os = "cygwin",
                target_os = "horizon",
                target_os = "openbsd",
                target_os = "redox",
                target_os = "wasi",
            ))
        ))]
        {
            let pid = self
                .id
                .and_then(|raw| rustix::process::Pid::from_raw(raw as i32))
                .ok_or(CodexCliVersionProbeError::OutputFailed)?;
            loop {
                match rustix::process::waitid(
                    rustix::process::WaitId::Pid(pid),
                    rustix::process::WaitIdOptions::EXITED
                        | rustix::process::WaitIdOptions::NOWAIT
                        | rustix::process::WaitIdOptions::NOHANG,
                ) {
                    Ok(Some(_)) => return Ok(()),
                    Ok(None) => {
                        self.exits
                            .recv()
                            .await
                            .ok_or(CodexCliVersionProbeError::OutputFailed)?;
                    }
                    Err(rustix::io::Errno::INTR) => {}
                    Err(_) => return Err(CodexCliVersionProbeError::OutputFailed),
                }
            }
        }
        #[cfg(all(
            unix,
            any(
                target_os = "cygwin",
                target_os = "horizon",
                target_os = "openbsd",
                target_os = "redox",
                target_os = "wasi",
            )
        ))]
        {
            Err(CodexCliVersionProbeError::OutputFailed)
        }
        #[cfg(not(unix))]
        {
            Ok(())
        }
    }

    fn kill(&mut self) {
        kill_probe_process_group(self.id.take());
    }
}

impl Drop for VersionProbeProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn collect_version_output(
    child: &mut tokio::process::Child,
    process_group: &mut VersionProbeProcessGroup,
) -> Result<std::process::Output, CodexCliVersionProbeError> {
    use tokio::io::AsyncReadExt;

    let mut stdout = Vec::new();
    let Some(mut pipe) = child.stdout.take() else {
        return Err(CodexCliVersionProbeError::OutputFailed);
    };
    let mut bounded = vec![0_u8; MAX_VERSION_BANNER_BYTES + 1];
    let mut filled = 0_usize;
    while filled < bounded.len() {
        let read = pipe
            .read(&mut bounded[filled..])
            .await
            .map_err(|_| CodexCliVersionProbeError::OutputFailed)?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    if filled > MAX_VERSION_BANNER_BYTES {
        return Err(CodexCliVersionProbeError::InvalidBanner);
    }
    bounded.truncate(filled);
    stdout.extend_from_slice(&bounded);
    // Keep the exited leader unreaped until group cleanup relinquishes its ID.
    process_group.wait_for_exit().await?;
    process_group.kill();
    let status = child
        .wait()
        .await
        .map_err(|_| CodexCliVersionProbeError::OutputFailed)?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

#[cfg(unix)]
fn kill_probe_process_group(group: Option<u32>) {
    if let Some(raw) = group
        && let Some(pid) = rustix::process::Pid::from_raw(raw as i32)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(not(unix))]
fn kill_probe_process_group(_group: Option<u32>) {}

/// Stateless subscription-backed Codex CLI adapter.
#[derive(Clone)]
pub struct CodexCliRuntime {
    executable: PathBuf,
    working_directory: PathBuf,
    credential_reference: signalbox_model_runtime::CredentialReference,
    credential_homes: HashMap<signalbox_model_runtime::CredentialReference, PathBuf>,
    oauth_profiles: std::collections::HashSet<signalbox_model_runtime::CredentialReference>,
    oauth_delivery: Option<(
        std::sync::Arc<dyn crate::OauthCredentialProvider>,
        std::sync::Arc<crate::OauthCredentialRoot>,
    )>,
    exchange_timeout: Option<Duration>,
    interrupt_grace: Duration,
    post_kill_reap_bound: Option<Duration>,
    event_limit: usize,
    stderr_limit: usize,
    model_capabilities: ModelCapabilityCatalog,
    model_context_window_overrides: HashMap<String, u32>,
}

/// Opaque one-shot capability for one Codex CLI spawn.
///
/// It owns the rendered full context and the private operation home.
/// It deliberately implements neither `Clone`, serialization, nor diagnostic
/// formatting.
#[must_use]
pub struct CodexCliPreparedRequest<C> {
    executable: PathBuf,
    working_directory: PathBuf,
    prompt: Vec<u8>,
    operation_home: OperationHome,
    correlation: C,
    resolved_target: String,
    delivery: DeliveryMode,
    translated: crate::translate::TranslatedOperation,
    exchange_timeout: Option<Duration>,
    interrupt_grace: Duration,
    post_kill_reap_bound: Option<Duration>,
    event_limit: usize,
    stderr_limit: usize,
    controls: CodexControls,
    model_context_window_override: Option<u32>,
    oauth_home: Option<crate::oauth::OauthCredentialHome>,
    credential_reference: signalbox_model_runtime::CredentialReference,
}

struct CodexControls {
    reasoning_effort: Option<&'static str>,
    service_tier: Option<&'static str>,
}

/// Why a [`CodexCliRuntime`] could not be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexCliConstructionError {
    /// Process-tree supervision is unavailable on this host platform.
    UnsupportedPlatform,
    /// No executable path was configured.
    EmptyExecutable,
    /// The executable path is relative and would change meaning under the
    /// configured child working directory.
    RelativeExecutable,
    /// The working directory does not exist or is not a directory.
    InvalidWorkingDirectory,
    /// The working directory is relative and would be resolved twice by the
    /// child process and its thread working-directory field.
    RelativeWorkingDirectory,
    /// Whole-process timeout is zero or cannot be represented by the runtime
    /// clock.
    InvalidExchangeTimeout,
    /// Interrupt grace is zero.
    InvalidInterruptGrace,
    /// One of the process-output evidence bounds is zero.
    InvalidOutputLimit,
    /// A configured credential-home path is relative.
    RelativeCredentialHome,
    /// A configured credential home is missing or is not a directory.
    InvalidCredentialHome,
    /// A configured credential home cannot be enumerated.
    UnreadableCredentialHome,
    /// A model context-window override has an invalid target or value.
    InvalidModelContextWindowOverride,
}

impl std::fmt::Display for CodexCliConstructionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                formatter.write_str("Codex CLI runtime requires Unix process-group supervision")
            }
            Self::EmptyExecutable => formatter.write_str("Codex executable path is empty"),
            Self::RelativeExecutable => {
                formatter.write_str("Codex executable path must be absolute")
            }
            Self::InvalidWorkingDirectory => {
                formatter.write_str("Codex working directory is not an existing directory")
            }
            Self::RelativeWorkingDirectory => {
                formatter.write_str("Codex working directory must be absolute")
            }
            Self::InvalidExchangeTimeout => {
                formatter.write_str("exchange timeout must be positive and representable")
            }
            Self::InvalidInterruptGrace => {
                formatter.write_str("interrupt grace must be greater than zero")
            }
            Self::InvalidOutputLimit => {
                formatter.write_str("event and stderr limits must be greater than zero")
            }
            Self::RelativeCredentialHome => {
                formatter.write_str("Codex credential-home paths must be absolute")
            }
            Self::InvalidCredentialHome => {
                formatter.write_str("Codex credential home is not an existing directory")
            }
            Self::UnreadableCredentialHome => {
                formatter.write_str("Codex credential home cannot be enumerated")
            }
            Self::InvalidModelContextWindowOverride => formatter.write_str(
                "Codex model context-window overrides require exact targets and positive values",
            ),
        }
    }
}

impl std::error::Error for CodexCliConstructionError {}

impl std::fmt::Debug for CodexCliRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexCliRuntime")
            .field("executable", &"[redacted]")
            .field("working_directory", &"[redacted]")
            .field("credential_reference", &self.credential_reference)
            .field("exchange_timeout", &self.exchange_timeout)
            .field("interrupt_grace", &self.interrupt_grace)
            .field("event_limit", &self.event_limit)
            .field("stderr_limit", &self.stderr_limit)
            .field("model_capabilities", &self.model_capabilities)
            .finish()
    }
}

impl CodexCliRuntime {
    /// Installs the process-shared OAuth authority and its private scratch root.
    pub fn with_oauth_delivery(
        mut self,
        provider: std::sync::Arc<dyn crate::OauthCredentialProvider>,
        root: std::sync::Arc<crate::OauthCredentialRoot>,
    ) -> Self {
        self.oauth_delivery = Some((provider, root));
        self
    }
    /// Validates adapter configuration without invoking Codex or inspecting
    /// its login store.
    pub fn new(config: CodexCliConfig) -> Result<Self, CodexCliConstructionError> {
        if !CLI_PROCESS_GROUP_SUPERVISION_SUPPORTED {
            return Err(CodexCliConstructionError::UnsupportedPlatform);
        }
        if config.executable.as_os_str().is_empty() {
            return Err(CodexCliConstructionError::EmptyExecutable);
        }
        if !config.executable.is_absolute() {
            return Err(CodexCliConstructionError::RelativeExecutable);
        }
        if !config.working_directory.is_absolute() {
            return Err(CodexCliConstructionError::RelativeWorkingDirectory);
        }
        if !config.working_directory.is_dir() {
            return Err(CodexCliConstructionError::InvalidWorkingDirectory);
        }
        if config.exchange_timeout.is_some_and(|timeout| {
            timeout.is_zero() || tokio::time::Instant::now().checked_add(timeout).is_none()
        }) {
            return Err(CodexCliConstructionError::InvalidExchangeTimeout);
        }
        if config.interrupt_grace.is_zero() {
            return Err(CodexCliConstructionError::InvalidInterruptGrace);
        }
        if config.event_limit == 0 || config.stderr_limit == 0 {
            return Err(CodexCliConstructionError::InvalidOutputLimit);
        }
        if config
            .model_context_window_overrides
            .iter()
            .any(|(target, value)| {
                target.is_empty() || target.trim() != target || target.contains('\0') || *value == 0
            })
        {
            return Err(CodexCliConstructionError::InvalidModelContextWindowOverride);
        }
        for home in config.credential_homes.values() {
            if !home.is_absolute() {
                return Err(CodexCliConstructionError::RelativeCredentialHome);
            }
            if !home.is_dir() {
                return Err(CodexCliConstructionError::InvalidCredentialHome);
            }
            let mut entries = std::fs::read_dir(home)
                .map_err(|_| CodexCliConstructionError::UnreadableCredentialHome)?;
            match entries.next() {
                Some(Ok(_)) | None => {}
                Some(Err(_)) => {
                    return Err(CodexCliConstructionError::UnreadableCredentialHome);
                }
            }
        }
        Ok(Self {
            executable: config.executable,
            working_directory: config.working_directory,
            credential_reference: config.credential_reference,
            credential_homes: config.credential_homes,
            oauth_profiles: config.oauth_profiles,
            oauth_delivery: None,
            exchange_timeout: config.exchange_timeout,
            interrupt_grace: config.interrupt_grace,
            post_kill_reap_bound: config.post_kill_reap_bound,
            event_limit: config.event_limit,
            stderr_limit: config.stderr_limit,
            model_capabilities: config.model_capabilities,
            model_context_window_overrides: config.model_context_window_overrides,
        })
    }

    fn prepare_request<C>(
        &self,
        operation: ModelOperation<C>,
    ) -> PreparationOutcome<C, CodexCliPreparedRequest<C>> {
        let correlation = operation.correlation;
        let mut operation = ModelOperation {
            image_presentation: operation.image_presentation,
            correlation: (),
            credential_reference: operation.credential_reference,
            requested_target: operation.requested_target,
            resolved_target: operation.resolved_target,
            retained_mapped_target: operation.retained_mapped_target,
            system: operation.system,
            messages: operation.messages,
            settings: operation.settings,
            tools: operation.tools,
            tool_choice: operation.tool_choice,
            output_contract: operation.output_contract,
            delivery: operation.delivery,
            provider_compaction: operation.provider_compaction,
            provider_compaction_supported: operation.provider_compaction_supported,
        };
        let capabilities = match self
            .model_capabilities
            .validate_explicit(&operation.resolved_target, &operation.settings)
        {
            Ok(capabilities) => capabilities,
            Err(error) => {
                return PreparationOutcome::Failed {
                    correlation,
                    failure: PreparationFailure::UnsupportedOperation {
                        detail: error.to_string(),
                    },
                };
            }
        };
        let mut request_fast_mode = operation.settings.fast_mode;
        if let Some(capabilities) = capabilities {
            let (target, effective_request_fast_mode) = match capabilities.effective_target(
                &operation.resolved_target,
                operation.settings.fast_mode,
                operation.retained_mapped_target.as_ref(),
            ) {
                Ok(application) => application,
                Err(error) => {
                    return PreparationOutcome::Failed {
                        correlation,
                        failure: PreparationFailure::UnsupportedOperation {
                            detail: error.to_string(),
                        },
                    };
                }
            };
            operation.resolved_target = target.clone();
            request_fast_mode = effective_request_fast_mode;
        }
        let controls = match codex_controls(&operation.settings, request_fast_mode) {
            Ok(controls) => controls,
            Err(failure) => {
                return PreparationOutcome::Failed {
                    correlation,
                    failure,
                };
            }
        };
        let image_limit = match signalbox_model_runtime::image_request_byte_limit(
            &operation,
            &crate::image_presentation_capability(),
        ) {
            Ok(limit) => limit,
            Err(failure) => {
                return PreparationOutcome::Failed {
                    correlation,
                    failure,
                };
            }
        };
        let credential_home = self
            .credential_homes
            .get(&operation.credential_reference)
            .cloned();
        if credential_home.as_ref().is_some_and(|home| {
            std::fs::read_dir(home)
                .ok()
                .and_then(|mut entries| entries.next())
                .is_none()
        }) {
            return PreparationOutcome::Failed {
                correlation,
                failure: PreparationFailure::CredentialUnavailable {
                    error: signalbox_model_runtime::CredentialAccessError::new(
                        operation.credential_reference,
                        signalbox_model_runtime::CredentialAccessFailure::Unavailable,
                    ),
                },
            };
        }
        if operation.credential_reference != self.credential_reference
            && credential_home.is_none()
            && !self
                .oauth_profiles
                .contains(&operation.credential_reference)
        {
            return PreparationOutcome::Failed {
                correlation,
                failure: PreparationFailure::CredentialUnavailable {
                    error: signalbox_model_runtime::CredentialAccessError::new(
                        operation.credential_reference,
                        signalbox_model_runtime::CredentialAccessFailure::Unmapped,
                    ),
                },
            };
        }
        let mut translated = match translate(&operation) {
            Ok(translated) => translated,
            Err(TranslationError::Failure(failure)) => {
                return PreparationOutcome::Failed {
                    correlation,
                    failure,
                };
            }
            Err(TranslationError::Defect(defect)) => {
                return PreparationOutcome::Defect {
                    correlation,
                    defect,
                };
            }
        };
        if let Some(limit) = image_limit {
            let input = crate::app_server::frame::TurnInput {
                input: std::iter::once(crate::app_server::frame::UserInput::Text {
                    text: String::from_utf8_lossy(&translated.prompt).into_owned(),
                })
                .chain(translated.images.iter().cloned())
                .collect(),
                output_schema: serde_json::from_str(OUTPUT_SCHEMA).unwrap_or_default(),
                effort: controls.reasoning_effort.map(str::to_owned),
            };
            let encoded = serde_json::to_vec(
                &serde_json::json!({"id":3,"method":"turn/start","params": {"threadId":"", "input":input.input,"outputSchema":input.output_schema,"effort":input.effort}}),
            );
            // The thread identity arrives in a bounded event; reserve worst-case JSON escaping.
            if encoded
                .ok()
                .and_then(|bytes| bytes.len().checked_add(self.event_limit.checked_mul(6)?))
                .and_then(|bytes| bytes.checked_add(1))
                .is_none_or(|bytes| bytes > limit)
            {
                return PreparationOutcome::Failed {
                    correlation,
                    failure: PreparationFailure::UnsupportedOperation {
                        detail: String::from(
                            "encoded Codex image request exceeds its presentation bound",
                        ),
                    },
                };
            }
        }
        let operation_home = if self
            .oauth_profiles
            .contains(&operation.credential_reference)
        {
            OperationHome::UnresolvableCredentialHome
        } else {
            match operation_home(credential_home) {
                Ok(home) => home,
                Err(_) => {
                    return PreparationOutcome::Defect {
                        correlation,
                        defect: PreparationDefect::RequestConstructionFailed {
                            detail: "could not prepare Codex operation home".into(),
                        },
                    };
                }
            }
        };
        let prompt = std::mem::take(&mut translated.prompt);
        let model_context_window_override = self
            .model_context_window_overrides
            .get(operation.resolved_target.as_str())
            .copied();
        PreparationOutcome::Prepared(CodexCliPreparedRequest {
            executable: self.executable.clone(),
            working_directory: self.working_directory.clone(),
            prompt,
            operation_home,
            correlation,
            resolved_target: operation.resolved_target.as_str().to_string(),
            delivery: operation.delivery,
            translated,
            exchange_timeout: self.exchange_timeout,
            interrupt_grace: self.interrupt_grace,
            post_kill_reap_bound: self.post_kill_reap_bound,
            event_limit: self.event_limit,
            stderr_limit: self.stderr_limit,
            controls,
            model_context_window_override,
            oauth_home: None,
            credential_reference: operation.credential_reference,
        })
    }
}

fn codex_controls(
    settings: &ModelSettings,
    request_fast_mode: FastMode,
) -> Result<CodexControls, PreparationFailure> {
    let reasoning_effort = settings.reasoning_level.map(|level| match level {
        ReasoningLevel::None => "none",
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "xhigh",
        ReasoningLevel::Max => "max",
        ReasoningLevel::Ultra => "ultra",
    });
    let service_tier = match (settings.fast_mode, settings.service_tier) {
        (FastMode::Disabled, None) => None,
        (FastMode::Disabled, Some(ServiceTier::CodexCli(CodexCliServiceTier::Default))) => {
            Some("default")
        }
        (FastMode::Disabled, Some(ServiceTier::CodexCli(CodexCliServiceTier::Flex))) => {
            Some("flex")
        }
        (FastMode::Enabled, None)
        | (FastMode::Enabled, Some(ServiceTier::CodexCli(CodexCliServiceTier::Priority))) => {
            Some("priority")
        }
        (FastMode::Disabled, Some(ServiceTier::CodexCli(CodexCliServiceTier::Priority)))
        | (FastMode::Enabled, Some(ServiceTier::CodexCli(CodexCliServiceTier::Default)))
        | (FastMode::Enabled, Some(ServiceTier::CodexCli(CodexCliServiceTier::Flex))) => {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: "Codex fast mode and service tier select incompatible serving modes"
                    .to_string(),
            });
        }
        (
            FastMode::Disabled | FastMode::Enabled,
            Some(
                ServiceTier::Anthropic(
                    AnthropicServiceTier::Auto | AnthropicServiceTier::StandardOnly,
                )
                | ServiceTier::OpenAi(
                    OpenAiServiceTier::Auto
                    | OpenAiServiceTier::Default
                    | OpenAiServiceTier::Flex
                    | OpenAiServiceTier::Scale
                    | OpenAiServiceTier::Priority
                    | OpenAiServiceTier::Fast,
                ),
            ),
        ) => {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: "Codex cannot enforce another provider's service tier".to_string(),
            });
        }
    };
    let service_tier = match (request_fast_mode, settings.service_tier) {
        (FastMode::Disabled, None) => None,
        _ => service_tier,
    };
    Ok(CodexControls {
        reasoning_effort,
        service_tier,
    })
}

/// Validates the complete settings combination enforced by this adapter.
///
/// Capability-set validation remains the caller's responsibility. This check
/// owns cross-knob constraints that independent capability sets cannot state.
pub fn validate_model_settings(settings: &ModelSettings) -> Result<(), PreparationFailure> {
    codex_controls(settings, settings.fast_mode).map(|_| ())
}

impl<C: Clone + Send + Sync> ModelRuntime<C> for CodexCliRuntime {
    type Prepared = CodexCliPreparedRequest<C>;

    async fn prepare(
        &self,
        operation: ModelOperation<C>,
        mut cancellation: CancellationSignal,
    ) -> PreparationOutcome<C, Self::Prepared> {
        let reference = operation.credential_reference.clone();
        let outcome = self.prepare_request(operation);
        if !self.oauth_profiles.contains(&reference) {
            return outcome;
        }
        let mut prepared = match outcome {
            PreparationOutcome::Prepared(prepared) => prepared,
            outcome => return outcome,
        };
        if cancellation.is_cancelled() {
            return PreparationOutcome::Cancelled {
                correlation: prepared.correlation,
            };
        }
        let result = if let Some((provider, root)) = &self.oauth_delivery {
            let mut installer = crate::oauth::Installer {
                root: root.clone(),
                home: None,
            };
            match provider
                .deliver(&reference, &mut installer, cancellation)
                .await
            {
                Ok(crate::OauthDeliveryOutcome::Cancelled) => {
                    return PreparationOutcome::Cancelled {
                        correlation: prepared.correlation,
                    };
                }
                Ok(crate::OauthDeliveryOutcome::Delivered) => installer
                    .home
                    .ok_or(signalbox_model_runtime::CredentialAccessFailure::OauthCredentialHome),
                Err(error) => Err(error),
            }
        } else {
            Err(signalbox_model_runtime::CredentialAccessFailure::OauthCredentialHome)
        };
        match result {
            Ok(home) => {
                prepared.oauth_home = Some(home);
                PreparationOutcome::Prepared(prepared)
            }
            Err(failure) => PreparationOutcome::Failed {
                correlation: prepared.correlation,
                failure: PreparationFailure::CredentialUnavailable {
                    error: signalbox_model_runtime::CredentialAccessError::new(reference, failure),
                },
            },
        }
    }

    async fn execute(
        &self,
        prepared: Self::Prepared,
        sink: &mut (dyn ObservationSink<C> + Send),
        mut cancellation: CancellationSignal,
    ) -> TerminalReport<C> {
        let correlation = prepared.correlation.clone();
        if cancellation.is_cancelled() {
            return TerminalReport {
                correlation,
                evidence: TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                    cause: UnsentCause::CancelledBeforeSend,
                }),
            };
        }
        let oauth_token = prepared
            .oauth_home
            .as_ref()
            .map(|home| home.material.access_token.clone());
        let reference = prepared.credential_reference.clone();
        let mut evidence = execute_process(prepared, sink, &mut cancellation).await;
        if let Some(token) = oauth_token
            && let Some((provider, _)) = &self.oauth_delivery
        {
            match &mut evidence {
                TerminalEvidence::ProviderError(error)
                    if error.kind
                        == signalbox_model_runtime::ProviderErrorKind::CredentialRejected =>
                {
                    error.credential_recovery = Some(
                        provider
                            .recover_rejection(&reference, &token, cancellation)
                            .await,
                    );
                }
                TerminalEvidence::Completed(_)
                | TerminalEvidence::CompletedWithProviderCompaction { .. }
                | TerminalEvidence::Refused(_) => {
                    provider.invocation_succeeded(&reference, &token).await;
                }
                _ => {}
            }
        }
        TerminalReport {
            correlation,
            evidence,
        }
    }
}

enum OperationHome {
    Ready(TempDir),
    UnresolvableCredentialHome,
}

async fn execute_process<C: Clone + Send + Sync>(
    mut prepared: CodexCliPreparedRequest<C>,
    sink: &mut (dyn ObservationSink<C> + Send),
    cancellation: &mut CancellationSignal,
) -> TerminalEvidence {
    let oauth = prepared.oauth_home.as_ref();
    let operation_home = match (oauth, &prepared.operation_home) {
        (Some(home), _) => home.path.as_path(),
        (None, OperationHome::Ready(home)) => home.path(),
        (None, OperationHome::UnresolvableCredentialHome) => {
            return TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                cause: UnsentCause::ConnectFailed(signalbox_model_runtime::TransportFacts::new(
                    "Codex credential home cannot be resolved to an absolute directory; exchange refused before spawn",
                )),
            });
        }
    };
    let mut command = if oauth.is_some() {
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("umask 077; exec \"$@\"")
            .arg("signalbox-oauth")
            .arg(&prepared.executable);
        command
            .arg("--config")
            .arg("cli_auth_credentials_store=\"file\"");
        command
    } else {
        std::process::Command::new(&prepared.executable)
    };
    for feature in DISABLED_CODEX_CLI_CAPABILITY_FEATURES {
        command.arg("--disable").arg(feature);
    }
    if prepared.controls.service_tier.is_some() {
        command.arg("--enable").arg("fast_mode");
    }
    if let Some(context_window) = prepared.model_context_window_override {
        command
            .arg("--config")
            .arg(format!("model_context_window={context_window}"));
    }
    command
        .arg("--config")
        .arg("agents.enabled=false")
        .arg("--config")
        .arg("skills.include_instructions=false")
        .arg("--config")
        .arg("mcp_servers={}")
        .arg("--config")
        .arg("web_search=\"disabled\"")
        .arg("--config")
        .arg("project_doc_max_bytes=0")
        .arg("app-server")
        .arg("--stdio")
        .arg("--strict-config")
        .arg("--ignore-user-config")
        .arg("--ignore-rules")
        .current_dir(&prepared.working_directory);
    use crate::app_server::{
        client::Client,
        frame::{ThreadOptions, TurnInput, UserInput},
    };
    let client = Client::new(
        ThreadOptions {
            model: prepared.resolved_target,
            cwd: prepared.working_directory.to_string_lossy().into_owned(),
            service_tier: prepared.controls.service_tier.map(str::to_owned),
        },
        TurnInput {
            input: std::iter::once(UserInput::Text {
                text: String::from_utf8(prepared.prompt).unwrap_or_default(),
            })
            .chain(prepared.translated.images.drain(..))
            .collect(),
            output_schema: serde_json::from_str(OUTPUT_SCHEMA).unwrap_or_default(),
            effort: prepared.controls.reasoning_effort.map(str::to_owned),
        },
    );
    let decoder = EventDecoder::new(
        prepared.correlation.clone(),
        prepared.delivery,
        &prepared.translated,
        client,
        prepared.event_limit,
    );
    let mut environment_overrides = vec![CliEnvironmentOverride::replacing_inherited(
        CODEX_CREDENTIAL_HOME,
        operation_home.as_os_str().to_owned(),
    )];
    if let Some(home) = oauth {
        environment_overrides.push(CliEnvironmentOverride::replacing_inherited(
            "HOME",
            home.path.clone().into_os_string(),
        ));
    }
    let request = CliProcessRequest {
        command,
        prompt: Vec::new(),
        decoder,
        exchange_timeout: prepared.exchange_timeout,
        interrupt_grace: prepared.interrupt_grace,
        post_kill_reap_bound: prepared.post_kill_reap_bound,
        event_limit: prepared.event_limit,
        stderr_limit: prepared.stderr_limit,
        environment: CODEX_ENVIRONMENT,
        environment_overrides,
    };
    if let Some(home) = oauth {
        let mut access_sink = signalbox_model_runtime::CredentialRedactingSink::new(
            sink,
            &home.material.access_token,
        );
        let mut identity_sink = signalbox_model_runtime::CredentialRedactingSink::new(
            &mut access_sink,
            &home.material.identity_token,
        );
        let evidence = signalbox_model_runtime::execute_cli_process_with_credentials(
            request,
            &mut identity_sink,
            cancellation,
            &[
                home.material.access_token.clone(),
                home.material.identity_token.clone(),
            ],
        )
        .await;
        identity_sink.flush();
        access_sink.flush();
        let evidence =
            signalbox_model_runtime::redact_evidence(evidence, &home.material.identity_token, None);
        signalbox_model_runtime::redact_evidence(evidence, &home.material.access_token, None)
    } else {
        execute_cli_process(request, sink, cancellation).await
    }
}

fn operation_home(selected: Option<PathBuf>) -> std::io::Result<OperationHome> {
    let directory = std::path::absolute(std::env::temp_dir())?;
    let home = tempfile::Builder::new()
        .prefix("signalbox-codex-")
        .tempdir_in(directory)?;
    std::fs::write(home.path().join("config.toml"), "")?;
    let source = selected
        .or_else(|| std::env::var_os(CODEX_CREDENTIAL_HOME).map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")));
    #[cfg(unix)]
    if let Some(source) = source {
        let source = match std::path::absolute(source) {
            Ok(source) if source.is_absolute() => source,
            _ => return Ok(OperationHome::UnresolvableCredentialHome),
        };
        // Only the CLI opens the login store; auxiliary state stays private.
        std::os::unix::fs::symlink(source.join("auth.json"), home.path().join("auth.json"))?;
    }
    Ok(OperationHome::Ready(home))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{
        CODEX_CREDENTIAL_HOME, CODEX_ENVIRONMENT, CliEnvironmentVariable, CodexCliServiceTier,
        CodexCliVersionProbeError, FORBIDDEN_DIRECT_CREDENTIAL_ENVIRONMENT, FastMode,
        ModelSettings, ReasoningLevel, SUPPORTED_CODEX_CLI_VERSION, ServiceTier, codex_controls,
        probe_pinned_codex_cli_version, validate_model_settings, verify_pinned_codex_cli_version,
    };

    #[cfg(unix)]
    fn version_fixture(script: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("temporary version fixture directory");
        let executable = directory.path().join("codex");
        std::fs::write(directory.path().join("version.sh"), script)
            .expect("version fixture is written");
        // Execute an immutable launcher so concurrent forks cannot inherit a
        // writable descriptor for its executable and cause ETXTBSY.
        let launcher = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/support/version_probe.sh")
            .canonicalize()
            .expect("version fixture launcher exists");
        std::os::unix::fs::symlink(launcher, &executable)
            .expect("version fixture launcher is linked");
        (directory, executable)
    }

    #[cfg(unix)]
    fn oversized_version_fixture() -> (tempfile::TempDir, PathBuf) {
        let banner = "x".repeat(4097);
        version_fixture(&format!("#!/bin/sh\nprintf '%s' '{banner}'\n"))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pinned_version_probe_rejects_an_unpinned_executable_reporting_the_supported_version() {
        let script =
            format!("#!/bin/sh\nprintf 'codex-cli %s\\n' '{SUPPORTED_CODEX_CLI_VERSION}'\n");
        let (_directory, executable) = version_fixture(&script);

        let result = verify_pinned_codex_cli_version(&executable, Duration::from_secs(1)).await;

        assert_eq!(result, Err(CodexCliVersionProbeError::VersionMismatch));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_probe_reports_installed_facts_for_a_pin_mismatch() {
        let script =
            format!("#!/bin/sh\nprintf 'codex-cli %s\\n' '{SUPPORTED_CODEX_CLI_VERSION}'\n");
        let (_directory, executable) = version_fixture(&script);

        let probe = probe_pinned_codex_cli_version(&executable, Duration::from_secs(1))
            .await
            .expect("an observable mismatch retains installed facts");

        assert_eq!(probe.version().to_string(), SUPPORTED_CODEX_CLI_VERSION);
        assert_eq!(probe.digest().len(), 64);
        assert!(!probe.matches_pin());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pinned_version_probe_rejects_executable_drift() {
        let (_directory, executable) =
            version_fixture("#!/bin/sh\nprintf 'codex-cli %s\\n' '0.0.1'\n");

        let result = verify_pinned_codex_cli_version(&executable, Duration::from_secs(1)).await;

        assert_eq!(result, Err(CodexCliVersionProbeError::VersionMismatch));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pinned_version_probe_rejects_a_non_semver_banner() {
        let (_directory, executable) = version_fixture("#!/bin/sh\nprintf 'codex-cli latest\\n'\n");

        let result = verify_pinned_codex_cli_version(&executable, Duration::from_secs(1)).await;

        assert_eq!(result, Err(CodexCliVersionProbeError::InvalidBanner));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pinned_version_probe_bounds_a_hung_executable() {
        let (_directory, executable) = version_fixture("#!/bin/sh\nsleep 30\n");

        let result = verify_pinned_codex_cli_version(&executable, Duration::from_millis(10)).await;

        assert_eq!(result, Err(CodexCliVersionProbeError::TimedOut));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pinned_version_probe_bounds_the_version_banner() {
        let (_directory, executable) = oversized_version_fixture();

        let result = verify_pinned_codex_cli_version(&executable, Duration::from_secs(1)).await;

        assert_eq!(result, Err(CodexCliVersionProbeError::InvalidBanner));
    }

    /// the CLI receives only a reference to its ambient login store;
    /// direct credential-value variables are absent from the inherited set.
    #[test]
    fn cli_environment_excludes_direct_credential_values() {
        assert!(
            CODEX_ENVIRONMENT
                .iter()
                .any(|variable| variable.name() == CODEX_CREDENTIAL_HOME)
        );
        assert!(
            CODEX_ENVIRONMENT.contains(&CliEnvironmentVariable::credential_home(
                CODEX_CREDENTIAL_HOME
            ))
        );
        assert!(
            !CODEX_ENVIRONMENT
                .iter()
                .any(|variable| variable.name() == FORBIDDEN_DIRECT_CREDENTIAL_ENVIRONMENT)
        );
    }

    #[test]
    fn codex_controls_map_ultra_reasoning() {
        let mut settings = ModelSettings::new(64);
        settings.reasoning_level = Some(ReasoningLevel::Ultra);

        let controls =
            codex_controls(&settings, settings.fast_mode).expect("supported reasoning maps");

        assert_eq!(controls.reasoning_effort, Some("ultra"));
    }

    #[test]
    fn codex_controls_map_the_fast_tier() {
        let mut settings = ModelSettings::new(64);
        settings.fast_mode = FastMode::Enabled;
        settings.service_tier = Some(ServiceTier::CodexCli(CodexCliServiceTier::Priority));

        let controls =
            codex_controls(&settings, settings.fast_mode).expect("supported controls map");

        assert_eq!(controls.service_tier, Some("priority"));
    }

    #[test]
    fn codex_controls_reject_priority_without_fast_mode() {
        let mut settings = ModelSettings::new(64);
        settings.service_tier = Some(ServiceTier::CodexCli(CodexCliServiceTier::Priority));

        assert!(validate_model_settings(&settings).is_err());
    }

    #[test]
    fn mapped_fast_mode_still_rejects_codex_flex_tier() {
        let mut settings = ModelSettings::new(64);
        settings.fast_mode = FastMode::Enabled;
        settings.service_tier = Some(ServiceTier::CodexCli(CodexCliServiceTier::Flex));

        assert!(codex_controls(&settings, FastMode::Disabled).is_err());
    }
}
