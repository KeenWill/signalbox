//! Daemon-owned local workflow attempts and restart recovery; docs/spec/workflows.md.

use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::{StreamExt, stream::FuturesUnordered};
use signalbox_domain::{
    DeliveryKind, EffectRequest, InlineFramePayload, ProgramCapability, ProgramFault, ProgramRunId,
    RejectReason, RequestFrame, RequestKind,
};
use signalbox_persistence::{
    program_journal::{ProgramJournalRepository, ProgramJournalRepositoryError},
    program_registration::{ProgramRegistrationError, ProgramRegistrationRepository},
};
use signalbox_workflow_runtime::{
    LiveDeliveryFailure, LiveDeliverySource, ProgramExecutionOutcome, WorkflowHost,
    WorkflowHostError, WorkflowHostProtocolError,
    effects::{EffectExecutor, EffectInvocation, EffectRecovery},
    native::NativeProgramError,
    primitives::{
        DurablePrimitives, PrimitiveClock, PrimitiveEvents, PrimitiveWake, SystemPrimitiveClock,
    },
};
use sqlx::PgPool;
use tokio::sync::{mpsc, oneshot, watch};

mod effects;
use effects::{AttemptEffects, RuntimeEffects};

#[cfg(target_os = "linux")]
use super::{CLOCK_ENTRY, CLOCK_REVISION, compiled_catalog};
use super::{
    WorkflowService,
    eval::{EvalServices, EvaluationEffects},
};

#[derive(Debug, signalbox_derive::OperatorError)]
pub enum WorkflowRuntimeError {
    #[error("workflow registration: {field_0}")]
    Registration(#[source] ProgramRegistrationError),
    #[error("workflow journal: {field_0}")]
    Journal(#[source] ProgramJournalRepositoryError),
    #[error("workflow receipt: {field_0}")]
    Receipt(#[source] LiveDeliveryFailure),
    #[error("workflow runtime: {field_0}")]
    Runtime(#[source] std::io::Error),
    #[error("workflow catalog: {field_0}")]
    Catalog(#[source] NativeProgramError),
    #[error("native executable is unavailable in the daemon catalog")]
    NativeUnavailable,
    #[error("workflow runner stopped; any committed admission remains recoverable")]
    Stopped,
    #[error("workflow runner task: {field_0}")]
    Join(#[source] tokio::task::JoinError),
    #[error("workflow attempt {run:?}: {source}")]
    Attempt {
        run: ProgramRunId,
        #[source]
        source: Box<WorkflowHostError>,
    },
}

impl From<ProgramRegistrationError> for WorkflowRuntimeError {
    fn from(error: ProgramRegistrationError) -> Self {
        Self::Registration(error)
    }
}

impl WorkflowRuntimeError {
    /// Operator-safe classification without program payloads or isolate exception text.
    pub fn cause_code(&self) -> &'static str {
        match self {
            Self::Registration(_) => "workflow_registration_failed",
            Self::Journal(_) => "workflow_journal_failed",
            Self::Receipt(_) => "workflow_delivery_failed",
            Self::Runtime(_) => "workflow_runtime_failed",
            Self::Catalog(_) => "workflow_catalog_failed",
            Self::NativeUnavailable => "workflow_native_unavailable",
            Self::Stopped => "workflow_runner_stopped",
            Self::Join(_) => "workflow_runner_join_failed",
            Self::Attempt { source, .. } => match source.as_ref() {
                WorkflowHostError::Journal(_) => "workflow_journal_failed",
                WorkflowHostError::Registration(_) => "workflow_registration_failed",
                WorkflowHostError::JournalMissing(_) => "workflow_journal_missing",
                WorkflowHostError::Isolate(_) => "workflow_isolate_failed",
                WorkflowHostError::LiveDelivery(_) => "workflow_delivery_failed",
                WorkflowHostError::Nondeterminism { .. } => "workflow_nondeterminism",
                WorkflowHostError::Protocol(_) => "workflow_host_protocol_failed",
            },
        }
    }
}

pub(super) enum WorkflowWake {
    Start(ProgramRunId),
    Cancel(ProgramRunId),
}

/// One runner per fenced daemon; only this runner starts its run attempts.
pub struct WorkflowRuntime {
    service: WorkflowService,
    pool: PgPool,
    host: WorkflowHost,
    journal: ProgramJournalRepository,
    registrations: ProgramRegistrationRepository,
    wake: mpsc::UnboundedReceiver<WorkflowWake>,
    repository_watch: Option<crate::repo_watch_runtime::RepositoryWatchRuntime>,
    eval: Option<EvalServices>,
    eval_ready: Arc<AtomicBool>,
}

impl WorkflowRuntime {
    pub fn new(pool: PgPool) -> Result<(WorkflowService, Self), WorkflowRuntimeError> {
        let admission = ProgramRegistrationRepository::new(pool.clone());
        // Keep the fencing hooks, but own the executor's connection lifetime.
        let pool = pool
            .options()
            .clone()
            .min_connections(0)
            .max_connections(1)
            .connect_lazy_with(pool.connect_options().as_ref().clone());
        let journal = ProgramJournalRepository::new(pool.clone());
        let host = WorkflowHost::new(journal.clone());
        #[cfg(target_os = "linux")]
        let (host, clock_executable, observation_executable, eval_executable) = {
            let catalog = compiled_catalog()?;
            let executable = catalog
                .executable(CLOCK_ENTRY, CLOCK_REVISION)
                .ok_or(WorkflowRuntimeError::NativeUnavailable)?;
            let observation = catalog
                .executable(
                    super::repo_watch::observe::OBSERVE_ENTRY,
                    super::repo_watch::observe::OBSERVE_REVISION,
                )
                .ok_or(WorkflowRuntimeError::NativeUnavailable)?;
            let eval = catalog.executable(super::eval::EVAL_ENTRY, super::eval::EVAL_REVISION);
            (
                host.with_native_catalog(catalog),
                Some(executable),
                Some(observation),
                eval,
            )
        };
        #[cfg(not(target_os = "linux"))]
        let clock_executable = None;
        #[cfg(not(target_os = "linux"))]
        let observation_executable = None;
        let registrations = ProgramRegistrationRepository::new(pool.clone());
        let (wake, receiver) = mpsc::unbounded_channel();
        let eval_ready = Arc::new(AtomicBool::new(false));
        let service = WorkflowService {
            registrations: admission,
            wake,
            clock_executable,
            observation_executable,
            eval_executable,
            eval_ready: eval_ready.clone(),
        };
        Ok((
            service.clone(),
            Self {
                service,
                pool,
                host,
                journal,
                registrations,
                wake: receiver,
                repository_watch: None,
                eval: None,
                eval_ready,
            },
        ))
    }

    /// Routes repository-watch effects through the daemon's serialized module services.
    pub fn with_repository_watch(
        mut self,
        runtime: Option<crate::repo_watch_runtime::RepositoryWatchRuntime>,
    ) -> Self {
        if let Some(runtime) = &runtime {
            runtime.set_workflow_service(self.service.clone());
        }
        self.repository_watch = runtime;
        self
    }

    /// Supplies host-owned corpus, blob and judge services for evaluation runs.
    pub fn with_eval(mut self, services: EvalServices) -> Self {
        self.eval = Some(services);
        self.eval_ready.store(true, Ordering::Release);
        self
    }

    /// Owns non-Send isolate/root futures on one local executor and joins it on shutdown.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<(), WorkflowRuntimeError> {
        self.run_with_primitives(shutdown, |events| DaemonPrimitives {
            durable: DurablePrimitives::new(events, SystemPrimitiveClock),
        })
        .await
    }

    async fn run_with_primitives<P: LiveDeliverySource + 'static>(
        self,
        shutdown: impl Future<Output = ()>,
        primitives: impl Fn(RuntimeEvents) -> P + Send + 'static,
    ) -> Result<(), WorkflowRuntimeError> {
        let (stop, stopped) = oneshot::channel();
        let stop = StopWorkflow {
            stop: Some(stop),
            host: self.host.clone(),
        };
        let mut worker = tokio::task::spawn_blocking(move || {
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(WorkflowRuntimeError::Runtime)?;
            executor.block_on(async move {
                let pool = self.pool.clone();
                let result = self.drive(stopped, primitives).await;
                pool.close().await;
                result
            })
        });
        tokio::select! {
            result = &mut worker => result.map_err(WorkflowRuntimeError::Join)?,
            () = shutdown => {
                drop(stop);
                worker.await.map_err(WorkflowRuntimeError::Join)?
            }
        }
    }

    async fn drive<P: LiveDeliverySource + 'static>(
        mut self,
        stopped: oneshot::Receiver<()>,
        primitives: impl Fn(RuntimeEvents) -> P,
    ) -> Result<(), WorkflowRuntimeError> {
        let mut receipt_effects =
            RuntimeEffects::new(self.repository_watch.clone(), self.journal.clone(), None);
        let execution = async {
            receipt_effects
                .acknowledge()
                .await
                .map_err(WorkflowRuntimeError::Receipt)?;
            let mut listener = self
                .journal
                .listen_all()
                .await
                .map_err(WorkflowRuntimeError::Journal)?;
            let (changed, wake) = watch::channel(());
            let events = RuntimeEvents {
                journal: self.journal.clone(),
                wake,
            };
            let mut active = BTreeMap::new();
            let mut attempts = FuturesUnordered::new();
            for run in self.registrations.unfinished_runs().await? {
                let (cancel, cancelled) = oneshot::channel();
                active.insert(run, Some(cancel));
                attempts.push(cancellable_attempt(
                    self.host.clone(),
                    self.journal.clone(),
                    run,
                    primitives(events.clone()),
                    self.repository_watch.clone(),
                    self.eval.clone(),
                    cancelled,
                ));
            }
            loop {
                tokio::select! {
                    result = listener.changed() => {
                        result.map_err(WorkflowRuntimeError::Journal)?;
                        changed.send_replace(());
                    }
                    Some(wake) = self.wake.recv() => {
                        match wake {
                            WorkflowWake::Start(run) => {
                                if let std::collections::btree_map::Entry::Vacant(entry) = active.entry(run) {
                                    let (cancel, cancelled) = oneshot::channel();
                                    entry.insert(Some(cancel));
                                    attempts.push(cancellable_attempt(self.host.clone(), self.journal.clone(), run, primitives(events.clone()), self.repository_watch.clone(), self.eval.clone(), cancelled));
                                }
                            }
                            WorkflowWake::Cancel(run) => {
                                if let Some(cancel) = active.get_mut(&run).and_then(Option::take) {
                                    let _ = cancel.send(());
                                }
                            }
                        }
                    }
                    Some(completed) = attempts.next(), if !attempts.is_empty() => {
                        active.remove(&completed?);
                    }
                    else => std::future::pending::<()>().await,
                }
            }
        };
        let result = interruptible(execution, stopped).await.unwrap_or(Ok(()));
        receipt_effects
            .acknowledge()
            .await
            .map_err(WorkflowRuntimeError::Receipt)?;
        result
    }
}

struct StopWorkflow {
    stop: Option<oneshot::Sender<()>>,
    host: WorkflowHost,
}

impl Drop for StopWorkflow {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.host.interrupt();
    }
}

fn cancellable_attempt<P: LiveDeliverySource + 'static>(
    host: WorkflowHost,
    journal: ProgramJournalRepository,
    run: ProgramRunId,
    primitives: P,
    repository_watch: Option<crate::repo_watch_runtime::RepositoryWatchRuntime>,
    eval: Option<EvalServices>,
    cancelled: oneshot::Receiver<()>,
) -> Pin<Box<dyn Future<Output = Result<ProgramRunId, WorkflowRuntimeError>>>> {
    Box::pin(async move {
        let mut receipts = RuntimeEffects::new(repository_watch.clone(), journal.clone(), None);
        let result = interruptible(
            attempt(host, journal, run, primitives, repository_watch, eval),
            cancelled,
        )
        .await
        .unwrap_or(Ok(run));
        receipts
            .acknowledge()
            .await
            .map_err(WorkflowRuntimeError::Receipt)?;
        result
    })
}

fn attempt<P: LiveDeliverySource + 'static>(
    host: WorkflowHost,
    journal: ProgramJournalRepository,
    run: ProgramRunId,
    mut primitives: P,
    repository_watch: Option<crate::repo_watch_runtime::RepositoryWatchRuntime>,
    eval: Option<EvalServices>,
) -> Pin<Box<dyn Future<Output = Result<ProgramRunId, WorkflowRuntimeError>>>> {
    Box::pin(async move {
        let mut effects = RuntimeEffects::new(repository_watch, journal.clone(), eval);
        let execution = drive_run(&host, &journal, run, &mut primitives, &mut effects).await;
        let Err(source) = execution else {
            return Ok(run);
        };
        if host.is_interrupted() {
            return Ok(run);
        }
        let program_failure = matches!(
            source,
            WorkflowHostError::Isolate(_)
                | WorkflowHostError::Protocol(WorkflowHostProtocolError::Stalled)
        ) || matches!(source, WorkflowHostError::LiveDelivery(_))
            && effects.rejected.is_some();
        let error = WorkflowRuntimeError::Attempt {
            run,
            source: Box::new(source),
        };
        if !program_failure {
            return Err(error);
        }
        tracing::warn!(?run, cause = error.cause_code(), "workflow program failed");
        record_program_failure(
            &journal,
            run,
            InlineFramePayload::new(error.to_string().into_bytes()),
        )
        .await
        .map_err(|source| WorkflowRuntimeError::Attempt { run, source })?;
        Ok(run)
    })
}

async fn interruptible<T>(execution: impl Future<Output = T>, stopped: impl Future) -> Option<T> {
    tokio::select! {
        biased;
        _ = stopped => None,
        result = execution => Some(result),
    }
}

#[allow(
    clippy::result_large_err,
    reason = "The host retains its replay fault inline."
)]
async fn drive_run(
    host: &WorkflowHost,
    journal: &ProgramJournalRepository,
    run: ProgramRunId,
    primitives: &mut impl LiveDeliverySource,
    effects: &mut impl AttemptEffects,
) -> Result<(), WorkflowHostError> {
    loop {
        let outcome = host.execute_registered(run, primitives, effects).await;
        effects.acknowledge().await?;
        let outcome = outcome?;
        let ProgramExecutionOutcome::Suspended(outstanding) = outcome else {
            return Ok::<(), WorkflowHostError>(());
        };
        let tail = {
            let loaded = journal
                .load(run)
                .await?
                .ok_or(WorkflowHostError::JournalMissing(run))?;
            if loaded.terminal_delivery().is_some() {
                return Ok(());
            }
            loaded
                .entries()
                .last()
                .map_or(0, |entry| entry.position().as_u64())
        };
        let delivery = primitives.next_delivery(&outstanding).await?;
        if journal
            .append_delivery_if_tail(run, tail, delivery)
            .await?
            .is_none()
        {
            let loaded = journal
                .load(run)
                .await?
                .ok_or(WorkflowHostError::JournalMissing(run))?;
            if loaded.terminal_delivery().is_some() {
                return Ok(());
            }
            return Err(WorkflowHostProtocolError::JournalTailChanged.into());
        }
    }
}

async fn record_program_failure(
    journal: &ProgramJournalRepository,
    run: ProgramRunId,
    evidence: InlineFramePayload,
) -> Result<(), Box<WorkflowHostError>> {
    let loaded = journal
        .load(run)
        .await
        .map_err(WorkflowHostError::from)?
        .ok_or(WorkflowHostError::JournalMissing(run))?;
    if loaded.terminal_delivery().is_some() {
        return Ok(());
    }
    let tail = loaded
        .entries()
        .last()
        .map_or(0, |entry| entry.position().as_u64());
    if journal
        .append_delivery_if_tail(
            run,
            tail,
            DeliveryKind::Fault(ProgramFault::ProgramError(evidence)),
        )
        .await
        .map_err(WorkflowHostError::from)?
        .is_none()
        && journal
            .load(run)
            .await
            .map_err(WorkflowHostError::from)?
            .is_none_or(|loaded| loaded.terminal_delivery().is_none())
    {
        return Err(WorkflowHostError::from(WorkflowHostProtocolError::JournalTailChanged).into());
    }
    Ok(())
}

#[derive(Clone)]
struct RuntimeEvents {
    journal: ProgramJournalRepository,
    wake: watch::Receiver<()>,
}

impl PrimitiveEvents for RuntimeEvents {
    type Wake = SharedJournalWake;

    async fn next_event(
        &mut self,
        wait: signalbox_domain::program_primitives::AwaitProgramEvent,
    ) -> Result<Option<signalbox_domain::program_primitives::ProgramEvent>, LiveDeliveryFailure>
    {
        PrimitiveEvents::next_event(&mut self.journal, wait).await
    }

    async fn listen(&mut self, _: &[ProgramRunId]) -> Result<Self::Wake, LiveDeliveryFailure> {
        Ok(SharedJournalWake(self.wake.clone()))
    }
}

struct SharedJournalWake(watch::Receiver<()>);

impl PrimitiveWake for SharedJournalWake {
    async fn changed(&mut self) -> Result<(), LiveDeliveryFailure> {
        self.0
            .changed()
            .await
            .map_err(|error| LiveDeliveryFailure::new(error.to_string()))
    }
}

struct DaemonPrimitives {
    durable: DurablePrimitives<SystemPrimitiveClock, RuntimeEvents>,
}
impl LiveDeliverySource for DaemonPrimitives {
    fn suspend_on_wait(&self, outstanding: &[RequestFrame]) -> bool {
        !outstanding.is_empty()
            && outstanding.iter().all(|frame| {
                matches!(
                    frame.kind(),
                    RequestKind::Sleep(_) | RequestKind::AwaitEvent(_)
                )
            })
    }

    fn next_delivery<'a>(
        &'a mut self,
        outstanding: &'a [RequestFrame],
    ) -> Pin<Box<dyn Future<Output = Result<DeliveryKind, LiveDeliveryFailure>> + 'a>> {
        Box::pin(async move {
            if outstanding
                .first()
                .is_some_and(|frame| matches!(frame.kind(), RequestKind::Now(_)))
            {
                ClockSource.next_delivery(outstanding).await
            } else {
                self.durable.next_delivery(outstanding).await
            }
        })
    }
}

struct ClockSource;
impl LiveDeliverySource for ClockSource {
    fn next_delivery<'a>(
        &'a mut self,
        outstanding: &'a [RequestFrame],
    ) -> Pin<Box<dyn Future<Output = Result<DeliveryKind, LiveDeliveryFailure>> + 'a>> {
        Box::pin(async move {
            let request = outstanding
                .first()
                .ok_or_else(|| LiveDeliveryFailure::new("clock source received no request"))?;
            match request.kind() {
                RequestKind::Now(payload) if payload.as_bytes().is_empty() => {
                    Ok(DeliveryKind::Answer {
                        resolves: request.ordinal(),
                        payload: SystemPrimitiveClock.now()?.encode(),
                    })
                }
                RequestKind::Now(_) => {
                    let seconds = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_err(|error| LiveDeliveryFailure::new(error.to_string()))?
                        .as_secs();
                    Ok(DeliveryKind::Answer {
                        resolves: request.ordinal(),
                        payload: InlineFramePayload::new(seconds.to_be_bytes().to_vec()),
                    })
                }
                _ => Ok(DeliveryKind::Reject {
                    resolves: request.ordinal(),
                    reason: RejectReason::UnsupportedOperation,
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_domain::{RequestOrdinal, program_primitives::UnixMillis};

    #[tokio::test]
    async fn workflows_clock_answers_an_empty_now_request_with_typed_unix_milliseconds() {
        let frame = RequestFrame::new(
            RequestOrdinal::try_from_u64(1).unwrap(),
            None,
            RequestKind::Now(InlineFramePayload::default()),
        );
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let delivery = ClockSource
            .next_delivery(std::slice::from_ref(&frame))
            .await
            .unwrap();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let DeliveryKind::Answer { resolves, payload } = delivery else {
            panic!("clock must answer")
        };
        assert_eq!(resolves, frame.ordinal());
        let milliseconds = UnixMillis::decode(&payload).expect("typed SDK clock answer");
        assert!((before..=after).contains(&u128::from(milliseconds.0)));
    }

    #[tokio::test]
    async fn workflows_clock_answers_a_native_pilot_request_with_unix_seconds() {
        use crate::workflows::ClockInput;
        use signalbox_workflow_runtime::native::NativeValue;

        const PILOT_INPUT: u64 = 731;
        let frame = RequestFrame::new(
            RequestOrdinal::try_from_u64(1).unwrap(),
            None,
            RequestKind::Now(InlineFramePayload::new(
                ClockInput(PILOT_INPUT).encode().unwrap(),
            )),
        );
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let delivery = ClockSource
            .next_delivery(std::slice::from_ref(&frame))
            .await
            .unwrap();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let DeliveryKind::Answer { resolves, payload } = delivery else {
            panic!("clock must answer")
        };
        assert_eq!(resolves, frame.ordinal());
        let seconds = u64::from_be_bytes(payload.as_bytes().try_into().unwrap());
        assert!((before..=after).contains(&seconds));
    }

    #[cfg(all(feature = "test-support", target_os = "linux"))]
    mod postgres {
        use super::*;
        use crate::workflows::{ClockInput, ClockResult};
        use signalbox_domain::{
            JournalFrame, ProgramRegistrationId,
            program_registration::{
                NativeProgramRegistrationRequest, ProgramExecutable, ProgramGrants,
                ProgramRegistrationRequest,
            },
        };
        use signalbox_workflow_runtime::native::NativeValue;
        use std::{error::Error, time::Duration};
        use uuid::Uuid;

        // Bounds only the fixture's wait for a recorded request/result.
        const TEST_TIMEOUT: Duration = Duration::from_secs(30);
        const POLL_INTERVAL: Duration = Duration::from_millis(10);
        const ARBITRARY_INPUT: u64 = 731;

        struct PausedClock {
            reached: mpsc::UnboundedSender<RequestFrame>,
        }
        impl LiveDeliverySource for PausedClock {
            fn next_delivery<'a>(
                &'a mut self,
                outstanding: &'a [RequestFrame],
            ) -> Pin<Box<dyn Future<Output = Result<DeliveryKind, LiveDeliveryFailure>> + 'a>>
            {
                self.reached.send(outstanding[0].clone()).unwrap();
                Box::pin(std::future::pending())
            }
        }

        /// Pauses a selected clock answer and signals when its owning attempt releases it.
        struct SelectedClock {
            selected: mpsc::UnboundedSender<oneshot::Sender<()>>,
            finished: mpsc::UnboundedSender<()>,
        }
        impl LiveDeliverySource for SelectedClock {
            fn suspend_on_wait(&self, outstanding: &[RequestFrame]) -> bool {
                outstanding
                    .iter()
                    .all(|frame| matches!(frame.kind(), RequestKind::Sleep(_)))
            }
            fn next_delivery<'a>(
                &'a mut self,
                outstanding: &'a [RequestFrame],
            ) -> Pin<Box<dyn Future<Output = Result<DeliveryKind, LiveDeliveryFailure>> + 'a>>
            {
                Box::pin(async move {
                    let delivery = match outstanding[0].kind() {
                        RequestKind::Sleep(payload) => DeliveryKind::Wake {
                            resolves: outstanding[0].ordinal(),
                            payload: signalbox_domain::program_primitives::SleepUntil::decode(
                                payload,
                            )
                            .unwrap()
                            .0
                            .encode(),
                        },
                        _ => ClockSource.next_delivery(outstanding).await?,
                    };
                    let (release, released) = oneshot::channel();
                    self.selected.send(release).unwrap();
                    released.await.unwrap();
                    Ok(delivery)
                })
            }
        }
        impl Drop for SelectedClock {
            fn drop(&mut self) {
                let _ = self.finished.send(());
            }
        }

        struct ObservedWait {
            primitives: DaemonPrimitives,
            waiting: mpsc::UnboundedSender<()>,
        }
        impl LiveDeliverySource for ObservedWait {
            fn suspend_on_wait(&self, outstanding: &[RequestFrame]) -> bool {
                self.primitives.suspend_on_wait(outstanding)
            }
            fn next_delivery<'a>(
                &'a mut self,
                outstanding: &'a [RequestFrame],
            ) -> Pin<Box<dyn Future<Output = Result<DeliveryKind, LiveDeliveryFailure>> + 'a>>
            {
                self.waiting.send(()).unwrap();
                self.primitives.next_delivery(outstanding)
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_concurrent_event_waits_share_one_listener_connection()
        -> Result<(), Box<dyn Error>> {
            // A population of concurrent waits, independent of PostgreSQL's connection limit.
            const CONCURRENT_WAITS: usize = 32;
            let (_database, pool, _) =
                signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
            let journal = ProgramJournalRepository::new(pool.clone());
            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            let (waiting, mut waited) = mpsc::unbounded_channel();
            let (stop, stopped) = oneshot::channel();
            let task = tokio::spawn(runner.run_with_primitives(
                async {
                    let _ = stopped.await;
                },
                move |events| ObservedWait {
                    primitives: DaemonPrimitives {
                        durable: DurablePrimitives::new(events, SystemPrimitiveClock),
                    },
                    waiting: waiting.clone(),
                },
            ));
            let mut runs = Vec::new();
            for _ in 0..CONCURRENT_WAITS {
                let source = ProgramRunId::from_uuid(Uuid::now_v7());
                journal.create_stream(source).await?;
                let registration = service.register_javascript(
                    ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                    javascript_request(format!(
                        "import {{ primitives }} from '@signalbox/program-sdk/v1'; await primitives.awaitEvent({{ source: {{ kind: 'program_answers', run: '{}' }}, after: '0' }});",
                        source.into_uuid()), ProgramGrants::new([ProgramCapability::Subscribe])),
                ).await?;
                let run = ProgramRunId::from_uuid(Uuid::now_v7());
                service.start(run, registration.id, &[]).await?;
                tokio::time::timeout(TEST_TIMEOUT, waited.recv())
                    .await?
                    .unwrap();
                runs.push((run, source));
            }
            let listeners: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND query LIKE 'LISTEN %'",
            ).fetch_one(&pool).await?;
            assert_eq!(
                listeners, 1,
                "all waiting runs share one PostgreSQL listener"
            );
            for (run, source) in runs {
                let suspended = journal.load(run).await?.unwrap();
                assert_eq!(suspended.entries().len(), 1);
                let request = journal
                    .append_request(
                        source,
                        None,
                        RequestKind::Now(InlineFramePayload::default()),
                    )
                    .await?;
                journal
                    .append_delivery(
                        source,
                        DeliveryKind::Answer {
                            resolves: request.ordinal(),
                            payload: InlineFramePayload::new(b"source event".as_slice()),
                        },
                    )
                    .await?;
                assert_eq!(result(&journal, run).await, InlineFramePayload::default());
                assert_eq!(journal.load(run).await?.unwrap().entries().len(), 4);
            }
            stop.send(()).unwrap();
            task.await??;
            pool.close().await;
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_suspended_event_wait_restarts_from_retained_delivery()
        -> Result<(), Box<dyn Error>> {
            let (_database, pool, _) =
                signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
            let journal = ProgramJournalRepository::new(pool.clone());
            let source = ProgramRunId::from_uuid(Uuid::now_v7());
            journal.create_stream(source).await?;
            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            let registration = service.register_javascript(
                ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                javascript_request(format!(
                    "import {{ primitives }} from '@signalbox/program-sdk/v1'; await primitives.awaitEvent({{ source: {{ kind: 'program_answers', run: '{}' }}, after: '0' }});",
                    source.into_uuid()),
                    ProgramGrants::new([ProgramCapability::Subscribe])),
            ).await?;
            let run = ProgramRunId::from_uuid(Uuid::now_v7());
            service.start(run, registration.id, &[]).await?;
            let (waiting, mut waited) = mpsc::unbounded_channel();
            let (stop, stopped) = oneshot::channel();
            let task = tokio::spawn(runner.run_with_primitives(
                async {
                    let _ = stopped.await;
                },
                move |events| ObservedWait {
                    primitives: DaemonPrimitives {
                        durable: DurablePrimitives::new(events, SystemPrimitiveClock),
                    },
                    waiting: waiting.clone(),
                },
            ));
            tokio::time::timeout(TEST_TIMEOUT, waited.recv())
                .await?
                .unwrap();
            let suspended = journal.load(run).await?.unwrap();
            assert_eq!(suspended.entries().len(), 1);
            assert!(suspended.has_outstanding_requests());
            service.start(run, registration.id, &[]).await?;
            let healthy = service
                .register_javascript(
                    ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                    javascript_request(String::new(), ProgramGrants::new([])),
                )
                .await?;
            let healthy_run = ProgramRunId::from_uuid(Uuid::now_v7());
            service.start(healthy_run, healthy.id, &[]).await?;
            assert_eq!(
                result(&journal, healthy_run).await,
                InlineFramePayload::default()
            );
            stop.send(()).unwrap();
            task.await??;
            assert!(
                waited.try_recv().is_err(),
                "equal admission shares the wait owner"
            );
            assert_eq!(journal.load(run).await?.unwrap(), suspended);
            let request = journal
                .append_request(
                    source,
                    None,
                    RequestKind::Now(InlineFramePayload::default()),
                )
                .await?;
            journal
                .append_delivery(
                    source,
                    DeliveryKind::Answer {
                        resolves: request.ordinal(),
                        payload: InlineFramePayload::new(b"retained event".as_slice()),
                    },
                )
                .await?;
            let (_service, runner) = WorkflowRuntime::new(pool.clone())?;
            let (stop, stopped) = oneshot::channel();
            let task = tokio::spawn(runner.run(async {
                let _ = stopped.await;
            }));
            assert_eq!(result(&journal, run).await, InlineFramePayload::default());
            stop.send(()).unwrap();
            task.await??;
            let completed = journal.load(run).await?.unwrap();
            assert_eq!(
                completed.entries().len(),
                4,
                "one wait/answer and one terminal pair"
            );
            assert_eq!(completed.entries()[0], suspended.entries()[0]);
            pool.close().await;
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_cancellation_drops_a_blocked_delivery_before_wake()
        -> Result<(), Box<dyn Error>> {
            assert_cancellation_drops_blocked_delivery(CancellationCommit::Confirmed).await
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_ambiguous_committed_cancellation_interrupts_a_blocked_delivery()
        -> Result<(), Box<dyn Error>> {
            assert_cancellation_drops_blocked_delivery(CancellationCommit::ReplyLost).await
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_ambiguous_rolled_back_cancellation_retries_the_same_command()
        -> Result<(), Box<dyn Error>> {
            assert_cancellation_drops_blocked_delivery(CancellationCommit::RolledBack).await
        }

        enum CancellationCommit {
            Confirmed,
            ReplyLost,
            RolledBack,
        }

        async fn assert_cancellation_drops_blocked_delivery(
            commit: CancellationCommit,
        ) -> Result<(), Box<dyn Error>> {
            use signalbox_domain::DurableCommandId;
            use signalbox_persistence::program_cancellation::{self, CancelProgramRun};
            let (_database, pool, _) =
                signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
            let journal = ProgramJournalRepository::new(pool.clone());
            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            let registration = service.register_javascript(
                ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                javascript_request("import { primitives } from '@signalbox/program-sdk/v1'; await primitives.sleepUntil('18446744073709551615');".into(), ProgramGrants::new([ProgramCapability::Sleep])),
            ).await?;
            let (selected, mut selection) = mpsc::unbounded_channel();
            let (finished, mut completion) = mpsc::unbounded_channel();
            let (stop, stopped) = oneshot::channel();
            let task = tokio::spawn(runner.run_with_primitives(
                async {
                    let _ = stopped.await;
                },
                move |_| SelectedClock {
                    selected: selected.clone(),
                    finished: finished.clone(),
                },
            ));
            let run = ProgramRunId::from_uuid(Uuid::now_v7());
            service.start(run, registration.id, &[]).await?;
            let release = tokio::time::timeout(TEST_TIMEOUT, selection.recv())
                .await?
                .unwrap();
            let command = CancelProgramRun {
                command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                run_id: run,
            };
            let attempt = match commit {
                CancellationCommit::Confirmed => {
                    program_cancellation::cancel(&pool, command.clone()).await
                }
                CancellationCommit::ReplyLost | CancellationCommit::RolledBack => {
                    if matches!(commit, CancellationCommit::ReplyLost) {
                        program_cancellation::cancel(&pool, command.clone()).await?;
                    }
                    Err(
                        program_cancellation::ProgramCancellationError::CommitAmbiguous(
                            sqlx::Error::Io(std::io::ErrorKind::ConnectionReset.into()),
                        ),
                    )
                }
            };
            assert_eq!(
                service
                    .complete_cancellation(&pool, command.clone(), attempt)
                    .await?,
                program_cancellation::ProgramCancellationResult::Recorded(
                    program_cancellation::ProgramCancellationOutcome::Applied,
                ),
            );
            assert_eq!(
                program_cancellation::cancel(&pool, command).await?,
                program_cancellation::ProgramCancellationResult::Recorded(
                    program_cancellation::ProgramCancellationOutcome::Applied,
                ),
                "reconciliation preserves the command's receipt",
            );
            tokio::time::timeout(TEST_TIMEOUT, completion.recv())
                .await?
                .unwrap();
            assert!(
                release.send(()).is_err(),
                "cancel drops the blocked operation"
            );
            let retained = journal.load(run).await?.unwrap();
            assert_eq!(retained.entries().len(), 2);
            assert!(matches!(
                retained.terminal_delivery().unwrap().kind(),
                DeliveryKind::RunCancel(_)
            ));
            stop.send(()).unwrap();
            task.await??;
            assert_eq!(journal.load(run).await?.unwrap(), retained);
            pool.close().await;
            Ok(())
        }

        struct PendingSessionEffect {
            started: mpsc::UnboundedSender<()>,
            dropped: mpsc::UnboundedSender<()>,
        }
        impl EffectExecutor for PendingSessionEffect {
            fn recovery(&self, _: &EffectRequest) -> EffectRecovery {
                EffectRecovery::Ambiguous
            }
            fn adopt<'a>(
                &'a mut self,
                _: EffectInvocation<'a>,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<Option<InlineFramePayload>, LiveDeliveryFailure>>
                        + 'a,
                >,
            > {
                Box::pin(async { Ok(None) })
            }
            fn execute<'a>(
                &'a mut self,
                invocation: EffectInvocation<'a>,
            ) -> Pin<Box<dyn Future<Output = Result<InlineFramePayload, LiveDeliveryFailure>> + 'a>>
            {
                assert_eq!(invocation.request.capability(), ProgramCapability::Session);
                Box::pin(async move {
                    struct DropOperation(mpsc::UnboundedSender<()>);
                    impl Drop for DropOperation {
                        fn drop(&mut self) {
                            let _ = self.0.send(());
                        }
                    }
                    let _operation = DropOperation(self.dropped.clone());
                    self.started.send(()).unwrap();
                    std::future::pending().await
                })
            }
        }

        impl AttemptEffects for PendingSessionEffect {}

        #[tokio::test(flavor = "current_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_shutdown_drops_a_pending_session_effect_without_answering()
        -> Result<(), Box<dyn Error>> {
            let (_database, pool, _) =
                signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
            let journal = ProgramJournalRepository::new(pool.clone());
            let (service, _runner) = WorkflowRuntime::new(pool.clone())?;
            let registration = service.register_javascript(
                ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                javascript_request("import { effect } from '@signalbox/program-sdk/v1'; await effect('session', 'turn', new Uint8Array());".into(), ProgramGrants::new([ProgramCapability::Session])),
            ).await?;
            let run = ProgramRunId::from_uuid(Uuid::now_v7());
            service.start(run, registration.id, &[]).await?;
            let (started, mut start) = mpsc::unbounded_channel();
            let (dropped, mut drop) = mpsc::unbounded_channel();
            let host = WorkflowHost::new(journal.clone());
            let mut effects = PendingSessionEffect { started, dropped };
            let mut primitives = ClockSource;
            let outcome = tokio::time::timeout(
                TEST_TIMEOUT,
                interruptible(
                    drive_run(&host, &journal, run, &mut primitives, &mut effects),
                    async {
                        start.recv().await.unwrap();
                    },
                ),
            )
            .await?;
            assert!(outcome.is_none());
            assert_eq!(
                drop.try_recv(),
                Ok(()),
                "shutdown drains the operation before returning"
            );
            let retained = journal.load(run).await?.unwrap();
            assert_eq!(retained.entries().len(), 1);
            assert!(retained.has_outstanding_requests());
            assert!(retained.terminal_delivery().is_none());
            pool.close().await;
            Ok(())
        }

        async fn result(
            journal: &ProgramJournalRepository,
            run: ProgramRunId,
        ) -> InlineFramePayload {
            tokio::time::timeout(TEST_TIMEOUT, async {
                loop {
                    let loaded = journal.load(run).await.unwrap().unwrap();
                    if let Some(result) = loaded.result() {
                        return result.clone();
                    }
                    assert!(
                        loaded.terminal_delivery().is_none(),
                        "unexpected terminal journal: {loaded:?}"
                    );
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            })
            .await
            .expect("workflow reaches a durable result")
        }

        /// Each artifact supplies the program failure; registration identities are arbitrary.
        async fn assert_program_failure_isolated(
            artifact: &str,
        ) -> Result<signalbox_domain::ProgramJournal, Box<dyn Error>> {
            let (_database, pool, _) =
                signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            let failed_registration = service
                .register_javascript(
                    ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                    ProgramRegistrationRequest {
                        name: Uuid::now_v7().to_string(),
                        revision: Uuid::now_v7().to_string(),
                        source: artifact.as_bytes().to_vec(),
                        artifact: artifact.into(),
                        grants: ProgramGrants::new([ProgramCapability::Blob]),
                    },
                )
                .await?;
            let failed_run = ProgramRunId::from_uuid(Uuid::now_v7());
            service
                .start(failed_run, failed_registration.id, &[])
                .await?;
            let failed = assert_run_failure_isolated(&pool, service, runner, failed_run).await?;
            pool.close().await;
            Ok(failed)
        }

        /// Runs an already admitted failure, then verifies continued service and terminal replay.
        async fn assert_run_failure_isolated(
            pool: &PgPool,
            service: WorkflowService,
            runner: WorkflowRuntime,
            failed_run: ProgramRunId,
        ) -> Result<signalbox_domain::ProgramJournal, Box<dyn Error>> {
            let journal = ProgramJournalRepository::new(pool.clone());
            let failed_registration = service.registrations.for_run(failed_run).await?.unwrap();
            let healthy_registration = service
                .register_javascript(
                    ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                    ProgramRegistrationRequest {
                        name: Uuid::now_v7().to_string(),
                        revision: Uuid::now_v7().to_string(),
                        source: Vec::new(),
                        artifact: String::new(),
                        grants: ProgramGrants::new([]),
                    },
                )
                .await?;
            let (stop, stopped) = oneshot::channel();
            let task = tokio::spawn(runner.run(async {
                let _ = stopped.await;
            }));
            let failed_journal = tokio::time::timeout(TEST_TIMEOUT, async {
                loop {
                    let loaded = journal.load(failed_run).await.unwrap().unwrap();
                    if let Some(terminal) = loaded.terminal_delivery() {
                        assert!(
                            matches!(
                                terminal.kind(),
                                DeliveryKind::Fault(ProgramFault::ProgramError(_))
                            ),
                            "unexpected terminal delivery: {terminal:?}"
                        );
                        return loaded;
                    }
                    assert!(
                        !task.is_finished(),
                        "a program error must not stop the daemon workflow runner"
                    );
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            })
            .await?;
            let healthy_run = ProgramRunId::from_uuid(Uuid::now_v7());
            service
                .start(healthy_run, healthy_registration.id, &[])
                .await?;
            assert_eq!(
                result(&journal, healthy_run).await,
                InlineFramePayload::default()
            );
            stop.send(()).unwrap();
            task.await??;
            drop(service);

            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            assert!(service.registrations.unfinished_runs().await?.is_empty());
            let (stop, stopped) = oneshot::channel();
            let task = tokio::spawn(runner.run(async {
                let _ = stopped.await;
            }));
            service
                .start(failed_run, failed_registration.id, &[])
                .await?;
            let healthy_retry = ProgramRunId::from_uuid(Uuid::now_v7());
            service
                .start(healthy_retry, healthy_registration.id, &[])
                .await?;
            assert_eq!(
                result(&journal, healthy_retry).await,
                InlineFramePayload::default()
            );
            stop.send(()).unwrap();
            task.await??;
            assert_eq!(journal.load(failed_run).await?.unwrap(), failed_journal);
            Ok(failed_journal)
        }

        /// Identity and revision are arbitrary; artifact and grants define the behavior.
        fn javascript_request(
            artifact: String,
            grants: ProgramGrants,
        ) -> ProgramRegistrationRequest {
            ProgramRegistrationRequest {
                name: Uuid::now_v7().to_string(),
                revision: Uuid::now_v7().to_string(),
                source: artifact.as_bytes().to_vec(),
                artifact,
                grants,
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_recovered_unavailable_effect_retains_a_fault()
        -> Result<(), Box<dyn Error>> {
            let (_database, pool, _) =
                signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
            let journal = ProgramJournalRepository::new(pool.clone());
            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            let registration = service.register_javascript(
                ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                javascript_request(
                    "import { effect } from '@signalbox/program-sdk/v1'; await effect('blob', 'unavailable', new Uint8Array());".into(),
                    ProgramGrants::new([ProgramCapability::Blob]),
                ),
            ).await?;
            let run = ProgramRunId::from_uuid(Uuid::now_v7());
            service.start(run, registration.id, &[]).await?;
            // Crash boundary: the request is durable, but execution recorded no delivery.
            let request = journal
                .append_request(
                    run,
                    None,
                    RequestKind::Effect(EffectRequest::new(
                        ProgramCapability::Blob,
                        "unavailable".into(),
                        InlineFramePayload::default(),
                    )),
                )
                .await?;
            let failed = assert_run_failure_isolated(&pool, service, runner, run).await?;
            assert_eq!(
                failed.entries().len(),
                2,
                "one retained request and its fault"
            );
            assert_eq!(failed.entries()[0].frame(), &JournalFrame::Request(request));
            pool.close().await;
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_child_registration_conflict_does_not_stop_the_daemon()
        -> Result<(), Box<dyn Error>> {
            let (_database, pool, _) =
                signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            let existing = service
                .register_javascript(
                    ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                    javascript_request(String::new(), ProgramGrants::new([])),
                )
                .await?;
            let input = serde_json::to_vec(&serde_json::json!({
                "id": existing.id.into_uuid().to_string(),
                "name": existing.content.name,
                "revision": existing.content.revision,
                "source": [],
                "artifact": "throw new Error('changed child');",
                "grants": [],
            }))?;
            let parent = service.register_javascript(
                ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                javascript_request(
                    format!("import {{ effect }} from '@signalbox/program-sdk/v1'; await effect('register', 'register', new Uint8Array({input:?}));"),
                    ProgramGrants::new([ProgramCapability::Register]),
                ),
            ).await?;
            let run = ProgramRunId::from_uuid(Uuid::now_v7());
            service.start(run, parent.id, &[]).await?;
            let failed = assert_run_failure_isolated(&pool, service, runner, run).await?;
            assert_eq!(
                failed.entries().len(),
                2,
                "registration request and its conflict fault"
            );
            assert!(
                matches!(failed.entries()[0].frame(), JournalFrame::Request(request)
                if matches!(request.kind(), RequestKind::Effect(effect) if effect.capability() == ProgramCapability::Register))
            );
            assert_eq!(
                ProgramRegistrationRepository::new(pool.clone())
                    .find(existing.id, &existing.content)
                    .await?,
                Some(existing)
            );
            pool.close().await;
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_uuid_conflict_survives_loss_before_fault_commit()
        -> Result<(), Box<dyn Error>> {
            assert_registration_conflict_recovery(RegistrationCollision::Identity).await
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_name_revision_conflict_survives_loss_before_fault_commit()
        -> Result<(), Box<dyn Error>> {
            assert_registration_conflict_recovery(RegistrationCollision::NameRevision).await
        }

        enum RegistrationCollision {
            Identity,
            NameRevision,
        }

        /// Selects the immutable key that conflicts; all registration identities are arbitrary.
        async fn assert_registration_conflict_recovery(
            collision: RegistrationCollision,
        ) -> Result<(), Box<dyn Error>> {
            let (_database, pool, _) =
                signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
            let journal = ProgramJournalRepository::new(pool.clone());
            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            let existing = service
                .register_javascript(
                    ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                    javascript_request(String::new(), ProgramGrants::new([])),
                )
                .await?;
            let (id, child) = match collision {
                RegistrationCollision::Identity => (
                    existing.id,
                    javascript_request(String::new(), ProgramGrants::new([])),
                ),
                RegistrationCollision::NameRevision => (
                    ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                    ProgramRegistrationRequest {
                        name: existing.content.name.clone(),
                        revision: existing.content.revision.clone(),
                        source: Vec::new(),
                        artifact: String::new(),
                        grants: ProgramGrants::new([]),
                    },
                ),
            };
            let input = serde_json::to_vec(&serde_json::json!({
                "id": id.into_uuid().to_string(), "name": child.name,
                "revision": child.revision, "source": child.source,
                "artifact": child.artifact, "grants": [],
            }))?;
            let parent = service.register_javascript(
                ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                javascript_request(
                    format!("import {{ effect }} from '@signalbox/program-sdk/v1'; await effect('register', 'register', new Uint8Array({input:?}));"),
                    ProgramGrants::new([ProgramCapability::Register]),
                ),
            ).await?;
            // The fixture function's database identity also identifies its fault-write barrier.
            sqlx::raw_sql(
                "CREATE FUNCTION test_pause_program_fault() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN PERFORM pg_advisory_xact_lock('test_pause_program_fault'::regproc::oid::bigint); RETURN NEW; END $$;
                 CREATE TRIGGER test_pause_program_fault BEFORE INSERT ON program_run_journal_entry
                FOR EACH ROW WHEN (NEW.frame_kind = 'fault') EXECUTE FUNCTION test_pause_program_fault();"
            ).execute(&pool).await?;
            let mut barrier = pool.begin().await?;
            sqlx::query(
                "SELECT pg_advisory_xact_lock('test_pause_program_fault'::regproc::oid::bigint)",
            )
            .execute(&mut *barrier)
            .await?;
            let run = ProgramRunId::from_uuid(Uuid::now_v7());
            service.start(run, parent.id, &[]).await?;
            let task = tokio::spawn(runner.run(std::future::pending()));
            let blocked = tokio::time::timeout(TEST_TIMEOUT, async {
                loop {
                    let pid: Option<i32> = sqlx::query_scalar(
                        "SELECT pid FROM pg_locks WHERE locktype = 'advisory'
                         AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
                         AND classid = 0 AND objid = 'test_pause_program_fault'::regproc::oid AND NOT granted",
                    ).fetch_optional(&pool).await.unwrap();
                    if let Some(pid) = pid { break pid; }
                    assert!(!task.is_finished(), "live conflict reaches the fault-write barrier");
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }).await?;
            // The live conflict has been detected. Lose its connection before the fault can commit.
            assert!(
                sqlx::query_scalar::<_, bool>("SELECT pg_terminate_backend($1)")
                    .bind(blocked)
                    .fetch_one(&pool)
                    .await?
            );
            assert!(tokio::time::timeout(TEST_TIMEOUT, task).await??.is_err());
            barrier.rollback().await?;
            drop(service);
            let partial = journal.load(run).await?.unwrap();
            assert_eq!(
                partial.entries().len(),
                1,
                "only the register request survived"
            );
            assert!(partial.terminal_delivery().is_none());
            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            let failed = assert_run_failure_isolated(&pool, service, runner, run).await?;
            assert_eq!(
                failed.entries().len(),
                2,
                "the recovered conflict appends one fault"
            );
            assert_eq!(failed.entries()[0], partial.entries()[0]);
            let expected = ProgramRegistrationError::RegistrationConflict { registration: id };
            assert_eq!(
                failed.terminal_delivery().unwrap().kind(),
                &DeliveryKind::Fault(ProgramFault::ProgramError(InlineFramePayload::new(
                    expected.to_string().into_bytes()
                )))
            );
            assert_eq!(
                ProgramRegistrationRepository::new(pool.clone())
                    .find(existing.id, &existing.content)
                    .await?,
                Some(existing)
            );
            pool.close().await;
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_registration_database_failure_remains_a_runtime_error()
        -> Result<(), Box<dyn Error>> {
            let (_database, pool, _) =
                signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            let input = serde_json::to_vec(&serde_json::json!({
                "id": Uuid::now_v7().to_string(),
                "name": Uuid::now_v7().to_string(),
                "revision": Uuid::now_v7().to_string(),
                "source": [], "artifact": "", "grants": [],
            }))?;
            let registration = service.register_javascript(
                ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                javascript_request(
                    format!("import {{ effect }} from '@signalbox/program-sdk/v1'; await effect('register', 'register', new Uint8Array({input:?}));"),
                    ProgramGrants::new([ProgramCapability::Register]),
                ),
            ).await?;
            let run = ProgramRunId::from_uuid(Uuid::now_v7());
            service.start(run, registration.id, &[]).await?;
            // Force a database error after admission, only for the child's insert.
            sqlx::query("ALTER TABLE program_registration ADD CONSTRAINT test_refuse_new_registration CHECK (false) NOT VALID")
                .execute(&pool).await?;
            let error = tokio::time::timeout(TEST_TIMEOUT, runner.run(std::future::pending()))
                .await?
                .expect_err("database failure must propagate");
            assert!(matches!(error, WorkflowRuntimeError::Attempt { source, .. }
                if matches!(source.as_ref(), WorkflowHostError::LiveDelivery(_))));
            let journal = ProgramJournalRepository::new(pool.clone())
                .load(run)
                .await?
                .unwrap();
            assert_eq!(
                journal.entries().len(),
                1,
                "the failed database operation has no answer"
            );
            assert!(journal.terminal_delivery().is_none());
            pool.close().await;
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_program_cancellation_adopts_a_selected_delivery_tail_race()
        -> Result<(), Box<dyn Error>> {
            use signalbox_domain::DurableCommandId;
            use signalbox_persistence::program_cancellation::{
                self, CancelProgramRun, ProgramCancellationOutcome, ProgramCancellationResult,
            };
            let (_database, pool, _) =
                signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
            let journal = ProgramJournalRepository::new(pool.clone());
            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            let registration = service.register_javascript(
                ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                javascript_request(
                    "import { now } from '@signalbox/program-sdk/v1'; await now(new Uint8Array());".into(),
                    ProgramGrants::new([ProgramCapability::Time]),
                ),
            ).await?;
            let healthy = service
                .register_javascript(
                    ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                    javascript_request(String::new(), ProgramGrants::new([])),
                )
                .await?;
            let (selected, mut selection) = mpsc::unbounded_channel();
            let (finished, mut completion) = mpsc::unbounded_channel();
            let (stop, stopped) = oneshot::channel();
            let task = tokio::spawn(runner.run_with_primitives(
                async {
                    let _ = stopped.await;
                },
                move |_| SelectedClock {
                    selected: selected.clone(),
                    finished: finished.clone(),
                },
            ));
            let run = ProgramRunId::from_uuid(Uuid::now_v7());
            service.start(run, registration.id, &[]).await?;
            let release = tokio::time::timeout(TEST_TIMEOUT, selection.recv())
                .await?
                .unwrap();
            let selected_tail = journal.load(run).await?.unwrap();
            assert_eq!(
                selected_tail.entries().len(),
                1,
                "the selected answer is not appended"
            );
            assert!(selected_tail.has_outstanding_requests());
            let command = CancelProgramRun {
                command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                run_id: run,
            };
            assert_eq!(
                program_cancellation::cancel(&pool, command.clone()).await?,
                ProgramCancellationResult::Recorded(ProgramCancellationOutcome::Applied)
            );
            let cancelled = journal.load(run).await?.unwrap();
            assert_eq!(
                cancelled.entries().len(),
                2,
                "request and concurrent cancellation"
            );
            assert!(matches!(
                cancelled.terminal_delivery().unwrap().kind(),
                DeliveryKind::RunCancel(_)
            ));
            release.send(()).unwrap();
            tokio::time::timeout(TEST_TIMEOUT, completion.recv())
                .await?
                .unwrap();
            // The raced attempt has returned before admitting another run.
            let healthy_run = ProgramRunId::from_uuid(Uuid::now_v7());
            service.start(healthy_run, healthy.id, &[]).await?;
            assert_eq!(
                result(&journal, healthy_run).await,
                InlineFramePayload::default()
            );
            assert_eq!(
                program_cancellation::cancel(&pool, command).await?,
                ProgramCancellationResult::Recorded(ProgramCancellationOutcome::Applied)
            );
            stop.send(()).unwrap();
            task.await??;
            assert_eq!(
                journal.load(run).await?.unwrap(),
                cancelled,
                "the selected answer and a replacement fault must not overwrite cancellation"
            );
            pool.close().await;
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_program_nonterminal_tail_changes_remain_runtime_errors()
        -> Result<(), Box<dyn Error>> {
            let (_database, pool, _) =
                signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
            let journal = ProgramJournalRepository::new(pool.clone());
            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            let registration = service.register_javascript(
                ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                javascript_request(
                    "import { now } from '@signalbox/program-sdk/v1'; await now(new Uint8Array());".into(),
                    ProgramGrants::new([ProgramCapability::Time]),
                ),
            ).await?;
            let (selected, mut selection) = mpsc::unbounded_channel();
            let (finished, _completion) = mpsc::unbounded_channel();
            let task = tokio::spawn(runner.run_with_primitives(
                std::future::pending(),
                move |_| SelectedClock {
                    selected: selected.clone(),
                    finished: finished.clone(),
                },
            ));
            let run = ProgramRunId::from_uuid(Uuid::now_v7());
            service.start(run, registration.id, &[]).await?;
            let release = tokio::time::timeout(TEST_TIMEOUT, selection.recv())
                .await?
                .unwrap();
            // A second writer's request changes the tail without committing a terminal outcome.
            journal
                .append_request(run, None, RequestKind::Now(InlineFramePayload::default()))
                .await?;
            let changed = journal.load(run).await?.unwrap();
            assert!(changed.terminal_delivery().is_none());
            release.send(()).unwrap();
            let error = tokio::time::timeout(TEST_TIMEOUT, task)
                .await??
                .expect_err("nonterminal journal conflict remains fatal");
            assert!(
                matches!(error, WorkflowRuntimeError::Attempt { source, .. } if matches!(source.as_ref(), WorkflowHostError::Protocol(WorkflowHostProtocolError::JournalTailChanged)))
            );
            assert_eq!(journal.load(run).await?.unwrap(), changed);
            pool.close().await;
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_shutdown_interrupts_non_yielding_javascript()
        -> Result<(), Box<dyn Error>> {
            let (_database, pool, _) =
                signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
            let journal = ProgramJournalRepository::new(pool.clone());
            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            let cleanup = runner.host.clone();
            let registration = service.register_javascript(
                ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                javascript_request(
                    "import { now } from '@signalbox/program-sdk/v1'; await now(new Uint8Array()); while (true) {}".into(),
                    ProgramGrants::new([ProgramCapability::Time]),
                ),
            ).await?;
            let run = ProgramRunId::from_uuid(Uuid::now_v7());
            service.start(run, registration.id, &[]).await?;
            let (stop, stopped) = oneshot::channel();
            let mut task = tokio::spawn(runner.run(async {
                let _ = stopped.await;
            }));
            tokio::time::timeout(TEST_TIMEOUT, async {
                loop {
                    if journal.load(run).await.unwrap().unwrap().entries().len() == 2 {
                        break;
                    }
                    assert!(!task.is_finished());
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            })
            .await?;
            // Let the answered await resume into the synchronous loop before requesting stop.
            tokio::time::sleep(POLL_INTERVAL).await;
            stop.send(()).unwrap();
            let stopped = tokio::time::timeout(TEST_TIMEOUT, &mut task).await;
            // Release the blocking worker even if shutdown wiring regresses.
            cleanup.interrupt();
            stopped???;
            let retained = journal.load(run).await?.unwrap();
            assert_eq!(retained.entries().len(), 2);
            assert!(
                retained.terminal_delivery().is_none(),
                "shutdown leaves the run recoverable"
            );
            pool.close().await;
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_throwing_artifact_does_not_stop_the_daemon() -> Result<(), Box<dyn Error>>
        {
            assert_program_failure_isolated("throw new Error('program failed');")
                .await
                .map(|_| ())
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_invalid_syntax_is_a_retained_program_failure()
        -> Result<(), Box<dyn Error>> {
            assert_program_failure_isolated("const = ;")
                .await
                .map(|_| ())
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_unavailable_granted_effect_is_a_retained_program_failure()
        -> Result<(), Box<dyn Error>> {
            let failed = assert_program_failure_isolated(
                "import { effect } from '@signalbox/program-sdk/v1'; await effect('blob', 'unavailable', new Uint8Array());"
            ).await?;
            assert_eq!(
                failed.entries().len(),
                2,
                "the effect request precedes its fault"
            );
            assert!(
                matches!(failed.entries()[0].frame(), JournalFrame::Request(request)
                if matches!(request.kind(), RequestKind::Effect(effect) if effect.capability() == ProgramCapability::Blob))
            );
            Ok(())
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_stalled_artifact_is_a_retained_program_failure()
        -> Result<(), Box<dyn Error>> {
            assert_program_failure_isolated("await new Promise(() => {});")
                .await
                .map(|_| ())
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires ephemeral PostgreSQL"]
        async fn workflows_restart_after_recorded_request_reaches_one_result()
        -> Result<(), Box<dyn Error>> {
            let (_database, pool, _) =
                signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
            let journal = ProgramJournalRepository::new(pool.clone());
            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            let ProgramExecutable::Native {
                entry,
                revision,
                binary_digest,
            } = service.clock_executable().unwrap().clone()
            else {
                panic!("compiled clock identity")
            };
            let registration = service
                .register_native(
                    ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                    NativeProgramRegistrationRequest {
                        name: CLOCK_ENTRY.into(),
                        revision: CLOCK_REVISION.into(),
                        entry,
                        native_revision: revision,
                        binary_digest,
                        grants: ProgramGrants::new([signalbox_domain::ProgramCapability::Time]),
                    },
                )
                .await?;
            let (reached, mut recorded) = mpsc::unbounded_channel();
            let (stop, stopped) = oneshot::channel();
            let task = tokio::spawn(runner.run_with_primitives(
                async {
                    let _ = stopped.await;
                },
                move |_| PausedClock {
                    reached: reached.clone(),
                },
            ));
            let run = ProgramRunId::from_uuid(Uuid::now_v7());
            let input = ClockInput(ARBITRARY_INPUT).encode()?;
            service.start(run, registration.id, &input).await?;
            let request = tokio::time::timeout(TEST_TIMEOUT, recorded.recv())
                .await?
                .unwrap();
            let partial = journal.load(run).await?.unwrap();
            assert_eq!(partial.entries().len(), 1);
            assert_eq!(
                partial.entries()[0].frame(),
                &JournalFrame::Request(request.clone())
            );
            assert_eq!(
                request.kind(),
                &RequestKind::Now(InlineFramePayload::new(input.clone()))
            );

            // Equal admissions while the first attempt waits cannot start another attempt.
            service.start(run, registration.id, &input).await?;
            service.start(run, registration.id, &input).await?;
            // Another admitted artifact completes while the native attempt is paused.
            let javascript = service
                .register_javascript(
                    ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                    ProgramRegistrationRequest {
                        name: "empty-javascript".into(),
                        revision: "1".into(),
                        source: Vec::new(),
                        artifact: String::new(),
                        grants: ProgramGrants::new([]),
                    },
                )
                .await?;
            let js_run = ProgramRunId::from_uuid(Uuid::now_v7());
            service.start(js_run, javascript.id, &[]).await?;
            assert_eq!(
                result(&journal, js_run).await,
                InlineFramePayload::default()
            );
            assert!(
                recorded.try_recv().is_err(),
                "equal starts must share the paused attempt"
            );
            stop.send(()).unwrap();
            task.await??;
            assert_eq!(journal.load(run).await?.unwrap(), partial);
            drop(service);

            let before = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            let (service, runner) = WorkflowRuntime::new(pool.clone())?;
            let (stop, stopped) = oneshot::channel();
            let task = tokio::spawn(runner.run(async {
                let _ = stopped.await;
            }));
            // Startup alone finds the durable admission; no second start is required.
            let retained = result(&journal, run).await;
            let after = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            let decoded = ClockResult::decode(retained.as_bytes())?;
            assert_eq!(decoded.input, ClockInput(ARBITRARY_INPUT));
            assert!((before..=after).contains(&decoded.time.0));
            let completed = journal.load(run).await?.unwrap();
            assert_eq!(
                completed.entries().len(),
                4,
                "one Now/Answer and one Terminal/Answer pair"
            );
            assert_eq!(
                completed.entries()[0].frame(),
                &JournalFrame::Request(request)
            );
            assert_eq!(completed.result(), Some(&retained));
            service.start(run, registration.id, &input).await?;
            stop.send(()).unwrap();
            task.await??;
            assert_eq!(journal.load(run).await?.unwrap(), completed);
            assert!(service.registrations.unfinished_runs().await?.is_empty());
            pool.close().await;
            Ok(())
        }
    }
}
