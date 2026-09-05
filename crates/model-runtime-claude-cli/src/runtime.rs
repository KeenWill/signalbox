//! One operation, one Claude Code process spawn.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use signalbox_model_runtime::{
    AnthropicServiceTier, CLI_PROCESS_GROUP_SUPERVISION_SUPPORTED, CancellationSignal,
    CliEnvironmentOverride, CliEnvironmentVariable, CliProcessRequest, CodexCliServiceTier,
    CredentialAccess, CredentialAccessError, CredentialRedactingSink, CredentialReference,
    CredentialValue, DeliveryMode, FastMode, ModelCapabilityCatalog, ModelOperation, ModelRuntime,
    ModelSettings, ObservationSink, OpenAiServiceTier, PreparationDefect, PreparationFailure,
    PreparationOutcome, ProvenUnsentEvidence, ReasoningLevel, ServiceTier, TerminalEvidence,
    TerminalReport, UnsentCause, execute_cli_process, redact_evidence,
};
use tempfile::TempDir;

use crate::bridge::SERVER_NAME;
use crate::config::ClaudeCliConfig;
use crate::event::EventDecoder;
use crate::translate::{TranslationError, qualified_tool_name, translate};

const CLAUDE_CONFIG_HOME_VARIABLE: &str = "CLAUDE_CONFIG_DIR";
/// The sole environment key Claude file-delivery profiles may materialize in
/// the CLI-owned request settings store.
pub const CLAUDE_CLI_FILE_CREDENTIAL_ENV_KEY: &str = "ANTHROPIC_API_KEY";
const CLAUDE_ENVIRONMENT: &[CliEnvironmentVariable] = &[
    CliEnvironmentVariable::inherited("ALL_PROXY"),
    CliEnvironmentVariable::credential_home(CLAUDE_CONFIG_HOME_VARIABLE),
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

/// Every built-in tool the pinned Claude Code CLI reports or documents.
///
/// `--tools ""` already selects an empty built-in surface; this inventory is
/// the independent second control passed through `--disallowedTools`, so it
/// names built-ins this invocation could not otherwise reach.
pub const DISABLED_CLAUDE_CLI_BUILTIN_TOOLS: &[&str] = &[
    "Task",
    "Bash",
    "CronCreate",
    "CronDelete",
    "CronList",
    "DesignSync",
    "Edit",
    "EnterWorktree",
    "ExitWorktree",
    // Reported only under `--restricted`.
    "Glob",
    // Reported only under `--restricted`.
    "Grep",
    // Enumerates other Claude Code sessions on this host.
    "ListAgents",
    "Monitor",
    "NotebookEdit",
    // Reported only under `CLAUDE_CODE_USE_POWERSHELL_TOOL`.
    "PowerShell",
    "PushNotification",
    // Named by `--restricted` among the code-running tools; never reported.
    "REPL",
    "Read",
    "RemoteTrigger",
    "ReportFindings",
    "ScheduleWakeup",
    "SendFeedback",
    // Delivers to other Claude Code sessions on this host.
    "SendMessage",
    // Agent-to-user channel enabled by `--brief`; never reported.
    "SendUserMessage",
    // Reported only without `--disable-slash-commands`.
    "Skill",
    "TaskCreate",
    "TaskGet",
    "TaskList",
    "TaskOutput",
    "TaskStop",
    "TaskUpdate",
    "ToolSearch",
    "WebFetch",
    "WebSearch",
    "Workflow",
    "Write",
];
/// Claude Code protocol snapshot covered by this adapter's offline fixtures.
///
/// Derived by `build.rs` from the exact pin in this crate's `package.json`, so
/// the manifest is the single source of truth and a Renovate bump carries the
/// adapter's supported-version claim with it instead of leaving a second
/// literal to update by hand.
pub const SUPPORTED_CLAUDE_CLI_VERSION: &str = env!("SIGNALBOX_CLAUDE_CLI_VERSION");

trait ClaudeCredentialAccess: Send + Sync {
    fn resolve<'a>(
        &'a self,
        reference: &'a CredentialReference,
    ) -> Pin<Box<dyn Future<Output = Result<CredentialValue, CredentialAccessError>> + Send + 'a>>;
}

impl<Credentials> ClaudeCredentialAccess for Credentials
where
    Credentials: CredentialAccess,
{
    fn resolve<'a>(
        &'a self,
        reference: &'a CredentialReference,
    ) -> Pin<Box<dyn Future<Output = Result<CredentialValue, CredentialAccessError>> + Send + 'a>>
    {
        Box::pin(CredentialAccess::resolve(self, reference))
    }
}

enum ClaudeCredentialDelivery {
    Ambient,
    File {
        credentials: Arc<dyn ClaudeCredentialAccess>,
    },
    Catalog {
        ambient_reference: Option<CredentialReference>,
        file_credentials: Arc<dyn ClaudeCredentialAccess>,
    },
}

impl std::fmt::Debug for ClaudeCredentialDelivery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Ambient => "Ambient",
            Self::File { .. } => "File([credential access])",
            Self::Catalog { .. } => "Catalog([credential access])",
        })
    }
}

/// Stateless Claude Code CLI adapter with ambient or file-delivered authentication.
pub struct ClaudeCliRuntime {
    executable: PathBuf,
    mcp_bridge_executable: PathBuf,
    working_directory: PathBuf,
    credential_reference: CredentialReference,
    credential_delivery: ClaudeCredentialDelivery,
    exchange_timeout: Option<Duration>,
    interrupt_grace: Duration,
    post_kill_reap_bound: Option<Duration>,
    event_limit: usize,
    stderr_limit: usize,
    native_message_limit: Option<usize>,
    model_capabilities: ModelCapabilityCatalog,
}

/// Opaque one-shot capability for one Claude Code process spawn.
#[must_use]
pub struct ClaudeCliPreparedRequest<C> {
    executable: PathBuf,
    working_directory: PathBuf,
    prompt: Vec<u8>,
    support_directory: TempDir,
    mcp_config: PathBuf,
    settings: PathBuf,
    correlation: C,
    resolved_target: String,
    delivery: DeliveryMode,
    translated: crate::translate::TranslatedOperation,
    exchange_timeout: Option<Duration>,
    interrupt_grace: Duration,
    post_kill_reap_bound: Option<Duration>,
    event_limit: usize,
    stderr_limit: usize,
    native_message_limit: Option<usize>,
    reasoning_effort: Option<&'static str>,
    max_output_tokens: u32,
    credential: Option<CredentialValue>,
    credential_home: Option<PathBuf>,
}

/// Why a [`ClaudeCliRuntime`] could not be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeCliConstructionError {
    /// Process-tree supervision is unavailable on this host platform.
    UnsupportedPlatform,
    /// No Claude Code executable path was configured.
    EmptyExecutable,
    /// The Claude Code executable path is relative.
    RelativeExecutable,
    /// No MCP bridge executable path was configured.
    EmptyMcpBridgeExecutable,
    /// The MCP bridge executable path is relative.
    RelativeMcpBridgeExecutable,
    /// The working directory does not exist or is not a directory.
    InvalidWorkingDirectory,
    /// The working directory is relative.
    RelativeWorkingDirectory,
    /// Whole-process timeout is zero or cannot be represented.
    InvalidExchangeTimeout,
    /// Interrupt grace is zero.
    InvalidInterruptGrace,
    /// One of the process-output evidence bounds is zero.
    InvalidOutputLimit,
    /// File delivery names an environment key other than the adapter contract.
    InvalidCredentialEnvironmentKey,
}

impl std::fmt::Display for ClaudeCliConstructionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedPlatform => {
                "Claude CLI runtime requires Unix process-group supervision"
            }
            Self::EmptyExecutable => "Claude executable path is empty",
            Self::RelativeExecutable => "Claude executable path must be absolute",
            Self::EmptyMcpBridgeExecutable => "Claude MCP bridge executable path is empty",
            Self::RelativeMcpBridgeExecutable => {
                "Claude MCP bridge executable path must be absolute"
            }
            Self::InvalidWorkingDirectory => {
                "Claude working directory is not an existing directory"
            }
            Self::RelativeWorkingDirectory => "Claude working directory must be absolute",
            Self::InvalidExchangeTimeout => "exchange timeout must be positive and representable",
            Self::InvalidInterruptGrace => "interrupt grace must be greater than zero",
            Self::InvalidOutputLimit => "event and stderr limits must be greater than zero",
            Self::InvalidCredentialEnvironmentKey => {
                "Claude file delivery requires the ANTHROPIC_API_KEY environment key"
            }
        })
    }
}

impl std::error::Error for ClaudeCliConstructionError {}

impl std::fmt::Debug for ClaudeCliRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClaudeCliRuntime")
            .field("executable", &self.executable)
            .field("mcp_bridge_executable", &self.mcp_bridge_executable)
            .field("working_directory", &self.working_directory)
            .field("credential_reference", &self.credential_reference)
            .field("credential_delivery", &self.credential_delivery)
            .field("exchange_timeout", &self.exchange_timeout)
            .field("interrupt_grace", &self.interrupt_grace)
            .field("event_limit", &self.event_limit)
            .field("stderr_limit", &self.stderr_limit)
            .field("model_capabilities", &self.model_capabilities)
            .finish()
    }
}

impl ClaudeCliRuntime {
    /// Validates adapter configuration without invoking Claude or inspecting its login store.
    pub fn new(config: ClaudeCliConfig) -> Result<Self, ClaudeCliConstructionError> {
        Self::new_with_delivery(config, ClaudeCredentialDelivery::Ambient)
    }

    /// Validates file delivery without reading the credential until request preparation.
    pub fn new_with_file_delivery<Credentials>(
        config: ClaudeCliConfig,
        credentials: Credentials,
        env_key: &str,
    ) -> Result<Self, ClaudeCliConstructionError>
    where
        Credentials: CredentialAccess + 'static,
    {
        if env_key != CLAUDE_CLI_FILE_CREDENTIAL_ENV_KEY {
            return Err(ClaudeCliConstructionError::InvalidCredentialEnvironmentKey);
        }
        Self::new_with_delivery(
            config,
            ClaudeCredentialDelivery::File {
                credentials: Arc::new(credentials),
            },
        )
    }

    /// Validates the complete operation-scoped Claude delivery catalog.
    ///
    /// The catalog remains complete across preferred-profile changes so a
    /// historical operation pin resolves its own ambient or file delivery.
    pub fn new_with_credential_catalog<Credentials>(
        config: ClaudeCliConfig,
        file_credentials: Credentials,
        ambient_reference: Option<CredentialReference>,
        file_env_key: &str,
    ) -> Result<Self, ClaudeCliConstructionError>
    where
        Credentials: CredentialAccess + 'static,
    {
        if file_env_key != CLAUDE_CLI_FILE_CREDENTIAL_ENV_KEY {
            return Err(ClaudeCliConstructionError::InvalidCredentialEnvironmentKey);
        }
        Self::new_with_delivery(
            config,
            ClaudeCredentialDelivery::Catalog {
                ambient_reference,
                file_credentials: Arc::new(file_credentials),
            },
        )
    }

    fn new_with_delivery(
        config: ClaudeCliConfig,
        credential_delivery: ClaudeCredentialDelivery,
    ) -> Result<Self, ClaudeCliConstructionError> {
        if !CLI_PROCESS_GROUP_SUPERVISION_SUPPORTED {
            return Err(ClaudeCliConstructionError::UnsupportedPlatform);
        }
        if config.executable.as_os_str().is_empty() {
            return Err(ClaudeCliConstructionError::EmptyExecutable);
        }
        if !config.executable.is_absolute() {
            return Err(ClaudeCliConstructionError::RelativeExecutable);
        }
        if config.mcp_bridge_executable.as_os_str().is_empty() {
            return Err(ClaudeCliConstructionError::EmptyMcpBridgeExecutable);
        }
        if !config.mcp_bridge_executable.is_absolute() {
            return Err(ClaudeCliConstructionError::RelativeMcpBridgeExecutable);
        }
        if !config.working_directory.is_absolute() {
            return Err(ClaudeCliConstructionError::RelativeWorkingDirectory);
        }
        if !config.working_directory.is_dir() {
            return Err(ClaudeCliConstructionError::InvalidWorkingDirectory);
        }
        if config.exchange_timeout.is_some_and(|timeout| {
            timeout.is_zero() || tokio::time::Instant::now().checked_add(timeout).is_none()
        }) {
            return Err(ClaudeCliConstructionError::InvalidExchangeTimeout);
        }
        if config.interrupt_grace.is_zero() {
            return Err(ClaudeCliConstructionError::InvalidInterruptGrace);
        }
        if config.event_limit == 0 || config.stderr_limit == 0 {
            return Err(ClaudeCliConstructionError::InvalidOutputLimit);
        }
        Ok(Self {
            executable: config.executable,
            mcp_bridge_executable: config.mcp_bridge_executable,
            working_directory: config.working_directory,
            credential_reference: config.credential_reference,
            credential_delivery,
            exchange_timeout: config.exchange_timeout,
            interrupt_grace: config.interrupt_grace,
            post_kill_reap_bound: config.post_kill_reap_bound,
            event_limit: config.event_limit,
            stderr_limit: config.stderr_limit,
            native_message_limit: config.native_message_limit,
            model_capabilities: config.model_capabilities,
        })
    }

    async fn prepare_request<C>(
        &self,
        operation: ModelOperation<C>,
        cancellation: &mut CancellationSignal,
    ) -> PreparationOutcome<C, ClaudeCliPreparedRequest<C>> {
        let correlation = operation.correlation;
        let mut operation = ModelOperation {
            correlation: (),
            credential_reference: operation.credential_reference,
            requested_target: operation.requested_target,
            resolved_target: operation.resolved_target,
            system: operation.system,
            messages: operation.messages,
            settings: operation.settings,
            tools: operation.tools,
            tool_choice: operation.tool_choice,
            output_contract: operation.output_contract,
            delivery: operation.delivery,
            provider_compaction: operation.provider_compaction,
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
            let (target, effective_request_fast_mode) = match capabilities
                .effective_target(&operation.resolved_target, operation.settings.fast_mode)
            {
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
        let reasoning_effort = match claude_reasoning_effort(&operation.settings) {
            Ok(reasoning_effort) => reasoning_effort,
            Err(failure) => {
                return PreparationOutcome::Failed {
                    correlation,
                    failure,
                };
            }
        };
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
        let file_credentials = match &self.credential_delivery {
            ClaudeCredentialDelivery::Ambient => {
                if operation.credential_reference != self.credential_reference {
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
                None
            }
            ClaudeCredentialDelivery::File { credentials } => Some(credentials),
            ClaudeCredentialDelivery::Catalog {
                ambient_reference,
                file_credentials,
            } => {
                if ambient_reference.as_ref() == Some(&operation.credential_reference) {
                    None
                } else {
                    Some(file_credentials)
                }
            }
        };
        let credential = match file_credentials {
            None => None,
            Some(credentials) => {
                let resolve = credentials.resolve(&operation.credential_reference);
                match cancellation.run_until_cancelled(resolve).await {
                    None => return PreparationOutcome::Cancelled { correlation },
                    Some(Err(error)) => {
                        return PreparationOutcome::Failed {
                            correlation,
                            failure: PreparationFailure::CredentialUnavailable { error },
                        };
                    }
                    Some(Ok(value)) => {
                        if value.expose_bytes().is_empty()
                            || std::str::from_utf8(value.expose_bytes()).is_err()
                            || value.expose_bytes().contains(&0)
                        {
                            return PreparationOutcome::Failed {
                                correlation,
                                failure: PreparationFailure::CredentialUnusable {
                                    detail: "Claude file credential must be nonempty, UTF-8, and NUL-free"
                                        .to_string(),
                                },
                            };
                        }
                        Some(value)
                    }
                }
            }
        };
        let support = match create_support_files(
            &self.mcp_bridge_executable,
            &translated,
            request_fast_mode,
            credential.as_ref(),
        ) {
            Ok(support) => support,
            Err(detail) => {
                return PreparationOutcome::Defect {
                    correlation,
                    defect: PreparationDefect::RequestConstructionFailed { detail },
                };
            }
        };
        let credential_home = credential
            .as_ref()
            .map(|_| support.directory.path().to_path_buf());
        let prompt = std::mem::take(&mut translated.prompt);
        PreparationOutcome::Prepared(ClaudeCliPreparedRequest {
            executable: self.executable.clone(),
            working_directory: self.working_directory.clone(),
            prompt,
            support_directory: support.directory,
            mcp_config: support.mcp_config,
            settings: support.settings,
            correlation,
            resolved_target: operation.resolved_target.as_str().to_string(),
            delivery: operation.delivery,
            translated,
            exchange_timeout: self.exchange_timeout,
            interrupt_grace: self.interrupt_grace,
            post_kill_reap_bound: self.post_kill_reap_bound,
            event_limit: self.event_limit,
            stderr_limit: self.stderr_limit,
            native_message_limit: self.native_message_limit,
            reasoning_effort,
            max_output_tokens: operation.settings.max_output_tokens,
            credential,
            credential_home,
        })
    }
}

fn claude_reasoning_effort(
    settings: &ModelSettings,
) -> Result<Option<&'static str>, PreparationFailure> {
    match settings.service_tier {
        None => {}
        Some(ServiceTier::Anthropic(
            AnthropicServiceTier::Auto | AnthropicServiceTier::StandardOnly,
        ))
        | Some(ServiceTier::OpenAi(
            OpenAiServiceTier::Auto
            | OpenAiServiceTier::Default
            | OpenAiServiceTier::Flex
            | OpenAiServiceTier::Scale
            | OpenAiServiceTier::Priority
            | OpenAiServiceTier::Fast,
        ))
        | Some(ServiceTier::CodexCli(
            CodexCliServiceTier::Default
            | CodexCliServiceTier::Priority
            | CodexCliServiceTier::Flex,
        )) => {
            return Err(PreparationFailure::UnsupportedOperation {
                detail: "Claude CLI cannot enforce an explicit service tier".to_string(),
            });
        }
    }
    settings
        .reasoning_level
        .map(|level| match level {
            ReasoningLevel::Low => Ok("low"),
            ReasoningLevel::Medium => Ok("medium"),
            ReasoningLevel::High => Ok("high"),
            ReasoningLevel::XHigh => Ok("xhigh"),
            ReasoningLevel::Max => Ok("max"),
            ReasoningLevel::None | ReasoningLevel::Minimal | ReasoningLevel::Ultra => {
                Err(PreparationFailure::UnsupportedOperation {
                    detail: "Claude CLI cannot enforce the requested reasoning level".to_string(),
                })
            }
        })
        .transpose()
}

/// Validates the complete settings combination enforced by this adapter.
///
/// Capability-set validation remains the caller's responsibility. This check
/// owns cross-knob constraints that independent capability sets cannot state.
pub fn validate_model_settings(settings: &ModelSettings) -> Result<(), PreparationFailure> {
    claude_reasoning_effort(settings).map(|_| ())
}

struct SupportFiles {
    directory: TempDir,
    mcp_config: PathBuf,
    settings: PathBuf,
}

fn create_support_files(
    bridge: &Path,
    translated: &crate::translate::TranslatedOperation,
    fast_mode: FastMode,
    credential: Option<&CredentialValue>,
) -> Result<SupportFiles, String> {
    let temporary_directory = std::path::absolute(std::env::temp_dir())
        .map_err(|error| format!("could not absolutize temporary directory: {error}"))?;
    let directory = tempfile::Builder::new()
        .prefix("signalbox-claude-")
        .tempdir_in(temporary_directory)
        .map_err(|error| format!("could not create private support directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not set private support directory mode: {error}"))?;
    }
    let catalog = directory.path().join("tools.json");
    let ready = directory.path().join("mcp-ready");
    let mcp_config = directory.path().join("mcp.json");
    let settings = directory.path().join("settings.json");
    let credential_file = directory.path().join("credential");
    let credential_helper = directory.path().join("credential-helper");
    let bridge_text = bridge
        .to_str()
        .ok_or_else(|| "MCP bridge path is not valid UTF-8".to_string())?;
    let catalog_text = catalog
        .to_str()
        .ok_or_else(|| "MCP catalog path is not valid UTF-8".to_string())?;
    let ready_text = ready
        .to_str()
        .ok_or_else(|| "MCP readiness path is not valid UTF-8".to_string())?;
    let catalog_bytes = serde_json::to_vec(&translated.catalog)
        .map_err(|error| format!("could not serialize MCP catalog: {error}"))?;
    std::fs::write(&catalog, catalog_bytes)
        .map_err(|error| format!("could not write MCP catalog: {error}"))?;
    let mcp = serde_json::json!({
        "mcpServers": { SERVER_NAME: {
            "type": "stdio",
            "command": bridge_text,
            "args": ["--serve", catalog_text, ready_text],
        }}
    });
    std::fs::write(
        &mcp_config,
        serde_json::to_vec(&mcp).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("could not write MCP config: {error}"))?;
    let hook_command = format!(
        "{} --wait-ready {}",
        shell_quote(bridge_text),
        shell_quote(ready_text)
    );
    let mut isolated_settings = serde_json::json!({
        "fastMode": fast_mode == FastMode::Enabled,
        "hooks": {"SessionStart": [{"hooks": [{
            "type": "command", "command": hook_command, "timeout": 10
        }]}]}
    });
    if let Some(credential) = credential {
        write_private_file(&credential_file, credential.expose_bytes())?;
        let credential_text = credential_file
            .to_str()
            .ok_or_else(|| "Claude credential-store path is not valid UTF-8".to_string())?;
        let credential_helper_text = credential_helper
            .to_str()
            .ok_or_else(|| "Claude credential-helper path is not valid UTF-8".to_string())?;
        let helper = format!(
            "#!/bin/sh\nseparator=\nwhile\n    IFS= read -r line\n    read_status=$?\n    case \"$read_status\" in\n        0) : ;;\n        *)\n            case \"$line\" in\n                '') break ;;\n                *) : ;;\n            esac\n            ;;\n    esac\ndo\n    printf '%s%s' \"$separator\" \"$line\"\n    separator='\n'\ndone < {}\n",
            shell_quote(credential_text)
        );
        write_private_file(&credential_helper, helper.as_bytes())?;
        isolated_settings["apiKeyHelper"] = serde_json::json!(format!(
            "exec /bin/sh {}",
            shell_quote(credential_helper_text)
        ));
    }
    write_private_file(
        &settings,
        &serde_json::to_vec(&isolated_settings).map_err(|error| error.to_string())?,
    )?;
    Ok(SupportFiles {
        directory,
        mcp_config,
        settings,
    })
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), String> {
    write_private_support_file(path, contents, 0o600)
}

fn write_private_support_file(path: &Path, contents: &[u8], mode: u32) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options
        .open(path)
        .map_err(|error| format!("could not create private support file: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|error| format!("could not set private support file mode: {error}"))?;
    }
    std::io::Write::write_all(&mut file, contents)
        .map_err(|error| format!("could not write private support file: {error}"))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

impl<C: Clone + Send + Sync> ModelRuntime<C> for ClaudeCliRuntime {
    type Prepared = ClaudeCliPreparedRequest<C>;

    async fn prepare(
        &self,
        operation: ModelOperation<C>,
        mut cancellation: CancellationSignal,
    ) -> PreparationOutcome<C, Self::Prepared> {
        self.prepare_request(operation, &mut cancellation).await
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
        let evidence = execute_process(prepared, sink, &mut cancellation).await;
        TerminalReport {
            correlation,
            evidence,
        }
    }
}

async fn execute_process<C: Clone + Send + Sync>(
    prepared: ClaudeCliPreparedRequest<C>,
    sink: &mut (dyn ObservationSink<C> + Send),
    cancellation: &mut CancellationSignal,
) -> TerminalEvidence {
    let allowed_tools = prepared
        .translated
        .catalog
        .tools
        .iter()
        .map(|tool| qualified_tool_name(&tool.name))
        .collect::<Vec<_>>()
        .join(",");
    let disabled_tools = DISABLED_CLAUDE_CLI_BUILTIN_TOOLS.join(",");
    let mut command = std::process::Command::new(&prepared.executable);
    command
        .arg("--print")
        .arg("--verbose")
        .arg("--output-format=stream-json")
        .arg("--no-session-persistence")
        .arg("--setting-sources")
        .arg("")
        .arg("--settings")
        .arg(&prepared.settings)
        .arg("--disable-slash-commands")
        .arg("--strict-mcp-config")
        .arg("--mcp-config")
        .arg(&prepared.mcp_config)
        .arg("--no-chrome")
        .arg("--tools")
        .arg("")
        .arg("--allowedTools")
        .arg(&allowed_tools)
        .arg("--disallowedTools")
        .arg(disabled_tools)
        .arg("--permission-mode")
        .arg("dontAsk")
        .arg("--model")
        .arg(&prepared.resolved_target)
        .current_dir(&prepared.working_directory);
    if let Some(effort) = prepared.reasoning_effort {
        command.arg("--effort").arg(effort);
    }
    let decoder = EventDecoder::new(
        prepared.correlation.clone(),
        prepared.delivery,
        &prepared.translated,
    );
    let mut environment_overrides = vec![CliEnvironmentOverride::new(
        "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
        prepared.max_output_tokens.to_string(),
    )];
    if let Some(credential_home) = prepared.credential_home.as_ref() {
        environment_overrides.push(CliEnvironmentOverride::replacing_inherited(
            CLAUDE_CONFIG_HOME_VARIABLE,
            credential_home.as_os_str(),
        ));
    }
    let request = CliProcessRequest {
        command,
        prompt: prepared.prompt,
        decoder,
        exchange_timeout: prepared.exchange_timeout,
        interrupt_grace: prepared.interrupt_grace,
        post_kill_reap_bound: prepared.post_kill_reap_bound,
        event_limit: prepared.event_limit,
        stderr_limit: prepared.stderr_limit,
        environment: CLAUDE_ENVIRONMENT,
        environment_overrides,
    };
    let _support_directory = prepared.support_directory;
    match prepared.credential.as_ref() {
        None => execute_cli_process(request, sink, cancellation).await,
        Some(credential) => {
            let mut redacting_sink = CredentialRedactingSink::new(sink, credential);
            let evidence = execute_cli_process(request, &mut redacting_sink, cancellation).await;
            redacting_sink.flush();
            redact_evidence(evidence, credential, prepared.native_message_limit)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CLAUDE_CONFIG_HOME_VARIABLE, CLAUDE_ENVIRONMENT, CliEnvironmentVariable, ModelSettings,
        ReasoningLevel, ServiceTier, claude_reasoning_effort,
    };
    use signalbox_model_runtime::AnthropicServiceTier;

    use super::CLAUDE_CLI_FILE_CREDENTIAL_ENV_KEY;

    #[test]
    fn cli_environment_routes_credential_store_without_direct_credential_values() {
        assert!(
            CLAUDE_ENVIRONMENT
                .iter()
                .any(|variable| variable.name() == CLAUDE_CONFIG_HOME_VARIABLE),
            "the provider credential-store selector reaches the child"
        );
        assert!(
            CLAUDE_ENVIRONMENT.contains(&CliEnvironmentVariable::credential_home(
                CLAUDE_CONFIG_HOME_VARIABLE
            )),
            "the selector receives credential-home path handling"
        );
        assert!(
            !CLAUDE_ENVIRONMENT
                .iter()
                .any(|variable| variable.name() == CLAUDE_CLI_FILE_CREDENTIAL_ENV_KEY),
            "direct credential values never reach the child"
        );
    }

    #[test]
    fn claude_effort_mapping_uses_the_supported_cli_value() {
        let mut settings = ModelSettings::new(64);
        settings.reasoning_level = Some(ReasoningLevel::Max);

        assert_eq!(
            claude_reasoning_effort(&settings).expect("max is supported"),
            Some("max")
        );
    }

    #[test]
    fn claude_effort_mapping_rejects_ultra() {
        let mut settings = ModelSettings::new(64);
        settings.reasoning_level = Some(ReasoningLevel::Ultra);

        assert!(claude_reasoning_effort(&settings).is_err());
    }

    #[test]
    fn claude_mapping_rejects_an_explicit_service_tier() {
        let mut settings = ModelSettings::new(64);
        settings.service_tier = Some(ServiceTier::Anthropic(AnthropicServiceTier::Auto));

        assert!(claude_reasoning_effort(&settings).is_err());
    }
}
