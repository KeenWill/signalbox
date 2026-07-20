//! Runtime scheduling over nonauthoritative session hints.
//!
//! ADR-0010 owns the durable-rows queue, same-process nudge, and periodic
//! reconciliation mechanics. This module keeps both hint sources behind one
//! application port and drives the existing authoritative eligibility pass.

use std::{collections::VecDeque, error::Error, fmt, future::Future, time::Duration};

use signalbox_domain::SessionId;
use tokio::{
    pin, select,
    sync::mpsc,
    time::{self, Instant, Interval},
};

use crate::{
    ClassifyOperatorFailure, StartEligibleTurnIdGenerator, StartEligibleTurnService,
    StartEligibleTurnTransaction,
};

/// The baseline reconciliation interval selected by the scheduler slice.
///
/// The composition root may supply another nonzero interval after validating
/// deployment configuration through [`ReconciliationSweepInterval::try_new`].
const BASELINE_RECONCILIATION_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// A validated nonzero reconciliation-sweep interval.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReconciliationSweepInterval(Duration);

impl ReconciliationSweepInterval {
    /// Returns the one-second baseline selected by the scheduler slice.
    pub const fn baseline() -> Self {
        Self(BASELINE_RECONCILIATION_SWEEP_INTERVAL)
    }

    /// Validates an operator-supplied interval.
    pub const fn try_new(interval: Duration) -> Result<Self, InvalidReconciliationSweepInterval> {
        if interval.is_zero() {
            Err(InvalidReconciliationSweepInterval)
        } else {
            Ok(Self(interval))
        }
    }

    /// Returns the validated duration.
    pub const fn get(self) -> Duration {
        self.0
    }
}

/// A zero duration cannot drive the periodic safety-net sweep.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidReconciliationSweepInterval;

impl fmt::Display for InvalidReconciliationSweepInterval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("scheduler reconciliation interval must be nonzero")
    }
}

impl Error for InvalidReconciliationSweepInterval {}

/// The observable result of handing a nonauthoritative hint to a work source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EligibilityNudgeOutcome {
    /// The in-process work source accepted the hint.
    Enqueued,
    /// The scheduler work source has already been dropped.
    WorkSourceClosed,
}

/// Typed post-commit hook for eligibility-affecting command paths.
///
/// Implementations must remain best effort: a failed handoff cannot change the
/// already-committed command result, and the reconciliation sweep restores
/// liveness after any lost hint.
pub trait EligibilityNudge {
    /// Hands the session hint to the scheduler without assigning it authority.
    fn nudge(&self, session: SessionId) -> EligibilityNudgeOutcome;
}

/// Finds sessions whose durable storage shape may admit an eligibility pass.
pub trait EligibilitySweep {
    /// Adapter-specific infrastructure failure.
    type Error;

    /// Returns session hints for queued work with no active slot owner.
    fn find_sessions(&mut self)
    -> impl Future<Output = Result<Vec<SessionId>, Self::Error>> + Send;
}

/// Supplies nonauthoritative session hints to the scheduler loop.
pub trait EligibilityWorkSource {
    /// Failure from the source's reconciliation path.
    type Error;

    /// Waits for the next same-process or reconciliation-derived hint.
    fn next(&mut self) -> impl Future<Output = Result<SessionId, Self::Error>> + Send;
}

/// Runs one authoritative per-session eligibility pass.
pub trait EligibilityPass {
    /// Adapter-specific failure from the authoritative pass.
    type Error;

    /// Revalidates durable state and applies at most one guarded transition.
    fn run(&mut self, session: SessionId) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

impl<Generator, Transaction> EligibilityPass for StartEligibleTurnService<Generator, Transaction>
where
    Generator: StartEligibleTurnIdGenerator + Send,
    Transaction: StartEligibleTurnTransaction + Send,
{
    type Error = Transaction::Error;

    async fn run(&mut self, session: SessionId) -> Result<(), Self::Error> {
        self.execute(session).await.map(drop)
    }
}

/// Cloneable same-process post-commit nudge hook.
#[derive(Clone, Debug)]
pub struct InProcessEligibilityNudge {
    sender: mpsc::UnboundedSender<SessionId>,
}

impl EligibilityNudge for InProcessEligibilityNudge {
    fn nudge(&self, session: SessionId) -> EligibilityNudgeOutcome {
        match self.sender.send(session) {
            Ok(()) => EligibilityNudgeOutcome::Enqueued,
            Err(_) => EligibilityNudgeOutcome::WorkSourceClosed,
        }
    }
}

/// Same-process nudges plus a periodic durable reconciliation sweep.
#[derive(Debug)]
pub struct InProcessEligibilityWorkSource<Sweep> {
    nudges: mpsc::UnboundedReceiver<SessionId>,
    sweep: Sweep,
    sweep_interval: Interval,
    initial_sweep_due: bool,
    pending_sweep_hints: VecDeque<SessionId>,
}

impl<Sweep> InProcessEligibilityWorkSource<Sweep> {
    /// Builds a work source with the one-second baseline sweep interval.
    pub fn new(sweep: Sweep) -> (InProcessEligibilityNudge, Self) {
        Self::with_interval(sweep, ReconciliationSweepInterval::baseline())
    }

    /// Builds a work source with an explicitly validated sweep interval.
    pub fn with_interval(
        sweep: Sweep,
        sweep_interval: ReconciliationSweepInterval,
    ) -> (InProcessEligibilityNudge, Self) {
        let (sender, nudges) = mpsc::unbounded_channel();
        let nudge = InProcessEligibilityNudge { sender };
        let source = Self {
            nudges,
            sweep,
            sweep_interval: time::interval_at(
                Instant::now() + sweep_interval.get(),
                sweep_interval.get(),
            ),
            initial_sweep_due: true,
            pending_sweep_hints: VecDeque::new(),
        };
        (nudge, source)
    }
}

impl<Sweep> EligibilityWorkSource for InProcessEligibilityWorkSource<Sweep>
where
    Sweep: EligibilitySweep + Send,
{
    type Error = Sweep::Error;

    async fn next(&mut self) -> Result<SessionId, Self::Error> {
        loop {
            if let Some(session) = self.pending_sweep_hints.pop_front() {
                return Ok(session);
            }
            if self.initial_sweep_due {
                self.initial_sweep_due = false;
                self.pending_sweep_hints
                    .extend(self.sweep.find_sessions().await?);
                continue;
            }

            select! {
                Some(session) = self.nudges.recv() => return Ok(session),
                _ = self.sweep_interval.tick() => {
                    self.pending_sweep_hints
                        .extend(self.sweep.find_sessions().await?);
                }
            }
        }
    }
}

/// Why the scheduler loop stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerLoopExit {
    /// The composition root requested shutdown.
    Shutdown,
}

/// Drives authoritative per-session passes from nonauthoritative work hints.
#[derive(Debug)]
pub struct SchedulerLoop<WorkSource, Pass> {
    work_source: WorkSource,
    pass: Pass,
}

impl<WorkSource, Pass> SchedulerLoop<WorkSource, Pass> {
    /// Composes the work-source and authoritative-pass ports.
    pub const fn new(work_source: WorkSource, pass: Pass) -> Self {
        Self { work_source, pass }
    }

    /// Returns both ports, primarily for explicit ownership handoff.
    pub fn into_parts(self) -> (WorkSource, Pass) {
        (self.work_source, self.pass)
    }
}

impl<WorkSource, Pass> SchedulerLoop<WorkSource, Pass>
where
    WorkSource: EligibilityWorkSource,
    Pass: EligibilityPass,
    WorkSource::Error: ClassifyOperatorFailure,
    Pass::Error: ClassifyOperatorFailure,
{
    /// Runs until shutdown, retrying source and pass failures on later hints.
    ///
    /// The loop admits no new pass once it observes shutdown. A pass already
    /// in progress is allowed to return so its authoritative transaction can
    /// commit or abort; the composition root owns the outer bounded window.
    pub async fn run_until<Shutdown>(&mut self, shutdown: Shutdown) -> SchedulerLoopExit
    where
        Shutdown: Future<Output = ()> + Send,
    {
        pin!(shutdown);

        loop {
            let session = select! {
                biased;

                () = &mut shutdown => return SchedulerLoopExit::Shutdown,
                hint = self.work_source.next() => match hint {
                    Ok(session) => session,
                    Err(error) => {
                        let failure_class = error.operator_failure_class();
                        tracing::error!(
                            ?failure_class,
                            "eligibility reconciliation sweep failed; \
                             the next interval will retry"
                        );
                        continue;
                    }
                },
            };

            if let Err(error) = self.pass.run(session).await {
                let failure_class = error.operator_failure_class();
                tracing::error!(
                    ?failure_class,
                    session_id = %session.as_uuid(),
                    "authoritative eligibility pass failed; \
                     a later nudge or sweep will retry"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::{Future, ready},
        time::Duration,
    };

    use signalbox_domain::SessionId;
    use tokio::sync::oneshot;
    use uuid::Uuid;

    use super::{
        ClassifyOperatorFailure, EligibilityNudge, EligibilityNudgeOutcome, EligibilityPass,
        EligibilitySweep, EligibilityWorkSource, InProcessEligibilityWorkSource,
        InvalidReconciliationSweepInterval, ReconciliationSweepInterval, SchedulerLoop,
        SchedulerLoopExit,
    };
    use crate::OperatorFailureClass;

    fn session(value: u128) -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(value))
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeSweepError {
        Unavailable,
    }

    impl ClassifyOperatorFailure for FakeSweepError {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            }
        }
    }

    #[derive(Debug)]
    struct FakeSweep {
        responses: VecDeque<Result<Vec<SessionId>, FakeSweepError>>,
    }

    impl FakeSweep {
        fn returning(
            responses: impl IntoIterator<Item = Result<Vec<SessionId>, FakeSweepError>>,
        ) -> Self {
            Self {
                responses: responses.into_iter().collect(),
            }
        }
    }

    impl EligibilitySweep for FakeSweep {
        type Error = FakeSweepError;

        fn find_sessions(
            &mut self,
        ) -> impl Future<Output = Result<Vec<SessionId>, Self::Error>> + Send {
            ready(
                self.responses
                    .pop_front()
                    .expect("test must supply one response per sweep"),
            )
        }
    }

    #[test]
    fn zero_reconciliation_interval_is_rejected() {
        assert_eq!(
            ReconciliationSweepInterval::try_new(Duration::ZERO),
            Err(InvalidReconciliationSweepInterval)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn inv007_same_process_nudge_is_the_primary_hint() {
        let nudged = session(1);
        let swept = session(2);
        let (nudge, mut source) = InProcessEligibilityWorkSource::new(FakeSweep::returning([
            Ok(vec![]),
            Ok(vec![swept]),
        ]));

        assert_eq!(nudge.nudge(nudged), EligibilityNudgeOutcome::Enqueued);
        assert_eq!(source.next().await, Ok(nudged));
        assert_eq!(source.next().await, Ok(swept));
    }

    #[tokio::test(start_paused = true)]
    async fn s03_inv007_lost_nudge_is_recovered_by_periodic_sweep() {
        let recovered = session(3);
        let interval = ReconciliationSweepInterval::try_new(Duration::from_secs(5))
            .expect("test interval is nonzero");
        let (_nudge, mut source) = InProcessEligibilityWorkSource::with_interval(
            FakeSweep::returning([Ok(vec![]), Ok(vec![recovered])]),
            interval,
        );
        let next = source.next();
        tokio::pin!(next);

        tokio::task::yield_now().await;
        tokio::time::advance(interval.get()).await;

        assert_eq!(next.await, Ok(recovered));
    }

    #[tokio::test(start_paused = true)]
    async fn sweep_failure_is_visible_to_the_loop_and_retried_next_interval() {
        let recovered = session(4);
        let interval = ReconciliationSweepInterval::try_new(Duration::from_secs(5))
            .expect("test interval is nonzero");
        let (_nudge, mut source) = InProcessEligibilityWorkSource::with_interval(
            FakeSweep::returning([Err(FakeSweepError::Unavailable), Ok(vec![recovered])]),
            interval,
        );

        assert_eq!(source.next().await, Err(FakeSweepError::Unavailable));
        let next = source.next();
        tokio::pin!(next);
        tokio::time::advance(interval.get()).await;
        assert_eq!(next.await, Ok(recovered));
    }

    #[derive(Debug)]
    struct FakeWorkSource {
        hints: VecDeque<Result<SessionId, FakeSweepError>>,
    }

    impl EligibilityWorkSource for FakeWorkSource {
        type Error = FakeSweepError;

        async fn next(&mut self) -> Result<SessionId, Self::Error> {
            self.hints
                .pop_front()
                .expect("shutdown must arrive after the supplied hints")
        }
    }

    #[derive(Debug)]
    struct FakePass {
        responses: VecDeque<Result<(), FakeSweepError>>,
        observed: Vec<SessionId>,
        shutdown: Option<oneshot::Sender<()>>,
    }

    impl EligibilityPass for FakePass {
        type Error = FakeSweepError;

        async fn run(&mut self, session: SessionId) -> Result<(), Self::Error> {
            self.observed.push(session);
            let response = self
                .responses
                .pop_front()
                .expect("test must supply one response per pass");
            if self.responses.is_empty() {
                self.shutdown
                    .take()
                    .expect("test shutdown sender is present")
                    .send(())
                    .expect("scheduler still waits for shutdown");
            }
            response
        }
    }

    #[tokio::test]
    async fn inv007_scheduler_continues_after_a_failed_authoritative_pass() {
        let first = session(5);
        let second = session(6);
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let mut scheduler = SchedulerLoop::new(
            FakeWorkSource {
                hints: VecDeque::from([Ok(first), Ok(second)]),
            },
            FakePass {
                responses: VecDeque::from([Err(FakeSweepError::Unavailable), Ok(())]),
                observed: Vec::new(),
                shutdown: Some(shutdown_sender),
            },
        );

        let exit = scheduler
            .run_until(async {
                shutdown_receiver
                    .await
                    .expect("fake pass sends shutdown after both hints");
            })
            .await;
        let (_source, pass) = scheduler.into_parts();

        assert_eq!(exit, SchedulerLoopExit::Shutdown);
        assert_eq!(pass.observed, vec![first, second]);
    }

    #[tokio::test]
    async fn closed_work_source_does_not_change_the_committed_command_path() {
        let (nudge, source) = InProcessEligibilityWorkSource::new(FakeSweep::returning([]));
        drop(source);

        assert_eq!(
            nudge.nudge(session(7)),
            EligibilityNudgeOutcome::WorkSourceClosed
        );
    }
}
