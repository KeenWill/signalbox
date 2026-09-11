//! Signalbox daemon composition root.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md owns startup ordering
//! (migrate, scan, then schedule), graceful shutdown, and composition-root
//! wiring; docs/spec/runtime-substrate.md and
//! docs/spec/configuration-and-credentials.md keep runtime, subscriber,
//! deployment configuration, and migration policy at this executable
//! boundary.

#[cfg(test)]
use signalboxd::credential_files_conflict;
use signalboxd::repo_watch_runtime::{
    RepositoryWatchRuntime, RepositoryWatchRuntimeError, RepositoryWatchServices,
    connect_repository_watch_pool,
};

use std::{
    cell::Cell,
    env,
    ffi::OsString,
    fmt, fs,
    future::Future,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use signalbox_application::{
    ClassifyOperatorFailure, GoalAwareEligibilityPass, InProcessAttemptDispatchGate,
    InProcessEligibilityWorkSource, InProcessToolDispatchGate, ModelCallCredentialReference,
    OperatorFailureClass, ReconciliationSweepInterval, SchedulerLoop, SchedulerLoopExit,
    SchedulerPassOccupancyBound, StaleActiveTurnBound, StartupScanService,
    TurnLivenessScanInterval, UuidV7StartupScanIdGenerator,
};
#[cfg(test)]
use signalbox_application::{EligibilityPass, EligibilityWorkSource};
use signalbox_domain::{SessionId, TurnId};
use signalbox_model_provider_runtime::{
    ApprovalJudgeModel, ContextCompactionModel, RuntimeApprovalJudgeModel,
    RuntimeContextCompactionModel, RuntimeModelCallProvider,
};
use signalbox_model_runtime::CredentialReference;
#[cfg(test)]
use signalbox_model_runtime_anthropic::AnthropicConstructionError;
use signalbox_model_runtime_codex_cli::verify_pinned_codex_cli_version;
#[cfg(test)]
use signalbox_model_runtime_openai::OpenAiConstructionError;
use signalbox_persistence::{
    automatic_reconciliation::RETRY_LADDER_ARITY, blob::BlobCatalogRepository,
    hub_fence::FENCED_POOL_MAX_CONNECTIONS, migrate, model_execution::PostgresModelCallRepository,
    scheduler::PostgresEligibilitySweep, session_deadline::SessionDeadlineBounds,
    start_eligible_turn::StartEligibleTurnRepository, startup::PostgresStartupScanRepository,
    turn_liveness::TurnLivenessPersistenceBounds,
};
use signalbox_tools_web::BRAVE_SEARCH_CREDENTIAL_REFERENCE;
use signalboxd::GUARD_CHECK_INTERVAL;
use signalboxd::runner_protocol_runtime::{
    PostgresRunnerRegistrationService, RunnerProtocolRuntime, RunnerProtocolRuntimeError,
    RunnerRegistrationFailureCause,
};
use signalboxd::{
    AttachmentPreparingModelCallProvider, BaseDaemonCredentialInputs, BlobStoreRegistry, BlobTools,
    CODE_HOST_CREDENTIAL_REFERENCE, CodeHostNumericBounds, ConfiguredApprovalPostureError,
    ContextGuardedTurnPass, ConvergenceSweepNumericBounds, DaemonTools,
    DaemonToolsConstructionError, ExpiredPassRecoveryPolicy, FatalExecutionSupervisor,
    FencedHubDatabase, FencedHubDatabaseError, FencedPoolFloorReconciliation, FileCredentialAccess,
    GitHubCodeHostTransport, GoalModeNumericBounds, HubModelConfiguration,
    HubModelConfigurationError, LifecycleDeadlineRuntime, LifecycleMetricsRuntime,
    LocalProcessListener, LocalSocketError, MappedDaemonCredentialInputs, OtlpRuntime,
    PostgresGoalPassDisposition, PostgresProviderModelExecution, ProcessRuntime,
    ProcessRuntimeError, PrometheusServer, ReportedUsageCompaction, SessionTemplateConfiguration,
    SessionTemplateConfigurationError, SingleHubGuardError, SystemCurrentTimeClock,
    TelemetryConfiguration, TelemetryConfigurationError, TelemetryExportFilter, TelemetryMetrics,
    TurnLivenessNumericBounds, TurnLivenessRuntime, WebBlobRuntime, WorkspaceInstructionRuntime,
    reconcile_fenced_pool_floor, run_web_image_derivative_worker_if_requested,
    usage_limits::UsageLimitedModelCallProvider,
    web_http::{
        WebHttpConfiguration, WebHttpConfigurationError, WebHttpRuntime, WebHttpRuntimeError,
    },
};
use tracing_subscriber::prelude::*;

use tokio::{
    pin, select,
    sync::{oneshot, watch},
    task::{JoinError, JoinSet},
    time::{sleep, timeout},
};

const MODEL_CONFIGURATION_FILE_ENVIRONMENT: &str = "SIGNALBOX_CONFIG_FILE";
const DATABASE_URL_ENVIRONMENT: &str = "DATABASE_URL";
const TEMPLATE_CONFIGURATION_FILE_ENVIRONMENT: &str = "SIGNALBOX_TEMPLATE_CONFIG_FILE";
const BRAVE_API_KEY_FILE_ENVIRONMENT: &str = "BRAVE_API_KEY_FILE";
const GITHUB_TOKEN_FILE_ENVIRONMENT: &str = "GITHUB_TOKEN_FILE";
const LOG_FILTER_ENVIRONMENT: &str = "RUST_LOG";
const PROCESS_SOCKET_PATH_ENVIRONMENT: &str = "SIGNALBOX_SOCKET_PATH";
const RUNNER_SOCKET_PATH_ENVIRONMENT: &str = "SIGNALBOX_RUNNER_SOCKET_PATH";

fn graceful_shutdown_window(
    model_exchange_timeout: Option<Duration>,
    cleanup_window: Option<Duration>,
) -> Option<Duration> {
    model_exchange_timeout
        .zip(cleanup_window)
        .map(|(exchange, cleanup)| exchange.saturating_add(cleanup))
}

fn validate_fenced_pool_min_connections(minimum: Option<u32>) -> Option<Option<u32>> {
    (!minimum.is_some_and(|minimum| minimum > FENCED_POOL_MAX_CONNECTIONS)).then_some(minimum)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FencedPoolFloorReconciliationPolicy {
    minimum: u32,
    interval: Duration,
    attempt_bound: Duration,
}

fn fenced_pool_floor_reconciliation_policy(
    minimum: Option<u32>,
    interval: Option<Duration>,
    attempt_bound: Option<Duration>,
) -> Option<Option<FencedPoolFloorReconciliationPolicy>> {
    let minimum = minimum.filter(|minimum| *minimum > 0);
    let Some(minimum) = minimum else {
        return Some(None);
    };
    let interval = interval.filter(|interval| !interval.is_zero())?;
    let attempt_bound = attempt_bound.filter(|bound| !bound.is_zero())?;
    Some(Some(FencedPoolFloorReconciliationPolicy {
        minimum,
        interval,
        attempt_bound,
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimePhase {
    Configuration,
    DatabaseConnection,
    Migration,
    StartupScan,
    SocketBinding,
    Scheduling,
    Runtime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HubRuntimeError {
    phase: RuntimePhase,
    failure_class: OperatorFailureClass,
    database_failure: bool,
    session: Option<SessionId>,
    turn: Option<TurnId>,
}

impl HubRuntimeError {
    const fn infrastructure(phase: RuntimePhase) -> Self {
        Self {
            phase,
            database_failure: matches!(phase, RuntimePhase::DatabaseConnection),
            failure_class: OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            session: None,
            turn: None,
        }
    }

    const fn startup_scan(
        failure_class: OperatorFailureClass,
        session: Option<SessionId>,
        turn: Option<TurnId>,
    ) -> Self {
        Self {
            phase: RuntimePhase::StartupScan,
            database_failure: true,
            failure_class,
            session,
            turn,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequiredSettingFailure {
    Missing,
    NotUnicode,
    Empty,
    Conflicts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HubConfigurationError {
    setting: &'static str,
    failure: RequiredSettingFailure,
}

impl HubConfigurationError {
    const fn new(setting: &'static str, failure: RequiredSettingFailure) -> Self {
        Self { setting, failure }
    }
}

impl fmt::Display for HubConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let failure = match self.failure {
            RequiredSettingFailure::Missing => "is missing",
            RequiredSettingFailure::NotUnicode => "is not valid Unicode",
            RequiredSettingFailure::Empty => "is empty",
            RequiredSettingFailure::Conflicts => "conflicts with another setting",
        };
        write!(formatter, "required setting {} {failure}", self.setting)
    }
}

struct HubConfiguration {
    database_url: String,
    model_configuration_file: PathBuf,
    template_configuration_file: PathBuf,
    brave_api_key_file: PathBuf,
    github_token_file: PathBuf,
    process_socket_path: PathBuf,
    runner_socket_path: PathBuf,
}

struct HubConfigurationValues {
    database_url: Option<OsString>,
    model_configuration_file: Option<OsString>,
    template_configuration_file: Option<OsString>,
    brave_api_key_file: Option<OsString>,
    github_token_file: Option<OsString>,
    process_socket_path: Option<OsString>,
    runner_socket_path: Option<OsString>,
}

impl HubConfiguration {
    fn from_environment() -> Result<Self, HubConfigurationError> {
        Self::from_values(HubConfigurationValues {
            database_url: env::var_os(DATABASE_URL_ENVIRONMENT),
            model_configuration_file: env::var_os(MODEL_CONFIGURATION_FILE_ENVIRONMENT),
            template_configuration_file: env::var_os(TEMPLATE_CONFIGURATION_FILE_ENVIRONMENT),
            brave_api_key_file: env::var_os(BRAVE_API_KEY_FILE_ENVIRONMENT),
            github_token_file: env::var_os(GITHUB_TOKEN_FILE_ENVIRONMENT),
            process_socket_path: env::var_os(PROCESS_SOCKET_PATH_ENVIRONMENT),
            runner_socket_path: env::var_os(RUNNER_SOCKET_PATH_ENVIRONMENT),
        })
    }

    fn from_values(values: HubConfigurationValues) -> Result<Self, HubConfigurationError> {
        let HubConfigurationValues {
            database_url,
            model_configuration_file,
            template_configuration_file,
            brave_api_key_file,
            github_token_file,
            process_socket_path,
            runner_socket_path,
        } = values;
        let database_url = database_url
            .ok_or_else(|| {
                HubConfigurationError::new(
                    DATABASE_URL_ENVIRONMENT,
                    RequiredSettingFailure::Missing,
                )
            })?
            .into_string()
            .map_err(|_| {
                HubConfigurationError::new(
                    DATABASE_URL_ENVIRONMENT,
                    RequiredSettingFailure::NotUnicode,
                )
            })?;
        if database_url.is_empty() {
            return Err(HubConfigurationError::new(
                DATABASE_URL_ENVIRONMENT,
                RequiredSettingFailure::Empty,
            ));
        }
        let model_configuration_file = required_path(
            MODEL_CONFIGURATION_FILE_ENVIRONMENT,
            model_configuration_file,
        )?;
        let template_configuration_file = required_path(
            TEMPLATE_CONFIGURATION_FILE_ENVIRONMENT,
            template_configuration_file,
        )?;
        let brave_api_key_file = required_path(BRAVE_API_KEY_FILE_ENVIRONMENT, brave_api_key_file)?;
        let github_token_file = github_token_file.map(PathBuf::from).unwrap_or_default();
        let process_socket_path =
            required_path(PROCESS_SOCKET_PATH_ENVIRONMENT, process_socket_path)?;
        let runner_socket_path = match runner_socket_path {
            Some(value) => required_path(RUNNER_SOCKET_PATH_ENVIRONMENT, Some(value))?,
            None => process_socket_path.with_extension("runner.sock"),
        };
        if socket_artifacts_conflict(&process_socket_path, &runner_socket_path) {
            return Err(HubConfigurationError::new(
                RUNNER_SOCKET_PATH_ENVIRONMENT,
                RequiredSettingFailure::Conflicts,
            ));
        }

        Ok(Self {
            database_url,
            model_configuration_file,
            template_configuration_file,
            brave_api_key_file,
            github_token_file,
            process_socket_path,
            runner_socket_path,
        })
    }

    fn database_url(&self) -> &str {
        &self.database_url
    }

    fn model_configuration_file(&self) -> &Path {
        &self.model_configuration_file
    }

    fn template_configuration_file(&self) -> &Path {
        &self.template_configuration_file
    }

    fn github_token_file(&self) -> PathBuf {
        self.github_token_file.clone()
    }

    fn repository_watch_credential_conflicts(&self, configuration: &HubModelConfiguration) -> bool {
        configuration.github_tool_credential_conflicts(&self.github_token_file)
    }

    fn brave_api_key_file(&self) -> PathBuf {
        self.brave_api_key_file.clone()
    }

    fn process_socket_path(&self) -> &Path {
        &self.process_socket_path
    }

    fn runner_socket_path(&self) -> &Path {
        &self.runner_socket_path
    }
}

fn required_path(
    setting: &'static str,
    value: Option<OsString>,
) -> Result<PathBuf, HubConfigurationError> {
    let value = value
        .ok_or_else(|| HubConfigurationError::new(setting, RequiredSettingFailure::Missing))?;
    if value.is_empty() {
        Err(HubConfigurationError::new(
            setting,
            RequiredSettingFailure::Empty,
        ))
    } else {
        Ok(PathBuf::from(value))
    }
}

fn socket_artifacts_conflict(process_path: &Path, runner_path: &Path) -> bool {
    let Some(process_artifacts) = socket_artifact_paths(process_path) else {
        return process_path == runner_path;
    };
    let Some(runner_artifacts) = socket_artifact_paths(runner_path) else {
        return process_path == runner_path;
    };
    let oauth_root = oauth_credential_root(&process_artifacts[0]);
    process_artifacts
        .iter()
        .any(|process| runner_artifacts.iter().any(|runner| runner == process))
        || runner_artifacts
            .iter()
            .any(|runner| runner.starts_with(&oauth_root) || oauth_root.starts_with(runner))
}

fn oauth_credential_root(process_socket: &Path) -> PathBuf {
    let mut root = process_socket.as_os_str().to_owned();
    root.push(".oauth");
    PathBuf::from(root)
}

fn socket_artifact_paths(path: &Path) -> Option<[PathBuf; 3]> {
    let file_name = path.file_name().filter(|name| !name.is_empty())?;
    let parent = path.parent()?;
    let resolved_parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    let public = resolved_parent.join(file_name);
    let mut lock = public.as_os_str().to_owned();
    lock.push(".lock");
    let mut identity = public.as_os_str().to_owned();
    identity.push(".identity");
    Some([public, PathBuf::from(lock), PathBuf::from(identity)])
}

/// Closed startup causes admitted to operator telemetry.
///
/// Every variant wraps a Display implementation audited to omit paths,
/// credentials, configuration content, provider prose, and user content.
enum SanitizedStartupCause<'a> {
    Configuration(&'a HubConfigurationError),
    ModelConfiguration(&'a HubModelConfigurationError),
    Credential(&'a signalbox_model_runtime::CredentialAccessError),
    TemplateConfiguration(&'a SessionTemplateConfigurationError),
    TelemetryConfiguration(&'a TelemetryConfigurationError),
    Database(&'a FencedHubDatabaseError),
    Migration(&'a sqlx::migrate::MigrateError),
    Reload(&'a signalbox_persistence::reload_configuration::ReloadRepositoryError),
    BlobStorage(&'a signalboxd::BlobStoreRegistryError),
    Tools(&'a DaemonToolsConstructionError),
    Socket(&'a LocalSocketError),
    WebHttpConfiguration(&'a WebHttpConfigurationError),
    Static(&'static str),
}

impl fmt::Display for SanitizedStartupCause<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(error) => error.fmt(formatter),
            Self::ModelConfiguration(error) => error.fmt(formatter),
            Self::Credential(error) => error.fmt(formatter),
            Self::TemplateConfiguration(error) => error.fmt(formatter),
            Self::TelemetryConfiguration(error) => error.fmt(formatter),
            Self::Database(error) => error.fmt(formatter),
            Self::Migration(_) => formatter.write_str("database migration failed"),
            Self::Reload(_) => formatter.write_str("configuration reload recovery failed"),
            Self::BlobStorage(error) => error.fmt(formatter),
            Self::Tools(error) => error.fmt(formatter),
            Self::Socket(error) => error.fmt(formatter),
            Self::WebHttpConfiguration(error) => error.fmt(formatter),
            Self::Static(cause) => formatter.write_str(cause),
        }
    }
}

/// Records one startup cause at the point typed evidence is erased.
///
/// `SanitizedStartupCause` is a closed admission boundary, so the emitted
/// cause cannot include configuration values, paths, credentials, or content.
fn erase_startup_cause(phase: RuntimePhase, cause: SanitizedStartupCause<'_>) -> HubRuntimeError {
    let mut error = HubRuntimeError::infrastructure(phase);
    if let SanitizedStartupCause::Database(FencedHubDatabaseError::AdvanceFence(fence)) = &cause {
        match fence {
            signalbox_persistence::hub_fence::HubFenceError::Database(_) => {
                error.database_failure = true;
            }
            signalbox_persistence::hub_fence::HubFenceError::Corruption(_) => {
                error.database_failure = false;
                error.failure_class = OperatorFailureClass::FailClosedCorruption;
            }
        }
    }
    if let SanitizedStartupCause::BlobStorage(signalboxd::BlobStoreRegistryError::Catalog(
        catalog,
    )) = &cause
    {
        error.failure_class = match catalog {
            signalbox_persistence::blob::BlobCatalogRepositoryError::Database(_) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                }
            }
            signalbox_persistence::blob::BlobCatalogRepositoryError::CommitAmbiguous(_) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                }
            }
            signalbox_persistence::blob::BlobCatalogRepositoryError::Corruption(_) => {
                OperatorFailureClass::FailClosedCorruption
            }
        };
        error.database_failure = matches!(
            catalog,
            signalbox_persistence::blob::BlobCatalogRepositoryError::Database(_)
                | signalbox_persistence::blob::BlobCatalogRepositoryError::CommitAmbiguous(_)
        );
    }
    let migration = match &cause {
        SanitizedStartupCause::Migration(migration) => Some(*migration),
        SanitizedStartupCause::Database(FencedHubDatabaseError::InitializeFence(migration)) => {
            Some(migration)
        }
        _ => None,
    };
    if let Some(migration) = migration {
        error.database_failure = matches!(
            migration,
            sqlx::migrate::MigrateError::Execute(_)
                | sqlx::migrate::MigrateError::ExecuteMigration(_, _)
        );
    }
    if let SanitizedStartupCause::Reload(failure) = &cause {
        use signalbox_persistence::reload_configuration::ReloadRepositoryError;
        (error.failure_class, error.database_failure) = match failure {
            ReloadRepositoryError::Database(_) => (
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                },
                true,
            ),
            ReloadRepositoryError::CommitAmbiguous(_) => (
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                },
                true,
            ),
            ReloadRepositoryError::Corruption(_) => {
                (OperatorFailureClass::FailClosedCorruption, false)
            }
            ReloadRepositoryError::InvalidCommandId => {
                (OperatorFailureClass::CallerOrHubBug, false)
            }
        };
    }
    tracing::error!(
        ?phase,
        failure_class = ?error.failure_class,
        cause = %cause,
        "daemon startup construction failed"
    );
    error
}

fn erase_startup_database_cause(
    phase: RuntimePhase,
    cause: SanitizedStartupCause<'_>,
) -> HubRuntimeError {
    let mut error = erase_startup_cause(phase, cause);
    error.database_failure = true;
    error
}

const fn configured_approval_posture_cause(error: &ConfiguredApprovalPostureError) -> &'static str {
    match error {
        ConfiguredApprovalPostureError::UnknownTool { .. } => {
            "tool_approval_posture_names_unknown_tool"
        }
    }
}

/// Records startup-scan failure evidence before reducing it to runtime status.
///
/// The cause is a closed application token and the optional session/turn are
/// daemon-minted identities; repository detail and transcript content stay out.
fn erase_startup_scan_cause(
    failure_class: OperatorFailureClass,
    cause_code: &'static str,
    session: Option<SessionId>,
    turn: Option<TurnId>,
) -> HubRuntimeError {
    tracing::error!(
        phase = ?RuntimePhase::StartupScan,
        ?failure_class,
        cause_code,
        session_id = ?session.map(SessionId::into_uuid),
        turn_id = ?turn.map(TurnId::into_uuid),
        "daemon startup scan failed"
    );
    HubRuntimeError::startup_scan(failure_class, session, turn)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShutdownOutcome {
    Clean,
    Interrupted,
    GraceWindowExpired,
    SignalListenerFailed,
    ExecutionFailed,
    ExecutionFailedAfterGraceWindow,
    GuardLost,
    GuardRecoveryExhausted,
    RuntimeFailed,
    RuntimeFailedAfterGraceWindow,
    RuntimeDefect,
    RuntimeDefectAfterGraceWindow,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SchedulerStopCause {
    Requested,
    SignalListenerFailed,
    ExecutionFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeStopCause {
    Requested,
    SignalListenerFailed,
    ExecutionFailed,
    GuardLost,
    RuntimeFailed,
    RuntimeDefect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeDrainOutcome {
    Complete,
    Interrupted,
    GraceWindowExpired,
    GuardLost,
}

enum RuntimeTaskExit {
    Scheduler(SchedulerLoopExit),
    FencedPoolFloor,
    Workflows(Result<(), signalboxd::workflows::WorkflowRuntimeError>),
    CredentialInvocations,
    Process(Result<(), ProcessRuntimeError>),
    Runner(Result<(), RunnerProtocolRuntimeError>),
    RepositoryWatch(Result<(), RepositoryWatchRuntimeError>),
    WebHttp(Result<(), WebHttpRuntimeError>),
    TurnLiveness,
    LifecycleDeadline,
    LifecycleMetrics,
    SessionSupervision,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeTaskCompletion {
    Clean,
    Failed,
    Defect,
}

impl RuntimeTaskCompletion {
    const fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Defect, _) | (_, Self::Defect) => Self::Defect,
            (Self::Failed, _) | (_, Self::Failed) => Self::Failed,
            (Self::Clean, Self::Clean) => Self::Clean,
        }
    }
}

const fn combine_runtime_stop_cause(
    cause: RuntimeStopCause,
    completion: RuntimeTaskCompletion,
) -> RuntimeStopCause {
    match (cause, completion) {
        (RuntimeStopCause::RuntimeDefect, _) | (_, RuntimeTaskCompletion::Defect) => {
            RuntimeStopCause::RuntimeDefect
        }
        (RuntimeStopCause::SignalListenerFailed, _) => RuntimeStopCause::SignalListenerFailed,
        (RuntimeStopCause::ExecutionFailed, _) => RuntimeStopCause::ExecutionFailed,
        (_, RuntimeTaskCompletion::Failed) => RuntimeStopCause::RuntimeFailed,
        (cause, RuntimeTaskCompletion::Clean) => cause,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeTaskDefect {
    SchedulerCompletedBeforeShutdown,
    FencedPoolFloorCompletedBeforeShutdown,
    ProcessCompletedBeforeShutdown,
    RunnerCompletedBeforeShutdown,
    RepositoryWatchCompletedBeforeShutdown,
    WebHttpCompletedBeforeShutdown,
    TurnLivenessCompletedBeforeShutdown,
    LifecycleDeadlineCompletedBeforeShutdown,
    LifecycleMetricsCompletedBeforeShutdown,
    SessionSupervisionCompletedBeforeShutdown,
    TaskCancelled,
    TaskPanicked,
    TaskJoinFailed,
    TaskSetEmpty,
}

impl RuntimeTaskDefect {
    const fn cause_code(self) -> &'static str {
        match self {
            Self::SchedulerCompletedBeforeShutdown => "scheduler_completed_before_shutdown",
            Self::FencedPoolFloorCompletedBeforeShutdown => {
                "fenced_pool_floor_completed_before_shutdown"
            }
            Self::ProcessCompletedBeforeShutdown => "process_runtime_completed_before_shutdown",
            Self::RunnerCompletedBeforeShutdown => "runner_runtime_completed_before_shutdown",
            Self::RepositoryWatchCompletedBeforeShutdown => {
                "repository_watch_completed_before_shutdown"
            }
            Self::WebHttpCompletedBeforeShutdown => "web_http_completed_before_shutdown",
            Self::TurnLivenessCompletedBeforeShutdown => "turn_liveness_completed_before_shutdown",
            Self::LifecycleDeadlineCompletedBeforeShutdown => {
                "lifecycle_deadline_completed_before_shutdown"
            }
            Self::LifecycleMetricsCompletedBeforeShutdown => {
                "lifecycle_metrics_completed_before_shutdown"
            }
            Self::SessionSupervisionCompletedBeforeShutdown => {
                "session_supervision_completed_before_shutdown"
            }
            Self::TaskCancelled => "runtime_task_cancelled",
            Self::TaskPanicked => "runtime_task_panicked",
            Self::TaskJoinFailed => "runtime_task_join_failed",
            Self::TaskSetEmpty => "runtime_task_set_empty",
        }
    }
}

const fn should_close_pool(outcome: &Result<ShutdownOutcome, HubRuntimeError>) -> bool {
    matches!(
        outcome,
        Ok(ShutdownOutcome::Clean
            | ShutdownOutcome::ExecutionFailed
            | ShutdownOutcome::RuntimeFailed
            | ShutdownOutcome::RuntimeDefect)
            | Err(_)
    )
}

const fn database_close_failure_outcome(outcome: ShutdownOutcome) -> ShutdownOutcome {
    match outcome {
        ShutdownOutcome::ExecutionFailed
        | ShutdownOutcome::ExecutionFailedAfterGraceWindow
        | ShutdownOutcome::RuntimeDefect
        | ShutdownOutcome::RuntimeDefectAfterGraceWindow => outcome,
        _ => ShutdownOutcome::RuntimeFailed,
    }
}

const fn staging_sweep_failure_outcome(outcome: ShutdownOutcome) -> ShutdownOutcome {
    match outcome {
        ShutdownOutcome::ExecutionFailed
        | ShutdownOutcome::ExecutionFailedAfterGraceWindow
        | ShutdownOutcome::RuntimeDefect
        | ShutdownOutcome::RuntimeDefectAfterGraceWindow => outcome,
        _ => ShutdownOutcome::RuntimeFailed,
    }
}

/// Records database-close failure without displacing its initiating cause.
///
/// `SingleHubGuardError` has a static sanitized Display that excludes SQLx
/// detail, so database URLs, credentials, query text, and server prose stay out.
fn report_database_close_failure(error: &SingleHubGuardError) {
    let failure_class = OperatorFailureClass::Infrastructure {
        commit_ambiguous: false,
    };
    tracing::error!(
        phase = ?RuntimePhase::Runtime,
        ?failure_class,
        cause = %error,
        "daemon database close failed"
    );
}

fn startup_failure_after_close(
    failure: HubRuntimeError,
    closed: Result<(), signalboxd::SingleHubGuardError>,
) -> Result<ShutdownOutcome, HubRuntimeError> {
    if matches!(closed, Err(signalboxd::SingleHubGuardError::GuardLost(_))) {
        tracing::warn!("database guard lost during startup cleanup");
        Ok(ShutdownOutcome::GuardLost)
    } else {
        Err(failure)
    }
}

async fn migrate_hub_database(pool: &sqlx::PgPool) -> Result<(), HubRuntimeError> {
    migrate(pool).await.map_err(|error| {
        tracing::error!(migration_detail = %error, "database migration rejected");
        erase_startup_cause(
            RuntimePhase::Migration,
            SanitizedStartupCause::Migration(&error),
        )
    })?;
    tracing::info!(phase = ?RuntimePhase::Migration, "daemon startup phase completed");
    Ok(())
}

async fn install_oauth_registrations(
    pool: &sqlx::PgPool,
    oauth_registrations: &[(
        String,
        signalbox_persistence::oauth_credential::OauthRegistration,
    )],
) -> Result<(), HubRuntimeError> {
    signalbox_persistence::oauth_credential::OauthCredentialRepository::new(pool.clone())
        .replace_registrations(oauth_registrations)
        .await
        .map_err(|error| {
            erase_startup_scan_cause(
                process_runtime_failure_class(&ProcessRuntimeError::OauthRecovery(error)),
                "oauth_registration_recovery_failed",
                None,
                None,
            )
        })
}

async fn migrate_scan_then_schedule<Migration, Scan, Schedule, Runtime, Output>(
    migration: Migration,
    scan: Scan,
    schedule: Schedule,
) -> Result<Output, HubRuntimeError>
where
    Migration: Future<Output = Result<(), HubRuntimeError>>,
    Scan: Future<Output = Result<(), HubRuntimeError>>,
    Schedule: FnOnce() -> Runtime,
    Runtime: Future<Output = Output>,
{
    migration.await?;
    scan.await?;
    Ok(schedule().await)
}

#[cfg(test)]
async fn run_scheduler_until_shutdown<WorkSource, Pass, Shutdown>(
    mut scheduler: SchedulerLoop<WorkSource, Pass>,
    shutdown: Shutdown,
    grace_window: Duration,
) -> ShutdownOutcome
where
    WorkSource: EligibilityWorkSource,
    Pass: EligibilityPass + Clone + Send + 'static,
    WorkSource::Error: ClassifyOperatorFailure,
    Pass::Error: ClassifyOperatorFailure + Send + 'static,
    Shutdown: Future<Output = SchedulerStopCause>,
{
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    let scheduler_run = scheduler.run_until(async move {
        let _ = shutdown_receiver.await;
    });
    pin!(scheduler_run);
    pin!(shutdown);

    let stop_cause = select! {
        stop_cause = &mut shutdown => stop_cause,
        _ = &mut scheduler_run => {
            return ShutdownOutcome::SignalListenerFailed;
        }
    };
    let _ = shutdown_sender.send(());

    match (stop_cause, timeout(grace_window, &mut scheduler_run).await) {
        (SchedulerStopCause::SignalListenerFailed, _) => ShutdownOutcome::SignalListenerFailed,
        (SchedulerStopCause::ExecutionFailed, Ok(_)) => ShutdownOutcome::ExecutionFailed,
        (SchedulerStopCause::ExecutionFailed, Err(_)) => {
            ShutdownOutcome::ExecutionFailedAfterGraceWindow
        }
        (SchedulerStopCause::Requested, Ok(_)) => ShutdownOutcome::Clean,
        (SchedulerStopCause::Requested, Err(_)) => ShutdownOutcome::GraceWindowExpired,
    }
}

async fn wait_for_guard_loss(database: &mut FencedHubDatabase) {
    loop {
        if database.check_guard().await.is_err() {
            return;
        }
        sleep(GUARD_CHECK_INTERVAL).await;
    }
}

async fn monitor_runtime_guard(database: &mut FencedHubDatabase, ready: oneshot::Sender<()>) {
    if database.check_guard().await.is_err() {
        return;
    }
    let _ = ready.send(());
    wait_for_guard_loss(database).await;
}

async fn run_fenced_pool_floor_reconciliation(
    pool: sqlx::PgPool,
    policy: FencedPoolFloorReconciliationPolicy,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
                continue;
            }
            () = sleep(policy.interval) => {}
        }
        let prior_size = pool.size();
        if prior_size >= policy.minimum {
            continue;
        }
        let attempt = timeout(
            policy.attempt_bound,
            reconcile_fenced_pool_floor(&pool, policy.minimum),
        );
        let outcome = select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
                continue;
            }
            outcome = attempt => outcome,
        };
        let current_size = pool.size();
        match outcome {
            Ok(Ok(FencedPoolFloorReconciliation::Replenished)) => tracing::info!(
                prior_size,
                current_size,
                minimum = policy.minimum,
                "fenced pool floor reconciliation added one physical session"
            ),
            Ok(Ok(
                FencedPoolFloorReconciliation::Satisfied
                | FencedPoolFloorReconciliation::DeferredForIdleCapacity,
            )) => {}
            Ok(Err(_)) => tracing::warn!(
                failure_class = ?OperatorFailureClass::Infrastructure { commit_ambiguous: false },
                cause_code = "fenced_pool_floor_reconciliation_failed",
                prior_size,
                current_size,
                minimum = policy.minimum,
                "fenced pool floor reconciliation will retry"
            ),
            Err(_) => tracing::warn!(
                failure_class = ?OperatorFailureClass::Infrastructure { commit_ambiguous: false },
                cause_code = "fenced_pool_floor_reconciliation_timed_out",
                prior_size,
                current_size,
                minimum = policy.minimum,
                attempt_bound_seconds = policy.attempt_bound.as_secs(),
                "fenced pool floor reconciliation will retry"
            ),
        }
    }
}

enum GuardedAwait<T> {
    Completed(T),
    GuardLost,
}

async fn await_while_guarded<T>(
    database: &mut FencedHubDatabase,
    operation: impl Future<Output = T>,
) -> GuardedAwait<T> {
    let guard_loss = wait_for_guard_loss(database);
    pin!(guard_loss);
    pin!(operation);
    select! {
        biased;
        () = &mut guard_loss => GuardedAwait::GuardLost,
        output = &mut operation => GuardedAwait::Completed(output),
    }
}

async fn disarm_staging_sweep_unless_guarded(
    database: &mut FencedHubDatabase,
    registry: &mut Option<Arc<BlobStoreRegistry>>,
) {
    if database.check_guard().await.is_err()
        && let Some(registry) = registry.as_mut()
    {
        registry.disarm_staging_sweep();
    }
}

/// Derives the shared operator class from one content-free runtime variant.
///
/// Nested error values are inspected only by variant; database, protocol, socket,
/// I/O, and join-error prose is never formatted into the classification.
fn process_runtime_failure_class(error: &ProcessRuntimeError) -> OperatorFailureClass {
    use signalbox_persistence::outbox::OutboxDispatchError;
    use signalbox_persistence::runner_protocol::{RunnerProtocolStoreError, RunnerRecoveryError};

    match error {
        ProcessRuntimeError::RunnerRecoveryCommands(RunnerRecoveryError::Store(
            RunnerProtocolStoreError::CommitAmbiguous(_),
        )) => OperatorFailureClass::Infrastructure {
            commit_ambiguous: true,
        },
        ProcessRuntimeError::RunnerRecoveryCommands(RunnerRecoveryError::Store(
            RunnerProtocolStoreError::Corruption(_),
        )) => OperatorFailureClass::FailClosedCorruption,
        ProcessRuntimeError::OauthRecovery(error) => OperatorFailureClass::Infrastructure {
            commit_ambiguous: matches!(error, signalbox_persistence::oauth_credential::OauthCredentialRepositoryError::CommitAmbiguous),
        },
        ProcessRuntimeError::Accept(_)
        | ProcessRuntimeError::SpoolIo(_)
        | ProcessRuntimeError::InsufficientPoolCapacity
        | ProcessRuntimeError::CleanupSocket(_)
        | ProcessRuntimeError::DatabaseNotifications(_)
        | ProcessRuntimeError::RunnerRecoveryCommands(_)
        | ProcessRuntimeError::Dispatch(OutboxDispatchError::Database(_)) => {
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            }
        }
        ProcessRuntimeError::Dispatch(OutboxDispatchError::Corruption(_)) => {
            OperatorFailureClass::FailClosedCorruption
        }
        ProcessRuntimeError::Encode(_)
        | ProcessRuntimeError::EncodeInvariant
        | ProcessRuntimeError::InboundFrameBudgetClosed
        | ProcessRuntimeError::ImportBudgetClosed
        | ProcessRuntimeError::ReviewCommandBudgetClosed
        | ProcessRuntimeError::SnapshotReaderBudgetClosed
        | ProcessRuntimeError::ConnectionTask(_)
        | ProcessRuntimeError::UnexpectedDispatcherRetry => OperatorFailureClass::CallerOrHubBug,
    }
}

/// Records a fatal local-process runtime error before supervision erases it.
///
/// `ProcessRuntimeError::Display` is deliberately content-free across all
/// thirteen variants: it names only the failed runtime stage and never renders
/// nested I/O, wire, database, socket, credential, or request detail.
fn report_process_runtime_failure(error: &ProcessRuntimeError) {
    tracing::error!(
        phase = ?RuntimePhase::Runtime,
        failure_class = ?process_runtime_failure_class(error),
        cause = %error,
        "local process runtime failed"
    );
}

fn runner_runtime_failure_class(error: &RunnerProtocolRuntimeError) -> OperatorFailureClass {
    match error {
        RunnerProtocolRuntimeError::Accept(_)
        | RunnerProtocolRuntimeError::Cleanup(_)
        | RunnerProtocolRuntimeError::Read(_)
        | RunnerProtocolRuntimeError::Write(_)
        | RunnerProtocolRuntimeError::Closed => OperatorFailureClass::Infrastructure {
            commit_ambiguous: false,
        },
        RunnerProtocolRuntimeError::Lifecycle(error) => {
            runner_lifecycle_failure_class(error.cause())
        }
        RunnerProtocolRuntimeError::ConnectionDrainTimeout {
            initiating: Some(error),
            ..
        } => runner_runtime_failure_class(error),
        RunnerProtocolRuntimeError::ConnectionDrainTimeout {
            initiating: None, ..
        } => OperatorFailureClass::Infrastructure {
            commit_ambiguous: false,
        },
        RunnerProtocolRuntimeError::Decode(_)
        | RunnerProtocolRuntimeError::Encode(_)
        | RunnerProtocolRuntimeError::HandshakeTimeout
        | RunnerProtocolRuntimeError::OwnershipUnavailable
        | RunnerProtocolRuntimeError::HeartbeatSequenceExhausted
        | RunnerProtocolRuntimeError::ConnectionTask(_) => OperatorFailureClass::CallerOrHubBug,
    }
}

fn runner_lifecycle_failure_class(cause: RunnerRegistrationFailureCause) -> OperatorFailureClass {
    match cause {
        RunnerRegistrationFailureCause::Database => OperatorFailureClass::Infrastructure {
            commit_ambiguous: false,
        },
        RunnerRegistrationFailureCause::CommitAmbiguous => OperatorFailureClass::Infrastructure {
            commit_ambiguous: true,
        },
        RunnerRegistrationFailureCause::Corruption => OperatorFailureClass::FailClosedCorruption,
        RunnerRegistrationFailureCause::PeerInput
        | RunnerRegistrationFailureCause::EnrollmentAuthority
        | RunnerRegistrationFailureCause::Policy => OperatorFailureClass::CallerOrHubBug,
    }
}

fn report_runner_runtime_failure(error: &RunnerProtocolRuntimeError) {
    tracing::error!(
        phase = ?RuntimePhase::Runtime,
        failure_class = ?runner_runtime_failure_class(error),
        cause = %error,
        "runner protocol runtime failed"
    );
}

fn report_web_http_runtime_failure(error: &WebHttpRuntimeError) {
    tracing::error!(
        phase = ?RuntimePhase::Runtime,
        failure_class = ?OperatorFailureClass::Infrastructure { commit_ambiguous: false },
        cause = %error,
        "browser HTTP runtime failed"
    );
}

/// Records an unexpected top-level task state using closed evidence only.
///
/// The cause names the task-control condition without formatting `JoinError`,
/// whose panic payload is not admitted to operator telemetry.
fn report_runtime_task_defect(cause: RuntimeTaskDefect) {
    tracing::error!(
        phase = ?RuntimePhase::Runtime,
        failure_class = ?OperatorFailureClass::CallerOrHubBug,
        cause_code = cause.cause_code(),
        "daemon runtime task violated its lifecycle contract"
    );
}

fn joined_task_defect(error: &JoinError) -> RuntimeTaskDefect {
    if error.is_cancelled() {
        RuntimeTaskDefect::TaskCancelled
    } else if error.is_panic() {
        RuntimeTaskDefect::TaskPanicked
    } else {
        RuntimeTaskDefect::TaskJoinFailed
    }
}

fn runtime_task_completion(completed: Result<RuntimeTaskExit, JoinError>) -> RuntimeTaskCompletion {
    match completed {
        Ok(RuntimeTaskExit::Scheduler(SchedulerLoopExit::Shutdown))
        | Ok(RuntimeTaskExit::FencedPoolFloor)
        | Ok(RuntimeTaskExit::CredentialInvocations)
        | Ok(RuntimeTaskExit::Process(Ok(())))
        | Ok(RuntimeTaskExit::Runner(Ok(())))
        | Ok(RuntimeTaskExit::RepositoryWatch(Ok(())))
        | Ok(RuntimeTaskExit::WebHttp(Ok(())))
        | Ok(RuntimeTaskExit::Workflows(Ok(())))
        | Ok(RuntimeTaskExit::TurnLiveness)
        | Ok(RuntimeTaskExit::LifecycleDeadline)
        | Ok(RuntimeTaskExit::LifecycleMetrics)
        | Ok(RuntimeTaskExit::SessionSupervision) => RuntimeTaskCompletion::Clean,
        Ok(RuntimeTaskExit::Process(Err(error))) => {
            report_process_runtime_failure(&error);
            RuntimeTaskCompletion::Failed
        }
        Ok(RuntimeTaskExit::Runner(Err(error))) => {
            report_runner_runtime_failure(&error);
            RuntimeTaskCompletion::Failed
        }
        Ok(RuntimeTaskExit::RepositoryWatch(Err(error))) => {
            tracing::error!(?error, "repository-watch runtime failed");
            RuntimeTaskCompletion::Failed
        }
        Ok(RuntimeTaskExit::Workflows(Err(error))) => {
            tracing::error!(cause = error.cause_code(), "workflow runtime failed");
            RuntimeTaskCompletion::Failed
        }
        Ok(RuntimeTaskExit::WebHttp(Err(error))) => {
            report_web_http_runtime_failure(&error);
            RuntimeTaskCompletion::Failed
        }
        Err(error) => {
            report_runtime_task_defect(joined_task_defect(&error));
            RuntimeTaskCompletion::Defect
        }
    }
}

/// Drains runtime tasks without losing failures observed before cancellation.
///
/// The completion accumulator lives outside the timeout-cancelled future, so a
/// task defect or failure already reduced to a closed class survives when a
/// different task exhausts the grace window. No task error payload is retained.
async fn drain_runtime_tasks<GuardLoss, Interrupt>(
    runtime_tasks: &mut JoinSet<RuntimeTaskExit>,
    guard_loss: GuardLoss,
    interrupt: Interrupt,
    grace_window: Option<Duration>,
) -> (RuntimeDrainOutcome, RuntimeTaskCompletion)
where
    GuardLoss: Future<Output = ()>,
    Interrupt: Future<Output = ()>,
{
    let completion = Cell::new(RuntimeTaskCompletion::Clean);
    let drain = async {
        while let Some(completed) = runtime_tasks.join_next().await {
            completion.set(completion.get().combine(runtime_task_completion(completed)));
        }
    };
    tokio::pin!(drain);
    let outcome = match grace_window {
        Some(grace_window) => select! {
            () = guard_loss => RuntimeDrainOutcome::GuardLost,
            () = interrupt => RuntimeDrainOutcome::Interrupted,
            result = timeout(grace_window, &mut drain) => match result {
                Ok(()) => RuntimeDrainOutcome::Complete,
                Err(_) => RuntimeDrainOutcome::GraceWindowExpired,
            }
        },
        None => select! {
            () = guard_loss => RuntimeDrainOutcome::GuardLost,
            () = interrupt => RuntimeDrainOutcome::Interrupted,
            () = &mut drain => RuntimeDrainOutcome::Complete,
        },
    };
    (outcome, completion.get())
}

const fn completed_runtime_outcome(
    cause: RuntimeStopCause,
    drain: RuntimeDrainOutcome,
) -> ShutdownOutcome {
    match (cause, drain) {
        (_, RuntimeDrainOutcome::GuardLost) | (RuntimeStopCause::GuardLost, _) => {
            ShutdownOutcome::GuardLost
        }
        (RuntimeStopCause::Requested, RuntimeDrainOutcome::Complete) => ShutdownOutcome::Clean,
        (RuntimeStopCause::Requested, RuntimeDrainOutcome::Interrupted) => {
            ShutdownOutcome::Interrupted
        }
        (RuntimeStopCause::Requested, RuntimeDrainOutcome::GraceWindowExpired) => {
            ShutdownOutcome::GraceWindowExpired
        }
        (RuntimeStopCause::SignalListenerFailed, _) => ShutdownOutcome::SignalListenerFailed,
        (
            RuntimeStopCause::ExecutionFailed,
            RuntimeDrainOutcome::Complete | RuntimeDrainOutcome::Interrupted,
        ) => ShutdownOutcome::ExecutionFailed,
        (RuntimeStopCause::ExecutionFailed, RuntimeDrainOutcome::GraceWindowExpired) => {
            ShutdownOutcome::ExecutionFailedAfterGraceWindow
        }
        (
            RuntimeStopCause::RuntimeFailed,
            RuntimeDrainOutcome::Complete | RuntimeDrainOutcome::Interrupted,
        ) => ShutdownOutcome::RuntimeFailed,
        (RuntimeStopCause::RuntimeFailed, RuntimeDrainOutcome::GraceWindowExpired) => {
            ShutdownOutcome::RuntimeFailedAfterGraceWindow
        }
        (
            RuntimeStopCause::RuntimeDefect,
            RuntimeDrainOutcome::Complete | RuntimeDrainOutcome::Interrupted,
        ) => ShutdownOutcome::RuntimeDefect,
        (RuntimeStopCause::RuntimeDefect, RuntimeDrainOutcome::GraceWindowExpired) => {
            ShutdownOutcome::RuntimeDefectAfterGraceWindow
        }
    }
}

struct TerminationSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

impl TerminationSignals {
    fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    async fn recv(&mut self) -> bool {
        select! {
            result = self.interrupt.recv() => result.is_none(),
            result = self.terminate.recv() => result.is_none(),
        }
    }
}

async fn shutdown_requested(signals: &mut std::io::Result<TerminationSignals>) -> bool {
    match signals {
        Ok(signals) => signals.recv().await,
        Err(_) => true,
    }
}

async fn initialize_prometheus(
    configuration: &TelemetryConfiguration,
) -> Option<(TelemetryMetrics, PrometheusServer)> {
    let address = configuration.prometheus_bind()?;
    let metrics = match TelemetryMetrics::new() {
        Ok(metrics) => metrics,
        Err(error) => {
            tracing::warn!(
                target: "signalbox_telemetry_internal",
                setting = error.setting(),
                failure = ?error.failure(),
                "Prometheus metrics were disabled after registry construction failed"
            );
            return None;
        }
    };
    match PrometheusServer::bind(address, metrics.clone()).await {
        Ok(server) => {
            tracing::info!(
                target: "signalbox_telemetry_internal",
                "Prometheus scrape listener enabled"
            );
            Some((metrics, server))
        }
        Err(_) => {
            tracing::warn!(
                target: "signalbox_telemetry_internal",
                cause_code = "prometheus_bind_failed",
                "Prometheus metrics were disabled after the scrape socket could not be bound"
            );
            None
        }
    }
}

async fn run_hub(
    telemetry_configuration: &TelemetryConfiguration,
) -> Result<ShutdownOutcome, HubRuntimeError> {
    use signalboxd::guard_recovery::GuardRecoveryPolicy;
    let configuration = HubConfiguration::from_environment().map_err(|error| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Configuration(&error),
        )
    })?;
    let on_disk = fs::read_to_string(configuration.model_configuration_file()).map_err(|_| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::ModelConfiguration(&HubModelConfigurationError::Read),
        )
    })?;
    let bounds = HubModelConfiguration::startup_numeric_bounds(&on_disk).map_err(|error| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::ModelConfiguration(&error),
        )
    })?;
    let policy = bounds
        .duration("guard_recovery_initial_delay")
        .flatten()
        .zip(bounds.duration("guard_recovery_maximum_delay").flatten())
        .and_then(|(initial, maximum)| {
            GuardRecoveryPolicy::new(
                initial,
                maximum,
                bounds.duration("guard_recovery_elapsed_bound").flatten(),
            )
        })
        .ok_or_else(|| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static("invalid_guard_recovery_backoff"),
            )
        })?;
    run_hub_recovery(
        policy,
        |observer| async move {
            recovery_incarnation_outcome(
                run_hub_incarnation(telemetry_configuration, observer.clone()).await,
                observer.is_recovering(),
            )
        },
        TerminationSignals::new(),
    )
    .await
}

async fn run_hub_recovery<Run, Incarnation>(
    policy: signalboxd::guard_recovery::GuardRecoveryPolicy,
    run: Run,
    mut recovery_signals: std::io::Result<TerminationSignals>,
) -> Result<ShutdownOutcome, HubRuntimeError>
where
    Run: FnMut(signalboxd::guard_recovery::GuardRecoveryObserver) -> Incarnation,
    Incarnation: std::future::Future<
            Output = signalboxd::guard_recovery::GuardedIncarnationOutcome<
                Result<ShutdownOutcome, HubRuntimeError>,
            >,
        >,
{
    use signalboxd::guard_recovery::{GuardRecoveryStop, run_guarded_incarnations};
    let mut listener_failed = false;
    match run_guarded_incarnations(policy, run, async {
        listener_failed = shutdown_requested(&mut recovery_signals).await;
        if listener_failed {
            tracing::error!("termination signal listener failed during guard recovery");
        }
    })
    .await
    {
        Ok(result) => result,
        Err(GuardRecoveryStop::ShutdownRequested) => Ok(if listener_failed {
            ShutdownOutcome::SignalListenerFailed
        } else {
            ShutdownOutcome::Clean
        }),
        Err(reason @ GuardRecoveryStop::ElapsedBoundExhausted) => {
            tracing::error!(?reason, "database guard recovery bound exhausted");
            Ok(ShutdownOutcome::GuardRecoveryExhausted)
        }
    }
}

fn recovery_incarnation_outcome(
    result: Result<ShutdownOutcome, HubRuntimeError>,
    recovering: bool,
) -> signalboxd::guard_recovery::GuardedIncarnationOutcome<Result<ShutdownOutcome, HubRuntimeError>>
{
    use signalboxd::guard_recovery::GuardedIncarnationOutcome;
    match result {
        Ok(ShutdownOutcome::GuardLost) => GuardedIncarnationOutcome::Reacquire,
        Err(error)
            if recovering
                && error.database_failure
                && matches!(
                    error.failure_class,
                    OperatorFailureClass::Infrastructure { .. }
                ) =>
        {
            GuardedIncarnationOutcome::Reacquire
        }
        result => GuardedIncarnationOutcome::Finished(result),
    }
}

fn reload_recovery_failure(
    failure: &signalbox_persistence::reload_configuration::ReloadRepositoryError,
) -> HubRuntimeError {
    erase_startup_cause(
        RuntimePhase::StartupScan,
        SanitizedStartupCause::Reload(failure),
    )
}

fn startup_goal_resumption_result(
    result: Result<usize, signalboxd::PostgresGoalPassDispositionError>,
    recovering: bool,
) -> Result<(), HubRuntimeError> {
    match result {
        Ok(rearmed) => tracing::info!(
            phase = ?RuntimePhase::StartupScan,
            rearmed_goal_resumption_count = rearmed,
            "daemon startup reconciled automatic goal resumptions"
        ),
        Err(error) => {
            tracing::error!(
                phase = ?RuntimePhase::StartupScan,
                cause_code = error.operator_failure_cause_code(),
                cause = %error,
                "daemon startup exhausted automatic goal-resumption reconciliation"
            );
            let failure_class = error.operator_failure_class();
            if recovering && matches!(failure_class, OperatorFailureClass::Infrastructure { .. }) {
                return Err(HubRuntimeError::startup_scan(failure_class, None, None));
            }
        }
    }
    Ok(())
}

async fn run_hub_incarnation(
    telemetry_configuration: &TelemetryConfiguration,
    guard_recovery: signalboxd::guard_recovery::GuardRecoveryObserver,
) -> Result<ShutdownOutcome, HubRuntimeError> {
    let configuration = HubConfiguration::from_environment().map_err(|error| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Configuration(&error),
        )
    })?;
    let web_configuration = WebHttpConfiguration::from_environment().map_err(|error| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::WebHttpConfiguration(&error),
        )
    })?;
    let on_disk = fs::read_to_string(configuration.model_configuration_file()).map_err(|_| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::ModelConfiguration(&HubModelConfigurationError::Read),
        )
    })?;
    let bootstrap_bounds =
        HubModelConfiguration::startup_numeric_bounds(&on_disk).map_err(|error| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::ModelConfiguration(&error),
            )
        })?;
    let numeric_bounds = &bootstrap_bounds;
    let configured_duration = |field| numeric_bounds.duration(field).flatten();
    let configured_usize = |field| {
        numeric_bounds
            .integer(field)
            .flatten()
            .map(usize::try_from)
            .transpose()
            .map_err(|_| {
                erase_startup_cause(
                    RuntimePhase::Configuration,
                    SanitizedStartupCause::Static("configured_numeric_bound_exceeds_platform"),
                )
            })
    };
    let configured_u32 = |field| {
        numeric_bounds
            .integer(field)
            .flatten()
            .map(u32::try_from)
            .transpose()
            .map_err(|_| {
                erase_startup_cause(
                    RuntimePhase::Configuration,
                    SanitizedStartupCause::Static("configured_numeric_bound_exceeds_u32"),
                )
            })
    };
    let model_exchange_timeout = configured_duration("model_exchange_timeout");
    let codex_cli_version_probe_bound = configured_duration("codex_cli_version_probe_bound")
        .filter(|bound| !bound.is_zero())
        .ok_or_else(|| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static("invalid_codex_cli_version_probe_bound"),
            )
        })?;
    let fenced_pool_min_connections =
        validate_fenced_pool_min_connections(configured_u32("fenced_pool_min_connections")?)
            .ok_or_else(|| {
                erase_startup_cause(
                    RuntimePhase::Configuration,
                    SanitizedStartupCause::Static("invalid_fenced_pool_min_connections"),
                )
            })?;
    let fenced_pool_floor_reconciliation = fenced_pool_floor_reconciliation_policy(
        fenced_pool_min_connections,
        configured_duration("fenced_pool_floor_reconciliation_interval"),
        configured_duration("fenced_pool_floor_reconciliation_attempt_bound"),
    )
    .ok_or_else(|| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Static("invalid_fenced_pool_floor_reconciliation_policy"),
        )
    })?;
    let scheduler_pass_occupancy_bound = configured_duration("scheduler_pass_occupancy_bound")
        .map(SchedulerPassOccupancyBound::try_new)
        .transpose()
        .map_err(|_| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static("invalid_scheduler_pass_occupancy_bound"),
            )
        })?
        .unwrap_or_else(SchedulerPassOccupancyBound::unbounded);
    let shutdown_grace_window = graceful_shutdown_window(
        model_exchange_timeout,
        configured_duration("graceful_shutdown_cleanup_window"),
    );
    let stale_active_turn_bound = configured_duration("stale_active_turn_bound")
        .map(StaleActiveTurnBound::try_new)
        .transpose()
        .map_err(|_| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static("invalid_stale_active_turn_bound"),
            )
        })?;
    let turn_liveness_scan_interval = configured_duration("turn_liveness_scan_interval")
        .map(TurnLivenessScanInterval::try_new)
        .transpose()
        .map_err(|_| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static("invalid_turn_liveness_scan_interval"),
            )
        })?;
    let session_admission_deadline = configured_duration("session_admission_deadline");
    let session_waiting_deadline = configured_duration("session_waiting_deadline");
    if turn_liveness_scan_interval.is_none()
        && (session_admission_deadline.is_some() || session_waiting_deadline.is_some())
    {
        return Err(erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Static("session_deadlines_require_liveness_scan_interval"),
        ));
    }
    let reconciliation_sweep_interval = configured_duration("reconciliation_sweep_interval")
        .map(ReconciliationSweepInterval::try_new)
        .transpose()
        .map_err(|_| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static("invalid_reconciliation_sweep_interval"),
            )
        })?;
    let nudge_buffer_capacity = match configured_usize("nudge_buffer_capacity")? {
        Some(capacity) => Some(NonZeroUsize::new(capacity).ok_or_else(|| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static("invalid_nudge_buffer_capacity"),
            )
        })?),
        None => None,
    };
    let scheduler_pass_admission_cap = configured_usize("scheduler_pass_admission_cap")?;
    let automatic_reconciliation_attempt_budget = numeric_bounds
        .integer("automatic_reconciliation_attempt_budget")
        .flatten()
        .map(u32::try_from)
        .transpose()
        .map_err(|_| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static(
                    "automatic_reconciliation_attempt_budget_exceeds_platform",
                ),
            )
        })?;
    if automatic_reconciliation_attempt_budget.is_some_and(|budget| i32::try_from(budget).is_err())
    {
        return Err(erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Static(
                "automatic_reconciliation_attempt_budget_exceeds_storage",
            ),
        ));
    }
    // The claim statement schedules one `CASE` arm per admitted attempt and ends
    // in an `ELSE`, so a budget above that arity is admitted silently and then
    // reuses the last rung's deadline for every attempt past it while the
    // failure path schedules the true exponential. The claim side is the shorter
    // of the two, so the abandonment sweep would settle attempts that are still
    // running. Refusing the budget here keeps the arity a configuration fact
    // rather than something a deployment discovers from a mis-settled attempt.
    if automatic_reconciliation_attempt_budget.is_some_and(|budget| {
        usize::try_from(budget).is_ok_and(|budget| budget > RETRY_LADDER_ARITY)
    }) {
        return Err(erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Static(
                "automatic_reconciliation_attempt_budget_exceeds_retry_ladder",
            ),
        ));
    }
    let automatic_reconciliation_base_backoff =
        configured_duration("automatic_reconciliation_base_backoff");
    let automatic_reconciliation_backoff_cap =
        configured_duration("automatic_reconciliation_backoff_cap");
    let expired_pass_recovery_policy = ExpiredPassRecoveryPolicy::new(
        configured_u32("expired_pass_recovery_attempts")?,
        configured_duration("expired_pass_recovery_attempt_bound"),
        configured_duration("expired_pass_recovery_lock_retry_delay"),
        configured_duration("expired_pass_recovery_conservative_retry_delay"),
    );
    let convergence_sweep_numeric_bounds = ConvergenceSweepNumericBounds::new(
        configured_duration("convergence_sweep_request_timeout"),
        configured_usize("max_convergence_sweep_connection_pages")?,
        configured_usize("max_concurrent_convergence_sweep_targets")?,
        configured_usize("max_convergence_sweep_request_attempts")?,
        configured_duration("convergence_sweep_request_retry_delay"),
        configured_duration("convergence_sweep_retry_backoff_base"),
        configured_duration("convergence_sweep_retry_backoff_cap"),
    );
    let turn_liveness_persistence_bounds = TurnLivenessPersistenceBounds::new(
        configured_duration("terminalization_lock_wait"),
        configured_duration("terminalization_acquire_wait"),
        configured_duration("terminalization_write_lock_wait"),
    );
    let turn_liveness_numeric_bounds = TurnLivenessNumericBounds::new(
        configured_usize("terminalizations_per_liveness_scan")?,
        configured_duration("turn_liveness_recovery_attempt_bound"),
        configured_usize("automatic_reconciliations_per_liveness_scan")?,
        configured_duration("automatic_reconciliation_attempt_bound"),
        turn_liveness_persistence_bounds,
    );
    let goal_mode_numeric_bounds = GoalModeNumericBounds::new(
        configured_duration("automatic_resume_base_backoff"),
        configured_duration("automatic_resume_backoff_cap"),
        configured_u32("automatic_resume_attempt_budget")?,
        configured_u32("automatic_resume_attempt_ceiling")?,
        configured_duration("automatic_resume_startup_retry_delay"),
    );
    // Zero is never refresh, which is what `"none"` already means: a zero
    // period panics `tokio::time::interval`, and a spawned task's panic stops
    // the daemon.
    let lifecycle_metric_scan_interval =
        configured_duration("session_lifecycle_metric_scan_interval")
            .filter(|interval| !interval.is_zero());
    let diagnostic_model_identity_limit = configured_usize("diagnostic_model_identity_limit")?;
    let automatic_tool_round_limit = configured_usize("max_automatic_tool_rounds_per_turn")?;
    let same_credential_attempt_bound = configured_usize("max_same_credential_attempts_per_turn")?
        .map(|bound| {
            NonZeroUsize::new(bound).ok_or_else(|| {
                erase_startup_cause(
                    RuntimePhase::Configuration,
                    SanitizedStartupCause::Static("invalid_same_credential_attempt_bound"),
                )
            })
        })
        .transpose()?;
    let post_kill_reap_bound = configured_duration("post_kill_reap_bound");
    let native_message_limit = configured_usize("max_native_message_bytes")?;
    let code_host_numeric_bounds = CodeHostNumericBounds::new(
        configured_duration("code_host_request_timeout"),
        configured_usize("max_job_log_bytes")?,
        configured_usize("max_stack_comparisons_in_flight")?,
        configured_usize("max_code_host_result_text_bytes")?,
        configured_usize("max_code_host_result_items")?,
        configured_usize("max_repository_file_content_bytes")?,
    );
    let mut database = FencedHubDatabase::connect_production(
        configuration.database_url(),
        fenced_pool_min_connections,
        guard_recovery.clone(),
    )
    .await
    .map_err(|error| {
        let phase = match &error {
            FencedHubDatabaseError::InitializeFence(_) => RuntimePhase::Migration,
            FencedHubDatabaseError::ParseOptions(_)
            | FencedHubDatabaseError::ConnectBootstrap(_)
            | FencedHubDatabaseError::AcquireGuard(_)
            | FencedHubDatabaseError::GuardLost(_)
            | FencedHubDatabaseError::AdvanceFence(_)
            | FencedHubDatabaseError::ConnectFencedPool(_) => RuntimePhase::DatabaseConnection,
        };
        erase_startup_cause(phase, SanitizedStartupCause::Database(&error))
    })?;
    let pool = database.pool().clone();
    let fenced_pool_floor_pool = pool.clone();
    match await_while_guarded(&mut database, migrate_hub_database(&pool)).await {
        GuardedAwait::Completed(Ok(())) => {}
        GuardedAwait::Completed(Err(error)) => {
            return startup_failure_after_close(error, database.close().await);
        }
        GuardedAwait::GuardLost => {
            let _ = database.close().await;
            return Ok(ShutdownOutcome::GuardLost);
        }
    }
    let reload_repository =
        signalbox_persistence::reload_configuration::ReloadConfigurationRepository::new(
            pool.clone(),
        );
    let pending_reload = match await_while_guarded(&mut database, reload_repository.pending()).await
    {
        GuardedAwait::Completed(Ok(pending)) => pending,
        GuardedAwait::Completed(Err(error)) => {
            let failure = reload_recovery_failure(&error);
            return startup_failure_after_close(failure, database.close().await);
        }
        GuardedAwait::GuardLost => {
            let _ = database.close().await;
            return Ok(ShutdownOutcome::GuardLost);
        }
    };
    let retained_startup = pending_reload
        .first()
        .map(|(_, intent)| {
            signalboxd::configuration_reload::ConfigurationReload::startup_snapshot(
                &on_disk,
                &intent.replacement_snapshot,
            )
        })
        .transpose()
        .map_err(|_| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static("configuration_reload_snapshot_incompatible"),
            )
        })?;
    let model_configuration = match &retained_startup {
        Some(catalogs) => (*catalogs.models).clone(),
        None => HubModelConfiguration::parse(&on_disk).map_err(|error| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::ModelConfiguration(&error),
            )
        })?,
    };
    model_configuration
        .validate_credential_files()
        .map_err(|error| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Credential(&error),
            )
        })?;
    let integration_credentials = FileCredentialAccess::from_files([(
        CredentialReference::new(BRAVE_SEARCH_CREDENTIAL_REFERENCE),
        configuration.brave_api_key_file(),
    )]);
    integration_credentials.validate().map_err(|error| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Credential(&error),
        )
    })?;
    if let Some(codex_cli) = model_configuration.codex_cli() {
        verify_pinned_codex_cli_version(codex_cli.executable(), codex_cli_version_probe_bound)
            .await
            .map_err(|_| {
                erase_startup_cause(
                    RuntimePhase::Configuration,
                    SanitizedStartupCause::Static("codex_cli_version_probe_failed"),
                )
            })?;
    }
    let prometheus_runtime = initialize_prometheus(telemetry_configuration).await;
    if configuration.repository_watch_credential_conflicts(&model_configuration) {
        let error = HubConfigurationError::new(
            GITHUB_TOKEN_FILE_ENVIRONMENT,
            RequiredSettingFailure::Conflicts,
        );
        return Err(erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Configuration(&error),
        ));
    }
    let daemon_tool_configuration = model_configuration.daemon_tools();
    let template_configuration = match retained_startup {
        Some(catalogs) => (*catalogs.templates).clone(),
        None => SessionTemplateConfiguration::read(
            configuration.template_configuration_file(),
            || env::var_os("HOME").map(PathBuf::from),
            &model_configuration,
        )
        .map_err(|error| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::TemplateConfiguration(&error),
            )
        })?,
    };
    if let Some(repository_watch) = model_configuration.repository_watch() {
        repository_watch
            .validate_convergence_template(template_configuration.summaries().map(|(name, _)| name))
            .map_err(|error| {
                erase_startup_cause(
                    RuntimePhase::Configuration,
                    SanitizedStartupCause::ModelConfiguration(&error),
                )
            })?;
    }
    let mut runtime_factory = signalboxd::model_catalog_runtime::ModelRuntimeFactory::new(
        model_exchange_timeout,
        post_kill_reap_bound,
        native_message_limit,
    );
    runtime_factory
        .build(&model_configuration)
        .map_err(|error| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static(error.cause_code()),
            )
        })?;
    let credential_reference =
        ModelCallCredentialReference::new(model_configuration.fallback_credential_profile());
    let code_host_reference = CredentialReference::new(CODE_HOST_CREDENTIAL_REFERENCE);
    let code_host_credentials = match model_configuration
        .github_credential_profile(CODE_HOST_CREDENTIAL_REFERENCE)
    {
        Some(profile) => FileCredentialAccess::from_github(profile, code_host_reference)
            .with_request_timeout(configured_duration("code_host_request_timeout")),
        None => FileCredentialAccess::new(configuration.github_token_file(), code_host_reference),
    };
    code_host_credentials.validate().map_err(|error| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Credential(&error),
        )
    })?;
    let web_search_credentials = FileCredentialAccess::new(
        configuration.brave_api_key_file(),
        CredentialReference::new(BRAVE_SEARCH_CREDENTIAL_REFERENCE),
    );
    let code_host_transport = GitHubCodeHostTransport::try_new(code_host_numeric_bounds)
        .map_err(|_| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static("github_transport_construction_failed"),
            )
        })?
        .with_convergence_policy(model_configuration.convergence().cloned())
        .with_app(code_host_credentials.github_app());
    let oauth_registrations = model_configuration.oauth_registrations();
    let root_path = oauth_credential_root(configuration.process_socket_path());
    let retained_root = match root_path.symlink_metadata() {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => {
            return Err(erase_startup_cause(
                RuntimePhase::StartupScan,
                SanitizedStartupCause::Static("oauth_credential_home_recovery_failed"),
            ));
        }
    };
    let oauth_service = if !oauth_registrations.is_empty() || retained_root {
        let root = signalbox_model_runtime_codex_cli::OauthCredentialRoot::open(&root_path)
            .map_err(|_| {
                erase_startup_cause(
                    RuntimePhase::StartupScan,
                    SanitizedStartupCause::Static("oauth_credential_home_recovery_failed"),
                )
            })?;
        let service = Arc::new(
            signalboxd::OauthCredentialService::new(pool.clone(), oauth_registrations).map_err(
                |_| {
                    erase_startup_cause(
                        RuntimePhase::Configuration,
                        SanitizedStartupCause::Static("oauth_delivery_construction_failed"),
                    )
                },
            )?,
        );
        runtime_factory = runtime_factory.with_oauth_delivery(service.clone(), root);
        Some(service)
    } else {
        None
    };
    let scheduler_pool = pool.clone();
    let sweep = PostgresEligibilitySweep::new(scheduler_pool.clone());
    let (eligibility_nudge, work_source) = InProcessEligibilityWorkSource::with_options(
        sweep,
        reconciliation_sweep_interval,
        nudge_buffer_capacity,
    );
    let approval_wait_wakeups = signalboxd::ApprovalWaitWakeups::new(
        signalbox_persistence::tool_loop::PostgresToolLoopRepository::new(pool.clone()),
        eligibility_nudge.clone(),
    );
    let invocation_processes =
        signalboxd::credential_invocations::CredentialInvocationProcesses::new(
            pool.clone(),
            eligibility_nudge.clone(),
        );

    let capacity_refresh = signalboxd::credential_invocations::CodexCapacityRefresh::new(
        pool.clone(),
        &model_configuration,
        reconciliation_sweep_interval,
        codex_cli_version_probe_bound,
    )
    .map_err(|_| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Static("codex_capacity_refresh_construction_failed"),
        )
    })?;

    let image_derivative_supervisor = daemon_tool_configuration
        .as_ref()
        .map(|configuration| configuration.exec_supervisor_executable().to_path_buf());
    let tool_composition = match daemon_tool_configuration {
        Some(_) => signalboxd::DaemonToolComposition::WithMappedFamilies,
        None => signalboxd::DaemonToolComposition::Base,
    };
    let tools = match daemon_tool_configuration {
        Some(tool_configuration) => DaemonTools::try_new_production(
            SystemCurrentTimeClock,
            pool.clone(),
            eligibility_nudge.clone(),
            MappedDaemonCredentialInputs {
                web_search: web_search_credentials,
                code_host: code_host_credentials.clone(),
                github: code_host_credentials,
            },
            code_host_transport,
            tool_configuration.github_egress_policy(),
            tool_configuration.workspace_root(),
            tool_configuration.git_identity().clone(),
            tool_configuration.exec_supervisor_executable(),
            tool_configuration.cargo_registry_cache(),
            tool_configuration.sandbox(),
            model_configuration
                .numeric_bounds()
                .integer("max_git_object_bytes")
                .flatten()
                .map(|bytes| bytes as usize),
            tool_configuration.sandboxed_exec_timeout_bound(),
            model_configuration.web_fetch_egress_policy(),
        ),
        None => DaemonTools::try_new_without_tool_mappings(
            SystemCurrentTimeClock,
            pool.clone(),
            eligibility_nudge.clone(),
            BaseDaemonCredentialInputs {
                web_search: web_search_credentials,
                code_host: code_host_credentials,
            },
            code_host_transport,
            model_configuration.web_fetch_egress_policy(),
        ),
    };
    let tools = match tools {
        Ok(tools) => tools,
        Err(error) => {
            let failure = erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Tools(&error),
            );
            return startup_failure_after_close(failure, database.close().await);
        }
    };
    let workspace_instruction_runtime = WorkspaceInstructionRuntime::new(
        pool.clone(),
        tools.workspace_instruction_root_resolver(),
        model_configuration
            .workspace_instructions()
            .roots()
            .to_vec(),
    )
    .with_discovery_limits(model_configuration.workspace_instructions().limits());
    let checkout_runner = tools.process_runner();
    let (mut tool_catalog, mut tool_executor) = tools.into_parts();

    let runner_service = match PostgresRunnerRegistrationService::registration_only(pool.clone()) {
        Ok(service) => service,
        Err(_) => {
            let failure = erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static("runner_catalog_construction_failed"),
            );
            return startup_failure_after_close(failure, database.close().await);
        }
    };
    let scan_runner_service = runner_service.clone();
    let migration_oauth_registrations = model_configuration.oauth_registrations();
    let invocation_registrations = model_configuration.credential_invocation_registrations();
    let scan_pool = pool.clone();
    let scan_approval_wait_wakeups = approval_wait_wakeups.clone();
    let startup = migrate_scan_then_schedule(
        async {
            install_oauth_registrations(&pool, &migration_oauth_registrations).await?;
            invocation_processes.recover().await.map_err(|_| {
                erase_startup_database_cause(
                    RuntimePhase::StartupScan,
                    SanitizedStartupCause::Static("credential_invocation_recovery_failed"),
                )
            })?;
            signalbox_persistence::credential_invocations::replace_registrations(
                &pool,
                &invocation_registrations,
            )
            .await
            .map_err(|_| {
                erase_startup_database_cause(
                    RuntimePhase::StartupScan,
                    SanitizedStartupCause::Static("credential_capacity_registration_failed"),
                )
            })
        },
        async move {
            scan_runner_service
                .mark_orphaned_connections_lost()
                .await
                .map_err(|_| {
                    erase_startup_database_cause(
                        RuntimePhase::StartupScan,
                        SanitizedStartupCause::Static("runner_connection_reconciliation_failed"),
                    )
                })?;
            let mut scan = StartupScanService::new(
                UuidV7StartupScanIdGenerator,
                PostgresStartupScanRepository::new(scan_pool),
            );
            let outcome = scan.execute().await.map_err(|error| {
                let failure_class = error.operator_failure_class();
                let cause_code = error.operator_failure_cause_code();
                let session = error.session();
                let turn = error.repository_error().corruption_turn();
                erase_startup_scan_cause(failure_class, cause_code, session, turn)
            })?;
            scan_runner_service
                .recovery_store()
                .resume_runner_replacements()
                .await
                .map_err(|_| {
                    erase_startup_database_cause(
                        RuntimePhase::StartupScan,
                        SanitizedStartupCause::Static("runner_replacement_recovery_failed"),
                    )
                })?;
            scan_approval_wait_wakeups
                .refresh(None)
                .await
                .map_err(|_| {
                    erase_startup_database_cause(
                        RuntimePhase::StartupScan,
                        SanitizedStartupCause::Static("approval_wait_deadline_restore_failed"),
                    )
                })?;
            tracing::info!(
                phase = ?RuntimePhase::StartupScan,
                recovered_turn_count = outcome.recovered_turn_count(),
                awaiting_recovery_decision_session_count =
                    outcome.awaiting_recovery_decision_sessions().len(),
                "daemon startup phase completed"
            );
            for session in outcome.skipped_corrupt_sessions() {
                tracing::error!(
                    session = %session.as_uuid(),
                    cause = "durable_state_corruption",
                    "startup skipped corrupt session; durable operator item recorded"
                );
            }
            for session in outcome.awaiting_recovery_decision_sessions() {
                tracing::warn!(
                    phase = ?RuntimePhase::StartupScan,
                    session = %session.into_uuid(),
                    "session holds its slot awaiting a durable recovery decision"
                );
            }
            Ok(())
        },
        || std::future::ready(()),
    );
    match await_while_guarded(&mut database, startup).await {
        GuardedAwait::Completed(Ok(())) => {}
        GuardedAwait::Completed(Err(error)) => {
            return startup_failure_after_close(error, database.close().await);
        }
        GuardedAwait::GuardLost => {
            let _ = database.close().await;
            return Ok(ShutdownOutcome::GuardLost);
        }
    }
    let blob_store_registry = match await_while_guarded(
        &mut database,
        BlobStoreRegistry::initialize(model_configuration.blob_storage(), pool.clone()),
    )
    .await
    {
        GuardedAwait::Completed(Ok(registry)) => registry,
        GuardedAwait::Completed(Err(error)) => {
            let failure = erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::BlobStorage(&error),
            );
            return startup_failure_after_close(failure, database.close().await);
        }
        GuardedAwait::GuardLost => {
            let _ = database.close().await;
            return Ok(ShutdownOutcome::GuardLost);
        }
    };
    let mut blob_store_registry = blob_store_registry.map(Arc::new);
    // The family is model-facing only where blob storage exists: an absent
    // registry means no configuration and an empty catalog, so advertising
    // `blob_metadata` and `blob_read` would declare tools no request can use.
    let mut blob_executor = None;
    if blob_store_registry.is_some() {
        let blob_tools = match BlobTools::try_new(
            BlobCatalogRepository::new(pool.clone()),
            blob_store_registry.clone(),
        ) {
            Ok(tools) => tools,
            Err(_) => {
                let failure = erase_startup_cause(
                    RuntimePhase::Configuration,
                    SanitizedStartupCause::Static("blob_read_tool_construction_failed"),
                );
                drop(blob_store_registry);
                return startup_failure_after_close(failure, database.close().await);
            }
        };
        let (blob_catalog, executor) = blob_tools.into_parts();
        tool_catalog = match tool_catalog.with_compiled_catalog(blob_catalog) {
            Ok(catalog) => catalog,
            Err(_) => {
                let failure = erase_startup_cause(
                    RuntimePhase::Configuration,
                    SanitizedStartupCause::Static("blob_read_tool_catalog_conflict"),
                );
                drop(executor);
                drop(blob_store_registry);
                return startup_failure_after_close(failure, database.close().await);
            }
        };
        blob_executor = Some(executor);
    }
    let file_media_executor = if model_configuration.file_media() {
        let Some(stores) = blob_store_registry.as_ref() else {
            return startup_failure_after_close(
                erase_startup_cause(
                    RuntimePhase::Configuration,
                    SanitizedStartupCause::Static("file_media_requires_blob_storage"),
                ),
                database.close().await,
            );
        };
        let composed = await_while_guarded(
            &mut database,
            signalboxd::DaemonFileMediaExecutor::compose(pool.clone(), Arc::clone(stores)),
        )
        .await;
        match composed {
            GuardedAwait::Completed(Ok((catalog, executor))) => {
                tool_catalog = match tool_catalog.with_compiled_catalog(catalog) {
                    Ok(catalog) => catalog,
                    Err(_) => {
                        return startup_failure_after_close(
                            erase_startup_cause(
                                RuntimePhase::Configuration,
                                SanitizedStartupCause::Static("file_media_catalog_conflict"),
                            ),
                            database.close().await,
                        );
                    }
                };
                Some(executor.with_model_configuration(&model_configuration))
            }
            GuardedAwait::GuardLost => {
                disarm_staging_sweep_unless_guarded(&mut database, &mut blob_store_registry).await;
                let _ = database.close().await;
                return Ok(ShutdownOutcome::GuardLost);
            }
            GuardedAwait::Completed(Err(_)) => {
                return startup_failure_after_close(
                    erase_startup_cause(
                        RuntimePhase::Configuration,
                        SanitizedStartupCause::Static("file_media_worker_unavailable"),
                    ),
                    database.close().await,
                );
            }
        }
    } else {
        None
    };
    let runner_listener = match LocalProcessListener::bind(configuration.runner_socket_path()) {
        Ok(listener) => listener,
        Err(error) => {
            let failure = erase_startup_cause(
                RuntimePhase::SocketBinding,
                SanitizedStartupCause::Socket(&error),
            );
            drop(blob_executor);
            disarm_staging_sweep_unless_guarded(&mut database, &mut blob_store_registry).await;
            drop(blob_store_registry);
            return startup_failure_after_close(failure, database.close().await);
        }
    };
    let listener = match LocalProcessListener::bind(configuration.process_socket_path()) {
        Ok(listener) => listener,
        Err(error) => {
            let failure = erase_startup_cause(
                RuntimePhase::SocketBinding,
                SanitizedStartupCause::Socket(&error),
            );
            let _ = runner_listener.cleanup();
            drop(blob_executor);
            disarm_staging_sweep_unless_guarded(&mut database, &mut blob_store_registry).await;
            drop(blob_store_registry);
            return startup_failure_after_close(failure, database.close().await);
        }
    };
    let snapshot_reader_budget = match signalboxd::shared_snapshot_reader_budget(
        pool.options().get_max_connections(),
        Some(&model_configuration),
    ) {
        Some(budget) => budget,
        None => {
            let failure = erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static("insufficient_snapshot_reader_pool_capacity"),
            );
            let _ = listener.cleanup();
            let _ = runner_listener.cleanup();
            disarm_staging_sweep_unless_guarded(&mut database, &mut blob_store_registry).await;
            drop(blob_store_registry);
            return startup_failure_after_close(failure, database.close().await);
        }
    };
    let web_blob_runtime = match blob_store_registry.as_ref() {
        Some(registry) => {
            let worker_program = match std::env::current_exe() {
                Ok(path) => path,
                Err(_) => {
                    let failure = erase_startup_cause(
                        RuntimePhase::Configuration,
                        SanitizedStartupCause::Static("web_blob_worker_path_failed"),
                    );
                    let _ = listener.cleanup();
                    let _ = runner_listener.cleanup();
                    drop(blob_executor);
                    disarm_staging_sweep_unless_guarded(&mut database, &mut blob_store_registry)
                        .await;
                    drop(blob_store_registry);
                    return startup_failure_after_close(failure, database.close().await);
                }
            };
            match WebBlobRuntime::new(
                pool.clone(),
                registry.clone(),
                image_derivative_supervisor,
                worker_program,
            ) {
                Ok(runtime) => Some(runtime),
                Err(_) => {
                    let failure = erase_startup_cause(
                        RuntimePhase::Configuration,
                        SanitizedStartupCause::Static("web_blob_runtime_construction_failed"),
                    );
                    let _ = listener.cleanup();
                    let _ = runner_listener.cleanup();
                    drop(blob_executor);
                    disarm_staging_sweep_unless_guarded(&mut database, &mut blob_store_registry)
                        .await;
                    drop(blob_store_registry);
                    return startup_failure_after_close(failure, database.close().await);
                }
            }
        }
        None => None,
    };
    let web_http_listener = match WebHttpRuntime::bind_listener_with_snapshot_reader_budget(
        web_configuration,
        pool.clone(),
        web_blob_runtime,
        model_configuration.clone(),
        blob_store_registry.clone(),
        Arc::clone(&snapshot_reader_budget),
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(_) => {
            let failure = erase_startup_cause(
                RuntimePhase::SocketBinding,
                SanitizedStartupCause::Static("web_http_listener_bind_failed"),
            );
            let _ = listener.cleanup();
            let _ = runner_listener.cleanup();
            drop(blob_executor);
            disarm_staging_sweep_unless_guarded(&mut database, &mut blob_store_registry).await;
            drop(blob_store_registry);
            return startup_failure_after_close(failure, database.close().await);
        }
    };
    tracing::info!(
        phase = ?RuntimePhase::SocketBinding,
        "daemon startup phase completed"
    );
    let runner_service = runner_service.with_eligibility_nudge(eligibility_nudge.clone());
    let tool_dispatch_gate = InProcessToolDispatchGate::default();
    let configuration_reload = signalboxd::configuration_reload::ConfigurationReload::new(
        scheduler_pool.clone(),
        model_configuration.clone(),
        template_configuration.clone(),
        configuration.model_configuration_file().to_path_buf(),
        configuration.template_configuration_file().to_path_buf(),
        env::var_os("HOME").map(PathBuf::from),
    )
    .map_err(|_| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Static("configuration_reload_composition_failed"),
        )
    })?;
    let configuration_reload = configuration_reload
        .with_runtime_factory(runtime_factory.clone())
        .with_github_tool_credential(configuration.github_token_file())
        .with_integration_credentials(integration_credentials);
    let goal_disposition = PostgresGoalPassDisposition::new(
        scheduler_pool.clone(),
        model_configuration.clone(),
        eligibility_nudge.clone(),
        goal_mode_numeric_bounds,
    )
    .with_configuration_reload(configuration_reload.clone());
    let repository_watch_runtime = {
        let start = async {
            let module_pool = connect_repository_watch_pool(&pool).await?;
            signalboxd::repo_watch_dispatch::scavenge_checkouts(
                &signalbox_module_repo_watch_v2::RepoWatchStore::new(module_pool.clone()),
                &pool,
            )
            .await
            .map_err(|_| RepositoryWatchRuntimeError::Dispatch)?;
            Ok::<_, RepositoryWatchRuntimeError>(RepositoryWatchRuntime::unstarted(
                module_pool,
                RepositoryWatchServices {
                    goal_resumption: goal_disposition.clone(),
                    checkout_runner: checkout_runner.clone(),
                    core_pool: pool.clone(),
                    models: Arc::new(model_configuration.clone()),
                    templates: Arc::new(template_configuration.clone()),
                    eligibility_nudge: eligibility_nudge.clone(),
                    tool_dispatch_gate: tool_dispatch_gate.clone(),
                },
            ))
        };
        match await_while_guarded(&mut database, start).await {
            GuardedAwait::Completed(Ok(runtime)) => Some(runtime),
            GuardedAwait::Completed(Err(_)) => {
                let failure = erase_startup_database_cause(
                    RuntimePhase::Configuration,
                    SanitizedStartupCause::Static("repository_watch_startup_failed"),
                );
                let _ = listener.cleanup();
                let _ = runner_listener.cleanup();
                drop(blob_executor);
                disarm_staging_sweep_unless_guarded(&mut database, &mut blob_store_registry).await;
                drop(blob_store_registry);
                return startup_failure_after_close(failure, database.close().await);
            }
            GuardedAwait::GuardLost => {
                let _ = listener.cleanup();
                let _ = runner_listener.cleanup();
                if let Some(registry) = blob_store_registry.as_ref() {
                    registry.disarm_staging_sweep();
                }
                drop(blob_executor);
                drop(blob_store_registry);
                let _ = database.close().await;
                return Ok(ShutdownOutcome::GuardLost);
            }
        }
    };
    tool_executor = tool_executor
        .with_blob_executor(blob_executor)
        .with_file_media_executor(file_media_executor)
        .with_repository_watch(repository_watch_runtime.clone());
    let configuration_reload = match &repository_watch_runtime {
        Some(watch) => {
            watch
                .set_sweep_bounds(convergence_sweep_numeric_bounds)
                .await;
            configuration_reload.with_repository_watch(watch.clone())
        }
        None => configuration_reload,
    };
    let (repository_watch_shutdown, repository_watch_shutdown_receiver) = watch::channel(false);
    let approval_judge_repository_watch = repository_watch_runtime.clone();
    let workflow_repository_watch = repository_watch_runtime.clone();
    let eval_runtime_factory = runtime_factory.clone();
    let workflows =
        signalboxd::workflows::WorkflowRuntime::new(pool.clone()).map(|(service, runtime)| {
            let runtime = if let Some(stores) = &blob_store_registry {
                let eval_pool = pool.clone();
                let eval_stores = stores.clone();
                let eval_reload = configuration_reload.clone();
                runtime.with_eval(move || {
                    let configuration = eval_reload.catalogs().models;
                    let model = eval_runtime_factory
                        .build(&configuration)
                        .map_err(|error| {
                            signalbox_workflow_runtime::LiveDeliveryFailure::new(error.to_string())
                        })?;
                    Ok(signalboxd::workflows::eval::EvalServices::new(
                        eval_pool.clone(),
                        eval_stores.clone(),
                        Arc::new(RuntimeApprovalJudgeModel::new(
                            model,
                            configuration.runtime_model_catalog(),
                        )),
                        signalboxd::workflows::eval::configured_binding(&configuration)
                            .unwrap_or_else(|_| signalboxd::workflows::eval::recorded_binding()),
                        configuration,
                    ))
                })
            } else {
                runtime
            };
            (
                service,
                runtime.with_repository_watch(workflow_repository_watch),
            )
        });
    let mut termination_signals = TerminationSignals::new();
    let (guard_ready, guarded_startup) = oneshot::channel();
    let mut guard_loss = Box::pin(monitor_runtime_guard(&mut database, guard_ready));
    let mut repository_watch_worker = None;
    let reconstruct = async {
        if guarded_startup.await.is_err() {
            return GuardedAwait::GuardLost;
        }
        GuardedAwait::Completed(
            async {
                startup_goal_resumption_result(
                    goal_disposition
                        .reconcile_automatic_resumptions_after_restart()
                        .await,
                    guard_recovery.is_recovering(),
                )?;
                repository_watch_worker = match repository_watch_runtime {
                    Some(runtime) => Some(runtime.spawn(repository_watch_shutdown_receiver).await),
                    None => None,
                };
                configuration_reload
                    .recover()
                    .await
                    .map_err(|error| reload_recovery_failure(&error))
            }
            .await,
        )
    };
    let recovery_failure = match select! {
        biased;
        () = &mut guard_loss => GuardedAwait::GuardLost,
        outcome = reconstruct => outcome,
    } {
        GuardedAwait::Completed(Ok(())) => None,
        GuardedAwait::Completed(Err(error)) => Some(Err(error)),
        GuardedAwait::GuardLost => Some(Ok(ShutdownOutcome::GuardLost)),
    };
    if let Some(outcome) = recovery_failure {
        drop(guard_loss);
        if matches!(outcome, Ok(ShutdownOutcome::GuardLost)) {
            if let Some(registry) = blob_store_registry.as_ref() {
                registry.disarm_staging_sweep();
            }
        } else {
            disarm_staging_sweep_unless_guarded(&mut database, &mut blob_store_registry).await;
        }
        let _ = repository_watch_shutdown.send(true);
        if let Some(worker) = repository_watch_worker {
            let _ = worker.await;
        }
        let _ = listener.cleanup();
        let _ = runner_listener.cleanup();
        drop(tool_executor);
        drop(blob_store_registry);
        let closed = database.close().await;
        return match outcome {
            Ok(outcome) => Ok(outcome),
            Err(failure) => startup_failure_after_close(failure, closed),
        };
    }
    let recovered_catalogs = configuration_reload.catalogs();
    let model_configuration = (*recovered_catalogs.models).clone();
    let template_configuration = (*recovered_catalogs.templates).clone();
    let startup_tool_catalog = tool_catalog
        .with_repository_push(model_configuration.repository_watch(), tool_composition)
        .map_err(|error| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Tools(&error),
            )
        })
        .and_then(|catalog| {
            catalog
                .with_approval_postures(model_configuration.tool_approval_postures())
                .map_err(|error| {
                    erase_startup_cause(
                        RuntimePhase::Configuration,
                        SanitizedStartupCause::Static(configured_approval_posture_cause(&error)),
                    )
                })
        });
    let tool_catalog = match startup_tool_catalog {
        Ok(catalog) => catalog,
        Err(failure) => {
            drop(guard_loss);
            disarm_staging_sweep_unless_guarded(&mut database, &mut blob_store_registry).await;
            let _ = repository_watch_shutdown.send(true);
            if let Some(worker) = repository_watch_worker {
                let _ = worker.await;
            }
            let _ = listener.cleanup();
            let _ = runner_listener.cleanup();
            drop(tool_executor);
            drop(blob_store_registry);
            return startup_failure_after_close(failure, database.close().await);
        }
    };
    let context_compaction_model: Arc<dyn ContextCompactionModel> = Arc::new(
        signalboxd::model_catalog_runtime::CatalogContextCompactionModel::new(
            configuration_reload.catalogs().models,
            runtime_factory.clone(),
        ),
    );
    let process_runtime = ProcessRuntime::new_with_templates(
        listener,
        scheduler_pool.clone(),
        eligibility_nudge.clone(),
        tool_dispatch_gate.clone(),
        model_configuration.clone(),
        template_configuration,
    )
    .with_configuration_reload(configuration_reload.clone())
    .with_context_compaction_model(Arc::clone(&context_compaction_model))
    .with_snapshot_reader_budget(snapshot_reader_budget);
    let process_runtime = match prometheus_runtime.as_ref() {
        Some((metrics, _server)) => process_runtime.with_metrics(metrics.clone()),
        None => process_runtime,
    };
    let process_runtime = match blob_store_registry {
        Some(ref registry) => process_runtime.with_blob_store_registry(Arc::clone(registry)),
        None => process_runtime,
    };
    let process_runtime = match oauth_service {
        Some(service) => process_runtime.with_oauth_service(service),
        None => process_runtime,
    };
    let web_http_runtime = web_http_listener
        .into_runtime(process_runtime.monitor(), eligibility_nudge.clone())
        .with_configuration_reload(configuration_reload.clone());
    let runner_recovery = runner_service
        .recovery_store()
        .with_recovery_notifications(process_runtime.runner_recovery_notifications());
    let runner_runtime = RunnerProtocolRuntime::new(runner_listener, runner_service);
    let text_deltas = process_runtime.provider_text_delta_sink();
    let (turn_execution_shutdown, turn_execution_shutdown_receiver) = watch::channel(false);
    let (execution_supervisor, fatal_execution) = FatalExecutionSupervisor::new(());
    let session_supervision = execution_supervisor.recovery_reporter();
    let process_runtime =
        process_runtime.with_recovery_reporter(execution_supervisor.recovery_reporter());
    let pass_pool = scheduler_pool.clone();
    let pass_nudge = eligibility_nudge.clone();
    let pass_invocation_processes = invocation_processes.clone();
    let pass_blobs = blob_store_registry.clone();
    let compose_pass = move |model_configuration: &HubModelConfiguration| {
        let runtime_models = model_configuration.runtime_model_catalog();
        let runtime = runtime_factory
            .build(model_configuration)?
            .with_media_preparation(pass_pool.clone(), pass_blobs.clone());
        let compaction: Arc<dyn ContextCompactionModel> = Arc::new(
            RuntimeContextCompactionModel::new(runtime.clone(), runtime_models.clone()),
        );
        let approval_judge: Arc<dyn ApprovalJudgeModel> = Arc::new(RuntimeApprovalJudgeModel::new(
            runtime.clone(),
            runtime_models.clone(),
        ));
        let provider = RuntimeModelCallProvider::new(
            runtime,
            runtime_models.clone(),
            diagnostic_model_identity_limit,
        )
        .with_tool_proposal_limits(model_configuration.tool_proposal_limits())
        .with_text_delta_sink(text_deltas.clone())
        .with_invocation_process_observer(pass_invocation_processes.clone());
        let counter = AttachmentPreparingModelCallProvider::for_counting(
            provider.clone(),
            pass_pool.clone(),
            pass_blobs.clone(),
            model_configuration.provider_input_count_targets(),
        );
        let model_repository = PostgresModelCallRepository::new(
            pass_pool.clone(),
            model_configuration.target_catalog(),
            credential_reference.clone(),
        )
        .with_session_credentials(model_configuration.credential_family_catalog())
        .with_credential_pools(model_configuration.credential_pool_runtime_catalog())
        .with_runner_recovery(runner_recovery.clone())
        .with_same_credential_attempt_bound(same_credential_attempt_bound)
        .with_cache_inclusive_input_targets(model_configuration.cache_inclusive_input_targets())
        .with_continuation_usage_limits(
            model_configuration
                .tool_continuation_usage_limits(&signalbox_application::ToolCatalog::definitions(
                    &tool_catalog,
                ))
                .map_err(signalboxd::model_catalog_runtime::ModelRuntimeBuildError::from)?,
        );
        let provider = AttachmentPreparingModelCallProvider::new(
            UsageLimitedModelCallProvider::new(provider, model_configuration),
            pass_pool.clone(),
            pass_blobs.clone(),
        );
        let reported_usage_compaction = ReportedUsageCompaction::new(
            StartEligibleTurnRepository::new(pass_pool.clone()),
            model_repository.clone(),
            tool_catalog.clone(),
            runtime_models.clone(),
            model_configuration.clone(),
            compaction.clone(),
        )
        .with_blob_store_registry(pass_blobs.clone())
        .with_repository_watch_continuation(pass_nudge.clone(), tool_dispatch_gate.clone());
        let execution = execution_supervisor.with_execution(
            PostgresProviderModelExecution::new(
                model_repository.clone(),
                InProcessAttemptDispatchGate::default(),
                provider,
                automatic_tool_round_limit,
            )
            .with_tool_loop(
                tool_dispatch_gate.clone(),
                tool_catalog.clone(),
                tool_executor.clone(),
            )
            .with_workspace_instructions(workspace_instruction_runtime.clone())
            .with_approval_judge(
                approval_judge,
                model_configuration.configured_approval_judge_selection(),
                model_configuration.clone(),
                approval_judge_repository_watch.clone(),
            )
            .with_approval_wait_wakeups(approval_wait_wakeups.clone())
            .with_shutdown_checkpoint(turn_execution_shutdown_receiver.clone()),
        );
        Ok::<_, signalboxd::model_catalog_runtime::ModelRuntimeBuildError>(
            ContextGuardedTurnPass::new(
                StartEligibleTurnRepository::new(pass_pool.clone()),
                model_repository,
                counter,
                tool_catalog.clone(),
                runtime_models,
                model_configuration.clone(),
                compaction,
                execution,
            )
            .with_blob_store_registry(pass_blobs.clone())
            .with_reported_usage_compaction(reported_usage_compaction)
            .with_workspace_instructions(workspace_instruction_runtime.clone())
            .with_occupancy_recovery(
                pass_pool.clone(),
                pass_nudge.clone(),
                expired_pass_recovery_policy,
                turn_liveness_persistence_bounds,
            ),
        )
    };
    let activated_pass = signalboxd::model_catalog_runtime::CatalogEligibilityPass::new(
        configuration_reload.clone(),
        compose_pass,
    );
    let turn_liveness_runtime = TurnLivenessRuntime::new(
        scheduler_pool.clone(),
        stale_active_turn_bound,
        turn_liveness_scan_interval,
        automatic_reconciliation_attempt_budget,
        automatic_reconciliation_base_backoff,
        automatic_reconciliation_backoff_cap,
        turn_liveness_numeric_bounds,
    );
    let lifecycle_deadline_runtime = LifecycleDeadlineRuntime::new(
        scheduler_pool.clone(),
        turn_liveness_scan_interval,
        SessionDeadlineBounds::new(session_admission_deadline, session_waiting_deadline),
    );
    let process_runtime = process_runtime.with_goal_resumption(goal_disposition.clone());
    let pass = GoalAwareEligibilityPass::new(activated_pass, goal_disposition);
    let scheduler_max_in_flight_passes = scheduler_pass_admission_cap;
    let mut scheduler = match scheduler_max_in_flight_passes {
        Some(limit) => match NonZeroUsize::new(limit) {
            Some(limit) => SchedulerLoop::with_max_in_flight(work_source, pass, limit),
            None => SchedulerLoop::paused(work_source, pass),
        },
        None => SchedulerLoop::new(work_source, pass),
    };
    scheduler = scheduler.with_occupancy_bound(scheduler_pass_occupancy_bound);
    // The exported gauges exist only where Prometheus does; without a scrape
    // listener there is nothing for the pass to publish to, and the operator
    // status command still reads the same views.
    let lifecycle_metrics_runtime = prometheus_runtime.as_ref().map(|(metrics, _server)| {
        LifecycleMetricsRuntime::new(
            pool.clone(),
            Arc::new(metrics.clone()),
            lifecycle_metric_scan_interval,
        )
    });
    if let Some((metrics, _server)) = prometheus_runtime.as_ref() {
        scheduler = scheduler.with_occupancy_observer(Arc::new(metrics.clone()));
    }
    if let Some(limit) = scheduler_max_in_flight_passes {
        tracing::info!(
            max_in_flight_passes = limit,
            "scheduler pass admission uses the deployment override"
        );
    }
    let (workflow_shutdown, workflow_shutdown_receiver) = oneshot::channel();
    let process_runtime = match &workflows {
        Ok((service, _)) => process_runtime.with_workflows(service.clone()),
        Err(_) => process_runtime,
    };
    let (scheduler_shutdown, scheduler_shutdown_receiver) = oneshot::channel();
    let (fenced_pool_floor_shutdown, fenced_pool_floor_shutdown_receiver) = watch::channel(false);
    let (process_shutdown, process_shutdown_receiver) = watch::channel(false);
    let (runner_shutdown, runner_shutdown_receiver) = watch::channel(false);
    let (web_http_shutdown, web_http_shutdown_receiver) = watch::channel(false);
    let (turn_liveness_shutdown, turn_liveness_shutdown_receiver) = watch::channel(false);
    let (lifecycle_deadline_shutdown, lifecycle_deadline_shutdown_receiver) = watch::channel(false);
    let (lifecycle_metrics_shutdown, lifecycle_metrics_shutdown_receiver) = watch::channel(false);
    let mut runtime_tasks = JoinSet::new();
    let supervision_pool = pool.clone();
    let supervision_nudge = eligibility_nudge.clone();
    let mut supervision_shutdown = process_shutdown.subscribe();
    let mut drain_interrupted = false;
    drop(guard_loss);
    let (guard_ready, guarded_admission) = oneshot::channel();
    let mut guard_loss = Box::pin(monitor_runtime_guard(&mut database, guard_ready));
    let mut outcome = {
        let mut cause = {
            let runtime = async {
                if guarded_admission.await.is_err() {
                    return RuntimeStopCause::GuardLost;
                }
                runtime_tasks.spawn(async move {
                    select! {
                        () = session_supervision.park_failed_sessions(supervision_pool, supervision_nudge) => {},
                        _ = supervision_shutdown.changed() => {},
                    }
                    RuntimeTaskExit::SessionSupervision
                });
                runtime_tasks.spawn(async move {
                    let result = async {
                        let (_service, workflows) = workflows?;
                        workflows
                            .run(async {
                                let _ = workflow_shutdown_receiver.await;
                            })
                            .await
                    }
                    .await;
                    RuntimeTaskExit::Workflows(result)
                });
                runtime_tasks.spawn(async move {
                    RuntimeTaskExit::Scheduler(
                        scheduler
                            .run_until(async move {
                                let _ = scheduler_shutdown_receiver.await;
                            })
                            .await,
                    )
                });
                if let Some(policy) = fenced_pool_floor_reconciliation {
                    runtime_tasks.spawn(async move {
                        run_fenced_pool_floor_reconciliation(
                            fenced_pool_floor_pool,
                            policy,
                            fenced_pool_floor_shutdown_receiver,
                        )
                        .await;
                        RuntimeTaskExit::FencedPoolFloor
                    });
                }
                runtime_tasks.spawn(async move {
                    RuntimeTaskExit::Process(process_runtime.run(process_shutdown_receiver).await)
                });
                runtime_tasks.spawn(async move {
                    RuntimeTaskExit::Runner(runner_runtime.run(runner_shutdown_receiver).await)
                });
                runtime_tasks.spawn(async move {
                    RuntimeTaskExit::WebHttp(web_http_runtime.run(web_http_shutdown_receiver).await)
                });
                if let Some(worker) = repository_watch_worker {
                    runtime_tasks.spawn(async move {
                        RuntimeTaskExit::RepositoryWatch(
                            worker
                                .await
                                .unwrap_or(Err(RepositoryWatchRuntimeError::RepositoryWorker)),
                        )
                    });
                }
                let invocation_shutdown = turn_liveness_shutdown_receiver.clone();
                runtime_tasks.spawn(async move {
                    let capacity_shutdown = invocation_shutdown.clone();
                    tokio::join!(invocation_processes.run(invocation_shutdown), async {
                        if let Some(refresh) = capacity_refresh {
                            refresh.run(capacity_shutdown).await;
                        }
                    },);
                    RuntimeTaskExit::CredentialInvocations
                });
                runtime_tasks.spawn(async move {
                    turn_liveness_runtime
                        .run(turn_liveness_shutdown_receiver)
                        .await;
                    RuntimeTaskExit::TurnLiveness
                });
                runtime_tasks.spawn(async move {
                    lifecycle_deadline_runtime
                        .run(lifecycle_deadline_shutdown_receiver)
                        .await;
                    RuntimeTaskExit::LifecycleDeadline
                });
                if let Some(lifecycle_metrics_runtime) = lifecycle_metrics_runtime {
                    runtime_tasks.spawn(async move {
                        lifecycle_metrics_runtime
                            .run(lifecycle_metrics_shutdown_receiver)
                            .await;
                        RuntimeTaskExit::LifecycleMetrics
                    });
                }
                guard_recovery.runtime_ready();
                tracing::info!(phase = ?RuntimePhase::Scheduling, "daemon runtime started");

                select! {
                    listener_failed = shutdown_requested(&mut termination_signals) => {
                        if listener_failed {
                            RuntimeStopCause::SignalListenerFailed
                        } else {
                            RuntimeStopCause::Requested
                        }
                    }
                    () = fatal_execution.wait_for_process_recovery() => RuntimeStopCause::ExecutionFailed,
                    completed = runtime_tasks.join_next() => {
                        match completed {
                            Some(Ok(RuntimeTaskExit::Workflows(result))) => {
                                match result {
                                    Ok(()) => tracing::error!("workflow runtime completed before shutdown"),
                                    Err(error) => tracing::error!(cause = error.cause_code(), "workflow runtime failed"),
                                }
                                RuntimeStopCause::RuntimeFailed
                            }
                            Some(Ok(RuntimeTaskExit::Process(Err(error)))) => {
                                report_process_runtime_failure(&error);
                                RuntimeStopCause::RuntimeFailed
                            }
                            Some(Ok(RuntimeTaskExit::FencedPoolFloor)) => {
                                report_runtime_task_defect(
                                    RuntimeTaskDefect::FencedPoolFloorCompletedBeforeShutdown,
                                );
                                RuntimeStopCause::RuntimeDefect
                            }
                            Some(Ok(RuntimeTaskExit::Process(Ok(())))) => {
                                report_runtime_task_defect(
                                    RuntimeTaskDefect::ProcessCompletedBeforeShutdown,
                                );
                                RuntimeStopCause::RuntimeDefect
                            }
                            Some(Ok(RuntimeTaskExit::Runner(Err(error)))) => {
                                report_runner_runtime_failure(&error);
                                RuntimeStopCause::RuntimeFailed
                            }
                            Some(Ok(RuntimeTaskExit::Runner(Ok(())))) => {
                                report_runtime_task_defect(
                                    RuntimeTaskDefect::RunnerCompletedBeforeShutdown,
                                );
                                RuntimeStopCause::RuntimeDefect
                            }
                            Some(Ok(RuntimeTaskExit::RepositoryWatch(Err(error)))) => {
                                tracing::error!(?error, "repository-watch runtime failed");
                                RuntimeStopCause::RuntimeFailed
                            }
                            Some(Ok(RuntimeTaskExit::RepositoryWatch(Ok(())))) => {
                                report_runtime_task_defect(RuntimeTaskDefect::RepositoryWatchCompletedBeforeShutdown);
                                RuntimeStopCause::RuntimeDefect
                            }
                            Some(Ok(RuntimeTaskExit::WebHttp(Err(error)))) => {
                                report_web_http_runtime_failure(&error);
                                RuntimeStopCause::RuntimeFailed
                            }
                            Some(Ok(RuntimeTaskExit::WebHttp(Ok(())))) => {
                                report_runtime_task_defect(
                                    RuntimeTaskDefect::WebHttpCompletedBeforeShutdown,
                                );
                                RuntimeStopCause::RuntimeDefect
                            }
                            Some(Ok(RuntimeTaskExit::SessionSupervision)) => {
                                report_runtime_task_defect(
                                    RuntimeTaskDefect::SessionSupervisionCompletedBeforeShutdown,
                                );
                                RuntimeStopCause::RuntimeDefect
                            }
                            Some(Ok(RuntimeTaskExit::LifecycleMetrics)) => {
                                report_runtime_task_defect(
                                    RuntimeTaskDefect::LifecycleMetricsCompletedBeforeShutdown,
                                );
                                RuntimeStopCause::RuntimeDefect
                            }
                            Some(Ok(RuntimeTaskExit::CredentialInvocations)) => {
                                tracing::error!("invocation reservation reconciliation completed before shutdown");
                                RuntimeStopCause::RuntimeDefect
                            }
                            Some(Ok(RuntimeTaskExit::TurnLiveness)) => {
                                report_runtime_task_defect(
                                    RuntimeTaskDefect::TurnLivenessCompletedBeforeShutdown,
                                );
                                RuntimeStopCause::RuntimeDefect
                            }
                            Some(Ok(RuntimeTaskExit::LifecycleDeadline)) => {
                                report_runtime_task_defect(
                                    RuntimeTaskDefect::LifecycleDeadlineCompletedBeforeShutdown,
                                );
                                RuntimeStopCause::RuntimeDefect
                            }
                            Some(Ok(RuntimeTaskExit::Scheduler(_))) => {
                                report_runtime_task_defect(
                                    RuntimeTaskDefect::SchedulerCompletedBeforeShutdown,
                                );
                                RuntimeStopCause::RuntimeDefect
                            }
                            Some(Err(error)) => {
                                report_runtime_task_defect(joined_task_defect(&error));
                                RuntimeStopCause::RuntimeDefect
                            }
                            None => {
                                report_runtime_task_defect(RuntimeTaskDefect::TaskSetEmpty);
                                RuntimeStopCause::RuntimeDefect
                            }
                        }
                    }
                }
            };
            pin!(runtime);
            select! {
                biased;
                () = &mut guard_loss => RuntimeStopCause::GuardLost,
                cause = &mut runtime => cause,
            }
        };

        let _ = workflow_shutdown.send(());
        let _ = repository_watch_shutdown.send(true);
        if cause == RuntimeStopCause::GuardLost {
            runtime_tasks.abort_all();
            while runtime_tasks.join_next().await.is_some() {}
            ShutdownOutcome::GuardLost
        } else {
            let _ = turn_execution_shutdown.send(true);
            let _ = scheduler_shutdown.send(());
            let _ = fenced_pool_floor_shutdown.send(true);
            let _ = process_shutdown.send(true);
            let _ = runner_shutdown.send(true);
            let _ = web_http_shutdown.send(true);
            let _ = turn_liveness_shutdown.send(true);
            let _ = lifecycle_deadline_shutdown.send(true);
            let _ = lifecycle_metrics_shutdown.send(true);
            let (drain, components_clean) = drain_runtime_tasks(
                &mut runtime_tasks,
                guard_loss.as_mut(),
                async {
                    if shutdown_requested(&mut termination_signals).await {
                        tracing::error!("termination signal listener failed during shutdown");
                    }
                },
                shutdown_grace_window,
            )
            .await;
            cause = combine_runtime_stop_cause(cause, components_clean);
            drain_interrupted = drain == RuntimeDrainOutcome::Interrupted;
            if drain_interrupted {
                tracing::warn!("shutdown drain interrupted");
            }
            if drain != RuntimeDrainOutcome::Complete {
                runtime_tasks.abort_all();
                while runtime_tasks.join_next().await.is_some() {}
            }
            completed_runtime_outcome(cause, drain)
        }
    };

    drop(guard_loss);

    // A timed-out component may still have held a connection before its task
    // was aborted. Waiting for an ordinary pool drain here would silently
    // extend the shutdown window. Guard loss is different: tasks are cancelled
    // immediately and the old fenced sessions must be terminated before
    // constructing a replacement incarnation.
    if outcome != ShutdownOutcome::GuardLost && database.check_guard().await.is_err() {
        outcome = ShutdownOutcome::GuardLost;
    }
    if outcome == ShutdownOutcome::GuardLost {
        if let Some(registry) = blob_store_registry.as_ref() {
            registry.disarm_staging_sweep();
        }
        drop(blob_store_registry);
        let _ = database.close().await;
    } else {
        let close_pool = !drain_interrupted && should_close_pool(&Ok(outcome));
        if let Some(registry) = blob_store_registry.as_ref()
            && registry.sweep_staging().is_err()
        {
            let failure_class = OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            };
            tracing::error!(
                phase = ?RuntimePhase::Runtime,
                ?failure_class,
                cause = "blob_staging_sweep_failed",
                "daemon staging cleanup failed"
            );
            outcome = staging_sweep_failure_outcome(outcome);
        }
        drop(blob_store_registry);
        if close_pool && let Err(error) = database.close().await {
            report_database_close_failure(&error);
            outcome = database_close_failure_outcome(outcome);
        }
    }
    Ok(outcome)
}

/// Whether an operator filter setting was admitted or rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperatorFilterDisposition {
    /// The absent, empty, or closed-level setting was admitted.
    Accepted,
    /// A non-level or non-Unicode setting fell back to INFO.
    Rejected,
}

/// Builds a first-party-only level override without exposing rejected input.
///
/// Absence preserves the existing global INFO default. A closed log level can
/// quiet every target or make Signalbox DEBUG sites reachable, but dependency
/// DEBUG/TRACE sites stay disabled because arbitrary target directives are
/// rejected.
fn operator_filter(
    value: Option<&str>,
) -> (tracing_subscriber::EnvFilter, OperatorFilterDisposition) {
    match value {
        None => (
            tracing_subscriber::EnvFilter::new("info"),
            OperatorFilterDisposition::Accepted,
        ),
        Some(value) if value.trim().is_empty() => (
            tracing_subscriber::EnvFilter::new("info"),
            OperatorFilterDisposition::Accepted,
        ),
        Some(value) => match value.trim().parse::<tracing::level_filters::LevelFilter>() {
            Ok(level) => match signalbox_level_filter(level) {
                Some(filter) => (filter, OperatorFilterDisposition::Accepted),
                None => (
                    tracing_subscriber::EnvFilter::new("info"),
                    OperatorFilterDisposition::Rejected,
                ),
            },
            Err(_) => (
                tracing_subscriber::EnvFilter::new("info"),
                OperatorFilterDisposition::Rejected,
            ),
        },
    }
}

/// Applies one closed level only to crates covered by Signalbox redaction.
///
/// The global directive preserves the INFO default, follows a quieter operator
/// selection, and caps dependencies at INFO for DEBUG or TRACE. The three
/// target overrides name the only crates that emit daemon telemetry, so
/// dependency verbosity cannot be raised through this process surface.
fn signalbox_level_filter(
    level: tracing::level_filters::LevelFilter,
) -> Option<tracing_subscriber::EnvFilter> {
    let dependency_level = match level {
        tracing::level_filters::LevelFilter::OFF
        | tracing::level_filters::LevelFilter::ERROR
        | tracing::level_filters::LevelFilter::WARN => level,
        _ => tracing::level_filters::LevelFilter::INFO,
    };
    let directives = [
        dependency_level.to_string(),
        format!("signalboxd={level}"),
        format!("signalbox_application={level}"),
        format!("signalbox_model_provider_runtime={level}"),
    ]
    .join(",");
    tracing_subscriber::EnvFilter::try_new(directives).ok()
}

/// Installs compact operator telemetry with a configurable closed level.
///
/// The setting value itself is never logged. Rejection records only the public
/// setting name, and third-party targets never exceed the selected level or the
/// INFO default.
fn report_operator_filter(disposition: OperatorFilterDisposition) {
    match disposition {
        OperatorFilterDisposition::Accepted => {}
        OperatorFilterDisposition::Rejected => tracing::warn!(
            setting = LOG_FILTER_ENVIRONMENT,
            "invalid tracing level rejected; using INFO default"
        ),
    }
}

fn install_tracing_subscriber(
    telemetry_configuration: &TelemetryConfiguration,
) -> Result<Option<OtlpRuntime>, TelemetryConfigurationError> {
    let configured = env::var(LOG_FILTER_ENVIRONMENT);
    let (filter, disposition) = match configured.as_deref() {
        Ok(value) => operator_filter(Some(value)),
        Err(env::VarError::NotPresent) => operator_filter(None),
        Err(env::VarError::NotUnicode(_)) => (
            tracing_subscriber::EnvFilter::new("info"),
            OperatorFilterDisposition::Rejected,
        ),
    };
    let otlp_runtime = match telemetry_configuration.build_otlp_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing_subscriber::registry()
                .with(
                    tracing_subscriber::fmt::layer()
                        .compact()
                        .with_filter(filter),
                )
                .init();
            report_operator_filter(disposition);
            return Err(error);
        }
    };
    let otlp_layer = otlp_runtime
        .as_ref()
        .map(|runtime| runtime.layer().with_filter(TelemetryExportFilter));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_filter(filter),
        )
        .with(otlp_layer)
        .init();
    report_operator_filter(disposition);
    Ok(otlp_runtime)
}

#[tokio::main]
async fn main() -> ExitCode {
    if let Some(exit_code) = run_web_image_derivative_worker_if_requested() {
        return exit_code;
    }
    let telemetry_configuration = match TelemetryConfiguration::from_environment() {
        Ok(configuration) => configuration,
        Err(error) => {
            let disabled = TelemetryConfiguration::disabled();
            let _ = install_tracing_subscriber(&disabled);
            let startup_error = erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::TelemetryConfiguration(&error),
            );
            tracing::error!(
                phase = ?startup_error.phase,
                failure_class = ?startup_error.failure_class,
                "daemon startup failed"
            );
            return ExitCode::FAILURE;
        }
    };
    let otlp_runtime = match install_tracing_subscriber(&telemetry_configuration) {
        Ok(runtime) => runtime,
        Err(error) => {
            let startup_error = erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::TelemetryConfiguration(&error),
            );
            tracing::error!(
                phase = ?startup_error.phase,
                failure_class = ?startup_error.failure_class,
                "daemon startup failed"
            );
            return ExitCode::FAILURE;
        }
    };

    let exit_code = match run_hub(&telemetry_configuration).await {
        Ok(ShutdownOutcome::Interrupted) => {
            tracing::warn!(
                "daemon shutdown abandoned in-flight work after the drain was interrupted"
            );
            ExitCode::FAILURE
        }
        Ok(ShutdownOutcome::Clean) => {
            tracing::info!("daemon shutdown completed");
            ExitCode::SUCCESS
        }
        Ok(ShutdownOutcome::GraceWindowExpired) => {
            tracing::warn!("daemon shutdown grace window expired; abandoning in-flight work");
            ExitCode::SUCCESS
        }
        Ok(ShutdownOutcome::SignalListenerFailed) => {
            let error = HubRuntimeError::infrastructure(RuntimePhase::Scheduling);
            tracing::error!(
                phase = ?error.phase,
                failure_class = ?error.failure_class,
                "daemon runtime failed"
            );
            ExitCode::FAILURE
        }
        Ok(ShutdownOutcome::ExecutionFailed) => {
            let error = HubRuntimeError::infrastructure(RuntimePhase::Scheduling);
            tracing::error!(
                phase = ?error.phase,
                failure_class = ?error.failure_class,
                "activated-turn execution failed; stopping for startup recovery"
            );
            ExitCode::FAILURE
        }
        Ok(ShutdownOutcome::ExecutionFailedAfterGraceWindow) => {
            let error = HubRuntimeError::infrastructure(RuntimePhase::Scheduling);
            tracing::error!(
                phase = ?error.phase,
                failure_class = ?error.failure_class,
                "activated-turn execution failed and shutdown grace expired; abandoning in-flight work for startup recovery"
            );
            ExitCode::FAILURE
        }
        Ok(ShutdownOutcome::GuardLost | ShutdownOutcome::GuardRecoveryExhausted) => {
            let error = HubRuntimeError::infrastructure(RuntimePhase::Runtime);
            tracing::error!(
                phase = ?error.phase,
                failure_class = ?error.failure_class,
                "database guard was lost; fenced runtime cancelled immediately"
            );
            ExitCode::FAILURE
        }
        Ok(ShutdownOutcome::RuntimeFailed) => {
            let error = HubRuntimeError::infrastructure(RuntimePhase::Runtime);
            tracing::error!(
                phase = ?error.phase,
                failure_class = ?error.failure_class,
                "daemon runtime component failed"
            );
            ExitCode::FAILURE
        }
        Ok(ShutdownOutcome::RuntimeFailedAfterGraceWindow) => {
            let error = HubRuntimeError::infrastructure(RuntimePhase::Runtime);
            tracing::error!(
                phase = ?error.phase,
                failure_class = ?error.failure_class,
                "daemon runtime component failed and shutdown grace expired; abandoning in-flight work"
            );
            ExitCode::FAILURE
        }
        Ok(ShutdownOutcome::RuntimeDefect) => {
            tracing::error!(
                phase = ?RuntimePhase::Runtime,
                failure_class = ?OperatorFailureClass::CallerOrHubBug,
                "daemon runtime stopped after a task lifecycle defect"
            );
            ExitCode::FAILURE
        }
        Ok(ShutdownOutcome::RuntimeDefectAfterGraceWindow) => {
            tracing::error!(
                phase = ?RuntimePhase::Runtime,
                failure_class = ?OperatorFailureClass::CallerOrHubBug,
                "daemon runtime task defect was followed by an expired shutdown grace window"
            );
            ExitCode::FAILURE
        }
        Err(error) => {
            tracing::error!(
                phase = ?error.phase,
                failure_class = ?error.failure_class,
                session_id = ?error.session,
                turn_id = ?error.turn,
                "daemon startup failed"
            );
            ExitCode::FAILURE
        }
    };
    if let Some(runtime) = otlp_runtime {
        runtime.shutdown();
    }
    exit_code
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::VecDeque,
        ffi::OsString,
        future::{Future, pending, ready},
        io::{self, Write},
        rc::Rc,
        sync::{Arc, Mutex, OnceLock},
        time::Duration,
    };

    use signalbox_application::{
        ClassifyOperatorFailure, EligibilityPass, EligibilityWorkSource, OperatorFailureClass,
        SchedulerLoop,
    };
    use signalbox_domain::{SessionId, TurnId};
    use tokio::{sync::oneshot, task::JoinSet, time::timeout};
    use tracing_subscriber::prelude::*;
    use uuid::Uuid;

    use super::{
        AnthropicConstructionError, BRAVE_API_KEY_FILE_ENVIRONMENT, DATABASE_URL_ENVIRONMENT,
        FENCED_POOL_MAX_CONNECTIONS, FencedPoolFloorReconciliationPolicy, HubConfiguration,
        HubConfigurationError, HubConfigurationValues, HubRuntimeError,
        MODEL_CONFIGURATION_FILE_ENVIRONMENT, OpenAiConstructionError, OperatorFilterDisposition,
        PROCESS_SOCKET_PATH_ENVIRONMENT, ProcessRuntimeError, RUNNER_SOCKET_PATH_ENVIRONMENT,
        RequiredSettingFailure, RuntimeDrainOutcome, RuntimePhase, RuntimeStopCause,
        RuntimeTaskCompletion, RuntimeTaskExit, SanitizedStartupCause, SchedulerStopCause,
        ShutdownOutcome, SingleHubGuardError, TEMPLATE_CONFIGURATION_FILE_ENVIRONMENT,
        combine_runtime_stop_cause, completed_runtime_outcome, credential_files_conflict,
        database_close_failure_outcome, drain_runtime_tasks, erase_startup_cause,
        fenced_pool_floor_reconciliation_policy, graceful_shutdown_window,
        migrate_scan_then_schedule, operator_filter, process_runtime_failure_class,
        report_database_close_failure, run_scheduler_until_shutdown,
        runner_lifecycle_failure_class, should_close_pool, staging_sweep_failure_outcome,
        validate_fenced_pool_min_connections,
    };
    use signalboxd::runner_protocol_runtime::RunnerRegistrationFailureCause;

    const BRAVE_KEY_FILE_FIXTURE: &str = "brave-key";

    #[test]
    fn fenced_pool_prewarm_cannot_exceed_the_compiled_capacity() {
        assert_eq!(
            validate_fenced_pool_min_connections(Some(FENCED_POOL_MAX_CONNECTIONS)),
            Some(Some(FENCED_POOL_MAX_CONNECTIONS))
        );
        assert_eq!(
            validate_fenced_pool_min_connections(Some(FENCED_POOL_MAX_CONNECTIONS + 1)),
            None
        );
        assert_eq!(validate_fenced_pool_min_connections(None), Some(None));
    }

    #[test]
    fn positive_fenced_pool_floor_requires_bounded_reconciliation() {
        let interval = Duration::from_secs(5);
        let attempt_bound = Duration::from_secs(30);

        assert_eq!(
            fenced_pool_floor_reconciliation_policy(
                Some(FENCED_POOL_MAX_CONNECTIONS),
                Some(interval),
                Some(attempt_bound),
            ),
            Some(Some(FencedPoolFloorReconciliationPolicy {
                minimum: FENCED_POOL_MAX_CONNECTIONS,
                interval,
                attempt_bound,
            }))
        );
        assert_eq!(
            fenced_pool_floor_reconciliation_policy(
                Some(FENCED_POOL_MAX_CONNECTIONS),
                None,
                Some(attempt_bound),
            ),
            None
        );
        assert_eq!(
            fenced_pool_floor_reconciliation_policy(
                Some(FENCED_POOL_MAX_CONNECTIONS),
                Some(interval),
                None,
            ),
            None
        );
        assert_eq!(
            fenced_pool_floor_reconciliation_policy(None, None, None),
            Some(None)
        );
    }

    fn hub_configuration_values() -> HubConfigurationValues {
        HubConfigurationValues {
            database_url: Some(OsString::from("postgres://secret")),
            model_configuration_file: Some(OsString::from("models.toml")),
            template_configuration_file: Some(OsString::from("templates.toml")),
            brave_api_key_file: Some(OsString::from(BRAVE_KEY_FILE_FIXTURE)),
            github_token_file: Some(OsString::from("github-token")),
            process_socket_path: Some(OsString::from("/tmp/signalbox.sock")),
            runner_socket_path: Some(OsString::from("/tmp/signalbox-runner.sock")),
        }
    }

    thread_local! {
        /// Telemetry captured on this thread alone.
        static CAPTURED_OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    /// Appends every formatted event to the emitting thread's own buffer.
    #[derive(Clone, Copy, Default)]
    struct CapturedOutput;

    impl Write for CapturedOutput {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            CAPTURED_OUTPUT.with(|captured| captured.borrow_mut().extend_from_slice(buffer));
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CapturedOutput {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            *self
        }
    }

    /// Records the telemetry `record` emits on this thread.
    ///
    /// The subscriber is installed once for the whole test process rather than
    /// scoped to this thread. `tracing` caches each callsite's interest
    /// process-wide, but `set_default` binds a subscriber to one thread, so a
    /// sibling test that reaches a callsite first on another thread registers
    /// it against no subscriber at all -- recording it as uninteresting for
    /// every thread, including the one that installed a capture. The event then
    /// is not merely written late; it is never emitted, and the assertion reads
    /// an empty buffer.
    ///
    /// Writes are routed per thread so concurrent tests never read each other's
    /// events, which keeps assertions on both presence and absence honest.
    ///
    /// The operator-filter tests below deliberately keep their own scoped
    /// subscribers: they assert on what a given filter enables rather than on
    /// captured text, and a thread-scoped default still overrides this one.
    fn capture_operator_telemetry(record: impl FnOnce()) -> String {
        static INSTALLED: OnceLock<()> = OnceLock::new();

        INSTALLED.get_or_init(|| {
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(CapturedOutput)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other global telemetry subscriber is installed");
        });
        CAPTURED_OUTPUT.with(|captured| captured.borrow_mut().clear());
        record();
        CAPTURED_OUTPUT
            .with(|captured| String::from_utf8(captured.borrow().clone()))
            .expect("captured telemetry is UTF-8")
    }

    fn capture_startup_cause(cause: SanitizedStartupCause<'_>) -> String {
        capture_operator_telemetry(|| {
            let _ = erase_startup_cause(RuntimePhase::Configuration, cause);
        })
    }

    fn capture_database_close_failure(error: &SingleHubGuardError) -> String {
        capture_operator_telemetry(|| {
            report_database_close_failure(error);
        })
    }

    #[test]
    fn runtime_failure_class_reports_dispatch_corruption() {
        let corruption = ProcessRuntimeError::Dispatch(
            signalbox_persistence::outbox::OutboxDispatchError::Corruption(
                signalbox_persistence::outbox::OutboxCorruption::MissingDeliveryState,
            ),
        );
        assert_eq!(
            process_runtime_failure_class(&corruption),
            OperatorFailureClass::FailClosedCorruption,
        );
    }

    #[test]
    fn runtime_failure_class_reports_internal_defects() {
        assert_eq!(
            process_runtime_failure_class(&ProcessRuntimeError::EncodeInvariant),
            OperatorFailureClass::CallerOrHubBug,
        );
        assert_eq!(
            process_runtime_failure_class(&ProcessRuntimeError::UnexpectedDispatcherRetry),
            OperatorFailureClass::CallerOrHubBug,
        );
    }

    #[test]
    fn runner_runtime_failure_class_reports_durable_corruption() {
        assert_eq!(
            runner_lifecycle_failure_class(RunnerRegistrationFailureCause::Corruption),
            OperatorFailureClass::FailClosedCorruption,
        );
    }

    #[test]
    fn startup_failure_cause_reaches_operator_log() {
        let error =
            HubConfigurationError::new(DATABASE_URL_ENVIRONMENT, RequiredSettingFailure::Missing);
        let encoded = capture_startup_cause(SanitizedStartupCause::Configuration(&error));
        assert!(encoded.contains("required setting DATABASE_URL is missing"));
    }

    #[test]
    fn startup_failure_omits_dynamic_adapter_detail() {
        let adapter_detail = "synthetic-credential-and-prompt-content";
        let error = AnthropicConstructionError::InvalidBaseUrl {
            detail: adapter_detail.to_owned(),
        };
        let cause_code =
            signalboxd::model_catalog_runtime::ModelRuntimeBuildError::from(error).cause_code();
        let encoded = capture_startup_cause(SanitizedStartupCause::Static(cause_code));
        assert!(encoded.contains("anthropic_invalid_base_url"));
        assert!(!encoded.contains(adapter_detail));
    }

    #[test]
    fn openai_startup_failure_omits_dynamic_adapter_detail() {
        let adapter_detail = "synthetic-credential-and-prompt-content";
        let error = OpenAiConstructionError::InvalidBaseUrl {
            detail: adapter_detail.to_owned(),
        };

        let cause_code =
            signalboxd::model_catalog_runtime::ModelRuntimeBuildError::from(error).cause_code();
        let encoded = capture_startup_cause(SanitizedStartupCause::Static(cause_code));

        assert!(encoded.contains("openai_invalid_base_url"));
        assert!(!encoded.contains(adapter_detail));
    }

    #[test]
    fn oauth_startup_recovery_preserves_commit_ambiguity() {
        use signalbox_persistence::oauth_credential::OauthCredentialRepositoryError;
        for (error, commit_ambiguous) in [
            (OauthCredentialRepositoryError::Database, false),
            (OauthCredentialRepositoryError::CommitAmbiguous, true),
        ] {
            assert_eq!(
                process_runtime_failure_class(&ProcessRuntimeError::OauthRecovery(error)),
                OperatorFailureClass::Infrastructure { commit_ambiguous }
            );
        }
    }

    #[test]
    fn tracing_filter_defaults_scopes_debug_and_quiets_dependencies() {
        let (default_filter, default_disposition) = operator_filter(None);
        let (empty_filter, empty_disposition) = operator_filter(Some(""));
        let (debug_filter, debug_disposition) = operator_filter(Some("debug"));
        let (warn_filter, warn_disposition) = operator_filter(Some("warn"));
        let (external_filter, external_disposition) = operator_filter(Some("hyper=trace"));
        let (invalid_filter, invalid_disposition) = operator_filter(Some("not a level"));
        assert_eq!(default_filter.to_string(), "info");
        assert_eq!(default_disposition, OperatorFilterDisposition::Accepted);
        assert_eq!(empty_filter.to_string(), "info");
        assert_eq!(empty_disposition, OperatorFilterDisposition::Accepted);
        let debug_subscriber = tracing_subscriber::registry().with(debug_filter);
        tracing::subscriber::with_default(debug_subscriber, || {
            assert!(tracing::enabled!(target: "signalboxd", tracing::Level::DEBUG));
            assert!(tracing::enabled!(
                target: "signalbox_application",
                tracing::Level::DEBUG
            ));
            assert!(tracing::enabled!(
                target: "signalbox_model_provider_runtime",
                tracing::Level::DEBUG
            ));
            assert!(!tracing::enabled!(target: "hyper", tracing::Level::DEBUG));
            assert!(tracing::enabled!(target: "hyper", tracing::Level::INFO));
        });
        assert_eq!(debug_disposition, OperatorFilterDisposition::Accepted);
        let warn_subscriber = tracing_subscriber::registry().with(warn_filter);
        tracing::subscriber::with_default(warn_subscriber, || {
            assert!(!tracing::enabled!(target: "signalboxd", tracing::Level::INFO));
            assert!(!tracing::enabled!(target: "hyper", tracing::Level::INFO));
            assert!(tracing::enabled!(target: "signalboxd", tracing::Level::WARN));
            assert!(tracing::enabled!(target: "hyper", tracing::Level::WARN));
        });
        assert_eq!(warn_disposition, OperatorFilterDisposition::Accepted);
        assert_eq!(external_filter.to_string(), "info");
        assert_eq!(external_disposition, OperatorFilterDisposition::Rejected);
        assert_eq!(invalid_filter.to_string(), "info");
        assert_eq!(invalid_disposition, OperatorFilterDisposition::Rejected);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn fresh_fenced_database_migrates_before_installing_oauth_catalog()
    -> Result<(), Box<dyn std::error::Error>> {
        use testcontainers_modules::{
            postgres::Postgres,
            testcontainers::{ImageExt, runners::AsyncRunner},
        };
        let container = Postgres::default()
            // Same PostgreSQL image as tests/process_substrate.rs.
            .with_tag("18.4-alpine3.23")
            .with_cmd(signalbox_persistence::disposable_postgres_server_args())
            .with_mount(signalbox_persistence::disposable_postgres_state_tmpfs_from_example()?)
            .with_labels(signalbox_persistence::disposable_test_container_labels())
            .start()
            .await?;
        let url = format!(
            "postgres://postgres:postgres@{}:{}/postgres",
            container.get_host().await?,
            container.get_host_port_ipv4(5432).await?,
        );
        let database = signalboxd::FencedHubDatabase::connect_with(
            signalbox_persistence::local_test_connection_options(&url)?,
            None,
        )
        .await?;
        let table: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('oauth_credential_registration')::text")
                .fetch_one(database.pool())
                .await?;
        assert!(
            table.is_none(),
            "fencing initializes only its migration baseline"
        );
        let registration = signalbox_persistence::oauth_credential::OauthRegistration {
            client_id: "startup-client".into(),
            token_url: "https://authorization.example/token".into(),
            refresh_token_url: "https://authorization.example/oauth/token".into(),
            device_authorization_url: "https://authorization.example/device".into(),
            scopes: vec!["openid".into()],
        };
        super::migrate_hub_database(database.pool())
            .await
            .map_err(|_| "startup migration failed")?;
        super::install_oauth_registrations(
            database.pool(),
            &[("startup-profile".into(), registration)],
        )
        .await
        .map_err(|_| "OAuth startup migration failed")?;
        let profile: String =
            sqlx::query_scalar("SELECT profile FROM oauth_credential_registration")
                .fetch_one(database.pool())
                .await?;
        assert_eq!(profile, "startup-profile");
        database.close().await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn startup_database_failure_reacquires_only_when_cleanup_lost_the_guard()
    -> Result<(), Box<dyn std::error::Error>> {
        use testcontainers_modules::{
            postgres::Postgres,
            testcontainers::{ImageExt, runners::AsyncRunner},
        };
        let container = Postgres::default()
            // Same PostgreSQL image as tests/process_substrate.rs.
            .with_tag("18.4-alpine3.23")
            .with_cmd(signalbox_persistence::disposable_postgres_server_args())
            .with_mount(signalbox_persistence::disposable_postgres_state_tmpfs_from_example()?)
            .with_labels(signalbox_persistence::disposable_test_container_labels())
            .start()
            .await?;
        let url = format!(
            "postgres://postgres:postgres@{}:{}/postgres",
            container.get_host().await?,
            container.get_host_port_ipv4(5432).await?,
        );
        use signalboxd::guard_recovery::GuardedIncarnationOutcome;
        let options = signalbox_persistence::local_test_connection_options(&url)?;
        let control = sqlx::PgPool::connect_with(options.clone()).await?;
        let database = signalboxd::FencedHubDatabase::connect_with(options.clone(), None).await?;
        let previous_generation = database.generation();
        let guard_backend: i32 = sqlx::query_scalar(
            "SELECT DISTINCT pid FROM pg_locks
             WHERE locktype = 'advisory' AND mode = 'ExclusiveLock' AND granted
               AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
        )
        .fetch_one(&control)
        .await?;
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(guard_backend)
            .execute(&control)
            .await?;
        database.pool().close().await;
        let failure = super::migrate_hub_database(database.pool())
            .await
            .expect_err("the interrupted migration cannot use a closed pool");
        let result = super::startup_failure_after_close(failure, database.close().await);
        assert!(matches!(
            super::recovery_incarnation_outcome(result, false),
            GuardedIncarnationOutcome::Reacquire
        ));

        let recovered = signalboxd::FencedHubDatabase::connect_with(options, None).await?;
        assert!(recovered.generation().get() > previous_generation.get());
        recovered.pool().close().await;
        let failure = super::migrate_hub_database(recovered.pool())
            .await
            .expect_err("an initial pool failure remains a startup error with a healthy guard");
        let result = super::startup_failure_after_close(failure, recovered.close().await);
        assert!(
            matches!(super::recovery_incarnation_outcome(result, false), GuardedIncarnationOutcome::Finished(Err(error)) if error == failure)
        );
        control.close().await;
        drop(container);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn runtime_guard_revalidation_withholds_admission_after_guard_death()
    -> Result<(), Box<dyn std::error::Error>> {
        use signalboxd::guard_recovery::{
            GuardRecoveryPolicy, GuardedIncarnationOutcome, run_guarded_incarnations,
        };
        use testcontainers_modules::{
            postgres::Postgres,
            testcontainers::{ImageExt, runners::AsyncRunner},
        };
        let container = Postgres::default()
            // Same PostgreSQL image as tests/process_substrate.rs.
            .with_tag("18.4-alpine3.23")
            .with_cmd(signalbox_persistence::disposable_postgres_server_args())
            .with_mount(signalbox_persistence::disposable_postgres_state_tmpfs_from_example()?)
            .with_labels(signalbox_persistence::disposable_test_container_labels())
            .start()
            .await?;
        let url = format!(
            "postgres://postgres:postgres@{}:{}/postgres",
            container.get_host().await?,
            container.get_host_port_ipv4(5432).await?,
        );
        let options = signalbox_persistence::local_test_connection_options(&url)?;
        let control = sqlx::PgPool::connect_with(options.clone()).await?;
        let mut database = signalboxd::FencedHubDatabase::connect_with(options, None).await?;
        {
            let (ready, admission) = tokio::sync::oneshot::channel();
            tokio::select! {
                biased;
                () = super::monitor_runtime_guard(&mut database, ready) => panic!("the live guard must permit admission"),
                result = admission => result.expect("the watcher checked the live guard"),
            }
        }
        let guard_backend: i32 = sqlx::query_scalar(
            "SELECT DISTINCT pid FROM pg_locks
             WHERE locktype = 'advisory' AND mode = 'ExclusiveLock' AND granted
               AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
        )
        .fetch_one(&control)
        .await?;
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(guard_backend)
            .execute(&control)
            .await?;
        let mut database = Some(database);
        let result = run_guarded_incarnations(
            GuardRecoveryPolicy::new(Duration::from_secs(1), Duration::from_secs(2), None).unwrap(),
            |observer| {
                let mut database = database
                    .take()
                    .unwrap()
                    .with_recovery_observer(observer.clone());
                async move {
                    observer.guard_lost();
                    let (ready, admission) = tokio::sync::oneshot::channel();
                    let admission = async {
                        admission
                            .await
                            .expect("admission requires a fresh guard check");
                        panic!("workers must not start after guard death between admission checks");
                    };
                    tokio::select! {
                        biased;
                        () = super::monitor_runtime_guard(&mut database, ready) => {},
                        () = admission => {},
                    }
                    assert!(observer.is_recovering());
                    let _ = database.close().await;
                    GuardedIncarnationOutcome::Finished(())
                }
            },
            std::future::pending(),
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), result).await?,
            Ok(())
        );
        control.close().await;
        drop(container);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn guarded_startup_interrupts_ambiguous_supervision_reconciliation_after_database_loss()
    -> Result<(), Box<dyn std::error::Error>> {
        use signalbox_domain::{
            CreateSession, DirectModelSelection, DurableCommandId, ModelSelectionRequest,
            SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance,
            SessionId, TranscriptAncestry,
        };
        use signalbox_persistence::{
            SessionCredentialPin, SessionModelCredential,
            create_session::CreateSessionRepository,
            session_lifecycle::SessionLifecycleRepositoryError,
            startup::{StartupScanCorruption, StartupScanRepositoryError},
        };
        use signalboxd::guard_recovery::{
            GuardRecoveryPolicy, GuardedIncarnationOutcome, run_guarded_incarnations,
        };
        use testcontainers_modules::{
            postgres::Postgres,
            testcontainers::{ImageExt, runners::AsyncRunner},
        };
        let container = Postgres::default()
            // Same PostgreSQL image as tests/process_substrate.rs.
            .with_tag("18.4-alpine3.23")
            .with_cmd(signalbox_persistence::disposable_postgres_server_args())
            .with_mount(signalbox_persistence::disposable_postgres_state_tmpfs_from_example()?)
            .with_labels(signalbox_persistence::disposable_test_container_labels())
            .start()
            .await?;
        let url = format!(
            "postgres://postgres:postgres@{}:{}/postgres",
            container.get_host().await?,
            container.get_host_port_ipv4(5432).await?,
        );
        let database = signalboxd::FencedHubDatabase::connect_with(
            signalbox_persistence::local_test_connection_options(&url)?,
            None,
        )
        .await?;
        signalbox_persistence::migrate(database.pool()).await?;
        let session = SessionId::from_uuid(uuid::Uuid::from_u128(1));
        let creation = CreateSession::new(
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(2)),
            SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::None,
            ),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(uuid::Uuid::from_u128(3)),
            )),
        )
        .prepare(session)
        .unwrap();
        CreateSessionRepository::new(
            database.pool().clone(),
            SessionCredentialPin::try_new(vec![SessionModelCredential::new(
                "startup-test-family",
                "startup-test-profile",
            )])
            .expect("the startup fixture has one credential pin"),
        )
        .handle(creation)
        .await?;
        let failure = StartupScanRepositoryError::from(StartupScanCorruption::Missing(
            "startup supervision fixture",
        ));
        let mut database = Some(database);
        let result = run_guarded_incarnations(
            GuardRecoveryPolicy::new(Duration::from_secs(1), Duration::from_secs(2), None).unwrap(),
            |observer| {
                let mut database = database
                    .take()
                    .unwrap()
                    .with_recovery_observer(observer.clone());
                let pool = database.pool().clone();
                let container = &container;
                let failure = &failure;
                async move {
                    let result = {
                        let (ambiguous, observed) = tokio::sync::oneshot::channel();
                        let mut supervision_write = signalbox_persistence::session_lifecycle::SessionSupervisionWrite::default();
                        let record =
                        signalbox_persistence::test_support::record_supervision_failure_with_commit(
                            &pool,
                            session,
                            failure,
                            &mut supervision_write,
                            |transaction| async move {
                                transaction.commit().await?;
                                container
                                    .stop()
                                    .await
                                    .expect("stop the disposable PostgreSQL server");
                                ambiguous.send(()).expect("observe the ambiguous acknowledgement");
                                Err(SessionLifecycleRepositoryError::CommitAmbiguous(
                                    sqlx::Error::Io(std::io::ErrorKind::ConnectionReset.into()),
                                ))
                            },
                        );
                        tokio::pin!(record);
                        tokio::select! {
                            biased;
                            result = &mut record => panic!("identity reconciliation ended while PostgreSQL was down: {result:?}"),
                            result = observed => result.expect("the park entered identity reconciliation"),
                        }
                        super::await_while_guarded(&mut database, &mut record).await
                    };
                    assert!(matches!(result, super::GuardedAwait::GuardLost));
                    assert!(observer.is_recovering());
                    let _ = database.close().await;
                    GuardedIncarnationOutcome::Finished(())
                }
            },
            std::future::pending(),
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), result).await?,
            Ok(())
        );
        drop(container);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn guard_recovery_retries_database_reconstruction_failures_with_capped_backoff() {
        use signalboxd::guard_recovery::{GuardRecoveryPolicy, run_guarded_incarnations};
        let failures = RefCell::new(VecDeque::from([
            Ok(ShutdownOutcome::GuardLost),
            Err(super::erase_startup_cause(
                RuntimePhase::Migration,
                super::SanitizedStartupCause::Migration(&sqlx::migrate::MigrateError::Execute(
                    sqlx::Error::PoolClosed,
                )),
            )),
            Err(HubRuntimeError::startup_scan(
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                },
                None,
                None,
            )),
            Err(super::erase_startup_database_cause(
                RuntimePhase::Configuration,
                super::SanitizedStartupCause::Static("repository_watch_startup_failed"),
            )),
            Err(super::erase_startup_database_cause(
                RuntimePhase::StartupScan,
                super::SanitizedStartupCause::Static("approval_wait_deadline_restore_failed"),
            )),
            Ok(ShutdownOutcome::Clean),
        ]));
        let attempts = RefCell::new(Vec::new());
        let started = tokio::time::Instant::now();
        let result = run_guarded_incarnations(
            GuardRecoveryPolicy::new(
                Duration::from_secs(1),
                Duration::from_secs(2),
                Some(Duration::from_secs(10)),
            )
            .unwrap(),
            |observer| {
                attempts.borrow_mut().push(started.elapsed());
                let result = failures
                    .borrow_mut()
                    .pop_front()
                    .expect("six reconstruction attempts");
                ready(super::recovery_incarnation_outcome(
                    result,
                    observer.is_recovering(),
                ))
            },
            pending(),
        )
        .await;
        assert_eq!(result, Ok(Ok(ShutdownOutcome::Clean)));
        assert_eq!(
            *attempts.borrow(),
            [
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(3),
                Duration::from_secs(5),
                Duration::from_secs(7),
                Duration::from_secs(9)
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn guard_recovery_reports_a_failed_termination_listener() {
        use signalboxd::guard_recovery::{GuardRecoveryPolicy, GuardedIncarnationOutcome};
        let result = super::run_hub_recovery(
            GuardRecoveryPolicy::new(Duration::from_secs(1), Duration::from_secs(10), None)
                .unwrap(),
            |observer| async move {
                observer.guard_lost();
                pending::<GuardedIncarnationOutcome<Result<ShutdownOutcome, HubRuntimeError>>>()
                    .await
            },
            Err(std::io::Error::from(std::io::ErrorKind::Other)),
        )
        .await
        .unwrap();
        assert_eq!(result, ShutdownOutcome::SignalListenerFailed);
    }

    #[test]
    fn reload_recovery_reacquires_only_database_failures() {
        use signalbox_persistence::reload_configuration::ReloadRepositoryError;
        use signalboxd::guard_recovery::GuardedIncarnationOutcome;
        for (failure, expected_class, database) in [
            (
                ReloadRepositoryError::Database(sqlx::Error::PoolClosed),
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                },
                true,
            ),
            (
                ReloadRepositoryError::CommitAmbiguous(sqlx::Error::PoolClosed),
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                },
                true,
            ),
            (
                ReloadRepositoryError::Corruption("fixture snapshot is incompatible"),
                OperatorFailureClass::FailClosedCorruption,
                false,
            ),
            (
                ReloadRepositoryError::InvalidCommandId,
                OperatorFailureClass::CallerOrHubBug,
                false,
            ),
        ] {
            let error = super::reload_recovery_failure(&failure);
            assert_eq!(error.failure_class, expected_class, "{failure}");
            assert_eq!(
                matches!(
                    super::recovery_incarnation_outcome(Err(error), true),
                    GuardedIncarnationOutcome::Reacquire
                ),
                database,
                "{failure}"
            );
        }
    }

    #[test]
    fn fence_recovery_surfaces_corruption_and_retries_database_failures() {
        use signalbox_persistence::hub_fence::{HubFenceCorruption, HubFenceError};
        use signalboxd::guard_recovery::GuardedIncarnationOutcome;
        let corrupt = signalboxd::FencedHubDatabaseError::AdvanceFence(HubFenceError::Corruption(
            HubFenceCorruption::GenerationExhausted,
        ));
        let corrupt = super::erase_startup_cause(
            RuntimePhase::DatabaseConnection,
            super::SanitizedStartupCause::Database(&corrupt),
        );
        assert_eq!(
            corrupt.failure_class,
            OperatorFailureClass::FailClosedCorruption
        );
        assert!(
            matches!(super::recovery_incarnation_outcome(Err(corrupt), true), GuardedIncarnationOutcome::Finished(Err(error)) if error == corrupt)
        );
        let unavailable = signalboxd::FencedHubDatabaseError::AdvanceFence(
            HubFenceError::Database(sqlx::Error::PoolClosed),
        );
        let unavailable = super::erase_startup_cause(
            RuntimePhase::DatabaseConnection,
            super::SanitizedStartupCause::Database(&unavailable),
        );
        assert!(matches!(
            super::recovery_incarnation_outcome(Err(unavailable), true),
            GuardedIncarnationOutcome::Reacquire
        ));
    }

    #[tokio::test]
    async fn migration_recovery_preserves_database_and_version_failures() {
        use signalboxd::guard_recovery::GuardedIncarnationOutcome;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://fixture:fixture@localhost/fixture")
            .unwrap();
        pool.close().await;
        let unavailable = super::migrate_hub_database(&pool)
            .await
            .expect_err("closed pool cannot migrate");
        assert!(matches!(
            super::recovery_incarnation_outcome(Err(unavailable), true),
            GuardedIncarnationOutcome::Reacquire
        ));
        let mismatch = sqlx::migrate::MigrateError::VersionMismatch(1);
        let migration = super::erase_startup_cause(
            RuntimePhase::Migration,
            super::SanitizedStartupCause::Migration(&mismatch),
        );
        assert!(
            matches!(super::recovery_incarnation_outcome(Err(migration), true), GuardedIncarnationOutcome::Finished(Err(error)) if error == migration)
        );
        let initialization = signalboxd::FencedHubDatabaseError::InitializeFence(mismatch);
        let initialization = super::erase_startup_cause(
            RuntimePhase::Migration,
            super::SanitizedStartupCause::Database(&initialization),
        );
        assert!(
            matches!(super::recovery_incarnation_outcome(Err(initialization), true), GuardedIncarnationOutcome::Finished(Err(error)) if error == initialization)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn automatic_resumption_inventory_failure_keeps_guard_recovery_pending() {
        use signalbox_persistence::goal::GoalRepositoryError;
        use signalboxd::PostgresGoalPassDispositionError;
        use signalboxd::guard_recovery::{GuardRecoveryPolicy, run_guarded_incarnations};
        let database_failure = super::startup_goal_resumption_result(
            Err(PostgresGoalPassDispositionError::Repository(
                GoalRepositoryError::Database(sqlx::Error::PoolClosed),
            )),
            true,
        )
        .expect_err("unrestored goal timers prevent recovery admission");
        let ambiguous = super::startup_goal_resumption_result(
            Err(PostgresGoalPassDispositionError::Repository(
                GoalRepositoryError::CommitAmbiguous(sqlx::Error::PoolClosed),
            )),
            true,
        )
        .expect_err("an ambiguous inventory failure prevents recovery admission");
        assert_eq!(
            ambiguous.failure_class,
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            }
        );
        let failures = RefCell::new(VecDeque::from([
            Ok(ShutdownOutcome::GuardLost),
            Err(database_failure),
            Err(ambiguous),
            Ok(ShutdownOutcome::Clean),
        ]));
        let started = tokio::time::Instant::now();
        let attempts = RefCell::new(Vec::new());
        let result = run_guarded_incarnations(
            GuardRecoveryPolicy::new(
                Duration::from_secs(1),
                Duration::from_secs(2),
                Some(Duration::from_secs(10)),
            )
            .unwrap(),
            |observer| {
                attempts.borrow_mut().push(started.elapsed());
                ready(super::recovery_incarnation_outcome(
                    failures
                        .borrow_mut()
                        .pop_front()
                        .expect("four reconstruction attempts"),
                    observer.is_recovering(),
                ))
            },
            pending(),
        )
        .await;
        assert_eq!(result, Ok(Ok(ShutdownOutcome::Clean)));
        assert_eq!(
            *attempts.borrow(),
            [
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(3),
                Duration::from_secs(5)
            ]
        );
    }

    #[test]
    fn automatic_resumption_inventory_preserves_initial_and_non_database_reporting() {
        use signalbox_persistence::goal::GoalRepositoryError;
        use signalboxd::PostgresGoalPassDispositionError;
        assert_eq!(
            super::startup_goal_resumption_result(
                Err(PostgresGoalPassDispositionError::Repository(
                    GoalRepositoryError::Database(sqlx::Error::PoolClosed),
                )),
                false,
            ),
            Ok(())
        );
        assert_eq!(
            super::startup_goal_resumption_result(
                Err(PostgresGoalPassDispositionError::InvalidStaticNeed),
                true,
            ),
            Ok(())
        );
        assert_eq!(super::startup_goal_resumption_result(Ok(1), true), Ok(()));
    }

    #[tokio::test(start_paused = true)]
    async fn blob_catalog_startup_failures_reacquire_with_capped_backoff() {
        use signalbox_persistence::blob::BlobCatalogRepositoryError;
        use signalboxd::guard_recovery::{GuardRecoveryPolicy, run_guarded_incarnations};
        use signalboxd::{BlobStoreRegistry, BlobStoreRegistryError};
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://fixture:fixture@localhost/fixture")
            .expect("fixture URL is valid");
        pool.close().await;
        let unavailable = BlobStoreRegistry::initialize(None, pool)
            .await
            .expect_err("catalog access reports the closed pool");
        let ambiguous = BlobStoreRegistryError::Catalog(
            BlobCatalogRepositoryError::CommitAmbiguous(sqlx::Error::PoolClosed),
        );
        let unavailable = super::erase_startup_cause(
            RuntimePhase::Configuration,
            super::SanitizedStartupCause::BlobStorage(&unavailable),
        );
        let ambiguous = super::erase_startup_cause(
            RuntimePhase::Configuration,
            super::SanitizedStartupCause::BlobStorage(&ambiguous),
        );
        assert_eq!(
            ambiguous.failure_class,
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            }
        );
        let failures = RefCell::new(VecDeque::from([
            Ok(ShutdownOutcome::GuardLost),
            Err(unavailable),
            Err(ambiguous),
            Ok(ShutdownOutcome::Clean),
        ]));
        let started = tokio::time::Instant::now();
        let attempts = RefCell::new(Vec::new());
        let result = run_guarded_incarnations(
            GuardRecoveryPolicy::new(
                Duration::from_secs(1),
                Duration::from_secs(2),
                Some(Duration::from_secs(10)),
            )
            .unwrap(),
            |observer| {
                attempts.borrow_mut().push(started.elapsed());
                ready(super::recovery_incarnation_outcome(
                    failures
                        .borrow_mut()
                        .pop_front()
                        .expect("four blob recovery attempts"),
                    observer.is_recovering(),
                ))
            },
            pending(),
        )
        .await;
        assert_eq!(result, Ok(Ok(ShutdownOutcome::Clean)));
        assert_eq!(
            *attempts.borrow(),
            [
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(3),
                Duration::from_secs(5)
            ]
        );
    }

    #[test]
    fn blob_configuration_and_corruption_failures_do_not_reacquire_the_database() {
        use signalbox_persistence::blob::{BlobCatalogCorruption, BlobCatalogRepositoryError};
        use signalboxd::BlobStoreRegistryError;
        use signalboxd::guard_recovery::GuardedIncarnationOutcome;
        for failure in [
            BlobStoreRegistryError::ConfigurationRequired,
            BlobStoreRegistryError::S3StartupDeadline,
            BlobStoreRegistryError::Catalog(BlobCatalogRepositoryError::Corruption(
                BlobCatalogCorruption::InvalidDigest,
            )),
        ] {
            let error = super::erase_startup_cause(
                RuntimePhase::Configuration,
                super::SanitizedStartupCause::BlobStorage(&failure),
            );
            assert!(
                matches!(super::recovery_incarnation_outcome(Err(error), true), GuardedIncarnationOutcome::Finished(Err(observed)) if observed == error)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn guard_recovery_reconstruction_failures_reach_the_configured_elapsed_bound() {
        use signalboxd::guard_recovery::{
            GuardRecoveryPolicy, GuardRecoveryStop, run_guarded_incarnations,
        };
        let result = run_guarded_incarnations(
            GuardRecoveryPolicy::new(
                Duration::from_secs(1),
                Duration::from_secs(2),
                Some(Duration::from_secs(4)),
            )
            .unwrap(),
            |observer| {
                let result = if observer.is_recovering() {
                    Err(HubRuntimeError::startup_scan(
                        OperatorFailureClass::Infrastructure {
                            commit_ambiguous: true,
                        },
                        None,
                        None,
                    ))
                } else {
                    Ok(ShutdownOutcome::GuardLost)
                };
                ready(super::recovery_incarnation_outcome(
                    result,
                    observer.is_recovering(),
                ))
            },
            pending(),
        )
        .await;
        assert_eq!(result, Err(GuardRecoveryStop::ElapsedBoundExhausted));
    }

    #[test]
    fn guard_recovery_preserves_initial_startup_and_non_database_failures() {
        use signalboxd::guard_recovery::GuardedIncarnationOutcome;
        let migration = HubRuntimeError::infrastructure(RuntimePhase::Migration);
        assert!(
            matches!(super::recovery_incarnation_outcome(Err(migration), false), GuardedIncarnationOutcome::Finished(Err(error)) if error == migration)
        );
        let filesystem = HubRuntimeError::infrastructure(RuntimePhase::StartupScan);
        assert!(
            matches!(super::recovery_incarnation_outcome(Err(filesystem), true), GuardedIncarnationOutcome::Finished(Err(error)) if error == filesystem)
        );
        let corruption =
            HubRuntimeError::startup_scan(OperatorFailureClass::FailClosedCorruption, None, None);
        assert!(
            matches!(super::recovery_incarnation_outcome(Err(corruption), true), GuardedIncarnationOutcome::Finished(Err(error)) if error == corruption)
        );
    }

    #[tokio::test]
    async fn adr0044_migration_precedes_scan_and_scheduling() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let migration_events = Rc::clone(&events);
        let scan_events = Rc::clone(&events);
        let scheduling_events = Rc::clone(&events);

        let result = migrate_scan_then_schedule(
            async move {
                migration_events.borrow_mut().push("migration");
                Ok(())
            },
            async move {
                scan_events.borrow_mut().push("startup_scan");
                Ok(())
            },
            || async move {
                scheduling_events.borrow_mut().push("scheduling");
                7
            },
        )
        .await;

        assert_eq!(result, Ok(7));
        assert_eq!(
            events.borrow().as_slice(),
            ["migration", "startup_scan", "scheduling"]
        );
    }

    #[tokio::test]
    async fn adr0044_failed_migration_prevents_scan_and_scheduling() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let migration_events = Rc::clone(&events);
        let scan_events = Rc::clone(&events);
        let scheduling_events = Rc::clone(&events);
        let failure = HubRuntimeError::infrastructure(RuntimePhase::Migration);

        let result = migrate_scan_then_schedule(
            async move {
                migration_events.borrow_mut().push("migration");
                Err(failure)
            },
            async move {
                scan_events.borrow_mut().push("startup_scan");
                Ok(())
            },
            || async move {
                scheduling_events.borrow_mut().push("scheduling");
            },
        )
        .await;

        assert_eq!(result, Err(failure));
        assert_eq!(events.borrow().as_slice(), ["migration"]);
    }

    #[test]
    fn deployment_paths_and_database_url_are_validated() {
        assert_eq!(
            HubConfiguration::from_values(HubConfigurationValues {
                database_url: None,
                ..hub_configuration_values()
            })
            .err(),
            Some(HubConfigurationError::new(
                DATABASE_URL_ENVIRONMENT,
                RequiredSettingFailure::Missing,
            ))
        );
        assert_eq!(
            HubConfiguration::from_values(HubConfigurationValues {
                model_configuration_file: Some(OsString::from("")),
                ..hub_configuration_values()
            })
            .err(),
            Some(HubConfigurationError::new(
                MODEL_CONFIGURATION_FILE_ENVIRONMENT,
                RequiredSettingFailure::Empty,
            ))
        );
        assert_eq!(
            HubConfiguration::from_values(HubConfigurationValues {
                template_configuration_file: None,
                ..hub_configuration_values()
            })
            .err(),
            Some(HubConfigurationError::new(
                TEMPLATE_CONFIGURATION_FILE_ENVIRONMENT,
                RequiredSettingFailure::Missing,
            ))
        );
        assert_eq!(
            HubConfiguration::from_values(HubConfigurationValues {
                brave_api_key_file: None,
                ..hub_configuration_values()
            })
            .err(),
            Some(HubConfigurationError::new(
                BRAVE_API_KEY_FILE_ENVIRONMENT,
                RequiredSettingFailure::Missing,
            ))
        );
        assert!(
            HubConfiguration::from_values(HubConfigurationValues {
                github_token_file: None,
                ..hub_configuration_values()
            })
            .is_ok()
        );
        assert_eq!(
            HubConfiguration::from_values(HubConfigurationValues {
                process_socket_path: None,
                ..hub_configuration_values()
            })
            .err(),
            Some(HubConfigurationError::new(
                PROCESS_SOCKET_PATH_ENVIRONMENT,
                RequiredSettingFailure::Missing,
            ))
        );
        let defaulted_runner_socket = HubConfiguration::from_values(HubConfigurationValues {
            runner_socket_path: None,
            ..hub_configuration_values()
        })
        .expect("an omitted runner socket uses the process-socket sibling");
        assert_eq!(
            defaulted_runner_socket.runner_socket_path(),
            std::path::Path::new("/tmp/signalbox.runner.sock")
        );
        assert_eq!(
            HubConfiguration::from_values(HubConfigurationValues {
                runner_socket_path: Some(OsString::from("")),
                ..hub_configuration_values()
            })
            .err(),
            Some(HubConfigurationError::new(
                RUNNER_SOCKET_PATH_ENVIRONMENT,
                RequiredSettingFailure::Empty,
            ))
        );

        let configuration = HubConfiguration::from_values(hub_configuration_values())
            .expect("nonempty deployment values are accepted before I/O");
        assert_eq!(configuration.database_url(), "postgres://secret");
        assert_eq!(
            configuration.model_configuration_file(),
            std::path::Path::new("models.toml")
        );
        assert_eq!(
            configuration.template_configuration_file(),
            std::path::Path::new("templates.toml")
        );
        assert_eq!(
            configuration.brave_api_key_file(),
            std::path::PathBuf::from(BRAVE_KEY_FILE_FIXTURE)
        );
        assert_eq!(
            configuration.github_token_file(),
            std::path::PathBuf::from("github-token")
        );
        assert_eq!(
            configuration.process_socket_path(),
            std::path::Path::new("/tmp/signalbox.sock")
        );
        assert_eq!(
            configuration.runner_socket_path(),
            std::path::Path::new("/tmp/signalbox-runner.sock")
        );
    }

    #[test]
    fn oauth_root_remains_distinct_when_the_socket_ends_in_oauth() {
        let socket = std::path::Path::new("/tmp/signalbox.oauth");
        let root = super::oauth_credential_root(socket);
        assert_ne!(root, socket);
        assert_eq!(root, std::path::Path::new("/tmp/signalbox.oauth.oauth"));
        assert_ne!(
            root,
            super::oauth_credential_root(std::path::Path::new("/tmp/signalbox.sock"))
        );
    }

    #[test]
    fn runner_socket_cannot_replace_the_oauth_credential_root() {
        let error = HubConfiguration::from_values(HubConfigurationValues {
            process_socket_path: Some(OsString::from("/tmp/signalbox.sock")),
            runner_socket_path: Some(OsString::from("/tmp/signalbox.sock.oauth")),
            ..hub_configuration_values()
        })
        .err()
        .expect("the OAuth root is a reserved process socket artifact");
        assert_eq!(
            error,
            HubConfigurationError::new(
                RUNNER_SOCKET_PATH_ENVIRONMENT,
                RequiredSettingFailure::Conflicts,
            )
        );
    }

    #[test]
    fn runner_socket_cannot_contain_or_enter_the_oauth_credential_root() {
        for runner in [
            "/tmp/oauth-overlap/process.sock.oauth/runner.sock",
            "/tmp/oauth-overlap",
        ] {
            let error = HubConfiguration::from_values(HubConfigurationValues {
                process_socket_path: Some(OsString::from("/tmp/oauth-overlap/process.sock")),
                runner_socket_path: Some(OsString::from(runner)),
                ..hub_configuration_values()
            })
            .err()
            .expect("runner artifacts cannot overlap the OAuth directory");
            assert_eq!(
                error,
                HubConfigurationError::new(
                    RUNNER_SOCKET_PATH_ENVIRONMENT,
                    RequiredSettingFailure::Conflicts,
                ),
                "{runner}",
            );
        }
        HubConfiguration::from_values(HubConfigurationValues {
            process_socket_path: Some(OsString::from("/tmp/oauth-overlap/process.sock")),
            runner_socket_path: Some(OsString::from(
                "/tmp/oauth-overlap/process.sock.oauth-sibling/runner.sock",
            )),
            ..hub_configuration_values()
        })
        .expect("a shared name prefix does not overlap directory components");
    }

    #[test]
    fn default_runner_socket_replaces_only_the_final_extension() {
        let process_socket = OsString::from("/tmp/signalbox.runner.sock");
        let expected_runner_socket = std::path::Path::new("/tmp/signalbox.runner.runner.sock");
        let configuration = HubConfiguration::from_values(HubConfigurationValues {
            process_socket_path: Some(process_socket),
            runner_socket_path: None,
            ..hub_configuration_values()
        })
        .expect("the derived runner socket remains a distinct sibling");

        assert_eq!(configuration.runner_socket_path(), expected_runner_socket);
    }

    #[test]
    fn explicit_runner_socket_cannot_equal_the_process_socket() {
        let shared_socket = OsString::from("/tmp/signalbox.sock");
        let error = HubConfiguration::from_values(HubConfigurationValues {
            process_socket_path: Some(shared_socket.clone()),
            runner_socket_path: Some(shared_socket),
            ..hub_configuration_values()
        })
        .err()
        .expect("the two listeners cannot share a filesystem path");

        assert_eq!(
            error,
            HubConfigurationError::new(
                RUNNER_SOCKET_PATH_ENVIRONMENT,
                RequiredSettingFailure::Conflicts,
            )
        );
    }

    #[test]
    fn socket_parent_aliases_cannot_resolve_to_the_same_artifacts() {
        let directory = tempfile::tempdir().expect("the socket fixture directory exists");
        let canonical_parent = directory.path().join("canonical");
        std::fs::create_dir(&canonical_parent).expect("the canonical parent exists");
        let alias_parent = directory.path().join("alias");
        std::os::unix::fs::symlink(&canonical_parent, &alias_parent)
            .expect("the parent alias exists");
        let process_socket = canonical_parent.join("signalbox.sock");
        let runner_socket = alias_parent.join("signalbox.sock");

        let error = HubConfiguration::from_values(HubConfigurationValues {
            process_socket_path: Some(process_socket.into_os_string()),
            runner_socket_path: Some(runner_socket.into_os_string()),
            ..hub_configuration_values()
        })
        .err()
        .expect("resolved listener artifacts cannot overlap");

        assert_eq!(
            error,
            HubConfigurationError::new(
                RUNNER_SOCKET_PATH_ENVIRONMENT,
                RequiredSettingFailure::Conflicts,
            )
        );
    }

    #[test]
    fn repository_watch_credential_cannot_equal_the_github_tool_credential() {
        let credential = std::path::Path::new("/tmp/signalbox-github-token");

        assert!(credential_files_conflict(credential, credential));
    }

    #[test]
    fn repository_watch_credential_alias_cannot_reach_the_github_tool_credential() {
        let directory = tempfile::tempdir().expect("the credential fixture directory exists");
        let credential = directory.path().join("github-token");
        std::fs::write(&credential, []).expect("the credential fixture exists");
        let alias = directory.path().join("watch-token");
        std::os::unix::fs::symlink(&credential, &alias).expect("the credential alias exists");

        assert!(credential_files_conflict(&credential, &alias));
    }

    #[cfg(unix)]
    #[test]
    fn repository_watch_hard_link_cannot_reach_the_github_tool_credential() {
        let directory = tempfile::tempdir().expect("the credential fixture directory exists");
        let credential = directory.path().join("github-token");
        std::fs::write(&credential, []).expect("the credential fixture exists");
        let hard_link = directory.path().join("watch-token");
        std::fs::hard_link(&credential, &hard_link).expect("the credential hard link exists");

        assert!(credential_files_conflict(&credential, &hard_link));
    }

    #[test]
    fn dangling_repository_watch_alias_cannot_reach_the_github_tool_credential() {
        let directory = tempfile::tempdir().expect("the credential fixture directory exists");
        let credential = directory.path().join("github-token");
        let alias = directory.path().join("watch-token");
        std::os::unix::fs::symlink(&credential, &alias).expect("the credential alias exists");

        assert!(credential_files_conflict(&credential, &alias));
    }

    #[test]
    fn unresolved_lexical_alias_cannot_reach_the_github_tool_credential() {
        let directory = tempfile::tempdir().expect("the credential fixture directory exists");
        let credential = directory.path().join("github-token");
        let alias = directory.path().join("pending/../github-token");

        assert!(credential_files_conflict(&credential, &alias));
    }

    #[test]
    fn dangling_intermediate_alias_cannot_reach_the_github_tool_credential() {
        let directory = tempfile::tempdir().expect("the credential fixture directory exists");
        let target_directory = directory.path().join("pending-target");
        let alias_directory = directory.path().join("pending-alias");
        std::os::unix::fs::symlink(&target_directory, &alias_directory)
            .expect("the intermediate credential alias exists");
        let credential = target_directory.join("github-token");
        let alias = alias_directory.join("github-token");

        assert!(credential_files_conflict(&credential, &alias));
    }

    #[test]
    fn runner_socket_cannot_collide_with_a_process_socket_sidecar() {
        let process_socket = std::path::PathBuf::from("/tmp/signalbox.sock");
        let mut runner_socket = process_socket.as_os_str().to_owned();
        runner_socket.push(".lock");

        let error = HubConfiguration::from_values(HubConfigurationValues {
            process_socket_path: Some(process_socket.into_os_string()),
            runner_socket_path: Some(runner_socket),
            ..hub_configuration_values()
        })
        .err()
        .expect("listener public paths cannot overlap peer sidecars");

        assert_eq!(
            error,
            HubConfigurationError::new(
                RUNNER_SOCKET_PATH_ENVIRONMENT,
                RequiredSettingFailure::Conflicts,
            )
        );
    }

    #[test]
    fn adr0044_startup_corruption_retains_safe_aggregate_context() {
        let session = SessionId::from_uuid(Uuid::from_u128(1));
        let turn = TurnId::from_uuid(Uuid::from_u128(2));

        assert_eq!(
            HubRuntimeError::startup_scan(
                OperatorFailureClass::FailClosedCorruption,
                Some(session),
                Some(turn),
            ),
            HubRuntimeError {
                phase: RuntimePhase::StartupScan,
                failure_class: OperatorFailureClass::FailClosedCorruption,
                database_failure: true,
                session: Some(session),
                turn: Some(turn),
            }
        );
    }

    #[derive(Clone, Copy, Debug)]
    struct FakeFailure;

    impl ClassifyOperatorFailure for FakeFailure {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            }
        }
    }

    struct OneHintThenPending {
        hints: VecDeque<SessionId>,
    }

    impl EligibilityWorkSource for OneHintThenPending {
        type Error = FakeFailure;

        async fn next(&mut self) -> Result<SessionId, Self::Error> {
            match self.hints.pop_front() {
                Some(session) => Ok(session),
                None => pending().await,
            }
        }
    }

    #[derive(Clone)]
    struct BlockingPass {
        entered: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    }

    impl EligibilityPass for BlockingPass {
        type Error = FakeFailure;

        fn run(
            &mut self,
            _session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            let entered = self
                .entered
                .lock()
                .expect("the fake pass state is not poisoned")
                .take()
                .expect("the test pass runs once");
            async move {
                entered.send(()).expect("the test waits for pass entry");
                pending().await
            }
        }
    }

    #[derive(Clone)]
    struct DelayedPass {
        entered: Arc<Mutex<Option<oneshot::Sender<()>>>>,
        duration: Duration,
    }

    impl EligibilityPass for DelayedPass {
        type Error = FakeFailure;

        fn run(
            &mut self,
            _session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            let entered = self
                .entered
                .lock()
                .expect("the fake pass state is not poisoned")
                .take()
                .expect("the test pass runs once");
            let duration = self.duration;
            async move {
                entered.send(()).expect("the test waits for pass entry");
                tokio::time::sleep(duration).await;
                Ok(())
            }
        }
    }

    struct PendingWorkSource;

    impl EligibilityWorkSource for PendingWorkSource {
        type Error = FakeFailure;

        async fn next(&mut self) -> Result<SessionId, Self::Error> {
            pending().await
        }
    }

    #[derive(Clone, Copy)]
    struct ReadyPass;

    impl EligibilityPass for ReadyPass {
        type Error = FakeFailure;

        fn run(
            &mut self,
            _session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            ready(Ok(()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn adr0044_shutdown_stops_admission_and_bounds_in_flight_work() {
        let (entered_sender, entered_receiver) = oneshot::channel();
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let session = SessionId::from_uuid(Uuid::from_u128(1));
        let scheduler = SchedulerLoop::new(
            OneHintThenPending {
                hints: VecDeque::from([session]),
            },
            BlockingPass {
                entered: Arc::new(Mutex::new(Some(entered_sender))),
            },
        );
        let runtime = tokio::spawn(run_scheduler_until_shutdown(
            scheduler,
            async move {
                shutdown_receiver.await.expect("the test requests shutdown");
                SchedulerStopCause::Requested
            },
            Duration::from_secs(5),
        ));

        entered_receiver
            .await
            .expect("the scheduler admitted the first pass");
        shutdown_sender
            .send(())
            .expect("the scheduler still listens for shutdown");
        tokio::time::advance(Duration::from_secs(5)).await;

        assert_eq!(
            runtime.await.expect("the runtime task completes"),
            ShutdownOutcome::GraceWindowExpired
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_drain_includes_the_configured_cleanup_window() {
        let (entered_sender, entered_receiver) = oneshot::channel();
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        const ARBITRARY_SESSION_ID: u128 = 1;
        let session = SessionId::from_uuid(Uuid::from_u128(ARBITRARY_SESSION_ID));
        let pass_duration = Duration::from_secs(3);
        let scheduler = SchedulerLoop::new(
            OneHintThenPending {
                hints: VecDeque::from([session]),
            },
            DelayedPass {
                entered: Arc::new(Mutex::new(Some(entered_sender))),
                duration: pass_duration,
            },
        );
        let runtime = tokio::spawn(run_scheduler_until_shutdown(
            scheduler,
            async move {
                shutdown_receiver.await.expect("the test requests shutdown");
                SchedulerStopCause::Requested
            },
            graceful_shutdown_window(Some(Duration::from_secs(2)), Some(Duration::from_secs(2)))
                .expect("the fixture cleanup window is bounded"),
        ));

        entered_receiver
            .await
            .expect("the scheduler admitted the first pass");
        shutdown_sender
            .send(())
            .expect("the scheduler still listens for shutdown");
        // Let the paused clock process the pass and grace timers in deadline order.
        assert_eq!(
            runtime.await.expect("the runtime task completes"),
            ShutdownOutcome::Clean
        );
    }

    #[tokio::test]
    async fn adr0044_idle_scheduler_exits_cleanly_on_shutdown() {
        let scheduler = SchedulerLoop::new(PendingWorkSource, ReadyPass);

        assert_eq!(
            run_scheduler_until_shutdown(
                scheduler,
                ready(SchedulerStopCause::Requested),
                Duration::from_secs(1),
            )
            .await,
            ShutdownOutcome::Clean
        );
    }

    #[tokio::test]
    async fn post_activation_execution_failure_stops_the_scheduler() {
        let scheduler = SchedulerLoop::new(PendingWorkSource, ReadyPass);

        assert_eq!(
            run_scheduler_until_shutdown(
                scheduler,
                ready(SchedulerStopCause::ExecutionFailed),
                Duration::from_secs(1),
            )
            .await,
            ShutdownOutcome::ExecutionFailed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn execution_failure_preserves_an_expired_grace_window() {
        let (entered_sender, entered_receiver) = oneshot::channel();
        let (failure_sender, failure_receiver) = oneshot::channel();
        let session = SessionId::from_uuid(Uuid::from_u128(1));
        let scheduler = SchedulerLoop::new(
            OneHintThenPending {
                hints: VecDeque::from([session]),
            },
            BlockingPass {
                entered: Arc::new(Mutex::new(Some(entered_sender))),
            },
        );
        let runtime = tokio::spawn(run_scheduler_until_shutdown(
            scheduler,
            async move {
                failure_receiver
                    .await
                    .expect("the execution supervisor reports failure");
                SchedulerStopCause::ExecutionFailed
            },
            Duration::from_secs(5),
        ));

        entered_receiver
            .await
            .expect("the scheduler admitted the first pass");
        failure_sender
            .send(())
            .expect("the scheduler still listens for execution failure");
        tokio::time::advance(Duration::from_secs(5)).await;

        assert_eq!(
            runtime.await.expect("the runtime task completes"),
            ShutdownOutcome::ExecutionFailedAfterGraceWindow
        );
    }

    #[tokio::test(start_paused = true)]
    async fn adr0044_signal_listener_failure_precedes_expired_grace_window() {
        let (entered_sender, entered_receiver) = oneshot::channel();
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let session = SessionId::from_uuid(Uuid::from_u128(1));
        let scheduler = SchedulerLoop::new(
            OneHintThenPending {
                hints: VecDeque::from([session]),
            },
            BlockingPass {
                entered: Arc::new(Mutex::new(Some(entered_sender))),
            },
        );
        let runtime = tokio::spawn(run_scheduler_until_shutdown(
            scheduler,
            async move {
                shutdown_receiver
                    .await
                    .expect("the listener reports failure");
                SchedulerStopCause::SignalListenerFailed
            },
            Duration::from_secs(5),
        ));

        entered_receiver
            .await
            .expect("the scheduler admitted the first pass");
        shutdown_sender
            .send(())
            .expect("the scheduler still listens for shutdown");
        tokio::time::advance(Duration::from_secs(5)).await;

        assert_eq!(
            runtime.await.expect("the runtime task completes"),
            ShutdownOutcome::SignalListenerFailed
        );
    }

    #[test]
    fn adr0044_expired_or_failed_shutdown_skips_unbounded_pool_drain() {
        assert!(!should_close_pool(&Ok(ShutdownOutcome::GraceWindowExpired)));
        assert!(!should_close_pool(&Ok(
            ShutdownOutcome::SignalListenerFailed
        )));
        assert!(!should_close_pool(&Ok(
            ShutdownOutcome::ExecutionFailedAfterGraceWindow
        )));
        assert!(!should_close_pool(&Ok(ShutdownOutcome::GuardLost)));
        assert!(!should_close_pool(&Ok(
            ShutdownOutcome::RuntimeFailedAfterGraceWindow
        )));
        assert!(!should_close_pool(&Ok(
            ShutdownOutcome::RuntimeDefectAfterGraceWindow
        )));
        assert!(should_close_pool(&Ok(ShutdownOutcome::ExecutionFailed)));
        assert!(should_close_pool(&Ok(ShutdownOutcome::RuntimeFailed)));
        assert!(should_close_pool(&Ok(ShutdownOutcome::RuntimeDefect)));
        assert!(should_close_pool(&Ok(ShutdownOutcome::Clean)));
        assert!(should_close_pool(&Err(HubRuntimeError::infrastructure(
            RuntimePhase::Migration
        ))));
    }

    #[test]
    fn database_close_failure_preserves_higher_signal_initiating_causes() {
        assert_eq!(
            database_close_failure_outcome(ShutdownOutcome::RuntimeDefect),
            ShutdownOutcome::RuntimeDefect
        );
        assert_eq!(
            database_close_failure_outcome(ShutdownOutcome::ExecutionFailed),
            ShutdownOutcome::ExecutionFailed
        );
    }

    #[test]
    fn ordinary_database_close_failure_remains_a_runtime_failure() {
        assert_eq!(
            database_close_failure_outcome(ShutdownOutcome::Clean),
            ShutdownOutcome::RuntimeFailed
        );
    }

    #[test]
    fn staging_sweep_failure_preserves_higher_signal_initiating_causes() {
        assert_eq!(
            staging_sweep_failure_outcome(ShutdownOutcome::RuntimeDefect),
            ShutdownOutcome::RuntimeDefect
        );
        assert_eq!(
            staging_sweep_failure_outcome(ShutdownOutcome::ExecutionFailed),
            ShutdownOutcome::ExecutionFailed
        );
    }

    #[test]
    fn clean_staging_sweep_failure_becomes_a_runtime_failure() {
        assert_eq!(
            staging_sweep_failure_outcome(ShutdownOutcome::Clean),
            ShutdownOutcome::RuntimeFailed
        );
    }

    #[test]
    fn staging_sweep_failure_preserves_the_expired_drain_decision() {
        let outcome = ShutdownOutcome::GraceWindowExpired;
        let close_pool = should_close_pool(&Ok(outcome));

        assert!(!close_pool);
        assert_eq!(
            staging_sweep_failure_outcome(outcome),
            ShutdownOutcome::RuntimeFailed
        );
    }

    #[test]
    fn database_close_failure_omits_dynamic_sqlx_detail() {
        let dynamic_detail = "synthetic-database-url-and-credential";
        let error = SingleHubGuardError::Close(sqlx::Error::Protocol(dynamic_detail.to_owned()));
        let encoded = capture_database_close_failure(&error);

        assert!(encoded.contains(&error.to_string()));
        assert!(!encoded.contains(dynamic_detail));
    }

    #[test]
    fn runtime_defect_outweighs_an_ordinary_drain_failure() {
        assert_eq!(
            combine_runtime_stop_cause(
                RuntimeStopCause::RuntimeDefect,
                RuntimeTaskCompletion::Failed
            ),
            RuntimeStopCause::RuntimeDefect
        );
        assert_eq!(
            combine_runtime_stop_cause(RuntimeStopCause::Requested, RuntimeTaskCompletion::Failed),
            RuntimeStopCause::RuntimeFailed
        );
        assert_eq!(
            combine_runtime_stop_cause(
                RuntimeStopCause::RuntimeFailed,
                RuntimeTaskCompletion::Defect
            ),
            RuntimeStopCause::RuntimeDefect
        );
    }

    #[test]
    fn initiating_failure_cause_outweighs_an_ordinary_drain_failure() {
        assert_eq!(
            combine_runtime_stop_cause(
                RuntimeStopCause::SignalListenerFailed,
                RuntimeTaskCompletion::Failed
            ),
            RuntimeStopCause::SignalListenerFailed
        );
        assert_eq!(
            combine_runtime_stop_cause(
                RuntimeStopCause::ExecutionFailed,
                RuntimeTaskCompletion::Failed
            ),
            RuntimeStopCause::ExecutionFailed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn second_termination_signal_interrupts_bounded_and_unbounded_drains() {
        for grace in [Some(Duration::from_secs(600)), None] {
            let (signals, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            signals.send(()).unwrap();
            receiver.recv().await.expect("first signal begins shutdown");
            let mut runtime_tasks = JoinSet::new();
            runtime_tasks.spawn(pending::<RuntimeTaskExit>());
            let drain = drain_runtime_tasks(
                &mut runtime_tasks,
                pending(),
                async {
                    receiver.recv().await.expect("second signal interrupts");
                },
                grace,
            );
            tokio::pin!(drain);
            assert!(timeout(Duration::from_secs(1), &mut drain).await.is_err());
            signals.send(()).unwrap();
            let (outcome, completion) = timeout(Duration::from_millis(1), drain).await.unwrap();
            assert_eq!(outcome, RuntimeDrainOutcome::Interrupted);
            assert_eq!(completion, RuntimeTaskCompletion::Clean);
            assert_eq!(
                completed_runtime_outcome(RuntimeStopCause::Requested, outcome),
                ShutdownOutcome::Interrupted
            );
            assert!(!should_close_pool(&Ok(ShutdownOutcome::Interrupted)));
        }
    }

    #[tokio::test]
    async fn termination_listeners_retain_signals_between_shutdown_and_drain() {
        let mut signals = super::TerminationSignals::new().unwrap();
        for signal in [rustix::process::Signal::TERM, rustix::process::Signal::INT] {
            rustix::process::kill_process(rustix::process::getpid(), signal).unwrap();
            assert!(
                !timeout(Duration::from_secs(5), signals.recv())
                    .await
                    .unwrap()
            );
            rustix::process::kill_process(rustix::process::getpid(), signal).unwrap();
            // Let Tokio deliver the signal before constructing the drain wait.
            tokio::task::yield_now().await;
            let mut tasks = JoinSet::new();
            tasks.spawn(pending::<RuntimeTaskExit>());
            let (outcome, _) = timeout(
                Duration::from_secs(5),
                drain_runtime_tasks(
                    &mut tasks,
                    pending(),
                    async {
                        assert!(!signals.recv().await);
                    },
                    None,
                ),
            )
            .await
            .unwrap();
            assert_eq!(outcome, RuntimeDrainOutcome::Interrupted);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn runtime_task_defect_before_drain_timeout_prevents_clean_exit() {
        let mut runtime_tasks: JoinSet<RuntimeTaskExit> = JoinSet::new();
        runtime_tasks.spawn(async {
            panic!("synthetic runtime task panic");
        });
        runtime_tasks.spawn(pending::<RuntimeTaskExit>());

        let (drain, completion) = drain_runtime_tasks(
            &mut runtime_tasks,
            pending(),
            pending(),
            Some(Duration::from_secs(5)),
        )
        .await;
        let cause = combine_runtime_stop_cause(RuntimeStopCause::Requested, completion);

        assert_eq!(drain, RuntimeDrainOutcome::GraceWindowExpired);
        assert_eq!(cause, RuntimeStopCause::RuntimeDefect);
        assert_eq!(
            completed_runtime_outcome(cause, drain),
            ShutdownOutcome::RuntimeDefectAfterGraceWindow
        );
    }

    #[tokio::test(start_paused = true)]
    async fn runtime_task_failure_before_drain_timeout_prevents_clean_exit() {
        let mut runtime_tasks: JoinSet<RuntimeTaskExit> = JoinSet::new();
        runtime_tasks.spawn(ready(RuntimeTaskExit::Process(Err(
            ProcessRuntimeError::EncodeInvariant,
        ))));
        runtime_tasks.spawn(pending::<RuntimeTaskExit>());

        let (drain, completion) = drain_runtime_tasks(
            &mut runtime_tasks,
            pending(),
            pending(),
            Some(Duration::from_secs(5)),
        )
        .await;
        let cause = combine_runtime_stop_cause(RuntimeStopCause::Requested, completion);

        assert_eq!(drain, RuntimeDrainOutcome::GraceWindowExpired);
        assert_eq!(cause, RuntimeStopCause::RuntimeFailed);
        assert_eq!(
            completed_runtime_outcome(cause, drain),
            ShutdownOutcome::RuntimeFailedAfterGraceWindow
        );
    }

    #[test]
    fn runtime_stop_causes_preserve_grace_and_fencing_policy() {
        assert_eq!(
            completed_runtime_outcome(RuntimeStopCause::Requested, RuntimeDrainOutcome::Complete),
            ShutdownOutcome::Clean
        );
        assert_eq!(
            completed_runtime_outcome(
                RuntimeStopCause::ExecutionFailed,
                RuntimeDrainOutcome::GraceWindowExpired
            ),
            ShutdownOutcome::ExecutionFailedAfterGraceWindow
        );
        assert_eq!(
            completed_runtime_outcome(
                RuntimeStopCause::RuntimeFailed,
                RuntimeDrainOutcome::GraceWindowExpired
            ),
            ShutdownOutcome::RuntimeFailedAfterGraceWindow
        );
        assert_eq!(
            completed_runtime_outcome(RuntimeStopCause::Requested, RuntimeDrainOutcome::GuardLost),
            ShutdownOutcome::GuardLost
        );
        assert_eq!(
            completed_runtime_outcome(
                RuntimeStopCause::RuntimeDefect,
                RuntimeDrainOutcome::Complete
            ),
            ShutdownOutcome::RuntimeDefect
        );
    }
}
