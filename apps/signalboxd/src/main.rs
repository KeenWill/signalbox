//! Signalbox daemon composition root.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md owns startup ordering
//! (migrate, scan, then schedule), graceful shutdown, and composition-root
//! wiring; docs/spec/runtime-substrate.md and
//! docs/spec/configuration-and-credentials.md keep runtime, subscriber,
//! deployment configuration, and migration policy at this executable
//! boundary.

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
    SchedulerPassOccupancyBound, StaleActiveTurnBound, StartEligibleTurnService,
    StartupScanService, TurnLivenessScanInterval, UuidV7StartEligibleTurnIdGenerator,
    UuidV7StartupScanIdGenerator,
};
#[cfg(test)]
use signalbox_application::{EligibilityPass, EligibilityWorkSource};
use signalbox_domain::{SessionId, TurnId};
use signalbox_model_provider_runtime::{
    ApprovalJudgeModel, ContextCompactionModel, RuntimeApprovalJudgeModel,
    RuntimeContextCompactionModel, RuntimeModelCallProvider,
};
use signalbox_model_runtime::CredentialReference;
use signalbox_model_runtime_anthropic::{
    AnthropicConfig, AnthropicConstructionError, AnthropicRuntime,
};
use signalbox_model_runtime_codex_cli::verify_pinned_codex_cli_version;
use signalbox_model_runtime_openai::{OpenAiConfig, OpenAiConstructionError, OpenAiRuntime};
use signalbox_persistence::{
    automatic_reconciliation::RETRY_LADDER_ARITY, blob::BlobCatalogRepository,
    hub_fence::FENCED_POOL_MAX_CONNECTIONS, migrate, model_execution::PostgresModelCallRepository,
    scheduler::PostgresEligibilitySweep, session_deadline::SessionDeadlineBounds,
    start_eligible_turn::StartEligibleTurnRepository, startup::PostgresStartupScanRepository,
    turn_liveness::TurnLivenessPersistenceBounds,
};
use signalbox_tools_web::BRAVE_SEARCH_CREDENTIAL_REFERENCE;
use signalboxd::runner_protocol_runtime::{
    PostgresRunnerRegistrationService, RunnerProtocolRuntime, RunnerProtocolRuntimeError,
    RunnerRegistrationFailureCause,
};
use signalboxd::{
    ActivatedTurnPass, AttachmentPreparingModelCallProvider, BaseDaemonCredentialInputs,
    BlobStoreRegistry, BlobTools, CODE_HOST_CREDENTIAL_REFERENCE, CodeHostNumericBounds,
    ConfiguredApprovalPostureError, DaemonToolCatalog, DaemonToolComposition, DaemonTools,
    DaemonToolsConstructionError, ExpiredPassRecoveryPolicy, FatalExecutionSupervisor,
    FencedHubDatabase, FencedHubDatabaseError, FencedPoolFloorReconciliation, FileCredentialAccess,
    GitHubCodeHostTransport, GoalModeNumericBounds, HubModelConfiguration,
    HubModelConfigurationError, LifecycleDeadlineRuntime, LifecycleMetricsRuntime,
    LocalProcessListener, LocalSocketError, MappedDaemonCredentialInputs, ModelAdapter,
    OtlpRuntime, PostgresGoalPassDisposition, PostgresProviderModelExecution, ProcessRuntime,
    ProcessRuntimeError, PrometheusServer, ReportedUsageCompaction, SessionTemplateConfiguration,
    SessionTemplateConfigurationError, SingleHubGuardError, SystemCurrentTimeClock,
    TelemetryConfiguration, TelemetryConfigurationError, TelemetryExportFilter, TelemetryMetrics,
    TurnLivenessNumericBounds, TurnLivenessRuntime, WebBlobRuntime, WorkspaceInstructionRuntime,
    model_adapter::ConfiguredModelRuntime,
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
const GUARD_CHECK_INTERVAL: Duration = Duration::from_secs(1);

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
    session: Option<SessionId>,
    turn: Option<TurnId>,
}

impl HubRuntimeError {
    const fn infrastructure(phase: RuntimePhase) -> Self {
        Self {
            phase,
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
        let github_token_file = required_path(GITHUB_TOKEN_FILE_ENVIRONMENT, github_token_file)?;
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
    process_artifacts
        .iter()
        .any(|process| runner_artifacts.iter().any(|runner| runner == process))
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
    TemplateConfiguration(&'a SessionTemplateConfigurationError),
    TelemetryConfiguration(&'a TelemetryConfigurationError),
    Database(&'a FencedHubDatabaseError),
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
            Self::TemplateConfiguration(error) => error.fmt(formatter),
            Self::TelemetryConfiguration(error) => error.fmt(formatter),
            Self::Database(error) => error.fmt(formatter),
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
    let error = HubRuntimeError::infrastructure(phase);
    tracing::error!(
        ?phase,
        failure_class = ?error.failure_class,
        cause = %cause,
        "daemon startup construction failed"
    );
    error
}

/// Converts Anthropic construction evidence to a closed classification.
///
/// The adapter's dynamic parser/client detail is deliberately excluded because
/// an adapter-owned string is not admitted to operator telemetry.
const fn anthropic_construction_cause(error: &AnthropicConstructionError) -> &'static str {
    match error {
        AnthropicConstructionError::InvalidBaseUrl { .. } => "anthropic_invalid_base_url",
        AnthropicConstructionError::InvalidVersion => "anthropic_invalid_version",
        AnthropicConstructionError::InvalidExchangeTimeout => "anthropic_invalid_timeout",
        AnthropicConstructionError::InvalidSseRecordLimit => "anthropic_invalid_record_limit",
        AnthropicConstructionError::ClientConstruction { .. } => "anthropic_client_construction",
    }
}

/// Converts OpenAI construction evidence to a closed classification.
///
/// The adapter's dynamic parser/client detail is deliberately excluded because
/// an adapter-owned string is not admitted to operator telemetry.
const fn openai_construction_cause(error: &OpenAiConstructionError) -> &'static str {
    match error {
        OpenAiConstructionError::InvalidBaseUrl { .. } => "openai_invalid_base_url",
        OpenAiConstructionError::InvalidExchangeTimeout => "openai_invalid_timeout",
        OpenAiConstructionError::InvalidSseRecordLimit => "openai_invalid_record_limit",
        OpenAiConstructionError::ClientConstruction { .. } => "openai_client_construction",
    }
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
    GraceWindowExpired,
    SignalListenerFailed,
    ExecutionFailed,
    ExecutionFailedAfterGraceWindow,
    GuardLost,
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
    GraceWindowExpired,
    GuardLost,
}

enum RuntimeTaskExit {
    Scheduler(SchedulerLoopExit),
    FencedPoolFloor,
    Process(Result<(), ProcessRuntimeError>),
    Runner(Result<(), RunnerProtocolRuntimeError>),
    WebHttp(Result<(), WebHttpRuntimeError>),
    TurnLiveness,
    LifecycleDeadline,
    LifecycleMetrics,
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
    WebHttpCompletedBeforeShutdown,
    TurnLivenessCompletedBeforeShutdown,
    LifecycleDeadlineCompletedBeforeShutdown,
    LifecycleMetricsCompletedBeforeShutdown,
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
            Self::WebHttpCompletedBeforeShutdown => "web_http_completed_before_shutdown",
            Self::TurnLivenessCompletedBeforeShutdown => "turn_liveness_completed_before_shutdown",
            Self::LifecycleDeadlineCompletedBeforeShutdown => {
                "lifecycle_deadline_completed_before_shutdown"
            }
            Self::LifecycleMetricsCompletedBeforeShutdown => {
                "lifecycle_metrics_completed_before_shutdown"
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

    match error {
        ProcessRuntimeError::Accept(_)
        | ProcessRuntimeError::SpoolIo(_)
        | ProcessRuntimeError::InsufficientPoolCapacity
        | ProcessRuntimeError::CleanupSocket(_)
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
        | Ok(RuntimeTaskExit::Process(Ok(())))
        | Ok(RuntimeTaskExit::Runner(Ok(())))
        | Ok(RuntimeTaskExit::WebHttp(Ok(())))
        | Ok(RuntimeTaskExit::TurnLiveness)
        | Ok(RuntimeTaskExit::LifecycleDeadline)
        | Ok(RuntimeTaskExit::LifecycleMetrics) => RuntimeTaskCompletion::Clean,
        Ok(RuntimeTaskExit::Process(Err(error))) => {
            report_process_runtime_failure(&error);
            RuntimeTaskCompletion::Failed
        }
        Ok(RuntimeTaskExit::Runner(Err(error))) => {
            report_runner_runtime_failure(&error);
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
async fn drain_runtime_tasks<GuardLoss>(
    runtime_tasks: &mut JoinSet<RuntimeTaskExit>,
    guard_loss: GuardLoss,
    grace_window: Option<Duration>,
) -> (RuntimeDrainOutcome, RuntimeTaskCompletion)
where
    GuardLoss: Future<Output = ()>,
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
            result = timeout(grace_window, &mut drain) => match result {
                Ok(()) => RuntimeDrainOutcome::Complete,
                Err(_) => RuntimeDrainOutcome::GraceWindowExpired,
            }
        },
        None => select! {
            () = guard_loss => RuntimeDrainOutcome::GuardLost,
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
        (RuntimeStopCause::Requested, RuntimeDrainOutcome::GraceWindowExpired) => {
            ShutdownOutcome::GraceWindowExpired
        }
        (RuntimeStopCause::SignalListenerFailed, _) => ShutdownOutcome::SignalListenerFailed,
        (RuntimeStopCause::ExecutionFailed, RuntimeDrainOutcome::Complete) => {
            ShutdownOutcome::ExecutionFailed
        }
        (RuntimeStopCause::ExecutionFailed, RuntimeDrainOutcome::GraceWindowExpired) => {
            ShutdownOutcome::ExecutionFailedAfterGraceWindow
        }
        (RuntimeStopCause::RuntimeFailed, RuntimeDrainOutcome::Complete) => {
            ShutdownOutcome::RuntimeFailed
        }
        (RuntimeStopCause::RuntimeFailed, RuntimeDrainOutcome::GraceWindowExpired) => {
            ShutdownOutcome::RuntimeFailedAfterGraceWindow
        }
        (RuntimeStopCause::RuntimeDefect, RuntimeDrainOutcome::Complete) => {
            ShutdownOutcome::RuntimeDefect
        }
        (RuntimeStopCause::RuntimeDefect, RuntimeDrainOutcome::GraceWindowExpired) => {
            ShutdownOutcome::RuntimeDefectAfterGraceWindow
        }
    }
}

async fn shutdown_requested() -> bool {
    #[cfg(unix)]
    {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(terminate) => terminate,
                Err(_) => return true,
            };
        select! {
            result = tokio::signal::ctrl_c() => result.is_err(),
            _ = terminate.recv() => false,
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.is_err()
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
    let prometheus_runtime = initialize_prometheus(telemetry_configuration).await;
    let model_configuration = HubModelConfiguration::read(configuration.model_configuration_file())
        .map_err(|error| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::ModelConfiguration(&error),
            )
        })?;
    let numeric_bounds = model_configuration.numeric_bounds();
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
    let daemon_tool_configuration = model_configuration.daemon_tools();
    let tool_composition = match daemon_tool_configuration {
        Some(_) => DaemonToolComposition::WithMappedFamilies,
        None => DaemonToolComposition::Base,
    };
    DaemonToolCatalog::validate_approval_postures_for_composition(
        model_configuration.tool_approval_postures(),
        tool_composition,
    )
    .map_err(|error| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Static(configured_approval_posture_cause(&error)),
        )
    })?;
    let template_configuration = SessionTemplateConfiguration::read(
        configuration.template_configuration_file(),
        || env::var_os("HOME").map(PathBuf::from),
        &model_configuration,
    )
    .map_err(|error| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::TemplateConfiguration(&error),
        )
    })?;
    let anthropic_model_credentials = FileCredentialAccess::from_files(
        model_configuration
            .file_credential_profiles(ModelAdapter::Anthropic)
            .map(|(reference, path)| (CredentialReference::new(reference), path.to_path_buf())),
    );
    let openai_model_credentials = FileCredentialAccess::from_files(
        model_configuration
            .file_credential_profiles(ModelAdapter::OpenAi)
            .map(|(reference, path)| (CredentialReference::new(reference), path.to_path_buf())),
    );
    let anthropic_credential_access = model_configuration
        .uses_anthropic_adapter()
        .then(|| anthropic_model_credentials.clone());
    let openai_credential_access = model_configuration
        .uses_openai_adapter()
        .then(|| openai_model_credentials.clone());
    let credential_reference =
        ModelCallCredentialReference::new(model_configuration.fallback_credential_profile());
    let code_host_credentials = FileCredentialAccess::new(
        configuration.github_token_file(),
        CredentialReference::new(CODE_HOST_CREDENTIAL_REFERENCE),
    );
    let web_search_credentials = FileCredentialAccess::new(
        configuration.brave_api_key_file(),
        CredentialReference::new(BRAVE_SEARCH_CREDENTIAL_REFERENCE),
    );
    let compaction_anthropic = anthropic_credential_access
        .clone()
        .map(|credential_access| {
            let mut adapter_configuration = AnthropicConfig::new(native_message_limit);
            adapter_configuration.exchange_timeout = model_exchange_timeout;
            AnthropicRuntime::new(adapter_configuration, credential_access)
        })
        .transpose()
        .map_err(|error| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static(anthropic_construction_cause(&error)),
            )
        })?;
    let compaction_openai = openai_credential_access
        .clone()
        .map(|credential_access| {
            let mut adapter_configuration = OpenAiConfig::new(native_message_limit);
            adapter_configuration.exchange_timeout = model_exchange_timeout;
            OpenAiRuntime::new(adapter_configuration, credential_access)
        })
        .transpose()
        .map_err(|error| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static(openai_construction_cause(&error)),
            )
        })?;
    let anthropic_model_capabilities = model_configuration.runtime_model_capability_catalog();
    let openai_model_capabilities = model_configuration.runtime_model_capability_catalog();
    let anthropic = model_configuration
        .uses_anthropic_adapter()
        .then(|| anthropic_model_credentials.clone())
        .map(|credential_access| {
            let mut adapter_configuration = AnthropicConfig::new(native_message_limit);
            adapter_configuration.exchange_timeout = model_exchange_timeout;
            adapter_configuration.model_capabilities = anthropic_model_capabilities;
            AnthropicRuntime::new(adapter_configuration, credential_access)
        })
        .transpose()
        .map_err(|error| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static(anthropic_construction_cause(&error)),
            )
        })?;
    let openai = openai_credential_access
        .map(|credential_access| {
            let mut adapter_configuration = OpenAiConfig::new(native_message_limit);
            adapter_configuration.exchange_timeout = model_exchange_timeout;
            adapter_configuration.model_capabilities = openai_model_capabilities;
            OpenAiRuntime::new(adapter_configuration, credential_access)
        })
        .transpose()
        .map_err(|error| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static(openai_construction_cause(&error)),
            )
        })?;
    let code_host_transport =
        GitHubCodeHostTransport::try_new(code_host_numeric_bounds).map_err(|_| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static("github_transport_construction_failed"),
            )
        })?;
    let runtime_models = model_configuration.runtime_model_catalog();
    let compaction_runtime = ConfiguredModelRuntime::new(
        compaction_anthropic,
        compaction_openai,
        &model_configuration,
        model_exchange_timeout,
        post_kill_reap_bound,
        native_message_limit,
    )
    .map_err(|error| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Static(error.cause_code()),
        )
    })?;
    let runtime = ConfiguredModelRuntime::new(
        anthropic,
        openai,
        &model_configuration,
        model_exchange_timeout,
        post_kill_reap_bound,
        native_message_limit,
    )
    .map_err(|error| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Static(error.cause_code()),
        )
    })?;
    let context_compaction_model: Arc<dyn ContextCompactionModel> = Arc::new(
        RuntimeContextCompactionModel::new(compaction_runtime, runtime_models.clone()),
    );
    let approval_judge_model: Arc<dyn ApprovalJudgeModel> = Arc::new(
        RuntimeApprovalJudgeModel::new(runtime.clone(), runtime_models.clone()),
    );
    let provider = RuntimeModelCallProvider::new(
        runtime,
        runtime_models.clone(),
        diagnostic_model_identity_limit,
    );
    let model_targets = model_configuration.target_catalog();
    let mut database = FencedHubDatabase::connect_production(
        configuration.database_url(),
        fenced_pool_min_connections,
    )
    .await
    .map_err(|error| {
        let phase = match &error {
            FencedHubDatabaseError::InitializeFence(_) => RuntimePhase::Migration,
            FencedHubDatabaseError::ParseOptions(_)
            | FencedHubDatabaseError::ConnectBootstrap(_)
            | FencedHubDatabaseError::AcquireGuard(_)
            | FencedHubDatabaseError::AdvanceFence(_)
            | FencedHubDatabaseError::ConnectFencedPool(_) => RuntimePhase::DatabaseConnection,
        };
        erase_startup_cause(phase, SanitizedStartupCause::Database(&error))
    })?;
    let pool = database.pool().clone();
    let fenced_pool_floor_pool = pool.clone();
    let image_derivative_supervisor = daemon_tool_configuration
        .as_ref()
        .map(|configuration| configuration.exec_supervisor_executable().to_path_buf());
    let tools = match daemon_tool_configuration {
        Some(tool_configuration) => DaemonTools::try_new_production(
            SystemCurrentTimeClock,
            pool.clone(),
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
            model_configuration.web_fetch_egress_policy(),
        ),
        None => DaemonTools::try_new_without_tool_mappings(
            SystemCurrentTimeClock,
            pool.clone(),
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
            let _ = database.close().await;
            return Err(failure);
        }
    };
    let workspace_instruction_runtime = WorkspaceInstructionRuntime::new(
        pool.clone(),
        tools.workspace_instruction_root_resolver(),
        model_configuration
            .workspace_instructions()
            .roots()
            .to_vec(),
    );
    let (mut tool_catalog, mut tool_executor) = tools.into_parts();

    let migration_pool = pool.clone();
    let scan_pool = pool.clone();
    let startup = migrate_scan_then_schedule(
        async move {
            migrate(&migration_pool).await.map_err(|error| {
                tracing::error!(
                    migration_detail = %error,
                    "database migration rejected"
                );
                erase_startup_cause(
                    RuntimePhase::Migration,
                    SanitizedStartupCause::Static("database_migration_failed"),
                )
            })?;
            tracing::info!(
                phase = ?RuntimePhase::Migration,
                "daemon startup phase completed"
            );
            Ok(())
        },
        async move {
            let mut scan = StartupScanService::new(
                UuidV7StartupScanIdGenerator,
                PostgresStartupScanRepository::new(scan_pool),
            );
            let outcome = scan.execute().await.map_err(|error| {
                tracing::error!(
                    startup_scan_detail = %error.repository_error(),
                    "startup scan rejected durable state"
                );
                let failure_class = error.operator_failure_class();
                let cause_code = error.operator_failure_cause_code();
                let session = error.session();
                let turn = error.repository_error().corruption_turn();
                erase_startup_scan_cause(failure_class, cause_code, session, turn)
            })?;
            tracing::info!(
                phase = ?RuntimePhase::StartupScan,
                recovered_turn_count = outcome.recovered_turn_count(),
                awaiting_recovery_decision_session_count =
                    outcome.awaiting_recovery_decision_sessions().len(),
                "daemon startup phase completed"
            );
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
    )
    .await;
    if let Err(error) = startup {
        let _ = database.close().await;
        return Err(error);
    }

    if database.check_guard().await.is_err() {
        let _ = database.close().await;
        return Ok(ShutdownOutcome::GuardLost);
    }
    let blob_store_registry = match await_while_guarded(
        &mut database,
        BlobStoreRegistry::initialize(model_configuration.blob_storage(), pool.clone()),
    )
    .await
    {
        GuardedAwait::Completed(Ok(registry)) => registry,
        GuardedAwait::Completed(Err(_)) => {
            let failure = erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static("blob_storage_startup_reconciliation_failed"),
            );
            let _ = database.close().await;
            return Err(failure);
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
                let _ = database.close().await;
                return Err(failure);
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
                let _ = database.close().await;
                return Err(failure);
            }
        };
        blob_executor = Some(executor);
    }
    tool_catalog =
        match tool_catalog.with_approval_postures(model_configuration.tool_approval_postures()) {
            Ok(catalog) => catalog,
            Err(error) => {
                let failure = erase_startup_cause(
                    RuntimePhase::Configuration,
                    SanitizedStartupCause::Static(configured_approval_posture_cause(&error)),
                );
                drop(blob_executor);
                drop(blob_store_registry);
                let _ = database.close().await;
                return Err(failure);
            }
        };
    let runner_service = match PostgresRunnerRegistrationService::registration_only(pool.clone()) {
        Ok(service) => service,
        Err(_) => {
            let failure = erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static("runner_catalog_construction_failed"),
            );
            drop(blob_executor);
            disarm_staging_sweep_unless_guarded(&mut database, &mut blob_store_registry).await;
            drop(blob_store_registry);
            let _ = database.close().await;
            return Err(failure);
        }
    };
    match await_while_guarded(
        &mut database,
        runner_service.mark_orphaned_connections_lost(),
    )
    .await
    {
        GuardedAwait::Completed(Ok(_)) => {}
        GuardedAwait::Completed(Err(_)) => {
            let failure = erase_startup_cause(
                RuntimePhase::StartupScan,
                SanitizedStartupCause::Static("runner_connection_reconciliation_failed"),
            );
            drop(blob_executor);
            disarm_staging_sweep_unless_guarded(&mut database, &mut blob_store_registry).await;
            drop(blob_store_registry);
            let _ = database.close().await;
            return Err(failure);
        }
        GuardedAwait::GuardLost => {
            if let Some(registry) = blob_store_registry.as_ref() {
                registry.disarm_staging_sweep();
            }
            drop(blob_executor);
            drop(blob_store_registry);
            let _ = database.close().await;
            return Ok(ShutdownOutcome::GuardLost);
        }
    }
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
            let _ = database.close().await;
            return Err(failure);
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
            let _ = database.close().await;
            return Err(failure);
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
            let _ = database.close().await;
            return Err(failure);
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
                    let _ = database.close().await;
                    return Err(failure);
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
                    let _ = database.close().await;
                    return Err(failure);
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
            let _ = database.close().await;
            return Err(failure);
        }
    };
    tracing::info!(
        phase = ?RuntimePhase::SocketBinding,
        "daemon startup phase completed"
    );
    let scheduler_pool = pool.clone();
    let sweep = PostgresEligibilitySweep::new(scheduler_pool.clone());
    let (eligibility_nudge, work_source) = InProcessEligibilityWorkSource::with_options(
        sweep,
        reconciliation_sweep_interval,
        nudge_buffer_capacity,
    );
    let tool_dispatch_gate = InProcessToolDispatchGate::default();
    tool_executor = tool_executor.with_blob_executor(blob_executor);
    let process_runtime = ProcessRuntime::new_with_templates(
        listener,
        scheduler_pool.clone(),
        eligibility_nudge.clone(),
        tool_dispatch_gate.clone(),
        model_configuration.clone(),
        template_configuration,
    )
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
    let web_http_runtime = web_http_listener.into_runtime(process_runtime.monitor());
    let runner_runtime = RunnerProtocolRuntime::new(runner_listener, runner_service);
    let provider = provider.with_text_delta_sink(process_runtime.provider_text_delta_sink());
    let model_repository = PostgresModelCallRepository::new(
        scheduler_pool.clone(),
        model_targets,
        credential_reference,
    )
    .with_session_credentials(model_configuration.credential_family_catalog())
    .with_credential_pools(model_configuration.credential_pool_runtime_catalog())
    .with_cache_inclusive_input_targets(model_configuration.cache_inclusive_input_targets())
    .with_continuation_usage_limits(model_configuration.tool_continuation_usage_limits());
    let provider = AttachmentPreparingModelCallProvider::new(
        UsageLimitedModelCallProvider::new(provider, &model_configuration),
        scheduler_pool.clone(),
        blob_store_registry.clone(),
    );
    let reported_usage_compaction = ReportedUsageCompaction::new(
        StartEligibleTurnRepository::new(scheduler_pool.clone()),
        model_repository.clone(),
        tool_catalog.clone(),
        runtime_models,
        model_configuration.clone(),
        Arc::clone(&context_compaction_model),
    );
    let (turn_execution_shutdown, turn_execution_shutdown_receiver) = watch::channel(false);
    let (execution, fatal_execution) = FatalExecutionSupervisor::new(
        PostgresProviderModelExecution::new(
            model_repository,
            InProcessAttemptDispatchGate::default(),
            provider,
            automatic_tool_round_limit,
        )
        .with_tool_loop(tool_dispatch_gate, tool_catalog, tool_executor)
        .with_workspace_instructions(workspace_instruction_runtime)
        .with_approval_judge(
            approval_judge_model,
            model_configuration.configured_approval_judge_selection(),
            model_configuration.clone(),
        )
        .with_shutdown_checkpoint(turn_execution_shutdown_receiver),
    );
    // The connection runtime has no execution role, so it reaches the same
    // fatal recovery signal through this handle rather than ending an
    // undecidable durable outcome at the client response.
    let process_runtime = process_runtime.with_recovery_reporter(execution.recovery_reporter());
    let activated_pass = ActivatedTurnPass::new(
        StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(scheduler_pool.clone()),
        ),
        execution,
    )
    .with_reported_usage_compaction(reported_usage_compaction)
    .with_occupancy_recovery(
        scheduler_pool.clone(),
        eligibility_nudge.clone(),
        expired_pass_recovery_policy,
        turn_liveness_persistence_bounds,
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
    let goal_disposition = PostgresGoalPassDisposition::new(
        scheduler_pool,
        model_configuration.clone(),
        eligibility_nudge,
        goal_mode_numeric_bounds,
    );
    let process_runtime = process_runtime.with_goal_resumption(goal_disposition.clone());
    match goal_disposition
        .reconcile_automatic_resumptions_after_restart()
        .await
    {
        Ok(rearmed) => tracing::info!(
            phase = ?RuntimePhase::StartupScan,
            rearmed_goal_resumption_count = rearmed,
            "daemon startup reconciled automatic goal resumptions"
        ),
        Err(error) => tracing::error!(
            phase = ?RuntimePhase::StartupScan,
            cause_code = error.operator_failure_cause_code(),
            cause = %error,
            "daemon startup exhausted automatic goal-resumption reconciliation"
        ),
    }
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
    let (scheduler_shutdown, scheduler_shutdown_receiver) = oneshot::channel();
    let (fenced_pool_floor_shutdown, fenced_pool_floor_shutdown_receiver) = watch::channel(false);
    let (process_shutdown, process_shutdown_receiver) = watch::channel(false);
    let (runner_shutdown, runner_shutdown_receiver) = watch::channel(false);
    let (web_http_shutdown, web_http_shutdown_receiver) = watch::channel(false);
    let (turn_liveness_shutdown, turn_liveness_shutdown_receiver) = watch::channel(false);
    let (lifecycle_deadline_shutdown, lifecycle_deadline_shutdown_receiver) = watch::channel(false);
    let (lifecycle_metrics_shutdown, lifecycle_metrics_shutdown_receiver) = watch::channel(false);
    let mut runtime_tasks = JoinSet::new();
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
    tracing::info!(phase = ?RuntimePhase::Scheduling, "daemon runtime started");

    let mut outcome = {
        let guard_loss = wait_for_guard_loss(&mut database);
        pin!(guard_loss);
        let mut cause = select! {
            listener_failed = shutdown_requested() => {
                if listener_failed {
                    RuntimeStopCause::SignalListenerFailed
                } else {
                    RuntimeStopCause::Requested
                }
            }
            () = fatal_execution.wait() => RuntimeStopCause::ExecutionFailed,
            () = &mut guard_loss => RuntimeStopCause::GuardLost,
            completed = runtime_tasks.join_next() => {
                match completed {
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
                    Some(Ok(RuntimeTaskExit::LifecycleMetrics)) => {
                        report_runtime_task_defect(
                            RuntimeTaskDefect::LifecycleMetricsCompletedBeforeShutdown,
                        );
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
        };

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
                shutdown_grace_window,
            )
            .await;
            cause = combine_runtime_stop_cause(cause, components_clean);
            if drain != RuntimeDrainOutcome::Complete {
                runtime_tasks.abort_all();
                while runtime_tasks.join_next().await.is_some() {}
            }
            completed_runtime_outcome(cause, drain)
        }
    };

    // A timed-out component may still have held a connection before its task
    // was aborted. Waiting for an ordinary pool drain here would silently
    // extend the shutdown window. Guard loss is different: tasks are cancelled
    // immediately and the old fenced sessions must be terminated before
    // returning control to process exit.
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
        let close_pool = should_close_pool(&Ok(outcome));
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
        Ok(ShutdownOutcome::GuardLost) => {
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
    use tokio::{sync::oneshot, task::JoinSet};
    use tracing_subscriber::prelude::*;
    use uuid::Uuid;

    use super::{
        AnthropicConstructionError, BRAVE_API_KEY_FILE_ENVIRONMENT, DATABASE_URL_ENVIRONMENT,
        FENCED_POOL_MAX_CONNECTIONS, FencedPoolFloorReconciliationPolicy,
        GITHUB_TOKEN_FILE_ENVIRONMENT, HubConfiguration, HubConfigurationError,
        HubConfigurationValues, HubRuntimeError, MODEL_CONFIGURATION_FILE_ENVIRONMENT,
        OpenAiConstructionError, OperatorFilterDisposition, PROCESS_SOCKET_PATH_ENVIRONMENT,
        ProcessRuntimeError, RUNNER_SOCKET_PATH_ENVIRONMENT, RequiredSettingFailure,
        RuntimeDrainOutcome, RuntimePhase, RuntimeStopCause, RuntimeTaskCompletion,
        RuntimeTaskExit, SanitizedStartupCause, SchedulerStopCause, ShutdownOutcome,
        SingleHubGuardError, TEMPLATE_CONFIGURATION_FILE_ENVIRONMENT, anthropic_construction_cause,
        combine_runtime_stop_cause, completed_runtime_outcome, database_close_failure_outcome,
        drain_runtime_tasks, erase_startup_cause, fenced_pool_floor_reconciliation_policy,
        graceful_shutdown_window, migrate_scan_then_schedule, openai_construction_cause,
        operator_filter, process_runtime_failure_class, report_database_close_failure,
        run_scheduler_until_shutdown, runner_lifecycle_failure_class, should_close_pool,
        staging_sweep_failure_outcome, validate_fenced_pool_min_connections,
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
        let cause_code = anthropic_construction_cause(&error);
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

        let cause_code = openai_construction_cause(&error);
        let encoded = capture_startup_cause(SanitizedStartupCause::Static(cause_code));

        assert!(encoded.contains("openai_invalid_base_url"));
        assert!(!encoded.contains(adapter_detail));
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
        assert_eq!(
            HubConfiguration::from_values(HubConfigurationValues {
                github_token_file: None,
                ..hub_configuration_values()
            })
            .err(),
            Some(HubConfigurationError::new(
                GITHUB_TOKEN_FILE_ENVIRONMENT,
                RequiredSettingFailure::Missing,
            ))
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
    async fn adr0044_shutdown_drain_includes_the_configured_cleanup_window() {
        let (entered_sender, entered_receiver) = oneshot::channel();
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let session = SessionId::from_uuid(Uuid::from_u128(1));
        let pass_duration = Duration::from_secs(2);
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
            graceful_shutdown_window(Some(Duration::from_secs(3)), Some(Duration::from_secs(1)))
                .expect("the fixture cleanup window is bounded"),
        ));

        entered_receiver
            .await
            .expect("the scheduler admitted the first pass");
        shutdown_sender
            .send(())
            .expect("the scheduler still listens for shutdown");
        tokio::time::advance(pass_duration).await;

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
    async fn runtime_task_defect_before_drain_timeout_prevents_clean_exit() {
        let mut runtime_tasks: JoinSet<RuntimeTaskExit> = JoinSet::new();
        runtime_tasks.spawn(async {
            panic!("synthetic runtime task panic");
        });
        runtime_tasks.spawn(pending::<RuntimeTaskExit>());

        let (drain, completion) =
            drain_runtime_tasks(&mut runtime_tasks, pending(), Some(Duration::from_secs(5))).await;
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

        let (drain, completion) =
            drain_runtime_tasks(&mut runtime_tasks, pending(), Some(Duration::from_secs(5))).await;
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
