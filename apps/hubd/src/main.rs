//! Signalbox hub composition root.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md owns startup ordering
//! (migrate, scan, then schedule), graceful shutdown, and composition-root
//! wiring; docs/spec/runtime-substrate.md and
//! docs/spec/configuration-and-credentials.md keep runtime, subscriber,
//! deployment configuration, and migration policy at this executable
//! boundary.

use std::{
    env,
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use signalbox_application::{
    ClassifyOperatorFailure, InProcessAttemptDispatchGate, InProcessEligibilityWorkSource,
    ModelCallCredentialReference, OperatorFailureClass, SchedulerLoop, SchedulerLoopExit,
    StartEligibleTurnService, StartupScanService, UuidV7StartEligibleTurnIdGenerator,
    UuidV7StartupScanIdGenerator,
};
#[cfg(test)]
use signalbox_application::{EligibilityPass, EligibilityWorkSource};
use signalbox_domain::{SessionId, TurnId};
use signalbox_hubd::{
    ANTHROPIC_CREDENTIAL_REFERENCE, ActivatedTurnPass, FatalExecutionSupervisor, FencedHubDatabase,
    FencedHubDatabaseError, FileCredentialAccess, HubModelConfiguration, LocalProcessListener,
    PostgresProviderModelExecution, ProcessRuntime, ProcessRuntimeError, SingleHubGuard,
};
use signalbox_model_provider_runtime::RuntimeModelCallProvider;
use signalbox_model_runtime::CredentialReference;
use signalbox_model_runtime_anthropic::{AnthropicConfig, AnthropicRuntime};
use signalbox_persistence::{
    migrate, model_execution::PostgresModelCallRepository, scheduler::PostgresEligibilitySweep,
    start_eligible_turn::StartEligibleTurnRepository, startup::PostgresStartupScanRepository,
};
use tokio::{
    pin, select,
    sync::{oneshot, watch},
    task::{JoinError, JoinSet},
    time::{sleep, timeout},
};

const GRACEFUL_SHUTDOWN_WINDOW: Duration = Duration::from_secs(30);
const MODEL_CONFIGURATION_FILE_ENVIRONMENT: &str = "SIGNALBOX_CONFIG_FILE";
const ANTHROPIC_API_KEY_FILE_ENVIRONMENT: &str = "ANTHROPIC_API_KEY_FILE";
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
    blocker_count: Option<u64>,
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
            blocker_count: None,
            session: None,
            turn: None,
        }
    }

    const fn classified(phase: RuntimePhase, failure_class: OperatorFailureClass) -> Self {
        Self {
            phase,
            failure_class,
            blocker_count: None,
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
            blocker_count: None,
            session,
            turn,
        }
    }

    const fn recovery_blocked(pending_steering_count: u64) -> Self {
        Self {
            phase: RuntimePhase::StartupScan,
            failure_class: OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            blocker_count: Some(pending_steering_count),
            session: None,
            turn: None,
        }
    }
}

struct HubConfiguration {
    database_url: String,
    model_configuration_file: PathBuf,
    anthropic_api_key_file: PathBuf,
    process_socket_path: PathBuf,
}

impl HubConfiguration {
    fn from_environment() -> Result<Self, HubRuntimeError> {
        Self::from_values(
            env::var_os("DATABASE_URL"),
            env::var_os(MODEL_CONFIGURATION_FILE_ENVIRONMENT),
            env::var_os(ANTHROPIC_API_KEY_FILE_ENVIRONMENT),
            env::var_os(PROCESS_SOCKET_PATH_ENVIRONMENT),
        )
    }

    fn from_values(
        database_url: Option<OsString>,
        model_configuration_file: Option<OsString>,
        anthropic_api_key_file: Option<OsString>,
        process_socket_path: Option<OsString>,
    ) -> Result<Self, HubRuntimeError> {
        let database_url = database_url
            .ok_or_else(|| HubRuntimeError::infrastructure(RuntimePhase::Configuration))?
            .into_string()
            .map_err(|_| HubRuntimeError::infrastructure(RuntimePhase::Configuration))?;
        if database_url.is_empty() {
            return Err(HubRuntimeError::infrastructure(RuntimePhase::Configuration));
        }
        let model_configuration_file = required_path(model_configuration_file)?;
        let anthropic_api_key_file = required_path(anthropic_api_key_file)?;
        let process_socket_path = required_path(process_socket_path)?;

        Ok(Self {
            database_url,
            model_configuration_file,
            anthropic_api_key_file,
            process_socket_path,
        })
    }

    fn database_url(&self) -> &str {
        &self.database_url
    }

    fn model_configuration_file(&self) -> &Path {
        &self.model_configuration_file
    }

    fn anthropic_api_key_file(&self) -> PathBuf {
        self.anthropic_api_key_file.clone()
    }

    fn process_socket_path(&self) -> &Path {
        &self.process_socket_path
    }
}

fn required_path(value: Option<OsString>) -> Result<PathBuf, HubRuntimeError> {
    let value =
        value.ok_or_else(|| HubRuntimeError::infrastructure(RuntimePhase::Configuration))?;
    if value.is_empty() {
        Err(HubRuntimeError::infrastructure(RuntimePhase::Configuration))
    } else {
        Ok(PathBuf::from(value))
    }
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
    SchedulerFailed,
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

const fn should_close_pool(outcome: &Result<ShutdownOutcome, HubRuntimeError>) -> bool {
    matches!(
        outcome,
        Ok(ShutdownOutcome::Clean
            | ShutdownOutcome::ExecutionFailed
            | ShutdownOutcome::RuntimeFailed)
            | Err(_)
    )
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

async fn wait_for_guard_loss(guard: &mut SingleHubGuard) {
    loop {
        sleep(GUARD_CHECK_INTERVAL).await;
        if guard.check().await.is_err() {
            return;
        }
    }
}

fn runtime_task_completed_cleanly(completed: Result<RuntimeTaskExit, JoinError>) -> bool {
    matches!(
        completed,
        Ok(RuntimeTaskExit::Scheduler(SchedulerLoopExit::Shutdown))
            | Ok(RuntimeTaskExit::Process(Ok(())))
    )
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
        (
            RuntimeStopCause::ProcessRuntimeFailed | RuntimeStopCause::SchedulerFailed,
            RuntimeDrainOutcome::Complete,
        ) => ShutdownOutcome::RuntimeFailed,
        (
            RuntimeStopCause::ProcessRuntimeFailed | RuntimeStopCause::SchedulerFailed,
            RuntimeDrainOutcome::GraceWindowExpired,
        ) => ShutdownOutcome::RuntimeFailedAfterGraceWindow,
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
    let configuration = HubConfiguration::from_environment()?;
    let model_configuration = HubModelConfiguration::read(configuration.model_configuration_file())
        .map_err(|_| HubRuntimeError::infrastructure(RuntimePhase::Configuration))?;
    let credential_access = FileCredentialAccess::new(
        configuration.anthropic_api_key_file(),
        CredentialReference::new(ANTHROPIC_CREDENTIAL_REFERENCE),
    );
    let credential_reference =
        ModelCallCredentialReference::new(credential_access.credential_reference().as_str());
    let anthropic = AnthropicRuntime::new(AnthropicConfig::new(), credential_access)
        .map_err(|_| HubRuntimeError::infrastructure(RuntimePhase::Configuration))?;
    let provider =
        RuntimeModelCallProvider::new(anthropic, model_configuration.runtime_model_catalog());
    let model_targets = model_configuration.target_catalog();
    let database = FencedHubDatabase::connect_production(configuration.database_url())
        .await
        .map_err(|error| {
            let phase = match error {
                FencedHubDatabaseError::InitializeFence(_) => RuntimePhase::Migration,
                FencedHubDatabaseError::ParseOptions(_)
                | FencedHubDatabaseError::ConnectBootstrap(_)
                | FencedHubDatabaseError::AcquireGuard(_)
                | FencedHubDatabaseError::AdvanceFence(_)
                | FencedHubDatabaseError::ConnectFencedPool(_) => RuntimePhase::DatabaseConnection,
            };
            HubRuntimeError::infrastructure(phase)
        })?;
    let (mut guard, pool, _generation) = database.into_parts();

    let migration_pool = pool.clone();
    let scan_pool = pool.clone();
    let startup = migrate_scan_then_schedule(
        async move {
            migrate(&migration_pool)
                .await
                .map_err(|_| HubRuntimeError::infrastructure(RuntimePhase::Migration))?;
            tracing::info!(phase = ?RuntimePhase::Migration, "hub startup phase completed");
            Ok(())
        },
        async move {
            let mut scan = StartupScanService::new(
                UuidV7StartupScanIdGenerator,
                PostgresStartupScanRepository::new(scan_pool),
            );
            let outcome = scan.execute().await.map_err(|error| {
                HubRuntimeError::startup_scan(
                    error.operator_failure_class(),
                    error.session(),
                    error.repository_error().corruption_turn(),
                )
            })?;
            if outcome.is_complete() {
                tracing::info!(
                    phase = ?RuntimePhase::StartupScan,
                    recovered_turn_count = outcome.recovered_turn_count(),
                    "hub startup phase completed"
                );
                Ok(())
            } else {
                let blocker_count = u64::try_from(outcome.pending_steering_sessions().len())
                    .map_err(|_| {
                        HubRuntimeError::classified(
                            RuntimePhase::StartupScan,
                            OperatorFailureClass::CallerOrHubBug,
                        )
                    })?;
                Err(HubRuntimeError::recovery_blocked(blocker_count))
            }
        },
        || std::future::ready(()),
    )
    .await;
    if let Err(error) = startup {
        pool.close().await;
        let _ = guard.close().await;
        return Err(error);
    }

    let listener = match LocalProcessListener::bind(configuration.process_socket_path()) {
        Ok(listener) => listener,
        Err(_) => {
            pool.close().await;
            let _ = guard.close().await;
            return Err(HubRuntimeError::infrastructure(RuntimePhase::SocketBinding));
        }
    };
    tracing::info!(
        phase = ?RuntimePhase::SocketBinding,
        "hub startup phase completed"
    );

    let scheduler_pool = pool.clone();
    let sweep = PostgresEligibilitySweep::new(scheduler_pool.clone());
    let (eligibility_nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
    let process_runtime = ProcessRuntime::new(
        listener,
        scheduler_pool.clone(),
        eligibility_nudge,
        model_configuration,
    );
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(PostgresProviderModelExecution::new(
            PostgresModelCallRepository::new(
                scheduler_pool.clone(),
                model_targets,
                credential_reference,
            ),
            InProcessAttemptDispatchGate::default(),
            provider,
        ));
    let pass = ActivatedTurnPass::new(
        StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(scheduler_pool),
        ),
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
    tracing::info!(phase = ?RuntimePhase::Scheduling, "hub runtime started");

    let outcome = {
        let guard_loss = wait_for_guard_loss(&mut guard);
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
                    Some(Ok(RuntimeTaskExit::Process(result))) => {
                        drop(result);
                        RuntimeStopCause::ProcessRuntimeFailed
                    }
                    Some(Ok(RuntimeTaskExit::Scheduler(_))) | Some(Err(_)) | None => {
                        RuntimeStopCause::SchedulerFailed
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
            let (drain, components_clean) = {
                let drain_tasks = async {
                    let mut clean = true;
                    while let Some(completed) = runtime_tasks.join_next().await {
                        clean &= runtime_task_completed_cleanly(completed);
                    }
                    clean
                };
                pin!(drain_tasks);
                select! {
                    () = &mut guard_loss => (RuntimeDrainOutcome::GuardLost, false),
                    result = timeout(GRACEFUL_SHUTDOWN_WINDOW, &mut drain_tasks) => {
                        match result {
                            Ok(clean) => (RuntimeDrainOutcome::Complete, clean),
                            Err(_) => (RuntimeDrainOutcome::GraceWindowExpired, false),
                        }
                    }
                }
            };
            if drain != RuntimeDrainOutcome::Complete {
                runtime_tasks.abort_all();
                while runtime_tasks.join_next().await.is_some() {}
            } else if !components_clean {
                cause = RuntimeStopCause::ProcessRuntimeFailed;
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
        pool.close().await;
    } else if should_close_pool(&Ok(outcome)) {
        pool.close().await;
        if guard.close().await.is_err() {
            return Ok(ShutdownOutcome::RuntimeFailed);
        }
    }
    Ok(outcome)
}

fn install_tracing_subscriber() {
    tracing_subscriber::fmt()
        .compact()
        .with_max_level(tracing::Level::INFO)
        .init();
}

#[tokio::main]
async fn main() -> ExitCode {
    install_tracing_subscriber();

    match run_hub().await {
        Ok(ShutdownOutcome::Clean) => {
            tracing::info!("hub shutdown completed");
            ExitCode::SUCCESS
        }
        Ok(ShutdownOutcome::GraceWindowExpired) => {
            tracing::warn!(
                grace_window_seconds = GRACEFUL_SHUTDOWN_WINDOW.as_secs(),
                "hub shutdown grace window expired; abandoning in-flight work"
            );
            ExitCode::SUCCESS
        }
        Ok(ShutdownOutcome::SignalListenerFailed) => {
            let error = HubRuntimeError::infrastructure(RuntimePhase::Scheduling);
            tracing::error!(
                phase = ?error.phase,
                failure_class = ?error.failure_class,
                "hub runtime failed"
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
                "hub runtime component failed"
            );
            ExitCode::FAILURE
        }
        Ok(ShutdownOutcome::RuntimeFailedAfterGraceWindow) => {
            let error = HubRuntimeError::infrastructure(RuntimePhase::Runtime);
            tracing::error!(
                phase = ?error.phase,
                failure_class = ?error.failure_class,
                grace_window_seconds = GRACEFUL_SHUTDOWN_WINDOW.as_secs(),
                "hub runtime component failed and shutdown grace expired; abandoning in-flight work"
            );
            ExitCode::FAILURE
        }
        Err(error) => {
            tracing::error!(
                phase = ?error.phase,
                failure_class = ?error.failure_class,
                blocker_count = error.blocker_count,
                session_id = ?error.session,
                turn_id = ?error.turn,
                "hub startup failed"
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
        rc::Rc,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use signalbox_application::{
        ClassifyOperatorFailure, EligibilityPass, EligibilityWorkSource, OperatorFailureClass,
        SchedulerLoop,
    };
    use signalbox_domain::{SessionId, TurnId};
    use tokio::sync::oneshot;
    use uuid::Uuid;

    use super::{
        HubConfiguration, HubRuntimeError, RuntimeDrainOutcome, RuntimePhase, RuntimeStopCause,
        SchedulerStopCause, ShutdownOutcome, completed_runtime_outcome, migrate_scan_then_schedule,
        run_scheduler_until_shutdown, should_close_pool,
    };

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
                Some(OsString::from("key")),
                Some(OsString::from("/tmp/signalbox.sock")),
            )
            .err(),
            Some(HubRuntimeError::infrastructure(RuntimePhase::Configuration))
        );
        assert_eq!(
            HubConfiguration::from_values(
                Some(OsString::from("postgres://secret")),
                Some(OsString::from("")),
                Some(OsString::from("key")),
                Some(OsString::from("/tmp/signalbox.sock")),
            )
            .err(),
            Some(HubRuntimeError::infrastructure(RuntimePhase::Configuration))
        );
        assert_eq!(
            HubConfiguration::from_values(
                Some(OsString::from("postgres://secret")),
                Some(OsString::from("models.toml")),
                Some(OsString::from("key")),
                None,
            )
            .err(),
            Some(HubRuntimeError::infrastructure(RuntimePhase::Configuration))
        );

        let configuration = HubConfiguration::from_values(
            Some(OsString::from("postgres://secret")),
            Some(OsString::from("models.toml")),
            Some(OsString::from("key")),
            Some(OsString::from("/tmp/signalbox.sock")),
        )
        .expect("nonempty deployment values are accepted before I/O");
        assert_eq!(configuration.database_url(), "postgres://secret");
        assert_eq!(
            configuration.model_configuration_file(),
            std::path::Path::new("models.toml")
        );
        assert_eq!(
            configuration.anthropic_api_key_file(),
            std::path::PathBuf::from("key")
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
                blocker_count: None,
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
        assert!(should_close_pool(&Ok(ShutdownOutcome::ExecutionFailed)));
        assert!(should_close_pool(&Ok(ShutdownOutcome::RuntimeFailed)));
        assert!(should_close_pool(&Ok(ShutdownOutcome::Clean)));
        assert!(should_close_pool(&Err(HubRuntimeError::infrastructure(
            RuntimePhase::Migration
        ))));
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
    }
}
