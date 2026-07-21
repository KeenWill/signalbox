//! Runtime scheduling over nonauthoritative session hints.
//!
//! ADR-0010 owns the durable-rows queue, same-process nudge, and periodic
//! reconciliation mechanics. This module keeps both hint sources behind one
//! application port and drives the existing authoritative eligibility pass.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    fmt,
    future::{Future, ready},
    num::NonZeroUsize,
    pin::Pin,
    time::Duration,
};

use signalbox_domain::SessionId;
use tokio::{
    pin, select,
    sync::mpsc::{
        self,
        error::{TryRecvError, TrySendError},
    },
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
    pub fn try_new(interval: Duration) -> Result<Self, InvalidReconciliationSweepInterval> {
        if interval.is_zero() || Instant::now().checked_add(interval).is_none() {
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

/// A zero or timer-unrepresentable duration cannot drive the safety-net sweep.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidReconciliationSweepInterval;

impl fmt::Display for InvalidReconciliationSweepInterval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("scheduler reconciliation interval must be nonzero and fit the timer range")
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

    /// Returns one bounded batch of session hints for queued work with no
    /// active slot owner.
    fn find_sessions(
        &mut self,
    ) -> impl Future<Output = Result<EligibilitySweepBatch, Self::Error>> + Send;
}

/// One bounded reconciliation result and whether its cycle has another page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EligibilitySweepBatch {
    sessions: Vec<SessionId>,
    continuation: bool,
}

impl EligibilitySweepBatch {
    /// Builds a reconciliation batch.
    pub fn new(sessions: Vec<SessionId>, continuation: bool) -> Self {
        Self {
            sessions,
            continuation,
        }
    }

    /// Splits the hints from the continuation marker.
    pub fn into_parts(self) -> (Vec<SessionId>, bool) {
        (self.sessions, self.continuation)
    }
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
    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static;
}

impl<Generator, Transaction> EligibilityPass for StartEligibleTurnService<Generator, Transaction>
where
    Generator: StartEligibleTurnIdGenerator + Send,
    Transaction: StartEligibleTurnTransaction + Clone + Send + 'static,
    Transaction::Error: Send + 'static,
{
    type Error = Transaction::Error;

    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let execution = self.execute_with_cloned_transaction(session);
        async move { execution.await.map(drop) }
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

type InProgressEligibilitySweep<Sweep> = Pin<
    Box<
        dyn Future<
                Output = (
                    Sweep,
                    Result<EligibilitySweepBatch, <Sweep as EligibilitySweep>::Error>,
                ),
            > + Send,
    >,
>;

/// Same-process nudges plus a periodic durable reconciliation sweep.
pub struct InProcessEligibilityWorkSource<Sweep>
where
    Sweep: EligibilitySweep,
{
    nudges: mpsc::Receiver<SessionId>,
    sweep: Option<Sweep>,
    sweep_in_progress: Option<InProgressEligibilitySweep<Sweep>>,
    sweep_interval: Interval,
    initial_sweep_due: bool,
    pending_sweep_hints: VecDeque<SessionId>,
    nudge_preferred_over_sweep_hint: bool,
    sweep_preferred_over_pending_hint: bool,
    sweep_continuation_due: bool,
}

impl<Sweep> fmt::Debug for InProcessEligibilityWorkSource<Sweep>
where
    Sweep: EligibilitySweep + fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InProcessEligibilityWorkSource")
            .field("sweep", &self.sweep)
            .field("sweep_in_progress", &self.sweep_in_progress.is_some())
            .field("initial_sweep_due", &self.initial_sweep_due)
            .field("pending_sweep_hints", &self.pending_sweep_hints)
            .finish_non_exhaustive()
    }
}

impl<Sweep> InProcessEligibilityWorkSource<Sweep>
where
    Sweep: EligibilitySweep,
{
    /// Builds a work source with the one-second baseline sweep interval.
    pub fn new(sweep: Sweep) -> (InProcessEligibilityNudge, Self) {
        Self::with_options(
            sweep,
            ReconciliationSweepInterval::baseline(),
            NonZeroUsize::new(BASELINE_NUDGE_BUFFER_CAPACITY).unwrap_or(NonZeroUsize::MIN),
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
            NonZeroUsize::new(BASELINE_NUDGE_BUFFER_CAPACITY).unwrap_or(NonZeroUsize::MIN),
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
        let now = Instant::now();
        let first_sweep_deadline = now.checked_add(sweep_interval.get()).unwrap_or(now);
        let mut interval = time::interval_at(first_sweep_deadline, sweep_interval.get());
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let source = Self {
            nudges,
            sweep: Some(sweep),
            sweep_in_progress: None,
            sweep_interval: interval,
            initial_sweep_due: true,
            pending_sweep_hints: VecDeque::new(),
            nudge_preferred_over_sweep_hint: true,
            sweep_preferred_over_pending_hint: false,
            sweep_continuation_due: false,
        };
        (nudge, source)
    }

    fn extend_pending_sweep_hints(&mut self, hints: impl IntoIterator<Item = SessionId>) {
        let mut pending = self
            .pending_sweep_hints
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        for session in hints {
            if pending.insert(session) {
                self.pending_sweep_hints.push_back(session);
            }
        }
    }

    fn take_interleaved_pending_hint(&mut self) -> Option<SessionId> {
        if self.nudge_preferred_over_sweep_hint {
            match self.nudges.try_recv() {
                Ok(session) => {
                    self.nudge_preferred_over_sweep_hint = false;
                    return Some(session);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
            }
        }
        self.nudge_preferred_over_sweep_hint = true;
        self.pending_sweep_hints.pop_front()
    }
}

impl<Sweep> InProcessEligibilityWorkSource<Sweep>
where
    Sweep: EligibilitySweep + Send + 'static,
{
    fn start_sweep(&mut self) {
        let Some(mut sweep) = self.sweep.take() else {
            return;
        };
        self.sweep_in_progress = Some(Box::pin(async move {
            let result = sweep.find_sessions().await;
            (sweep, result)
        }));
    }

    fn complete_sweep(
        &mut self,
        completion: (Sweep, Result<EligibilitySweepBatch, Sweep::Error>),
    ) -> Result<(), Sweep::Error> {
        let (sweep, result) = completion;
        self.sweep_in_progress = None;
        self.sweep = Some(sweep);
        let (hints, continuation) = result?.into_parts();
        self.extend_pending_sweep_hints(hints);
        self.sweep_continuation_due = continuation;
        self.sweep_preferred_over_pending_hint = false;
        Ok(())
    }
}

impl<Sweep> EligibilityWorkSource for InProcessEligibilityWorkSource<Sweep>
where
    Sweep: EligibilitySweep + Send + 'static,
{
    type Error = Sweep::Error;

    async fn next(&mut self) -> Result<SessionId, Self::Error> {
        loop {
            if self.initial_sweep_due {
                self.initial_sweep_due = false;
                self.start_sweep();
            }
            if !self.pending_sweep_hints.is_empty() {
                if !self.sweep_preferred_over_pending_hint {
                    self.sweep_preferred_over_pending_hint = true;
                    if let Some(session) = self.take_interleaved_pending_hint() {
                        return Ok(session);
                    }
                    continue;
                }
                if let Some(sweep_in_progress) = self.sweep_in_progress.as_mut() {
                    let completion = select! {
                        biased;

                        completion = sweep_in_progress => Some(completion),
                        () = ready(()) => None,
                    };
                    if let Some(completion) = completion {
                        self.complete_sweep(completion)?;
                        continue;
                    }
                    if let Some(session) = self.take_interleaved_pending_hint() {
                        return Ok(session);
                    }
                    continue;
                }
                select! {
                    biased;

                    _ = self.sweep_interval.tick() => {
                        self.start_sweep();
                    }
                    () = ready(()) => {
                        if let Some(session) = self.take_interleaved_pending_hint() {
                            return Ok(session);
                        }
                    }
                }
                continue;
            }

            if self.sweep_continuation_due && self.sweep_in_progress.is_none() {
                self.sweep_continuation_due = false;
                self.start_sweep();
            }

            if let Some(sweep_in_progress) = self.sweep_in_progress.as_mut() {
                let completion = select! {
                    biased;

                    completion = sweep_in_progress => completion,
                    Some(session) = self.nudges.recv() => return Ok(session),
                };
                self.complete_sweep(completion)?;
                continue;
            }

            select! {
                Some(session) = self.nudges.recv() => return Ok(session),
                _ = self.sweep_interval.tick() => {
                    self.start_sweep();
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
    Pass: EligibilityPass + Send,
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
        let mut pending_hints = VecDeque::new();
        let mut pending_reruns = HashSet::new();

        'scheduler: loop {
            if task_sessions.len() == self.max_in_flight_passes {
                select! {
                    biased;

                    () = &mut shutdown => break,
                    completed = passes.join_next_with_id() => {
                        if let Some(completed) = completed
                            && let Some(session) = observe_pass_completion(
                                completed,
                                &mut task_sessions,
                                &mut in_flight_sessions,
                            )
                            && pending_reruns.remove(&session)
                        {
                            pending_hints.push_back(session);
                        }
                    }
                    hint = self.work_source.next(), if pending_hints.is_empty() => {
                        match hint {
                            Ok(session) if in_flight_sessions.contains(&session) => {
                                pending_reruns.insert(session);
                            }
                            Ok(session) => pending_hints.push_back(session),
                            Err(error) => log_sweep_failure(&error),
                        }
                    }
                }
                continue;
            }

            if let Some(session) = pending_hints.pop_front() {
                select! {
                    biased;

                    () = &mut shutdown => break,
                    () = ready(()) => {
                        if in_flight_sessions.insert(session) {
                            let task = passes.spawn(self.pass.run(session));
                            task_sessions.insert(task.id(), session);
                        } else {
                            pending_reruns.insert(session);
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
                        if let Some(completed) = completed
                            && let Some(session) = observe_pass_completion(
                                completed,
                                &mut task_sessions,
                                &mut in_flight_sessions,
                            )
                            && pending_reruns.remove(&session)
                        {
                            pending_hints.push_back(session);
                            break None;
                        }
                    }
                    hint = &mut next_hint => break Some(hint),
                }
            };
            let Some(hint) = hint else {
                continue;
            };

            match hint {
                Ok(session) => {
                    if in_flight_sessions.insert(session) {
                        let task = passes.spawn(self.pass.run(session));
                        task_sessions.insert(task.id(), session);
                    } else {
                        pending_reruns.insert(session);
                    }
                }
                Err(error) => log_sweep_failure(&error),
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
) -> Option<SessionId>
where
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
        return None;
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
    Some(session)
}

fn log_sweep_failure<Error>(error: &Error)
where
    Error: ClassifyOperatorFailure,
{
    let failure_class = error.operator_failure_class();
    tracing::error!(
        ?failure_class,
        "eligibility reconciliation sweep failed; the next interval will retry"
    );
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::{Future, pending, ready},
        num::NonZeroUsize,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use signalbox_domain::{
        AcceptedInputTurnActivationIdentities, ContextFrontierId, SemanticTranscriptEntryId,
        SessionId, TurnAttemptId,
    };
    use tokio::{
        sync::{Notify, oneshot},
        time::timeout,
    };
    use uuid::Uuid;

    use super::{
        ClassifyOperatorFailure, EligibilityNudge, EligibilityNudgeOutcome, EligibilityPass,
        EligibilitySweep, EligibilitySweepBatch, EligibilityWorkSource,
        InProcessEligibilityWorkSource, InvalidReconciliationSweepInterval,
        ReconciliationSweepInterval, SchedulerLoop, SchedulerLoopExit,
    };
    use crate::{
        OperatorFailureClass, StartEligibleTurnIdGenerator, StartEligibleTurnOutcome,
        StartEligibleTurnService, StartEligibleTurnTransaction,
    };

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
        responses: VecDeque<Result<EligibilitySweepBatch, FakeSweepError>>,
    }

    impl FakeSweep {
        fn returning(
            responses: impl IntoIterator<Item = Result<Vec<SessionId>, FakeSweepError>>,
        ) -> Self {
            Self {
                responses: responses
                    .into_iter()
                    .map(|result| {
                        result.map(|sessions| EligibilitySweepBatch::new(sessions, false))
                    })
                    .collect(),
            }
        }
    }

    impl EligibilitySweep for FakeSweep {
        type Error = FakeSweepError;

        fn find_sessions(
            &mut self,
        ) -> impl Future<Output = Result<EligibilitySweepBatch, Self::Error>> + Send {
            ready(
                self.responses
                    .pop_front()
                    .expect("test must supply one response per sweep"),
            )
        }
    }

    #[derive(Debug)]
    struct SlowSweep {
        calls: Arc<AtomicUsize>,
        delay: Duration,
        hints: Vec<SessionId>,
    }

    impl EligibilitySweep for SlowSweep {
        type Error = FakeSweepError;

        fn find_sessions(
            &mut self,
        ) -> impl Future<Output = Result<EligibilitySweepBatch, Self::Error>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let delay = self.delay;
            let hints = self.hints.clone();
            async move {
                tokio::time::sleep(delay).await;
                Ok(EligibilitySweepBatch::new(hints, false))
            }
        }
    }

    #[derive(Debug)]
    struct BlockingSweep {
        calls: Arc<AtomicUsize>,
        started: Arc<Notify>,
        release: Arc<Notify>,
        hint: SessionId,
    }

    impl EligibilitySweep for BlockingSweep {
        type Error = FakeSweepError;

        fn find_sessions(
            &mut self,
        ) -> impl Future<Output = Result<EligibilitySweepBatch, Self::Error>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let started = Arc::clone(&self.started);
            let release = Arc::clone(&self.release);
            let hint = self.hint;
            async move {
                started.notify_one();
                release.notified().await;
                Ok(EligibilitySweepBatch::new(vec![hint], false))
            }
        }
    }

    #[test]
    fn zero_reconciliation_interval_is_rejected() {
        assert_eq!(
            ReconciliationSweepInterval::try_new(Duration::ZERO),
            Err(InvalidReconciliationSweepInterval)
        );
    }

    #[test]
    fn timer_unrepresentable_reconciliation_interval_is_rejected() {
        assert_eq!(
            ReconciliationSweepInterval::try_new(Duration::MAX),
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

    #[tokio::test]
    async fn inv007_nudge_proceeds_while_reconciliation_is_in_progress() {
        let nudged = session(35);
        let swept = session(36);
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (nudge, mut source) = InProcessEligibilityWorkSource::new(BlockingSweep {
            calls: Arc::clone(&calls),
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            hint: swept,
        });

        {
            let next = source.next();
            tokio::pin!(next);
            tokio::select! {
                () = started.notified() => {}
                result = &mut next => panic!("blocked reconciliation yielded unexpectedly: {result:?}"),
            }
            assert_eq!(nudge.nudge(nudged), EligibilityNudgeOutcome::Enqueued);
            assert_eq!(next.await, Ok(nudged));
        }

        release.notify_one();
        assert_eq!(source.next().await, Ok(swept));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn inv007_nudge_interleaves_with_pending_sweep_backlog() {
        let first_swept = session(37);
        let second_swept = session(38);
        let first_nudged = session(39);
        let second_nudged = session(40);
        let (nudge, mut source) =
            InProcessEligibilityWorkSource::new(FakeSweep::returning([Ok(vec![
                first_swept,
                second_swept,
            ])]));

        assert_eq!(source.next().await, Ok(first_swept));
        assert_eq!(nudge.nudge(first_nudged), EligibilityNudgeOutcome::Enqueued);
        assert_eq!(
            nudge.nudge(second_nudged),
            EligibilityNudgeOutcome::Enqueued
        );
        assert_eq!(source.next().await, Ok(first_nudged));
        assert_eq!(source.next().await, Ok(second_swept));
        assert_eq!(source.next().await, Ok(second_nudged));
    }

    #[tokio::test(start_paused = true)]
    async fn inv007_continuation_pages_do_not_wait_for_another_interval() {
        let first = session(43);
        let second = session(44);
        let third = session(47);
        let (_nudge, mut source) = InProcessEligibilityWorkSource::new(FakeSweep {
            responses: VecDeque::from([
                Ok(EligibilitySweepBatch::new(vec![first, second], true)),
                Ok(EligibilitySweepBatch::new(vec![third], false)),
            ]),
        });

        assert_eq!(source.next().await, Ok(first));
        assert_eq!(source.next().await, Ok(second));
        assert!(source.sweep_in_progress.is_none());
        assert_eq!(
            source
                .sweep
                .as_ref()
                .expect("sweep is idle between pages")
                .responses
                .len(),
            1
        );
        assert_eq!(source.next().await, Ok(third));
    }

    #[tokio::test(start_paused = true)]
    async fn inv007_slow_sweep_yields_and_deduplicates_pending_hints() {
        let first = session(41);
        let second = session(42);
        let interval = ReconciliationSweepInterval::try_new(Duration::from_secs(5))
            .expect("test interval is timer-representable");
        let calls = Arc::new(AtomicUsize::new(0));
        let (_nudge, mut source) = InProcessEligibilityWorkSource::with_interval(
            SlowSweep {
                calls: Arc::clone(&calls),
                delay: interval.get(),
                hints: vec![first, second],
            },
            interval,
        );

        assert_eq!(
            timeout(Duration::from_secs(16), source.next()).await,
            Ok(Ok(first))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        assert_eq!(source.next().await, Ok(second));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(source.pending_sweep_hints.is_empty());
        assert!(source.sweep_in_progress.is_some());

        assert_eq!(
            timeout(Duration::from_secs(6), source.next()).await,
            Ok(Ok(first))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            source
                .pending_sweep_hints
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![second]
        );
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

        fn run(
            &mut self,
            session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
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
            ready(response)
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

    #[derive(Debug)]
    struct StatefulActivationIds {
        next: u128,
    }

    impl StartEligibleTurnIdGenerator for StatefulActivationIds {
        fn next_origin_entry_id(&mut self) -> SemanticTranscriptEntryId {
            let id = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(self.next));
            self.next += 1;
            id
        }

        fn next_starting_frontier_id(&mut self) -> ContextFrontierId {
            let id = ContextFrontierId::from_uuid(Uuid::from_u128(self.next));
            self.next += 1;
            id
        }

        fn next_initial_attempt_id(&mut self) -> TurnAttemptId {
            let id = TurnAttemptId::from_uuid(Uuid::from_u128(self.next));
            self.next += 1;
            id
        }
    }

    #[derive(Clone, Debug)]
    struct RecordingActivationTransaction {
        identities: Arc<Mutex<Vec<AcceptedInputTurnActivationIdentities>>>,
        shutdown: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    }

    impl StartEligibleTurnTransaction for RecordingActivationTransaction {
        type Error = FakeSweepError;

        fn handle(
            &mut self,
            _session: SessionId,
            identities: AcceptedInputTurnActivationIdentities,
        ) -> impl Future<Output = Result<StartEligibleTurnOutcome, Self::Error>> + Send {
            let mut observed = self
                .identities
                .lock()
                .expect("recorded identities are not poisoned");
            observed.push(identities);
            if observed.len() == 2 {
                self.shutdown
                    .lock()
                    .expect("shutdown state is not poisoned")
                    .take()
                    .expect("second transaction owns shutdown")
                    .send(())
                    .expect("scheduler still waits for shutdown");
            }
            ready(Ok(StartEligibleTurnOutcome::NoEligibleTurn))
        }
    }

    #[tokio::test]
    async fn inv001_inv007_stateful_activation_ids_are_not_cloned_per_pass() {
        let first = session(48);
        let second = session(49);
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let identities = Arc::new(Mutex::new(Vec::new()));
        let pass = StartEligibleTurnService::new(
            StatefulActivationIds { next: 1 },
            RecordingActivationTransaction {
                identities: Arc::clone(&identities),
                shutdown: Arc::new(Mutex::new(Some(shutdown_sender))),
            },
        );
        let mut scheduler = SchedulerLoop::new(
            FakeWorkSource {
                hints: VecDeque::from([Ok(first), Ok(second)]),
            },
            pass,
        );

        assert_eq!(
            scheduler
                .run_until(async {
                    shutdown_receiver
                        .await
                        .expect("second transaction requests shutdown");
                })
                .await,
            SchedulerLoopExit::Shutdown
        );
        let identities = identities
            .lock()
            .expect("recorded identities are not poisoned");
        assert_eq!(identities.len(), 2);
        assert_ne!(identities[0], identities[1]);
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

        fn run(
            &mut self,
            session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            let blocked_session = self.blocked_session;
            let blocked_started = Arc::clone(&self.blocked_started);
            let release_blocked = Arc::clone(&self.release_blocked);
            let unrelated_seen = Arc::clone(&self.unrelated_seen);
            async move {
                if session == blocked_session {
                    blocked_started.notify_one();
                    release_blocked.notified().await;
                } else {
                    unrelated_seen.notify_one();
                }
                Ok(())
            }
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

        fn run(
            &mut self,
            session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            let first = self.first;
            let first_started = Arc::clone(&self.first_started);
            let release_first = Arc::clone(&self.release_first);
            let second_seen = Arc::clone(&self.second_seen);
            async move {
                if session == first {
                    first_started.notify_one();
                    release_first.notified().await;
                } else {
                    second_seen.notify_one();
                }
                Ok(())
            }
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

    #[derive(Clone, Debug)]
    struct RerunPass {
        calls: Arc<AtomicUsize>,
        first_started: Arc<Notify>,
        release_first: Arc<Notify>,
        shutdown: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    }

    impl EligibilityPass for RerunPass {
        type Error = FakeSweepError;

        fn run(
            &mut self,
            _session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            let calls = Arc::clone(&self.calls);
            let first_started = Arc::clone(&self.first_started);
            let release_first = Arc::clone(&self.release_first);
            let shutdown = Arc::clone(&self.shutdown);
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    first_started.notify_one();
                    release_first.notified().await;
                } else {
                    shutdown
                        .lock()
                        .expect("shutdown state is not poisoned")
                        .take()
                        .expect("second pass owns shutdown")
                        .send(())
                        .expect("scheduler still waits for shutdown");
                }
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn inv007_nudge_during_in_flight_pass_schedules_one_rerun() {
        let target = session(45);
        let first_started = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let scheduler = SchedulerLoop::new(
            FakeWorkSource {
                hints: VecDeque::from([Ok(target), Ok(target)]),
            },
            RerunPass {
                calls: Arc::clone(&calls),
                first_started: Arc::clone(&first_started),
                release_first: Arc::clone(&release_first),
                shutdown: Arc::new(Mutex::new(Some(shutdown_sender))),
            },
        );
        let runtime = tokio::spawn(async move {
            let mut scheduler = scheduler;
            scheduler
                .run_until(async {
                    shutdown_receiver
                        .await
                        .expect("second pass requests shutdown");
                })
                .await
        });

        first_started.notified().await;
        tokio::task::yield_now().await;
        release_first.notify_one();

        assert_eq!(
            runtime.await.expect("scheduler completes"),
            SchedulerLoopExit::Shutdown
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[derive(Debug)]
    struct CapacitySensitiveWorkSource {
        calls: usize,
        session: SessionId,
        sweep_driven: Arc<Notify>,
    }

    impl EligibilityWorkSource for CapacitySensitiveWorkSource {
        type Error = FakeSweepError;

        async fn next(&mut self) -> Result<SessionId, Self::Error> {
            self.calls += 1;
            if self.calls == 1 {
                Ok(self.session)
            } else {
                self.sweep_driven.notify_one();
                pending().await
            }
        }
    }

    #[derive(Clone, Debug)]
    struct PassWaitingForSweep {
        sweep_driven: Arc<Notify>,
        shutdown: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    }

    impl EligibilityPass for PassWaitingForSweep {
        type Error = FakeSweepError;

        fn run(
            &mut self,
            _session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            let sweep_driven = Arc::clone(&self.sweep_driven);
            let shutdown = Arc::clone(&self.shutdown);
            async move {
                sweep_driven.notified().await;
                shutdown
                    .lock()
                    .expect("shutdown state is not poisoned")
                    .take()
                    .expect("pass owns shutdown")
                    .send(())
                    .expect("scheduler still waits for shutdown");
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn inv007_work_source_remains_driven_at_pass_capacity() {
        let sweep_driven = Arc::new(Notify::new());
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let mut scheduler = SchedulerLoop::with_max_in_flight(
            CapacitySensitiveWorkSource {
                calls: 0,
                session: session(46),
                sweep_driven: Arc::clone(&sweep_driven),
            },
            PassWaitingForSweep {
                sweep_driven,
                shutdown: Arc::new(Mutex::new(Some(shutdown_sender))),
            },
            NonZeroUsize::new(1).expect("test capacity is nonzero"),
        );

        assert_eq!(
            timeout(
                Duration::from_secs(1),
                scheduler.run_until(async {
                    shutdown_receiver.await.expect("pass requests shutdown");
                })
            )
            .await,
            Ok(SchedulerLoopExit::Shutdown)
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
