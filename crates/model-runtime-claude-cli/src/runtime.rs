//! One operation, one Claude Code process spawn.

use std::path::{Path, PathBuf};
use std::time::Duration;

use signalbox_model_runtime::{
    CLI_PROCESS_GROUP_SUPERVISION_SUPPORTED, CancellationSignal, CliEnvironmentVariable,
    CliProcessRequest, DeliveryMode, ModelOperation, ModelRuntime, ObservationSink,
    PreparationDefect, PreparationFailure, PreparationOutcome, ProvenUnsentEvidence,
    TerminalEvidence, TerminalReport, UnsentCause, execute_cli_process,
};
use tempfile::TempDir;

use crate::bridge::SERVER_NAME;
use crate::config::ClaudeCliConfig;
use crate::event::EventDecoder;
use crate::translate::{TranslationError, qualified_tool_name, translate};

const CLAUDE_CONFIG_HOME_VARIABLE: &str = "CLAUDE_CONFIG_DIR";
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

/// Every built-in tool reported by the pinned isolated Claude Code CLI.
///
/// The invocation both selects an empty built-in surface with `--tools` and
/// explicitly passes this entire inventory through `--disallowedTools`.
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
    "Monitor",
    "NotebookEdit",
    "PushNotification",
    "Read",
    "RemoteTrigger",
    "ReportFindings",
    "ScheduleWakeup",
    "SendMessage",
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
pub const SUPPORTED_CLAUDE_CLI_VERSION: &str = "2.1.220";

/// Stateless subscription-backed Claude Code CLI adapter.
pub struct ClaudeCliRuntime {
    executable: PathBuf,
    mcp_bridge_executable: PathBuf,
    working_directory: PathBuf,
    credential_reference: signalbox_model_runtime::CredentialReference,
    exchange_timeout: Duration,
    interrupt_grace: Duration,
    event_limit: usize,
    stderr_limit: usize,
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
    exchange_timeout: Duration,
    interrupt_grace: Duration,
    event_limit: usize,
    stderr_limit: usize,
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
            .field("exchange_timeout", &self.exchange_timeout)
            .field("interrupt_grace", &self.interrupt_grace)
            .field("event_limit", &self.event_limit)
            .field("stderr_limit", &self.stderr_limit)
            .finish()
    }
}

impl ClaudeCliRuntime {
    /// Validates adapter configuration without invoking Claude or inspecting its login store.
    pub fn new(config: ClaudeCliConfig) -> Result<Self, ClaudeCliConstructionError> {
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
        if config.exchange_timeout.is_zero()
            || tokio::time::Instant::now()
                .checked_add(config.exchange_timeout)
                .is_none()
        {
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
            exchange_timeout: config.exchange_timeout,
            interrupt_grace: config.interrupt_grace,
            event_limit: config.event_limit,
            stderr_limit: config.stderr_limit,
        })
    }

    fn prepare_request<C>(
        &self,
        operation: ModelOperation<C>,
    ) -> PreparationOutcome<C, ClaudeCliPreparedRequest<C>> {
        let correlation = operation.correlation;
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
        let operation = ModelOperation {
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
        let support = match create_support_files(&self.mcp_bridge_executable, &translated) {
            Ok(support) => support,
            Err(detail) => {
                return PreparationOutcome::Defect {
                    correlation,
                    defect: PreparationDefect::RequestConstructionFailed { detail },
                };
            }
        };
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
            event_limit: self.event_limit,
            stderr_limit: self.stderr_limit,
        })
    }
}

struct SupportFiles {
    directory: TempDir,
    mcp_config: PathBuf,
    settings: PathBuf,
}

fn create_support_files(
    bridge: &Path,
    translated: &crate::translate::TranslatedOperation,
) -> Result<SupportFiles, String> {
    let temporary_directory = std::path::absolute(std::env::temp_dir())
        .map_err(|error| format!("could not absolutize temporary directory: {error}"))?;
    let directory = tempfile::Builder::new()
        .prefix("signalbox-claude-")
        .tempdir_in(temporary_directory)
        .map_err(|error| format!("could not create private support directory: {error}"))?;
    let catalog = directory.path().join("tools.json");
    let ready = directory.path().join("mcp-ready");
    let mcp_config = directory.path().join("mcp.json");
    let settings = directory.path().join("settings.json");
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
    let isolated_settings = serde_json::json!({
        "hooks": {"SessionStart": [{"hooks": [{
            "type": "command", "command": hook_command, "timeout": 10
        }]}]}
    });
    std::fs::write(
        &settings,
        serde_json::to_vec(&isolated_settings).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("could not write isolated settings: {error}"))?;
    Ok(SupportFiles {
        directory,
        mcp_config,
        settings,
    })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

impl<C: Clone + Send + Sync> ModelRuntime<C> for ClaudeCliRuntime {
    type Prepared = ClaudeCliPreparedRequest<C>;

    async fn prepare(
        &self,
        operation: ModelOperation<C>,
        _cancellation: CancellationSignal,
    ) -> PreparationOutcome<C, Self::Prepared> {
        self.prepare_request(operation)
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
    let decoder = EventDecoder::new(
        prepared.correlation.clone(),
        prepared.delivery,
        &prepared.translated,
    );
    let request = CliProcessRequest {
        command,
        prompt: prepared.prompt,
        decoder,
        exchange_timeout: prepared.exchange_timeout,
        interrupt_grace: prepared.interrupt_grace,
        event_limit: prepared.event_limit,
        stderr_limit: prepared.stderr_limit,
        environment: CLAUDE_ENVIRONMENT,
    };
    let _support_directory = prepared.support_directory;
    execute_cli_process(request, sink, cancellation).await
}

#[cfg(test)]
mod tests {
    use super::{CLAUDE_CONFIG_HOME_VARIABLE, CLAUDE_ENVIRONMENT, CliEnvironmentVariable};

    const DIRECT_CREDENTIAL_VARIABLE: &str = "ANTHROPIC_API_KEY";

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
                .any(|variable| variable.name() == DIRECT_CREDENTIAL_VARIABLE),
            "direct credential values never reach the child"
        );
    }
}
