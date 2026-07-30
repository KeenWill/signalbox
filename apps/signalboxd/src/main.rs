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
    fmt,
    future::Future,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use signalbox_application::{
    ClassifyOperatorFailure, InProcessAttemptDispatchGate, InProcessEligibilityWorkSource,
    InProcessToolDispatchGate, ModelCallCredentialReference, OperatorFailureClass, SchedulerLoop,
    SchedulerLoopExit, StartupScanService, UuidV7StartupScanIdGenerator,
};
#[cfg(test)]
use signalbox_application::{EligibilityPass, EligibilityWorkSource};
use signalbox_domain::{SessionId, TurnId};
use signalbox_model_provider_runtime::{
    ContextCompactionModel, RuntimeContextCompactionModel, RuntimeModelCallProvider,
};
use signalbox_model_runtime::CredentialReference;
use signalbox_model_runtime_anthropic::{
    AnthropicConfig, AnthropicConstructionError, AnthropicRuntime,
};
use signalbox_persistence::{
    conversation_import::backfill_imported_conversation_display_titles, migrate,
    model_execution::PostgresModelCallRepository, scheduler::PostgresEligibilitySweep,
    start_eligible_turn::StartEligibleTurnRepository, startup::PostgresStartupScanRepository,
};
use signalboxd::{
    ANTHROPIC_CREDENTIAL_REFERENCE, CODE_HOST_CREDENTIAL_REFERENCE, ContextGuardedTurnPass,
    DaemonTools, DaemonToolsConstructionError, FatalExecutionSupervisor, FencedHubDatabase,
    FencedHubDatabaseError, FileCredentialAccess, GitHubCodeHostTransport, HubModelConfiguration,
    HubModelConfigurationError, LocalProcessListener, LocalSocketError,
    PostgresProviderModelExecution, ProcessRuntime, ProcessRuntimeError,
    SessionTemplateConfiguration, SessionTemplateConfigurationError, SingleHubGuardError,
    SystemCurrentTimeClock,
};
use tokio::{
    pin, select,
    sync::{oneshot, watch},
    task::{JoinError, JoinSet},
    time::{sleep, timeout},
};

const GRACEFUL_SHUTDOWN_WINDOW: Duration = Duration::from_secs(30);
const MODEL_CONFIGURATION_FILE_ENVIRONMENT: &str = "SIGNALBOX_CONFIG_FILE";
const DATABASE_URL_ENVIRONMENT: &str = "DATABASE_URL";
const TEMPLATE_CONFIGURATION_FILE_ENVIRONMENT: &str = "SIGNALBOX_TEMPLATE_CONFIG_FILE";
const ANTHROPIC_API_KEY_FILE_ENVIRONMENT: &str = "ANTHROPIC_API_KEY_FILE";
const GITHUB_TOKEN_FILE_ENVIRONMENT: &str = "GITHUB_TOKEN_FILE";
const LOG_FILTER_ENVIRONMENT: &str = "RUST_LOG";
const PROCESS_SOCKET_PATH_ENVIRONMENT: &str = "SIGNALBOX_SOCKET_PATH";
const GUARD_CHECK_INTERVAL: Duration = Duration::from_secs(1);

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
        };
        write!(formatter, "required setting {} {failure}", self.setting)
    }
}

struct HubConfiguration {
    database_url: String,
    model_configuration_file: PathBuf,
    template_configuration_file: PathBuf,
    anthropic_api_key_file: PathBuf,
    github_token_file: PathBuf,
    process_socket_path: PathBuf,
}

impl HubConfiguration {
    fn from_environment() -> Result<Self, HubConfigurationError> {
        Self::from_values(
            env::var_os(DATABASE_URL_ENVIRONMENT),
            env::var_os(MODEL_CONFIGURATION_FILE_ENVIRONMENT),
            env::var_os(TEMPLATE_CONFIGURATION_FILE_ENVIRONMENT),
            env::var_os(ANTHROPIC_API_KEY_FILE_ENVIRONMENT),
            env::var_os(GITHUB_TOKEN_FILE_ENVIRONMENT),
            env::var_os(PROCESS_SOCKET_PATH_ENVIRONMENT),
        )
    }

    fn from_values(
        database_url: Option<OsString>,
        model_configuration_file: Option<OsString>,
        template_configuration_file: Option<OsString>,
        anthropic_api_key_file: Option<OsString>,
        github_token_file: Option<OsString>,
        process_socket_path: Option<OsString>,
    ) -> Result<Self, HubConfigurationError> {
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
        let anthropic_api_key_file =
            required_path(ANTHROPIC_API_KEY_FILE_ENVIRONMENT, anthropic_api_key_file)?;
        let github_token_file = required_path(GITHUB_TOKEN_FILE_ENVIRONMENT, github_token_file)?;
        let process_socket_path =
            required_path(PROCESS_SOCKET_PATH_ENVIRONMENT, process_socket_path)?;

        Ok(Self {
            database_url,
            model_configuration_file,
            template_configuration_file,
            anthropic_api_key_file,
            github_token_file,
            process_socket_path,
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

    fn anthropic_api_key_file(&self) -> PathBuf {
        self.anthropic_api_key_file.clone()
    }

    fn github_token_file(&self) -> PathBuf {
        self.github_token_file.clone()
    }

    fn process_socket_path(&self) -> &Path {
        &self.process_socket_path
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

/// Closed startup causes admitted to operator telemetry.
///
/// Every variant wraps a Display implementation audited to omit paths,
/// credentials, configuration content, provider prose, and user content.
enum SanitizedStartupCause<'a> {
    Configuration(&'a HubConfigurationError),
    ModelConfiguration(&'a HubModelConfigurationError),
    TemplateConfiguration(&'a SessionTemplateConfigurationError),
    Database(&'a FencedHubDatabaseError),
    Tools(&'a DaemonToolsConstructionError),
    Socket(&'a LocalSocketError),
    Static(&'static str),
}

impl fmt::Display for SanitizedStartupCause<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(error) => error.fmt(formatter),
            Self::ModelConfiguration(error) => error.fmt(formatter),
            Self::TemplateConfiguration(error) => error.fmt(formatter),
            Self::Database(error) => error.fmt(formatter),
            Self::Tools(error) => error.fmt(formatter),
            Self::Socket(error) => error.fmt(formatter),
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
    ProcessRuntimeFailed,
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
    Process(Result<(), ProcessRuntimeError>),
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
        (_, RuntimeTaskCompletion::Failed) => RuntimeStopCause::ProcessRuntimeFailed,
        (cause, RuntimeTaskCompletion::Clean) => cause,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeTaskDefect {
    SchedulerCompletedBeforeShutdown,
    ProcessCompletedBeforeShutdown,
    TaskCancelled,
    TaskPanicked,
    TaskJoinFailed,
    TaskSetEmpty,
}

impl RuntimeTaskDefect {
    const fn cause_code(self) -> &'static str {
        match self {
            Self::SchedulerCompletedBeforeShutdown => "scheduler_completed_before_shutdown",
            Self::ProcessCompletedBeforeShutdown => "process_runtime_completed_before_shutdown",
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
        sleep(GUARD_CHECK_INTERVAL).await;
        if database.check_guard().await.is_err() {
            return;
        }
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
        | Ok(RuntimeTaskExit::Process(Ok(()))) => RuntimeTaskCompletion::Clean,
        Ok(RuntimeTaskExit::Process(Err(error))) => {
            report_process_runtime_failure(&error);
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
    grace_window: Duration,
) -> (RuntimeDrainOutcome, RuntimeTaskCompletion)
where
    GuardLoss: Future<Output = ()>,
{
    let completion = Cell::new(RuntimeTaskCompletion::Clean);
    let drain = select! {
        () = guard_loss => RuntimeDrainOutcome::GuardLost,
        result = timeout(grace_window, async {
            while let Some(completed) = runtime_tasks.join_next().await {
                completion.set(completion.get().combine(runtime_task_completion(completed)));
            }
        }) => match result {
            Ok(()) => RuntimeDrainOutcome::Complete,
            Err(_) => RuntimeDrainOutcome::GraceWindowExpired,
        }
    };
    (drain, completion.get())
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
        (RuntimeStopCause::ProcessRuntimeFailed, RuntimeDrainOutcome::Complete) => {
            ShutdownOutcome::RuntimeFailed
        }
        (RuntimeStopCause::ProcessRuntimeFailed, RuntimeDrainOutcome::GraceWindowExpired) => {
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

async fn run_hub() -> Result<ShutdownOutcome, HubRuntimeError> {
    let configuration = HubConfiguration::from_environment().map_err(|error| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Configuration(&error),
        )
    })?;
    let model_configuration = HubModelConfiguration::read(configuration.model_configuration_file())
        .map_err(|error| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::ModelConfiguration(&error),
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
    let credential_access = FileCredentialAccess::new(
        configuration.anthropic_api_key_file(),
        CredentialReference::new(ANTHROPIC_CREDENTIAL_REFERENCE),
    );
    let credential_reference =
        ModelCallCredentialReference::new(credential_access.credential_reference().as_str());
    let code_host_credentials = FileCredentialAccess::new(
        configuration.github_token_file(),
        CredentialReference::new(CODE_HOST_CREDENTIAL_REFERENCE),
    );
    let compaction_anthropic =
        AnthropicRuntime::new(AnthropicConfig::new(), credential_access.clone()).map_err(
            |error| {
                erase_startup_cause(
                    RuntimePhase::Configuration,
                    SanitizedStartupCause::Static(anthropic_construction_cause(&error)),
                )
            },
        )?;
    let anthropic =
        AnthropicRuntime::new(AnthropicConfig::new(), credential_access).map_err(|error| {
            erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Static(anthropic_construction_cause(&error)),
            )
        })?;
    let code_host_transport = GitHubCodeHostTransport::try_new().map_err(|_| {
        erase_startup_cause(
            RuntimePhase::Configuration,
            SanitizedStartupCause::Static("github_transport_construction_failed"),
        )
    })?;
    let runtime_models = model_configuration.runtime_model_catalog();
    let context_compaction_model: Arc<dyn ContextCompactionModel> = Arc::new(
        RuntimeContextCompactionModel::new(compaction_anthropic, runtime_models.clone()),
    );
    let provider = RuntimeModelCallProvider::new(anthropic, runtime_models.clone());
    let context_compaction_credential_reference = credential_reference.as_str().to_owned();
    let model_targets = model_configuration.target_catalog();
    let mut database = FencedHubDatabase::connect_production(configuration.database_url())
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
    let (tool_catalog, tool_executor) = match DaemonTools::try_new_production(
        SystemCurrentTimeClock,
        pool.clone(),
        code_host_credentials,
        code_host_transport,
        model_configuration.web_fetch_egress_policy(),
    ) {
        Ok(tools) => tools.into_parts(),
        Err(error) => {
            let failure = erase_startup_cause(
                RuntimePhase::Configuration,
                SanitizedStartupCause::Tools(&error),
            );
            let _ = database.close().await;
            return Err(failure);
        }
    };

    let migration_pool = pool.clone();
    let scan_pool = pool.clone();
    let startup = migrate_scan_then_schedule(
        async move {
            migrate(&migration_pool).await.map_err(|_| {
                erase_startup_cause(
                    RuntimePhase::Migration,
                    SanitizedStartupCause::Static("database_migration_failed"),
                )
            })?;
            let resolved_display_titles =
                backfill_imported_conversation_display_titles(&migration_pool)
                    .await
                    .map_err(|_| {
                        erase_startup_cause(
                            RuntimePhase::Migration,
                            SanitizedStartupCause::Static("imported_title_backfill_failed"),
                        )
                    })?;
            tracing::info!(
                phase = ?RuntimePhase::Migration,
                resolved_display_titles,
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
                    "session holds its slot awaiting an owner reconciliation decision"
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

    let listener = match LocalProcessListener::bind(configuration.process_socket_path()) {
        Ok(listener) => listener,
        Err(error) => {
            let failure = erase_startup_cause(
                RuntimePhase::SocketBinding,
                SanitizedStartupCause::Socket(&error),
            );
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
    let (eligibility_nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
    let tool_dispatch_gate = InProcessToolDispatchGate::default();
    let process_runtime = ProcessRuntime::new_with_templates(
        listener,
        scheduler_pool.clone(),
        eligibility_nudge,
        tool_dispatch_gate.clone(),
        model_configuration.clone(),
        template_configuration,
    )
    .with_context_compaction_model(
        Arc::clone(&context_compaction_model),
        context_compaction_credential_reference.clone(),
    );
    let provider = provider.with_text_delta_sink(process_runtime.provider_text_delta_sink());
    let counter = provider.clone();
    let model_repository = PostgresModelCallRepository::new(
        scheduler_pool.clone(),
        model_targets,
        credential_reference,
    );
    let guarded_model_repository = model_repository.clone();
    let guarded_tool_catalog = tool_catalog.clone();
    let (execution, fatal_execution) = FatalExecutionSupervisor::new(
        PostgresProviderModelExecution::new(
            model_repository,
            InProcessAttemptDispatchGate::default(),
            provider,
        )
        .with_tool_loop(tool_dispatch_gate, tool_catalog, tool_executor),
    );
    // The connection runtime has no execution role, so it reaches the same
    // fatal recovery signal through this handle rather than ending an
    // undecidable durable outcome at the client response.
    let process_runtime = process_runtime.with_recovery_reporter(execution.recovery_reporter());
    let pass = ContextGuardedTurnPass::new(
        StartEligibleTurnRepository::new(scheduler_pool),
        guarded_model_repository,
        counter,
        guarded_tool_catalog,
        runtime_models,
        model_configuration,
        context_compaction_model,
        context_compaction_credential_reference,
        execution,
    );
    let mut scheduler = SchedulerLoop::new(work_source, pass);
    let (scheduler_shutdown, scheduler_shutdown_receiver) = oneshot::channel();
    let (process_shutdown, process_shutdown_receiver) = watch::channel(false);
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
    runtime_tasks.spawn(async move {
        RuntimeTaskExit::Process(process_runtime.run(process_shutdown_receiver).await)
    });
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
                        RuntimeStopCause::ProcessRuntimeFailed
                    }
                    Some(Ok(RuntimeTaskExit::Process(Ok(())))) => {
                        report_runtime_task_defect(
                            RuntimeTaskDefect::ProcessCompletedBeforeShutdown,
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
            let _ = scheduler_shutdown.send(());
            let _ = process_shutdown.send(true);
            let (drain, components_clean) = drain_runtime_tasks(
                &mut runtime_tasks,
                guard_loss.as_mut(),
                GRACEFUL_SHUTDOWN_WINDOW,
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
    if outcome == ShutdownOutcome::GuardLost {
        let _ = database.close().await;
    } else if should_close_pool(&Ok(outcome))
        && let Err(error) = database.close().await
    {
        report_database_close_failure(&error);
        outcome = database_close_failure_outcome(outcome);
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
fn install_tracing_subscriber() {
    let configured = env::var(LOG_FILTER_ENVIRONMENT);
    let (filter, disposition) = match configured.as_deref() {
        Ok(value) => operator_filter(Some(value)),
        Err(env::VarError::NotPresent) => operator_filter(None),
        Err(env::VarError::NotUnicode(_)) => (
            tracing_subscriber::EnvFilter::new("info"),
            OperatorFilterDisposition::Rejected,
        ),
    };
    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(filter)
        .init();
    match disposition {
        OperatorFilterDisposition::Accepted => {}
        OperatorFilterDisposition::Rejected => tracing::warn!(
            setting = LOG_FILTER_ENVIRONMENT,
            "invalid tracing level rejected; using INFO default"
        ),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    install_tracing_subscriber();

    match run_hub().await {
        Ok(ShutdownOutcome::Clean) => {
            tracing::info!("daemon shutdown completed");
            ExitCode::SUCCESS
        }
        Ok(ShutdownOutcome::GraceWindowExpired) => {
            tracing::warn!(
                grace_window_seconds = GRACEFUL_SHUTDOWN_WINDOW.as_secs(),
                "daemon shutdown grace window expired; abandoning in-flight work"
            );
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
                grace_window_seconds = GRACEFUL_SHUTDOWN_WINDOW.as_secs(),
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
                grace_window_seconds = GRACEFUL_SHUTDOWN_WINDOW.as_secs(),
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
                grace_window_seconds = GRACEFUL_SHUTDOWN_WINDOW.as_secs(),
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
    }
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
        sync::{Arc, Mutex},
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
        AnthropicConstructionError, DATABASE_URL_ENVIRONMENT, GITHUB_TOKEN_FILE_ENVIRONMENT,
        HubConfiguration, HubConfigurationError, HubRuntimeError,
        MODEL_CONFIGURATION_FILE_ENVIRONMENT, OperatorFilterDisposition,
        PROCESS_SOCKET_PATH_ENVIRONMENT, ProcessRuntimeError, RequiredSettingFailure,
        RuntimeDrainOutcome, RuntimePhase, RuntimeStopCause, RuntimeTaskCompletion,
        RuntimeTaskExit, SanitizedStartupCause, SchedulerStopCause, ShutdownOutcome,
        SingleHubGuardError, TEMPLATE_CONFIGURATION_FILE_ENVIRONMENT, anthropic_construction_cause,
        combine_runtime_stop_cause, completed_runtime_outcome, database_close_failure_outcome,
        drain_runtime_tasks, erase_startup_cause, migrate_scan_then_schedule, operator_filter,
        process_runtime_failure_class, report_database_close_failure, run_scheduler_until_shutdown,
        should_close_pool,
    };

    #[derive(Clone, Default)]
    struct CapturedOutput(Arc<Mutex<Vec<u8>>>);

    impl CapturedOutput {
        fn text(&self) -> String {
            String::from_utf8(
                self.0
                    .lock()
                    .expect("captured telemetry lock is available")
                    .clone(),
            )
            .expect("captured telemetry is UTF-8")
        }
    }

    impl Write for CapturedOutput {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let mut bytes = self.0.lock().expect("captured telemetry lock is available");
            bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CapturedOutput {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_startup_cause(cause: SanitizedStartupCause<'_>) -> String {
        let output = CapturedOutput::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(output.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let _ = erase_startup_cause(RuntimePhase::Configuration, cause);
        });
        output.text()
    }

    fn capture_database_close_failure(error: &SingleHubGuardError) -> String {
        let output = CapturedOutput::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(output.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            report_database_close_failure(error);
        });
        output.text()
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
    fn deployment_paths_and_database_url_are_required() {
        assert_eq!(
            HubConfiguration::from_values(
                None,
                Some(OsString::from("models.toml")),
                Some(OsString::from("templates.toml")),
                Some(OsString::from("key")),
                Some(OsString::from("github-token")),
                Some(OsString::from("/tmp/signalbox.sock")),
            )
            .err(),
            Some(HubConfigurationError::new(
                DATABASE_URL_ENVIRONMENT,
                RequiredSettingFailure::Missing,
            ))
        );
        assert_eq!(
            HubConfiguration::from_values(
                Some(OsString::from("postgres://secret")),
                Some(OsString::from("")),
                Some(OsString::from("templates.toml")),
                Some(OsString::from("key")),
                Some(OsString::from("github-token")),
                Some(OsString::from("/tmp/signalbox.sock")),
            )
            .err(),
            Some(HubConfigurationError::new(
                MODEL_CONFIGURATION_FILE_ENVIRONMENT,
                RequiredSettingFailure::Empty,
            ))
        );
        assert_eq!(
            HubConfiguration::from_values(
                Some(OsString::from("postgres://secret")),
                Some(OsString::from("models.toml")),
                None,
                Some(OsString::from("key")),
                Some(OsString::from("github-token")),
                Some(OsString::from("/tmp/signalbox.sock")),
            )
            .err(),
            Some(HubConfigurationError::new(
                TEMPLATE_CONFIGURATION_FILE_ENVIRONMENT,
                RequiredSettingFailure::Missing,
            ))
        );
        assert_eq!(
            HubConfiguration::from_values(
                Some(OsString::from("postgres://secret")),
                Some(OsString::from("models.toml")),
                Some(OsString::from("templates.toml")),
                Some(OsString::from("key")),
                None,
                Some(OsString::from("/tmp/signalbox.sock")),
            )
            .err(),
            Some(HubConfigurationError::new(
                GITHUB_TOKEN_FILE_ENVIRONMENT,
                RequiredSettingFailure::Missing,
            ))
        );
        assert_eq!(
            HubConfiguration::from_values(
                Some(OsString::from("postgres://secret")),
                Some(OsString::from("models.toml")),
                Some(OsString::from("templates.toml")),
                Some(OsString::from("key")),
                Some(OsString::from("github-token")),
                None,
            )
            .err(),
            Some(HubConfigurationError::new(
                PROCESS_SOCKET_PATH_ENVIRONMENT,
                RequiredSettingFailure::Missing,
            ))
        );

        let configuration = HubConfiguration::from_values(
            Some(OsString::from("postgres://secret")),
            Some(OsString::from("models.toml")),
            Some(OsString::from("templates.toml")),
            Some(OsString::from("key")),
            Some(OsString::from("github-token")),
            Some(OsString::from("/tmp/signalbox.sock")),
        )
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
            configuration.anthropic_api_key_file(),
            std::path::PathBuf::from("key")
        );
        assert_eq!(
            configuration.github_token_file(),
            std::path::PathBuf::from("github-token")
        );
        assert_eq!(
            configuration.process_socket_path(),
            std::path::Path::new("/tmp/signalbox.sock")
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
            RuntimeStopCause::ProcessRuntimeFailed
        );
        assert_eq!(
            combine_runtime_stop_cause(
                RuntimeStopCause::ProcessRuntimeFailed,
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
            drain_runtime_tasks(&mut runtime_tasks, pending(), Duration::from_secs(5)).await;
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
            drain_runtime_tasks(&mut runtime_tasks, pending(), Duration::from_secs(5)).await;
        let cause = combine_runtime_stop_cause(RuntimeStopCause::Requested, completion);

        assert_eq!(drain, RuntimeDrainOutcome::GraceWindowExpired);
        assert_eq!(cause, RuntimeStopCause::ProcessRuntimeFailed);
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
                RuntimeStopCause::ProcessRuntimeFailed,
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
