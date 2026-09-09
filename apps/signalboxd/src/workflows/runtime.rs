//! Daemon-owned local workflow attempts and restart recovery; docs/spec/workflows.md.

use std::{
    collections::BTreeSet,
    future::Future,
    pin::Pin,
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::{StreamExt, stream::FuturesUnordered};
use signalbox_domain::{
    DeliveryKind, EffectRequest, InlineFramePayload, ProgramRunId, RejectReason, RequestFrame,
    RequestKind,
};
use signalbox_persistence::{
    program_journal::ProgramJournalRepository,
    program_registration::{ProgramRegistrationError, ProgramRegistrationRepository},
};
use signalbox_workflow_runtime::{
    LiveDeliveryFailure, LiveDeliverySource, WorkflowHost, WorkflowHostError,
    effects::{EffectExecutor, EffectInvocation, EffectRecovery},
    native::NativeProgramError,
};
use sqlx::PgPool;
use tokio::sync::{mpsc, oneshot};

use super::WorkflowService;
#[cfg(target_os = "linux")]
use super::{CLOCK_ENTRY, CLOCK_REVISION, compiled_catalog};

#[derive(Debug, signalbox_derive::OperatorError)]
pub enum WorkflowRuntimeError {
    #[error("workflow registration: {field_0}")]
    Registration(#[source] ProgramRegistrationError),
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

/// One runner per fenced daemon; only this runner starts its run attempts.
pub struct WorkflowRuntime {
    host: WorkflowHost,
    registrations: ProgramRegistrationRepository,
    wake: mpsc::UnboundedReceiver<ProgramRunId>,
}

impl WorkflowRuntime {
    pub fn new(pool: PgPool) -> Result<(WorkflowService, Self), WorkflowRuntimeError> {
        let host = WorkflowHost::new(ProgramJournalRepository::new(pool.clone()));
        #[cfg(target_os = "linux")]
        let (host, clock_executable) = {
            let catalog = compiled_catalog()?;
            let executable = catalog
                .executable(CLOCK_ENTRY, CLOCK_REVISION)
                .ok_or(WorkflowRuntimeError::NativeUnavailable)?;
            (host.with_native_catalog(catalog), Some(executable))
        };
        #[cfg(not(target_os = "linux"))]
        let clock_executable = None;
        let registrations = ProgramRegistrationRepository::new(pool);
        let (wake, receiver) = mpsc::unbounded_channel();
        Ok((
            WorkflowService {
                registrations: registrations.clone(),
                wake,
                clock_executable,
            },
            Self {
                host,
                registrations,
                wake: receiver,
            },
        ))
    }

    /// Owns non-Send isolate/root futures on one local executor and joins it on shutdown.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<(), WorkflowRuntimeError> {
        self.run_with_primitives(shutdown, || ClockSource).await
    }

    async fn run_with_primitives<P: LiveDeliverySource + 'static>(
        self,
        shutdown: impl Future<Output = ()>,
        primitives: impl Fn() -> P + Send + 'static,
    ) -> Result<(), WorkflowRuntimeError> {
        // Pool connections must retain the daemon's I/O driver across runner restarts.
        let executor = tokio::runtime::Handle::current();
        // Dropping this sender on outer-task cancellation also stops the local executor.
        let (stop, stopped) = oneshot::channel();
        let mut worker =
            tokio::task::spawn_blocking(move || executor.block_on(self.drive(stopped, primitives)));
        tokio::select! {
            result = &mut worker => result.map_err(WorkflowRuntimeError::Join)?,
            () = shutdown => {
                let _ = stop.send(());
                worker.await.map_err(WorkflowRuntimeError::Join)?
            }
        }
    }

    async fn drive<P: LiveDeliverySource + 'static>(
        mut self,
        stopped: oneshot::Receiver<()>,
        primitives: impl Fn() -> P,
    ) -> Result<(), WorkflowRuntimeError> {
        let execution = async {
            let mut active = BTreeSet::new();
            let mut attempts = FuturesUnordered::new();
            for run in self.registrations.unfinished_runs().await? {
                active.insert(run);
                attempts.push(attempt(self.host.clone(), run, primitives()));
            }
            loop {
                tokio::select! {
                    Some(run) = self.wake.recv() => {
                        if active.insert(run) {
                            attempts.push(attempt(self.host.clone(), run, primitives()));
                        }
                    }
                    Some(completed) = attempts.next(), if !attempts.is_empty() => {
                        active.remove(&completed?);
                    }
                    else => std::future::pending::<()>().await,
                }
            }
        };
        tokio::select! {
            _ = stopped => Ok(()),
            result = execution => result,
        }
    }
}

fn attempt<P: LiveDeliverySource + 'static>(
    host: WorkflowHost,
    run: ProgramRunId,
    mut primitives: P,
) -> Pin<Box<dyn Future<Output = Result<ProgramRunId, WorkflowRuntimeError>>>> {
    Box::pin(async move {
        host.execute_registered(run, &mut primitives, &mut UnavailableEffects)
            .await
            .map_err(|source| WorkflowRuntimeError::Attempt {
                run,
                source: Box::new(source),
            })?;
        Ok(run)
    })
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

struct UnavailableEffects;
impl EffectExecutor for UnavailableEffects {
    fn recovery(&self, _: &EffectRequest) -> EffectRecovery {
        EffectRecovery::Ambiguous
    }
    fn adopt<'a>(
        &'a mut self,
        _: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<InlineFramePayload>, LiveDeliveryFailure>> + 'a>>
    {
        Box::pin(async { Ok(None) })
    }
    fn execute<'a>(
        &'a mut self,
        _: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<InlineFramePayload, LiveDeliveryFailure>> + 'a>> {
        Box::pin(async {
            Err(LiveDeliveryFailure::new(
                "daemon workflow effect is unavailable",
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_domain::RequestOrdinal;

    #[tokio::test]
    async fn workflows_clock_answers_a_now_request_with_unix_seconds() {
        let frame = RequestFrame::new(
            RequestOrdinal::try_from_u64(1).unwrap(),
            None,
            RequestKind::Now(InlineFramePayload::default()),
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
                move || PausedClock {
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
