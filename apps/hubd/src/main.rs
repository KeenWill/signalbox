//! Signalbox hub composition root.
//!
//! ADR-0044 keeps runtime, subscriber, deployment configuration, migration,
//! startup ordering, and shutdown policy at this executable boundary.

use std::{env, ffi::OsString, future::Future, process::ExitCode, time::Duration};

use signalbox_application::{
    ClassifyOperatorFailure, EligibilityPass, EligibilityWorkSource,
    InProcessEligibilityWorkSource, OperatorFailureClass, SchedulerLoop, StartEligibleTurnService,
    StartupScanService, UuidV7StartEligibleTurnIdGenerator, UuidV7StartupScanIdGenerator,
};
use signalbox_domain::{SessionId, TurnId};
use signalbox_persistence::{
    connect_production, migrate, scheduler::PostgresEligibilitySweep,
    start_eligible_turn::StartEligibleTurnRepository, startup::PostgresStartupScanRepository,
};
use tokio::{pin, select, sync::oneshot, time::timeout};

const GRACEFUL_SHUTDOWN_WINDOW: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimePhase {
    Configuration,
    DatabaseConnection,
    Migration,
    StartupScan,
    Scheduling,
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
}

impl HubConfiguration {
    fn from_environment() -> Result<Self, HubRuntimeError> {
        Self::from_database_url(env::var_os("DATABASE_URL"))
    }

    fn from_database_url(value: Option<OsString>) -> Result<Self, HubRuntimeError> {
        let database_url = value
            .ok_or_else(|| HubRuntimeError::infrastructure(RuntimePhase::Configuration))?
            .into_string()
            .map_err(|_| HubRuntimeError::infrastructure(RuntimePhase::Configuration))?;
        if database_url.is_empty() {
            return Err(HubRuntimeError::infrastructure(RuntimePhase::Configuration));
        }

        Ok(Self { database_url })
    }

    fn database_url(&self) -> &str {
        &self.database_url
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShutdownOutcome {
    Clean,
    GraceWindowExpired,
    SignalListenerFailed,
}

const fn should_close_pool(outcome: &Result<ShutdownOutcome, HubRuntimeError>) -> bool {
    matches!(outcome, Ok(ShutdownOutcome::Clean) | Err(_))
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
    Shutdown: Future<Output = bool>,
{
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    let scheduler_run = scheduler.run_until(async move {
        let _ = shutdown_receiver.await;
    });
    pin!(scheduler_run);
    pin!(shutdown);

    let listener_failed = select! {
        listener_failed = &mut shutdown => listener_failed,
        _ = &mut scheduler_run => {
            return ShutdownOutcome::SignalListenerFailed;
        }
    };
    let _ = shutdown_sender.send(());

    match timeout(grace_window, &mut scheduler_run).await {
        _ if listener_failed => ShutdownOutcome::SignalListenerFailed,
        Ok(_) => ShutdownOutcome::Clean,
        Err(_) => ShutdownOutcome::GraceWindowExpired,
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
    let pool = connect_production(configuration.database_url())
        .await
        .map_err(|_| HubRuntimeError::infrastructure(RuntimePhase::DatabaseConnection))?;

    let migration_pool = pool.clone();
    let scan_pool = pool.clone();
    let scheduler_pool = pool.clone();
    let outcome = migrate_scan_then_schedule(
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
        || async move {
            let sweep = PostgresEligibilitySweep::new(scheduler_pool.clone());
            let (eligibility_nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
            let pass = StartEligibleTurnService::new(
                UuidV7StartEligibleTurnIdGenerator,
                StartEligibleTurnRepository::new(scheduler_pool),
            );
            let scheduler = SchedulerLoop::new(work_source, pass);
            tracing::info!(
                phase = ?RuntimePhase::Scheduling,
                "hub scheduler started"
            );
            let outcome = run_scheduler_until_shutdown(
                scheduler,
                shutdown_requested(),
                GRACEFUL_SHUTDOWN_WINDOW,
            )
            .await;
            drop(eligibility_nudge);
            outcome
        },
    )
    .await;

    // A timed-out scheduler may still hold a connection. Waiting for that
    // checkout here would silently extend the shutdown window that just
    // expired. Dropping the pool is safe because startup recovery owns any
    // abandoned durable work.
    if should_close_pool(&outcome) {
        pool.close().await;
    }
    outcome
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
        HubConfiguration, HubRuntimeError, RuntimePhase, ShutdownOutcome,
        migrate_scan_then_schedule, run_scheduler_until_shutdown, should_close_pool,
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
    fn database_url_is_required_and_never_debug_rendered() {
        assert_eq!(
            HubConfiguration::from_database_url(None).err(),
            Some(HubRuntimeError::infrastructure(RuntimePhase::Configuration))
        );
        assert_eq!(
            HubConfiguration::from_database_url(Some(OsString::from(""))).err(),
            Some(HubRuntimeError::infrastructure(RuntimePhase::Configuration))
        );

        let configuration =
            HubConfiguration::from_database_url(Some(OsString::from("postgres://secret")))
                .expect("nonempty deployment value is accepted before SQLx parsing");
        assert_eq!(configuration.database_url(), "postgres://secret");
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

        async fn run(&mut self, _session: SessionId) -> Result<(), Self::Error> {
            let entered = self
                .entered
                .lock()
                .expect("the fake pass state is not poisoned")
                .take()
                .expect("the test pass runs once")
                .send(());
            entered.expect("the test waits for pass entry");
            pending().await
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
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
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
                false
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
            run_scheduler_until_shutdown(scheduler, ready(false), Duration::from_secs(1)).await,
            ShutdownOutcome::Clean
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
                true
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
        assert!(should_close_pool(&Ok(ShutdownOutcome::Clean)));
        assert!(should_close_pool(&Err(HubRuntimeError::infrastructure(
            RuntimePhase::Migration
        ))));
    }
}
