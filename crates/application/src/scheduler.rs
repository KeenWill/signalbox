//! Runtime scheduling over nonauthoritative session hints.
//!
//! ADR-0010 owns the durable-rows queue, same-process nudge, and periodic
//! reconciliation mechanics. This module keeps both hint sources behind one
//! application port and drives the existing authoritative eligibility pass.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    fmt,
    future::Future,
    num::NonZeroUsize,
    time::Duration,
};

use signalbox_domain::SessionId;
use tokio::{
    pin, select,
    sync::mpsc::{self, error::TrySendError},
    task::{Id, JoinError, JoinSet},
    time::{self, Instant, Interval, MissedTickBehavior},
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
const BASELINE_NUDGE_BUFFER_CAPACITY: usize = 1_024;
const BASELINE_MAX_IN_FLIGHT_PASSES: usize = 16;

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
    /// The bounded hint buffer was full; reconciliation remains the backstop.
    DroppedAtCapacity,
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
    sender: mpsc::Sender<SessionId>,
}

impl EligibilityNudge for InProcessEligibilityNudge {
    fn nudge(&self, session: SessionId) -> EligibilityNudgeOutcome {
        match self.sender.try_send(session) {
            Ok(()) => EligibilityNudgeOutcome::Enqueued,
            Err(TrySendError::Full(_)) => EligibilityNudgeOutcome::DroppedAtCapacity,
            Err(TrySendError::Closed(_)) => EligibilityNudgeOutcome::WorkSourceClosed,
        }
    }
}

/// Same-process nudges plus a periodic durable reconciliation sweep.
#[derive(Debug)]
pub struct InProcessEligibilityWorkSource<Sweep> {
    nudges: mpsc::Receiver<SessionId>,
    sweep: Sweep,
    sweep_interval: Interval,
    initial_sweep_due: bool,
    pending_sweep_hints: VecDeque<SessionId>,
}

impl<Sweep> InProcessEligibilityWorkSource<Sweep> {
    /// Builds a work source with the one-second baseline sweep interval.
    pub fn new(sweep: Sweep) -> (InProcessEligibilityNudge, Self) {
        Self::with_options(
            sweep,
            ReconciliationSweepInterval::baseline(),
            NonZeroUsize::new(BASELINE_NUDGE_BUFFER_CAPACITY)
                .expect("the baseline nudge capacity is nonzero"),
        )
    }

    /// Builds a work source with an explicitly validated sweep interval.
    pub fn with_interval(
        sweep: Sweep,
        sweep_interval: ReconciliationSweepInterval,
    ) -> (InProcessEligibilityNudge, Self) {
        Self::with_options(
            sweep,
            sweep_interval,
            NonZeroUsize::new(BASELINE_NUDGE_BUFFER_CAPACITY)
                .expect("the baseline nudge capacity is nonzero"),
        )
    }

    /// Builds a work source with explicit validated timing and buffer bounds.
    pub fn with_options(
        sweep: Sweep,
        sweep_interval: ReconciliationSweepInterval,
        nudge_buffer_capacity: NonZeroUsize,
    ) -> (InProcessEligibilityNudge, Self) {
        let (sender, nudges) = mpsc::channel(nudge_buffer_capacity.get());
        let nudge = InProcessEligibilityNudge { sender };
        let mut interval =
            time::interval_at(Instant::now() + sweep_interval.get(), sweep_interval.get());
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let source = Self {
            nudges,
            sweep,
            sweep_interval: interval,
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
    max_in_flight_passes: usize,
}

impl<WorkSource, Pass> SchedulerLoop<WorkSource, Pass> {
    /// Composes the work-source and authoritative-pass ports.
    pub const fn new(work_source: WorkSource, pass: Pass) -> Self {
        Self {
            work_source,
            pass,
            max_in_flight_passes: BASELINE_MAX_IN_FLIGHT_PASSES,
        }
    }

    /// Composes the ports with an explicit nonzero in-flight pass bound.
    pub const fn with_max_in_flight(
        work_source: WorkSource,
        pass: Pass,
        max_in_flight_passes: NonZeroUsize,
    ) -> Self {
        Self {
            work_source,
            pass,
            max_in_flight_passes: max_in_flight_passes.get(),
        }
    }

    /// Returns both ports, primarily for explicit ownership handoff.
    pub fn into_parts(self) -> (WorkSource, Pass) {
        (self.work_source, self.pass)
    }
}

impl<WorkSource, Pass> SchedulerLoop<WorkSource, Pass>
where
    WorkSource: EligibilityWorkSource,
    Pass: EligibilityPass + Clone + Send + 'static,
    WorkSource::Error: ClassifyOperatorFailure,
    Pass::Error: ClassifyOperatorFailure + Send + 'static,
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
        let mut passes = JoinSet::new();
        let mut task_sessions = HashMap::new();
        let mut in_flight_sessions = HashSet::new();

        'scheduler: loop {
            if task_sessions.len() == self.max_in_flight_passes {
                select! {
                    biased;

                    () = &mut shutdown => break,
                    completed = passes.join_next_with_id() => {
                        if let Some(completed) = completed {
                            observe_pass_completion(
                                completed,
                                &mut task_sessions,
                                &mut in_flight_sessions,
                            );
                        }
                    }
                }
                continue;
            }

            // A completion may win this select many times, but it must not
            // cancel an in-progress reconciliation read after that read has
            // consumed its interval tick. Keep the same next-hint future
            // pinned until it yields a hint, a visible failure, or shutdown.
            let next_hint = self.work_source.next();
            pin!(next_hint);
            let hint = loop {
                select! {
                    biased;

                    () = &mut shutdown => break 'scheduler,
                    completed = passes.join_next_with_id(),
                        if !task_sessions.is_empty() =>
                    {
                        if let Some(completed) = completed {
                            observe_pass_completion(
                                completed,
                                &mut task_sessions,
                                &mut in_flight_sessions,
                            );
                        }
                    }
                    hint = &mut next_hint => break hint,
                }
            };

            match hint {
                Ok(session) => {
                    if in_flight_sessions.insert(session) {
                        let mut pass = self.pass.clone();
                        let task = passes.spawn(async move { pass.run(session).await });
                        task_sessions.insert(task.id(), session);
                    }
                }
                Err(error) => {
                    let failure_class = error.operator_failure_class();
                    tracing::error!(
                        ?failure_class,
                        "eligibility reconciliation sweep failed; \
                         the next interval will retry"
                    );
                }
            }
        }

        while let Some(completed) = passes.join_next_with_id().await {
            observe_pass_completion(completed, &mut task_sessions, &mut in_flight_sessions);
        }
        SchedulerLoopExit::Shutdown
    }
}

fn observe_pass_completion<PassError>(
    completed: Result<(Id, Result<(), PassError>), JoinError>,
    task_sessions: &mut HashMap<Id, SessionId>,
    in_flight_sessions: &mut HashSet<SessionId>,
) where
    PassError: ClassifyOperatorFailure,
{
    let task = match &completed {
        Ok((task, _)) => *task,
        Err(error) => error.id(),
    };
    let Some(session) = task_sessions.remove(&task) else {
        tracing::error!(
            failure_class = ?crate::OperatorFailureClass::CallerOrHubBug,
            "eligibility-pass task completed without its session correlation"
        );
        return;
    };
    in_flight_sessions.remove(&session);

    match completed {
        Ok((_, Ok(()))) => {}
        Ok((_, Err(error))) => {
            let failure_class = error.operator_failure_class();
            tracing::error!(
                ?failure_class,
                session_id = %session.as_uuid(),
                "authoritative eligibility pass failed; \
                 a later nudge or sweep will retry"
            );
        }
        Err(_) => {
            tracing::error!(
                failure_class = ?crate::OperatorFailureClass::CallerOrHubBug,
                session_id = %session.as_uuid(),
                "authoritative eligibility pass task terminated unexpectedly; \
                 a later nudge or sweep will retry"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::{Future, pending, ready},
        num::NonZeroUsize,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use signalbox_domain::SessionId;
    use tokio::{
        sync::{Notify, oneshot},
        time::timeout,
    };
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
    async fn inv007_missed_reconciliation_ticks_do_not_burst() {
        let initial = session(30);
        let first_periodic = session(31);
        let second_periodic = session(32);
        let interval = ReconciliationSweepInterval::try_new(Duration::from_secs(5))
            .expect("test interval is nonzero");
        let (_nudge, mut source) = InProcessEligibilityWorkSource::with_interval(
            FakeSweep::returning([
                Ok(vec![initial]),
                Ok(vec![first_periodic]),
                Ok(vec![second_periodic]),
            ]),
            interval,
        );

        assert_eq!(source.next().await, Ok(initial));
        tokio::time::advance(Duration::from_secs(15)).await;
        assert_eq!(source.next().await, Ok(first_periodic));
        assert!(timeout(Duration::ZERO, source.next()).await.is_err());
        tokio::time::advance(interval.get()).await;
        assert_eq!(source.next().await, Ok(second_periodic));
    }

    #[tokio::test]
    async fn inv007_full_nudge_buffer_drops_only_the_hint() {
        let first = session(33);
        let second = session(34);
        let (nudge, _source) = InProcessEligibilityWorkSource::with_options(
            FakeSweep::returning([]),
            ReconciliationSweepInterval::baseline(),
            NonZeroUsize::new(1).expect("the test capacity is nonzero"),
        );

        assert_eq!(nudge.nudge(first), EligibilityNudgeOutcome::Enqueued);
        assert_eq!(
            nudge.nudge(second),
            EligibilityNudgeOutcome::DroppedAtCapacity
        );
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
            match self.hints.pop_front() {
                Some(hint) => hint,
                None => pending().await,
            }
        }
    }

    #[derive(Debug)]
    struct FakePassState {
        observed: Vec<SessionId>,
        failing_session: SessionId,
        remaining_calls: usize,
        shutdown: Option<oneshot::Sender<()>>,
    }

    #[derive(Clone, Debug)]
    struct FakePass {
        state: Arc<Mutex<FakePassState>>,
    }

    impl FakePass {
        fn failing_once(
            failing_session: SessionId,
            expected_calls: usize,
            shutdown: oneshot::Sender<()>,
        ) -> Self {
            Self {
                state: Arc::new(Mutex::new(FakePassState {
                    observed: Vec::new(),
                    failing_session,
                    remaining_calls: expected_calls,
                    shutdown: Some(shutdown),
                })),
            }
        }
    }

    impl EligibilityPass for FakePass {
        type Error = FakeSweepError;

        async fn run(&mut self, session: SessionId) -> Result<(), Self::Error> {
            let (response, shutdown) = {
                let mut state = self.state.lock().expect("fake-pass state is not poisoned");
                state.observed.push(session);
                state.remaining_calls = state
                    .remaining_calls
                    .checked_sub(1)
                    .expect("test must supply one response per pass");
                let response = if session == state.failing_session {
                    Err(FakeSweepError::Unavailable)
                } else {
                    Ok(())
                };
                let shutdown = (state.remaining_calls == 0).then(|| {
                    state
                        .shutdown
                        .take()
                        .expect("test shutdown sender is present")
                });
                (response, shutdown)
            };
            if let Some(shutdown) = shutdown {
                shutdown
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
        let pass = FakePass::failing_once(first, 2, shutdown_sender);
        let observed = Arc::clone(&pass.state);
        let mut scheduler = SchedulerLoop::new(
            FakeWorkSource {
                hints: VecDeque::from([Ok(first), Ok(second)]),
            },
            pass,
        );

        let exit = scheduler
            .run_until(async {
                shutdown_receiver
                    .await
                    .expect("fake pass sends shutdown after both hints");
            })
            .await;
        let observed = observed
            .lock()
            .expect("fake-pass state is not poisoned")
            .observed
            .clone();

        assert_eq!(exit, SchedulerLoopExit::Shutdown);
        assert_eq!(observed.len(), 2);
        assert!(observed.contains(&first));
        assert!(observed.contains(&second));
    }

    #[derive(Clone, Debug)]
    struct BlockingSessionPass {
        blocked_session: SessionId,
        blocked_started: Arc<Notify>,
        release_blocked: Arc<Notify>,
        unrelated_seen: Arc<Notify>,
    }

    impl EligibilityPass for BlockingSessionPass {
        type Error = FakeSweepError;

        async fn run(&mut self, session: SessionId) -> Result<(), Self::Error> {
            if session == self.blocked_session {
                self.blocked_started.notify_one();
                self.release_blocked.notified().await;
            } else {
                self.unrelated_seen.notify_one();
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn inv007_blocked_session_does_not_block_unrelated_session() {
        let blocked = session(35);
        let unrelated = session(36);
        let blocked_started = Arc::new(Notify::new());
        let release_blocked = Arc::new(Notify::new());
        let unrelated_seen = Arc::new(Notify::new());
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let scheduler = SchedulerLoop::new(
            FakeWorkSource {
                hints: VecDeque::from([Ok(blocked), Ok(unrelated)]),
            },
            BlockingSessionPass {
                blocked_session: blocked,
                blocked_started: Arc::clone(&blocked_started),
                release_blocked: Arc::clone(&release_blocked),
                unrelated_seen: Arc::clone(&unrelated_seen),
            },
        );
        let runtime = tokio::spawn(async move {
            let mut scheduler = scheduler;
            scheduler
                .run_until(async {
                    shutdown_receiver.await.expect("the test requests shutdown");
                })
                .await
        });

        blocked_started.notified().await;
        timeout(Duration::from_secs(1), unrelated_seen.notified())
            .await
            .expect("an unrelated pass starts while the first is blocked");
        shutdown_sender
            .send(())
            .expect("the scheduler still listens for shutdown");
        release_blocked.notify_one();

        assert_eq!(
            runtime.await.expect("scheduler task completes"),
            SchedulerLoopExit::Shutdown
        );
    }

    #[derive(Debug)]
    struct CancellationSensitiveWorkSource {
        calls: usize,
        first: SessionId,
        second: SessionId,
        reconciliation_started: Arc<Notify>,
        release_reconciliation: Arc<Notify>,
    }

    impl EligibilityWorkSource for CancellationSensitiveWorkSource {
        type Error = FakeSweepError;

        async fn next(&mut self) -> Result<SessionId, Self::Error> {
            self.calls += 1;
            match self.calls {
                1 => Ok(self.first),
                2 => {
                    self.reconciliation_started.notify_one();
                    self.release_reconciliation.notified().await;
                    Ok(self.second)
                }
                _ => pending().await,
            }
        }
    }

    #[derive(Clone, Debug)]
    struct CompletionDuringReconciliationPass {
        first: SessionId,
        first_started: Arc<Notify>,
        release_first: Arc<Notify>,
        second_seen: Arc<Notify>,
    }

    impl EligibilityPass for CompletionDuringReconciliationPass {
        type Error = FakeSweepError;

        async fn run(&mut self, session: SessionId) -> Result<(), Self::Error> {
            if session == self.first {
                self.first_started.notify_one();
                self.release_first.notified().await;
            } else {
                self.second_seen.notify_one();
            }
            Ok(())
        }
    }

    /// INV-007: a pass completion cannot cancel a reconciliation read after
    /// its interval tick has been consumed.
    #[tokio::test]
    async fn inv007_pass_completion_preserves_in_progress_reconciliation() {
        let first = session(37);
        let second = session(38);
        let first_started = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        let reconciliation_started = Arc::new(Notify::new());
        let release_reconciliation = Arc::new(Notify::new());
        let second_seen = Arc::new(Notify::new());
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let scheduler = SchedulerLoop::new(
            CancellationSensitiveWorkSource {
                calls: 0,
                first,
                second,
                reconciliation_started: Arc::clone(&reconciliation_started),
                release_reconciliation: Arc::clone(&release_reconciliation),
            },
            CompletionDuringReconciliationPass {
                first,
                first_started: Arc::clone(&first_started),
                release_first: Arc::clone(&release_first),
                second_seen: Arc::clone(&second_seen),
            },
        );
        let runtime = tokio::spawn(async move {
            let mut scheduler = scheduler;
            scheduler
                .run_until(async {
                    shutdown_receiver.await.expect("the test requests shutdown");
                })
                .await
        });

        first_started.notified().await;
        reconciliation_started.notified().await;
        release_first.notify_one();
        tokio::task::yield_now().await;
        release_reconciliation.notify_one();
        timeout(Duration::from_secs(1), second_seen.notified())
            .await
            .expect("the same in-progress reconciliation yields its hint");
        shutdown_sender
            .send(())
            .expect("the scheduler still listens for shutdown");

        assert_eq!(
            runtime.await.expect("scheduler task completes"),
            SchedulerLoopExit::Shutdown
        );
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
