//! One operation, one Codex CLI process spawn.

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
use tempfile::NamedTempFile;

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
    "guardianv2",
    "hooks",
    "image_generation",
    "in_app_browser",
    // The CLI's own update check and update flow: it reaches package-registry
    // hosts unrelated to the model exchange and can replace the executable
    // whose version this adapter pins. Its machinery lives in the interactive
    // front end and the app-server daemon, so a `codex exec` dispatch does not
    // reach it today; disabling it keeps that boundary explicit rather than
    // inherited from where the CLI happens to wire the feature.
    "in_app_updates",
    "mcp_2026_07_28",
    "memories",
    "multi_agent",
    "multi_agent_v2",
    "plugin_sharing",
    "plugins",
    "realtime_conversation",
    "recommended_plugins",
    "remote_plugin",
    "request_permissions_tool",
    "shell_snapshot",
    "shell_tool",
    "skill_mcp_dependency_install",
    "skill_search",
    "standalone_web_search",
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
/// `tooling/codex-cli/package.json`, so a Renovate change is mechanically
/// complete and the binding smoke tests that same version. That live exchange
/// does not prove the offline fixture corpus still represents the CLI's current
/// event shapes; fixture regeneration or validation against the installed CLI
/// remains the missing check. The runtime does not add a version-probe process
/// to a model dispatch.
pub const SUPPORTED_CODEX_CLI_VERSION: &str = env!("SIGNALBOX_CODEX_CLI_VERSION");

/// Stateless subscription-backed Codex CLI adapter.
pub struct CodexCliRuntime {
    executable: PathBuf,
    working_directory: PathBuf,
    credential_reference: signalbox_model_runtime::CredentialReference,
    credential_homes: HashMap<signalbox_model_runtime::CredentialReference, PathBuf>,
    exchange_timeout: Duration,
    interrupt_grace: Duration,
    event_limit: usize,
    stderr_limit: usize,
    model_capabilities: ModelCapabilityCatalog,
}

/// Opaque one-shot capability for one Codex CLI spawn.
///
/// It owns the rendered full context and the temporary output-schema file.
/// It deliberately implements neither `Clone`, serialization, nor diagnostic
/// formatting.
#[must_use]
pub struct CodexCliPreparedRequest<C> {
    executable: PathBuf,
    working_directory: PathBuf,
    prompt: Vec<u8>,
    output_schema: NamedTempFile,
    correlation: C,
    resolved_target: String,
    delivery: DeliveryMode,
    translated: crate::translate::TranslatedOperation,
    exchange_timeout: Duration,
    interrupt_grace: Duration,
    event_limit: usize,
    stderr_limit: usize,
    controls: CodexControls,
    credential_home: Option<PathBuf>,
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
    /// child process and its `--cd` argument.
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
    /// A configured credential home contains no provisioned entries.
    EmptyCredentialHome,
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
            Self::EmptyCredentialHome => formatter.write_str("Codex credential home is empty"),
        }
    }
}

impl std::error::Error for CodexCliConstructionError {}

impl std::fmt::Debug for CodexCliRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexCliRuntime")
            .field("executable", &self.executable)
            .field("working_directory", &self.working_directory)
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
        if config.exchange_timeout.is_zero()
            || tokio::time::Instant::now()
                .checked_add(config.exchange_timeout)
                .is_none()
        {
            return Err(CodexCliConstructionError::InvalidExchangeTimeout);
        }
        if config.interrupt_grace.is_zero() {
            return Err(CodexCliConstructionError::InvalidInterruptGrace);
        }
        if config.event_limit == 0 || config.stderr_limit == 0 {
            return Err(CodexCliConstructionError::InvalidOutputLimit);
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
                Some(Ok(_)) => {}
                Some(Err(_)) => {
                    return Err(CodexCliConstructionError::UnreadableCredentialHome);
                }
                None => return Err(CodexCliConstructionError::EmptyCredentialHome),
            }
        }
        Ok(Self {
            executable: config.executable,
            working_directory: config.working_directory,
            credential_reference: config.credential_reference,
            credential_homes: config.credential_homes,
            exchange_timeout: config.exchange_timeout,
            interrupt_grace: config.interrupt_grace,
            event_limit: config.event_limit,
            stderr_limit: config.stderr_limit,
            model_capabilities: config.model_capabilities,
        })
    }

    fn prepare_request<C>(
        &self,
        operation: ModelOperation<C>,
    ) -> PreparationOutcome<C, CodexCliPreparedRequest<C>> {
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
        let controls = match codex_controls(&operation.settings, request_fast_mode) {
            Ok(controls) => controls,
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
        if operation.credential_reference != self.credential_reference && credential_home.is_none()
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
        // The child interprets `--output-schema` after `current_dir` moves it
        // to the configured working root, so a schema path that is relative
        // there — as under a relative `TMPDIR` — would name a file that
        // preparation never created. Create the file under the absolutized
        // temporary directory so its retained path cannot be relative.
        let temporary_directory = match std::path::absolute(std::env::temp_dir()) {
            Ok(directory) => directory,
            Err(error) => {
                return PreparationOutcome::Defect {
                    correlation,
                    defect: PreparationDefect::RequestConstructionFailed {
                        detail: format!(
                            "could not absolutize the temporary directory for the \
                             output-schema file: {error}"
                        ),
                    },
                };
            }
        };
        let mut output_schema = match tempfile::Builder::new()
            .prefix("signalbox-codex-output-")
            .suffix(".json")
            .tempfile_in(temporary_directory)
        {
            Ok(file) => file,
            Err(error) => {
                return PreparationOutcome::Defect {
                    correlation,
                    defect: PreparationDefect::RequestConstructionFailed {
                        detail: format!("could not create output-schema file: {error}"),
                    },
                };
            }
        };
        if let Err(error) = std::io::Write::write_all(&mut output_schema, OUTPUT_SCHEMA.as_bytes())
        {
            return PreparationOutcome::Defect {
                correlation,
                defect: PreparationDefect::RequestConstructionFailed {
                    detail: format!("could not write output-schema file: {error}"),
                },
            };
        }
        let prompt = std::mem::take(&mut translated.prompt);
        PreparationOutcome::Prepared(CodexCliPreparedRequest {
            executable: self.executable.clone(),
            working_directory: self.working_directory.clone(),
            prompt,
            output_schema,
            correlation,
            resolved_target: operation.resolved_target.as_str().to_string(),
            delivery: operation.delivery,
            translated,
            exchange_timeout: self.exchange_timeout,
            interrupt_grace: self.interrupt_grace,
            event_limit: self.event_limit,
            stderr_limit: self.stderr_limit,
            controls,
            credential_home,
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
    prepared: CodexCliPreparedRequest<C>,
    sink: &mut (dyn ObservationSink<C> + Send),
    cancellation: &mut CancellationSignal,
) -> TerminalEvidence {
    let mut command = std::process::Command::new(&prepared.executable);
    command
        .arg("exec")
        .arg("--json")
        .arg("--ephemeral")
        .arg("--ignore-user-config")
        .arg("--ignore-rules")
        .arg("--strict-config");
    for feature in DISABLED_CODEX_CLI_CAPABILITY_FEATURES {
        command.arg("--disable").arg(feature);
    }
    if prepared.controls.service_tier.is_some() {
        command.arg("--enable").arg("fast_mode");
    }
    if let Some(effort) = prepared.controls.reasoning_effort {
        command
            .arg("--config")
            .arg(format!("model_reasoning_effort=\"{effort}\""));
    }
    if let Some(tier) = prepared.controls.service_tier {
        command
            .arg("--config")
            .arg(format!("service_tier=\"{tier}\""));
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
        .arg("--sandbox")
        .arg("read-only")
        .arg("--skip-git-repo-check")
        .arg("--cd")
        .arg(&prepared.working_directory)
        .arg("--model")
        .arg(&prepared.resolved_target)
        .arg("--output-schema")
        .arg(prepared.output_schema.path())
        .arg("-")
        .current_dir(&prepared.working_directory);
    let decoder = EventDecoder::new(
        prepared.correlation.clone(),
        prepared.delivery,
        &prepared.translated,
    );
    // The selected profile controls this child only; the adapter passes the
    // path reference and never opens the login material, as required by
    // `docs/spec/configuration-and-credentials.md#the-codex_home-delivery`.
    let environment_overrides = prepared
        .credential_home
        .map(|home| {
            vec![CliEnvironmentOverride::replacing_inherited(
                CODEX_CREDENTIAL_HOME,
                home.into_os_string(),
            )]
        })
        .unwrap_or_default();
    let request = CliProcessRequest {
        command,
        prompt: prepared.prompt,
        decoder,
        exchange_timeout: prepared.exchange_timeout,
        interrupt_grace: prepared.interrupt_grace,
        event_limit: prepared.event_limit,
        stderr_limit: prepared.stderr_limit,
        environment: CODEX_ENVIRONMENT,
        environment_overrides,
    };
    let _output_schema = prepared.output_schema;
    execute_cli_process(request, sink, cancellation).await
}

#[cfg(test)]
mod tests {
    use super::{
        CODEX_CREDENTIAL_HOME, CODEX_ENVIRONMENT, CliEnvironmentVariable, CodexCliServiceTier,
        FORBIDDEN_DIRECT_CREDENTIAL_ENVIRONMENT, FastMode, ModelSettings, ReasoningLevel,
        ServiceTier, codex_controls, validate_model_settings,
    };

    /// INV-035: the CLI receives only a reference to its ambient login store;
    /// direct credential-value variables are absent from the inherited set.
    #[test]
    fn inv_035_cli_environment_excludes_direct_credential_values() {
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
