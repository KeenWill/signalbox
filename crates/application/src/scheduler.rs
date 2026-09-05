//! Runtime scheduling over nonauthoritative session hints.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md owns the durable-rows queue,
//! same-process nudge, and periodic reconciliation mechanics. This module
//! keeps both hint sources behind one application port and drives the
//! existing authoritative eligibility pass.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    fmt,
    future::{Future, pending, ready},
    num::NonZeroUsize,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use signalbox_domain::{SessionId, TurnId};
use tokio::{
    pin, select,
    sync::{
        Notify,
        mpsc::{
            self,
            error::{TryRecvError, TrySendError},
        },
        watch,
    },
    task::{Id, JoinError, JoinSet},
    time::{self, Instant, Interval, MissedTickBehavior},
};
use tracing::Instrument;

use crate::{
    ClassifyOperatorFailure, StartEligibleTurnIdGenerator, StartEligibleTurnService,
    StartEligibleTurnTransaction,
};

// numeric-bound: guard - prevents nudge backpressure from dropping part of one rule's admitted dispatch
const MINIMUM_DISPATCH_START_BACKLOG_CAPACITY: usize = 32;
/// A configured optional bound on one authoritative pass's occupancy.
///
/// This bounds *occupancy*, not turn duration, and the difference matters: one
/// admitted pass drives a turn's entire model/tools loop, including provider
/// retry-backoff sleeps, and returns only at a terminal or durable-park outcome.
/// A healthy multi-round turn can therefore reach a configured bound while
/// making continuous durable progress, so reaching it is not evidence that
/// anything is wedged — it is only the point at which the scheduler reclaims the
/// slot, so that one long-running session cannot hold one indefinitely.
///
/// Expiry consequently hands the turn to a recovery path that decides for
/// itself, and that path requires the turn's durable evidence to be unchanged
/// across a confirmation delay before terminalizing it — the same
/// unchanged-evidence requirement both liveness watchdogs impose. A pass that
/// expired while progressing has its turn left active and eligibility nudged, so
/// a fresh pass may be admitted for it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SchedulerPassOccupancyBound(Option<Duration>);

impl SchedulerPassOccupancyBound {
    /// Disables occupancy expiry.
    pub const fn unbounded() -> Self {
        Self(None)
    }

    /// Accepts a configured nonzero whole-second occupancy bound.
    pub fn try_new(bound: Duration) -> Result<Self, InvalidSchedulerPassOccupancyBound> {
        if bound.is_zero() || bound.subsec_nanos() != 0 {
            Err(InvalidSchedulerPassOccupancyBound)
        } else {
            Ok(Self(Some(bound)))
        }
    }

    /// Returns the configured duration, or `None` when expiry is disabled.
    pub const fn get(self) -> Option<Duration> {
        self.0
    }
}

/// A proposed scheduler-pass occupancy bound was not a valid lowering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidSchedulerPassOccupancyBound;

impl fmt::Display for InvalidSchedulerPassOccupancyBound {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("scheduler pass occupancy bound must be a nonzero whole-second duration")
    }
}

impl Error for InvalidSchedulerPassOccupancyBound {}

/// Oldest-pass identity and start time retained by occupancy telemetry.
#[derive(Clone, Copy, Debug)]
pub struct SchedulerOldestInFlightPass {
    session: SessionId,
    started_at: Instant,
}

impl SchedulerOldestInFlightPass {
    /// Records the oldest admitted pass.
    pub const fn new(session: SessionId, started_at: Instant) -> Self {
        Self {
            session,
            started_at,
        }
    }

    /// Returns the pass's session identity.
    pub const fn session(self) -> SessionId {
        self.session
    }

    /// Returns its age at observation time.
    pub fn age(self) -> Duration {
        self.started_at.elapsed()
    }
}

/// Synchronous, content-free scheduler occupancy telemetry sink.
pub trait SchedulerOccupancyObserver: Send + Sync + 'static {
    /// Replaces the current occupancy snapshot.
    fn observe(&self, occupancy: usize, oldest: Option<SchedulerOldestInFlightPass>);
}

/// Synchronous handoff for a scheduler pass ended by its occupancy bound.
pub trait SchedulerPassExpiryHandler: fmt::Debug + Send + Sync + 'static {
    /// Starts daemon-owned recovery before the pass future is dropped.
    fn occupancy_expired(&self, session: SessionId);
}

/// A validated nonzero reconciliation-sweep interval.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReconciliationSweepInterval(Duration);

impl ReconciliationSweepInterval {
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
    /// A pending hint already represents the session, upgraded if necessary.
    Coalesced,
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

    /// Hands off a dispatched session that has no durable model-call evidence.
    ///
    /// The default preserves adapters that do not distinguish dispatch starts.
    /// The in-process scheduler source upgrades an equal ordinary hint rather
    /// than adding another pending item.
    fn nudge_dispatch_start(&self, session: SessionId) -> EligibilityNudgeOutcome {
        self.nudge(session)
    }
}

/// Admission class attached to one nonauthoritative scheduler hint.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
enum EligibilityHintPriority {
    /// Ordinary recovery, continuation, or execution work.
    #[default]
    Ordinary,
    /// A repository-watch dispatch with no durable model-call evidence.
    DispatchStart,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingEligibilityHint {
    priority: EligibilityHintPriority,
    queued_channel_tokens: u8,
}

/// Finds sessions whose durable storage shape requires an authoritative pass.
pub trait EligibilitySweep {
    /// Adapter-specific infrastructure failure.
    type Error;

    /// Returns one bounded batch of durable scheduling or disposition hints.
    fn find_sessions(
        &mut self,
    ) -> impl Future<Output = Result<EligibilitySweepBatch, Self::Error>> + Send;
}

/// One bounded reconciliation result and whether its cycle has another page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EligibilitySweepBatch {
    sessions: Vec<SessionId>,
    dispatch_starts: HashSet<SessionId>,
    unmonitored: HashSet<SessionId>,
    continuation: bool,
}

impl EligibilitySweepBatch {
    /// Builds an ordinary reconciliation batch.
    pub fn new(sessions: Vec<SessionId>, continuation: bool) -> Self {
        Self {
            sessions,
            dispatch_starts: HashSet::new(),
            unmonitored: HashSet::new(),
            continuation,
        }
    }

    /// Builds a reconciliation batch carrying durable dispatch-start priority.
    pub fn with_dispatch_starts(
        sessions: Vec<SessionId>,
        dispatch_starts: HashSet<SessionId>,
        continuation: bool,
    ) -> Self {
        Self {
            sessions,
            dispatch_starts,
            unmonitored: HashSet::new(),
            continuation,
        }
    }

    /// Marks the hinted sessions the daemon holds no liveness obligation for.
    ///
    /// An unmonitored session is a conversation: it still runs the turns a
    /// person submits, but it is excluded from occupancy accounting, so a
    /// person's open chat window never reads as driven work in flight.
    #[must_use]
    pub fn with_unmonitored(mut self, unmonitored: HashSet<SessionId>) -> Self {
        self.unmonitored = unmonitored;
        self
    }

    /// Splits the hints, durable priorities, and continuation marker.
    pub fn into_parts(self) -> (Vec<SessionId>, HashSet<SessionId>, bool) {
        (self.sessions, self.dispatch_starts, self.continuation)
    }

    /// Borrows the hinted sessions excluded from occupancy accounting.
    pub const fn unmonitored(&self) -> &HashSet<SessionId> {
        &self.unmonitored
    }
}

/// Supplies nonauthoritative session hints to the scheduler loop.
pub trait EligibilityWorkSource {
    /// Failure from the source's reconciliation path.
    type Error;

    /// Waits for the next same-process or reconciliation-derived hint.
    fn next(&mut self) -> impl Future<Output = Result<SessionId, Self::Error>> + Send;

    /// Takes the admission class attached to the session most recently returned.
    ///
    /// Sources that do not carry a class retain ordinary scheduling. The
    /// scheduler calls this immediately after each successful next result.
    fn take_returned_dispatch_start(&mut self, _session: SessionId) -> bool {
        false
    }

    /// Takes the ownership marker the source attached to this session's hint.
    ///
    /// An unmonitored session is a conversation. It still runs the turns a
    /// person submits, and its turns keep every liveness guarantee; what it
    /// has no place in is occupancy accounting, so the occupancy the daemon
    /// reports stays a measure of driven work rather than of open chat
    /// windows.
    ///
    /// The marker is taken, not read: it authorizes exactly the one admission
    /// the reconciliation that produced it described. A source with no durable
    /// ownership hint — and any session admitted from a nudge rather than that
    /// reconciliation — reports owned, which is the answer that keeps the
    /// session's watchdog rather than removing it.
    fn take_returned_unmonitored(&mut self, _session: SessionId) -> bool {
        false
    }

    /// Takes one buffered dispatch-start hint without consuming ordinary work.
    fn take_pending_dispatch_start(&mut self) -> Option<SessionId> {
        None
    }

    /// Waits for one buffered dispatch-start hint without consuming ordinary work.
    fn next_pending_dispatch_start(
        &mut self,
    ) -> impl Future<Output = Result<SessionId, Self::Error>> + Send {
        std::future::pending()
    }
}

/// Runs one authoritative per-session eligibility pass.
pub trait EligibilityPass {
    /// Adapter-specific failure from the authoritative pass.
    type Error;

    /// Returns the closed stage label for one failed pass.
    ///
    /// Implementations override this when their error retains a narrower
    /// application stage. The returned token must never contain adapter prose
    /// or caller content because the scheduler emits it as operator telemetry.
    fn failure_stage(_error: &Self::Error) -> &'static str {
        "eligibility_pass"
    }

    /// Returns the affected turn when the pass had selected one before failing.
    fn failure_turn(_error: &Self::Error) -> Option<TurnId> {
        None
    }

    /// Captures the handoff invoked if this pass exceeds its occupancy bound.
    ///
    /// The returned callback is independent of mutable pass state so identity
    /// generators remain shared across concurrent passes. Implementations
    /// spawn any required durable work from its synchronous callback.
    fn occupancy_expiry_handler(&self) -> Option<Arc<dyn SchedulerPassExpiryHandler>> {
        None
    }

    /// Revalidates durable state and applies at most one guarded transition.
    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static;

    /// Runs a hint carrying reserved dispatch-start admission.
    ///
    /// Implementations that can encounter long-lived already-active work
    /// override this boundary so stale priority hints release the reserved
    /// lane before that work resumes through an ordinary rerun.
    fn run_dispatch_start(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.run(session)
    }
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

/// Durable goal disposition owed after one authoritative eligibility pass.
pub trait GoalPassDisposition: Clone + Send + 'static {
    /// Adapter failure while reading or changing durable goal state.
    type Error: Send + 'static;

    /// Reconciles the current generation's latest turn after a successful pass.
    fn reconcile_success(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static;

    /// Blocks pursuit after a pass failed with one exact selected turn.
    fn block_execution_failure(
        &self,
        session: SessionId,
        turn: TurnId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static;
}

/// Failure from an underlying eligibility pass plus goal disposition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalAwareEligibilityPassError<PassError, GoalError> {
    /// The pass failed; optional secondary evidence records failed blocking.
    Pass {
        /// Original authoritative pass failure.
        source: PassError,
        /// Goal blocking also failed after the pass selected a turn.
        blocking: Option<GoalError>,
    },
    /// A successful pass could not reconcile durable goal continuation.
    Reconciliation(GoalError),
}

impl<PassError, GoalError> fmt::Display for GoalAwareEligibilityPassError<PassError, GoalError>
where
    PassError: fmt::Display,
    GoalError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pass {
                source,
                blocking: None,
            } => write!(formatter, "{source}"),
            Self::Pass {
                source,
                blocking: Some(blocking),
            } => write!(
                formatter,
                "{source}; goal execution-failure blocking also failed: {blocking}"
            ),
            Self::Reconciliation(error) => {
                write!(
                    formatter,
                    "goal continuation reconciliation failed: {error}"
                )
            }
        }
    }
}

impl<PassError, GoalError> Error for GoalAwareEligibilityPassError<PassError, GoalError>
where
    PassError: Error + 'static,
    GoalError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Pass { source, .. } => Some(source),
            Self::Reconciliation(error) => Some(error),
        }
    }
}

impl<PassError, GoalError> ClassifyOperatorFailure
    for GoalAwareEligibilityPassError<PassError, GoalError>
where
    PassError: ClassifyOperatorFailure,
    GoalError: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> crate::OperatorFailureClass {
        match self {
            Self::Pass {
                source,
                blocking: None,
            } => source.operator_failure_class(),
            Self::Pass {
                blocking: Some(error),
                ..
            } => error.operator_failure_class(),
            Self::Reconciliation(error) => error.operator_failure_class(),
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Pass {
                source,
                blocking: None,
            } => source.operator_failure_cause_code(),
            Self::Pass {
                blocking: Some(error),
                ..
            } => error.operator_failure_cause_code(),
            Self::Reconciliation(error) => error.operator_failure_cause_code(),
        }
    }
}

/// Eligibility pass wrapper that makes goal continuation and failure blocking
/// part of the same scheduler disposition path.
#[derive(Clone, Debug)]
pub struct GoalAwareEligibilityPass<Pass, Disposition> {
    pass: Pass,
    disposition: Disposition,
}

impl<Pass, Disposition> GoalAwareEligibilityPass<Pass, Disposition> {
    /// Binds one authoritative pass to its durable goal disposition adapter.
    pub const fn new(pass: Pass, disposition: Disposition) -> Self {
        Self { pass, disposition }
    }

    /// Returns both owned composition roles.
    pub fn into_parts(self) -> (Pass, Disposition) {
        (self.pass, self.disposition)
    }
}

impl<Pass, Disposition> EligibilityPass for GoalAwareEligibilityPass<Pass, Disposition>
where
    Pass: EligibilityPass + Send + 'static,
    Pass::Error: Send + 'static,
    Disposition: GoalPassDisposition,
{
    type Error = GoalAwareEligibilityPassError<Pass::Error, Disposition::Error>;

    fn failure_stage(error: &Self::Error) -> &'static str {
        match error {
            GoalAwareEligibilityPassError::Pass { source, .. } => Pass::failure_stage(source),
            GoalAwareEligibilityPassError::Reconciliation(_) => "goal_reconciliation",
        }
    }

    fn failure_turn(error: &Self::Error) -> Option<TurnId> {
        match error {
            GoalAwareEligibilityPassError::Pass { source, .. } => Pass::failure_turn(source),
            GoalAwareEligibilityPassError::Reconciliation(_) => None,
        }
    }

    fn occupancy_expiry_handler(&self) -> Option<Arc<dyn SchedulerPassExpiryHandler>> {
        self.pass.occupancy_expiry_handler()
    }

    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let pass = self.pass.run(session);
        let disposition = self.disposition.clone();
        async move {
            match pass.await {
                Ok(()) => disposition
                    .reconcile_success(session)
                    .await
                    .map_err(GoalAwareEligibilityPassError::Reconciliation),
                Err(source) => {
                    let blocking = match Pass::failure_turn(&source) {
                        Some(turn) => disposition
                            .block_execution_failure(session, turn)
                            .await
                            .err(),
                        None => None,
                    };
                    Err(GoalAwareEligibilityPassError::Pass { source, blocking })
                }
            }
        }
    }

    fn run_dispatch_start(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let pass = self.pass.run_dispatch_start(session);
        let disposition = self.disposition.clone();
        async move {
            match pass.await {
                Ok(()) => disposition
                    .reconcile_success(session)
                    .await
                    .map_err(GoalAwareEligibilityPassError::Reconciliation),
                Err(source) => {
                    let blocking = match Pass::failure_turn(&source) {
                        Some(turn) => disposition
                            .block_execution_failure(session, turn)
                            .await
                            .err(),
                        None => None,
                    };
                    Err(GoalAwareEligibilityPassError::Pass { source, blocking })
                }
            }
        }
    }
}

/// Cloneable same-process post-commit nudge hook.
#[derive(Clone, Debug)]
pub struct InProcessEligibilityNudge {
    sender: EligibilityNudgeSender,
    pending: Arc<Mutex<HashMap<SessionId, PendingEligibilityHint>>>,
    dispatch_start_available: Arc<Notify>,
    dispatch_start_backlog_capacity: usize,
}

/// Either half of the configured nudge channel.
///
/// A deployment that configures no nudge-buffer capacity gets an unbounded
/// channel, whose send never reports `Full`. Reusing the bounded channel's error
/// type keeps one admission path for both shapes: the unbounded arm simply never
/// reaches the backpressure branches.
#[derive(Clone, Debug)]
enum EligibilityNudgeSender {
    Bounded(mpsc::Sender<SessionId>),
    Unbounded(mpsc::UnboundedSender<SessionId>),
}

impl EligibilityNudgeSender {
    fn try_send(&self, session: SessionId) -> Result<(), TrySendError<SessionId>> {
        match self {
            Self::Bounded(sender) => sender.try_send(session),
            Self::Unbounded(sender) => sender
                .send(session)
                .map_err(|error| TrySendError::Closed(error.0)),
        }
    }
}

enum EligibilityNudgeReceiver {
    Bounded(mpsc::Receiver<SessionId>),
    Unbounded(mpsc::UnboundedReceiver<SessionId>),
}

impl EligibilityNudgeReceiver {
    fn try_recv(&mut self) -> Result<SessionId, TryRecvError> {
        match self {
            Self::Bounded(receiver) => receiver.try_recv(),
            Self::Unbounded(receiver) => receiver.try_recv(),
        }
    }

    async fn recv(&mut self) -> Option<SessionId> {
        match self {
            Self::Bounded(receiver) => receiver.recv().await,
            Self::Unbounded(receiver) => receiver.recv().await,
        }
    }
}

impl InProcessEligibilityNudge {
    fn pending_hints(&self) -> MutexGuard<'_, HashMap<SessionId, PendingEligibilityHint>> {
        match self.pending.lock() {
            Ok(pending) => pending,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn nudge_with_priority(
        &self,
        session: SessionId,
        priority: EligibilityHintPriority,
    ) -> EligibilityNudgeOutcome {
        let mut pending = self.pending_hints();
        if let Some(existing) = pending.get_mut(&session) {
            if existing.priority < priority {
                existing.priority = priority;
            }
            drop(pending);
            if priority == EligibilityHintPriority::DispatchStart {
                self.dispatch_start_available.notify_one();
            }
            return EligibilityNudgeOutcome::Coalesced;
        }
        let pending_dispatch_starts = pending
            .values()
            .filter(|pending_hint| pending_hint.priority == EligibilityHintPriority::DispatchStart)
            .count();
        pending.insert(
            session,
            PendingEligibilityHint {
                priority,
                queued_channel_tokens: 0,
            },
        );
        let outcome = match self.sender.try_send(session) {
            Ok(()) => {
                if let Some(pending_hint) = pending.get_mut(&session) {
                    pending_hint.queued_channel_tokens = 1;
                }
                EligibilityNudgeOutcome::Enqueued
            }
            Err(TrySendError::Full(_))
                if priority == EligibilityHintPriority::DispatchStart
                    && pending_dispatch_starts < self.dispatch_start_backlog_capacity =>
            {
                // Preserve one complete multi-action dispatch outside the
                // ordinary channel so backpressure cannot starve the reserved
                // lane after its first priority pass.
                EligibilityNudgeOutcome::Enqueued
            }
            Err(TrySendError::Full(_)) => EligibilityNudgeOutcome::DroppedAtCapacity,
            Err(TrySendError::Closed(_)) => EligibilityNudgeOutcome::WorkSourceClosed,
        };
        if outcome != EligibilityNudgeOutcome::Enqueued {
            pending.remove(&session);
        }
        drop(pending);
        if outcome == EligibilityNudgeOutcome::Enqueued
            && priority == EligibilityHintPriority::DispatchStart
        {
            self.dispatch_start_available.notify_one();
        }
        outcome
    }
}

impl EligibilityNudge for InProcessEligibilityNudge {
    fn nudge(&self, session: SessionId) -> EligibilityNudgeOutcome {
        self.nudge_with_priority(session, EligibilityHintPriority::Ordinary)
    }

    fn nudge_dispatch_start(&self, session: SessionId) -> EligibilityNudgeOutcome {
        self.nudge_with_priority(session, EligibilityHintPriority::DispatchStart)
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
    nudges: EligibilityNudgeReceiver,
    pending_nudges: Arc<Mutex<HashMap<SessionId, PendingEligibilityHint>>>,
    dispatch_start_available: Arc<Notify>,
    returned_priority: Option<(SessionId, EligibilityHintPriority)>,
    sweep: Option<Sweep>,
    sweep_in_progress: Option<InProgressEligibilitySweep<Sweep>>,
    sweep_interval: Option<Interval>,
    initial_sweep_due: bool,
    pending_sweep_hints: VecDeque<SessionId>,
    pending_sweep_dispatch_starts: HashSet<SessionId>,
    unmonitored_sessions: HashSet<SessionId>,
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

impl<Sweep> Drop for InProcessEligibilityWorkSource<Sweep>
where
    Sweep: EligibilitySweep,
{
    fn drop(&mut self) {
        let mut pending = match self.pending_nudges.lock() {
            Ok(pending) => pending,
            Err(poisoned) => poisoned.into_inner(),
        };
        pending.clear();
    }
}

impl<Sweep> InProcessEligibilityWorkSource<Sweep>
where
    Sweep: EligibilitySweep,
{
    /// Builds an unbounded work source with only its initial reconciliation sweep.
    pub fn new(sweep: Sweep) -> (InProcessEligibilityNudge, Self) {
        Self::with_options(sweep, None, None)
    }

    /// Builds a work source with an explicitly validated sweep interval.
    pub fn with_interval(
        sweep: Sweep,
        sweep_interval: ReconciliationSweepInterval,
    ) -> (InProcessEligibilityNudge, Self) {
        Self::with_options(sweep, Some(sweep_interval), None)
    }

    /// Builds a work source with explicit validated timing and buffer bounds.
    pub fn with_options(
        sweep: Sweep,
        sweep_interval: Option<ReconciliationSweepInterval>,
        nudge_buffer_capacity: Option<NonZeroUsize>,
    ) -> (InProcessEligibilityNudge, Self) {
        let (sender, nudges, dispatch_start_backlog_capacity) = match nudge_buffer_capacity {
            Some(capacity) => {
                let (sender, receiver) = mpsc::channel(capacity.get());
                (
                    EligibilityNudgeSender::Bounded(sender),
                    EligibilityNudgeReceiver::Bounded(receiver),
                    capacity.get().max(MINIMUM_DISPATCH_START_BACKLOG_CAPACITY),
                )
            }
            None => {
                let (sender, receiver) = mpsc::unbounded_channel();
                // An unbounded nudge channel applies no backpressure, so its
                // send can never report `Full` and the reserved dispatch-start
                // backlog can never be starved by ordinary capacity. The
                // backlog bound is correspondingly unbounded; the compiled
                // minimum stays the floor for configured bounded buffers.
                (
                    EligibilityNudgeSender::Unbounded(sender),
                    EligibilityNudgeReceiver::Unbounded(receiver),
                    usize::MAX,
                )
            }
        };
        let pending_nudges = Arc::new(Mutex::new(HashMap::new()));
        let dispatch_start_available = Arc::new(Notify::new());
        let nudge = InProcessEligibilityNudge {
            sender,
            pending: Arc::clone(&pending_nudges),
            dispatch_start_available: Arc::clone(&dispatch_start_available),
            dispatch_start_backlog_capacity,
        };
        let interval = sweep_interval.map(|sweep_interval| {
            let now = Instant::now();
            let first_sweep_deadline = now.checked_add(sweep_interval.get()).unwrap_or(now);
            let mut interval = time::interval_at(first_sweep_deadline, sweep_interval.get());
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
            interval
        });
        let source = Self {
            nudges,
            pending_nudges,
            dispatch_start_available,
            returned_priority: None,
            sweep: Some(sweep),
            sweep_in_progress: None,
            sweep_interval: interval,
            initial_sweep_due: true,
            pending_sweep_hints: VecDeque::new(),
            pending_sweep_dispatch_starts: HashSet::new(),
            unmonitored_sessions: HashSet::new(),
            nudge_preferred_over_sweep_hint: true,
            sweep_preferred_over_pending_hint: false,
            sweep_continuation_due: false,
        };
        (nudge, source)
    }

    fn extend_pending_sweep_hints(
        &mut self,
        hints: impl IntoIterator<Item = SessionId>,
        dispatch_starts: HashSet<SessionId>,
    ) {
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
        self.pending_sweep_dispatch_starts.extend(dispatch_starts);
    }

    fn take_pending_sweep_dispatch_start(&mut self) -> Option<SessionId> {
        let position = self
            .pending_sweep_hints
            .iter()
            .position(|session| self.pending_sweep_dispatch_starts.contains(session))?;
        let session = self.pending_sweep_hints.remove(position)?;
        self.pending_sweep_dispatch_starts.remove(&session);
        Some(session)
    }

    fn take_nudge(&mut self, session: SessionId) -> Option<SessionId> {
        let priority = {
            let mut pending = match self.pending_nudges.lock() {
                Ok(pending) => pending,
                Err(poisoned) => poisoned.into_inner(),
            };
            pending.remove(&session)?.priority
        };
        self.returned_priority = Some((session, priority));
        Some(session)
    }

    fn take_interleaved_pending_hint(&mut self) -> Option<SessionId> {
        if self.nudge_preferred_over_sweep_hint {
            match self.nudges.try_recv() {
                Ok(session) => {
                    self.nudge_preferred_over_sweep_hint = false;
                    if let Some(session) = self.take_nudge(session) {
                        return Some(session);
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
            }
        }
        self.nudge_preferred_over_sweep_hint = true;
        let session = self.pending_sweep_hints.pop_front()?;
        let priority = if self.pending_sweep_dispatch_starts.remove(&session) {
            EligibilityHintPriority::DispatchStart
        } else {
            EligibilityHintPriority::Ordinary
        };
        self.returned_priority = Some((session, priority));
        Some(session)
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
        let batch = result?;
        self.unmonitored_sessions = batch.unmonitored().clone();
        let (hints, dispatch_starts, continuation) = batch.into_parts();
        self.extend_pending_sweep_hints(hints, dispatch_starts);
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
        'source: loop {
            if let Some(session) = self.take_pending_dispatch_start() {
                self.returned_priority = Some((session, EligibilityHintPriority::DispatchStart));
                return Ok(session);
            }
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

                    _ = next_sweep_tick(&mut self.sweep_interval) => {
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
                    Some(session) = self.nudges.recv() => {
                        if let Some(session) = self.take_nudge(session) {
                            return Ok(session);
                        }
                        continue 'source;
                    },
                };
                self.complete_sweep(completion)?;
                continue;
            }

            select! {
                Some(session) = self.nudges.recv() => {
                    if let Some(session) = self.take_nudge(session) {
                        return Ok(session);
                    }
                    continue 'source;
                },
                _ = next_sweep_tick(&mut self.sweep_interval) => {
                    self.start_sweep();
                }
            }
        }
    }

    fn take_returned_unmonitored(&mut self, session: SessionId) -> bool {
        self.unmonitored_sessions.remove(&session)
    }

    fn take_returned_dispatch_start(&mut self, session: SessionId) -> bool {
        self.returned_priority
            .take()
            .filter(|(returned, _)| *returned == session)
            .is_some_and(|(_, priority)| priority == EligibilityHintPriority::DispatchStart)
    }

    fn take_pending_dispatch_start(&mut self) -> Option<SessionId> {
        let session = {
            let mut pending = match self.pending_nudges.lock() {
                Ok(pending) => pending,
                Err(poisoned) => poisoned.into_inner(),
            };
            let session = pending.iter().find_map(|(session, hint)| {
                (hint.priority == EligibilityHintPriority::DispatchStart).then_some(*session)
            });
            if let Some(session) = session {
                if pending
                    .get(&session)
                    .is_some_and(|hint| hint.queued_channel_tokens > 0)
                {
                    if let Some(hint) = pending.get_mut(&session) {
                        hint.priority = EligibilityHintPriority::Ordinary;
                    }
                } else {
                    pending.remove(&session);
                }
            }
            session
        };
        session.or_else(|| self.take_pending_sweep_dispatch_start())
    }

    async fn next_pending_dispatch_start(&mut self) -> Result<SessionId, Self::Error> {
        let dispatch_start_available = Arc::clone(&self.dispatch_start_available);
        loop {
            let notified = dispatch_start_available.notified();
            if let Some(session) = self.take_pending_dispatch_start() {
                return Ok(session);
            }
            if self.initial_sweep_due {
                self.initial_sweep_due = false;
                self.start_sweep();
            }
            if self.sweep_continuation_due && self.sweep_in_progress.is_none() {
                self.sweep_continuation_due = false;
                self.start_sweep();
            }
            if let Some(sweep_in_progress) = self.sweep_in_progress.as_mut() {
                let completion = select! {
                    completion = sweep_in_progress => Some(completion),
                    () = notified => None,
                };
                if let Some(completion) = completion {
                    self.complete_sweep(completion)?;
                }
                continue;
            }
            select! {
                _ = next_sweep_tick(&mut self.sweep_interval) => self.start_sweep(),
                () = notified => {}
            }
        }
    }
}

async fn next_sweep_tick(interval: &mut Option<Interval>) {
    match interval {
        Some(interval) => {
            interval.tick().await;
        }
        None => pending().await,
    }
}

/// Why the scheduler loop stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerLoopExit {
    /// The composition root requested shutdown.
    Shutdown,
}

/// Drives authoritative per-session passes from nonauthoritative work hints.
pub struct SchedulerLoop<WorkSource, Pass> {
    work_source: WorkSource,
    pass: Pass,
    max_in_flight_passes: usize,
    occupancy_bound: SchedulerPassOccupancyBound,
    occupancy_observer: Option<Arc<dyn SchedulerOccupancyObserver>>,
}

impl<WorkSource, Pass> SchedulerLoop<WorkSource, Pass> {
    /// Composes the work-source and authoritative-pass ports.
    pub const fn new(work_source: WorkSource, pass: Pass) -> Self {
        Self {
            work_source,
            pass,
            max_in_flight_passes: usize::MAX,
            occupancy_bound: SchedulerPassOccupancyBound::unbounded(),
            occupancy_observer: None,
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
            occupancy_bound: SchedulerPassOccupancyBound::unbounded(),
            occupancy_observer: None,
        }
    }

    /// Composes the ports without admitting authoritative passes.
    pub const fn paused(work_source: WorkSource, pass: Pass) -> Self {
        Self {
            work_source,
            pass,
            max_in_flight_passes: 0,
            occupancy_bound: SchedulerPassOccupancyBound::unbounded(),
            occupancy_observer: None,
        }
    }

    /// Applies the configured pass-occupancy policy to this loop.
    pub fn with_occupancy_bound(mut self, bound: SchedulerPassOccupancyBound) -> Self {
        self.occupancy_bound = bound;
        self
    }

    /// Installs the content-free occupancy observer.
    pub fn with_occupancy_observer(
        mut self,
        observer: Arc<dyn SchedulerOccupancyObserver>,
    ) -> Self {
        self.occupancy_observer = Some(observer);
        self
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
    /// in progress stops spending its ordinary occupancy deadline and drains
    /// under the composition root's shutdown grace window instead.
    pub async fn run_until<Shutdown>(&mut self, shutdown: Shutdown) -> SchedulerLoopExit
    where
        Shutdown: Future<Output = ()> + Send,
    {
        pin!(shutdown);
        if self.max_in_flight_passes == 0 {
            shutdown.await;
            return SchedulerLoopExit::Shutdown;
        }
        let mut passes = JoinSet::new();
        let mut task_sessions = HashMap::new();
        let mut in_flight_sessions = HashSet::new();
        let mut ordinary_in_flight = 0;
        let mut pending_dispatch_starts = VecDeque::new();
        let mut pending_ordinary = VecDeque::new();
        let mut pending_hints = HashMap::new();
        let mut pending_reruns = HashMap::new();
        let mut deferred_dispatch_start_retries = HashSet::new();
        let (shutdown_drain, shutdown_drain_receiver) = watch::channel(false);
        observe_occupancy(&self.occupancy_observer, &task_sessions);

        'scheduler: loop {
            if let Some((session, priority)) = take_admissible_hint(
                PendingHintQueues {
                    dispatch_starts: &mut pending_dispatch_starts,
                    ordinary: &mut pending_ordinary,
                    priorities: &mut pending_hints,
                },
                AdmissionState {
                    total_in_flight: task_sessions.len(),
                    ordinary_in_flight,
                    max_in_flight_passes: self.max_in_flight_passes,
                },
            ) {
                select! {
                    biased;

                    () = &mut shutdown => break,
                    () = ready(()) => {
                        if in_flight_sessions.insert(session) {
                            // The bound stays on every pass: its expiry is
                            // turn-liveness recovery, which ownership does not
                            // govern. What ownership governs is the
                            // accounting, so the marker moves the metric only.
                            let counts_toward_occupancy =
                                !self.work_source.take_returned_unmonitored(session);
                            spawn_pass(
                                &mut passes,
                                &mut self.pass,
                                session,
                                priority,
                                counts_toward_occupancy,
                                self.occupancy_bound,
                                shutdown_drain_receiver.clone(),
                                &mut task_sessions,
                                &self.occupancy_observer,
                            );
                            if priority == EligibilityHintPriority::Ordinary {
                                ordinary_in_flight += 1;
                            }
                        } else {
                            record_pending_rerun(
                                &mut pending_reruns,
                                session,
                                priority,
                            );
                        }
                    }
                }
                continue;
            }

            // A completion may win this select many times, but it must not
            // cancel an in-progress reconciliation read after that read has
            // consumed its interval tick. Keep the same next-hint future
            // pinned until it yields a hint, a visible failure, an admission
            // opening for queued work, or shutdown.
            let hint = if pending_hints.is_empty() {
                let next_hint = self.work_source.next();
                pin!(next_hint);
                loop {
                    select! {
                        biased;

                        () = &mut shutdown => break 'scheduler,
                        completed = passes.join_next_with_id(),
                            if !task_sessions.is_empty() =>
                        {
                            if let Some(completed) = completed
                                && apply_pass_completion::<Pass>(
                                    completed,
                                    PassCompletionState {
                                        task_sessions: &mut task_sessions,
                                        in_flight_sessions: &mut in_flight_sessions,
                                        ordinary_in_flight: &mut ordinary_in_flight,
                                        pending_hints: &mut pending_hints,
                                        pending_dispatch_starts: &mut pending_dispatch_starts,
                                        pending_ordinary: &mut pending_ordinary,
                                        pending_reruns: &mut pending_reruns,
                                        deferred_dispatch_start_retries:
                                            &mut deferred_dispatch_start_retries,
                                    },
                                    &self.occupancy_observer,
                                )
                                && has_admissible_hint(
                                    &pending_hints,
                                    AdmissionState {
                                        total_in_flight: task_sessions.len(),
                                        ordinary_in_flight,
                                        max_in_flight_passes: self.max_in_flight_passes,
                                    },
                                )
                            {
                                break None;
                            }
                        }
                        hint = &mut next_hint => break Some(hint),
                    }
                }
            } else {
                if let Some(session) = self.work_source.take_pending_dispatch_start() {
                    deferred_dispatch_start_retries.remove(&session);
                    enqueue_pending_hint(
                        session,
                        EligibilityHintPriority::DispatchStart,
                        &in_flight_sessions,
                        &mut pending_reruns,
                        &mut pending_hints,
                        &mut pending_dispatch_starts,
                        &mut pending_ordinary,
                    );
                    continue;
                }
                let pending_dispatch_start = self.work_source.next_pending_dispatch_start();
                pin!(pending_dispatch_start);
                loop {
                    select! {
                        biased;

                        () = &mut shutdown => break 'scheduler,
                        session = &mut pending_dispatch_start => {
                            let session = match session {
                                Ok(session) => session,
                                Err(error) => {
                                    log_sweep_failure(&error);
                                    break None;
                                }
                            };
                            deferred_dispatch_start_retries.remove(&session);
                            enqueue_pending_hint(
                                session,
                                EligibilityHintPriority::DispatchStart,
                                &in_flight_sessions,
                                &mut pending_reruns,
                                &mut pending_hints,
                                &mut pending_dispatch_starts,
                                &mut pending_ordinary,
                            );
                            break None;
                        }
                        completed = passes.join_next_with_id(),
                            if !task_sessions.is_empty() =>
                        {
                            if let Some(completed) = completed
                                && apply_pass_completion::<Pass>(
                                    completed,
                                    PassCompletionState {
                                        task_sessions: &mut task_sessions,
                                        in_flight_sessions: &mut in_flight_sessions,
                                        ordinary_in_flight: &mut ordinary_in_flight,
                                        pending_hints: &mut pending_hints,
                                        pending_dispatch_starts: &mut pending_dispatch_starts,
                                        pending_ordinary: &mut pending_ordinary,
                                        pending_reruns: &mut pending_reruns,
                                        deferred_dispatch_start_retries:
                                            &mut deferred_dispatch_start_retries,
                                    },
                                    &self.occupancy_observer,
                                )
                            {
                                break None;
                            }
                        }
                    }
                }
            };
            let Some(hint) = hint else {
                continue;
            };

            match hint {
                Ok(session) => {
                    let priority = if self.work_source.take_returned_dispatch_start(session)
                        || deferred_dispatch_start_retries.remove(&session)
                    {
                        EligibilityHintPriority::DispatchStart
                    } else {
                        EligibilityHintPriority::Ordinary
                    };
                    enqueue_pending_hint(
                        session,
                        priority,
                        &in_flight_sessions,
                        &mut pending_reruns,
                        &mut pending_hints,
                        &mut pending_dispatch_starts,
                        &mut pending_ordinary,
                    );
                }
                Err(error) => log_sweep_failure(&error),
            }
        }

        shutdown_drain.send_replace(true);
        while let Some(completed) = passes.join_next_with_id().await {
            observe_pass_completion::<Pass>(
                completed,
                &mut task_sessions,
                &mut in_flight_sessions,
                &self.occupancy_observer,
            );
        }
        SchedulerLoopExit::Shutdown
    }
}

fn record_pending_rerun(
    pending_reruns: &mut HashMap<SessionId, EligibilityHintPriority>,
    session: SessionId,
    priority: EligibilityHintPriority,
) {
    pending_reruns
        .entry(session)
        .and_modify(|pending| *pending = (*pending).max(priority))
        .or_insert(priority);
}

fn pass_continuation_priority(
    completed_priority: EligibilityHintPriority,
    succeeded: bool,
) -> Option<EligibilityHintPriority> {
    match (completed_priority, succeeded) {
        (EligibilityHintPriority::DispatchStart, true) => Some(EligibilityHintPriority::Ordinary),
        (EligibilityHintPriority::DispatchStart, false) => None,
        (EligibilityHintPriority::Ordinary, _) => None,
    }
}

fn enqueue_pending_hint(
    session: SessionId,
    priority: EligibilityHintPriority,
    in_flight_sessions: &HashSet<SessionId>,
    pending_reruns: &mut HashMap<SessionId, EligibilityHintPriority>,
    pending_hints: &mut HashMap<SessionId, EligibilityHintPriority>,
    pending_dispatch_starts: &mut VecDeque<SessionId>,
    pending_ordinary: &mut VecDeque<SessionId>,
) {
    if in_flight_sessions.contains(&session) {
        record_pending_rerun(pending_reruns, session, priority);
        return;
    }
    match pending_hints.get_mut(&session) {
        Some(pending) if *pending < priority => {
            *pending = priority;
            pending_dispatch_starts.push_back(session);
        }
        Some(_) => {}
        None => {
            pending_hints.insert(session, priority);
            match priority {
                EligibilityHintPriority::Ordinary => pending_ordinary.push_back(session),
                EligibilityHintPriority::DispatchStart => {
                    pending_dispatch_starts.push_back(session);
                }
            }
        }
    }
}

fn pop_pending_hint(
    queue: &mut VecDeque<SessionId>,
    priority: EligibilityHintPriority,
    pending_hints: &mut HashMap<SessionId, EligibilityHintPriority>,
) -> Option<(SessionId, EligibilityHintPriority)> {
    while let Some(session) = queue.pop_front() {
        if pending_hints.get(&session) == Some(&priority) {
            pending_hints.remove(&session);
            return Some((session, priority));
        }
    }
    None
}

const fn ordinary_pass_limit(max_in_flight_passes: usize) -> usize {
    max_in_flight_passes
}

struct PendingHintQueues<'a> {
    dispatch_starts: &'a mut VecDeque<SessionId>,
    ordinary: &'a mut VecDeque<SessionId>,
    priorities: &'a mut HashMap<SessionId, EligibilityHintPriority>,
}

#[derive(Clone, Copy)]
struct AdmissionState {
    total_in_flight: usize,
    ordinary_in_flight: usize,
    max_in_flight_passes: usize,
}

fn has_admissible_hint(
    pending_hints: &HashMap<SessionId, EligibilityHintPriority>,
    admission: AdmissionState,
) -> bool {
    admission.total_in_flight < admission.max_in_flight_passes
        && (pending_hints
            .values()
            .any(|priority| *priority == EligibilityHintPriority::DispatchStart)
            || (admission.ordinary_in_flight < ordinary_pass_limit(admission.max_in_flight_passes)
                && pending_hints
                    .values()
                    .any(|priority| *priority == EligibilityHintPriority::Ordinary)))
}

fn take_admissible_hint(
    queues: PendingHintQueues<'_>,
    admission: AdmissionState,
) -> Option<(SessionId, EligibilityHintPriority)> {
    if admission.total_in_flight == admission.max_in_flight_passes {
        return None;
    }
    if let Some(hint) = pop_pending_hint(
        queues.dispatch_starts,
        EligibilityHintPriority::DispatchStart,
        queues.priorities,
    ) {
        return Some(hint);
    }
    if admission.ordinary_in_flight == ordinary_pass_limit(admission.max_in_flight_passes) {
        return None;
    }
    pop_pending_hint(
        queues.ordinary,
        EligibilityHintPriority::Ordinary,
        queues.priorities,
    )
}

#[derive(Clone, Copy, Debug)]
struct InFlightPass {
    session: SessionId,
    priority: EligibilityHintPriority,
    started_at: Instant,
    counts_toward_occupancy: bool,
}

enum PassTaskOutcome<PassError> {
    Completed(Result<(), PassError>),
    OccupancyExpired { bound: SchedulerPassOccupancyBound },
}

type ErasedPassExecution<PassError> =
    Pin<Box<dyn Future<Output = Result<(), PassError>> + Send + 'static>>;

fn erased_pass_execution<Pass>(
    pass: &mut Pass,
    session: SessionId,
) -> ErasedPassExecution<Pass::Error>
where
    Pass: EligibilityPass,
{
    Box::pin(pass.run(session))
}

fn erased_dispatch_start_execution<Pass>(
    pass: &mut Pass,
    session: SessionId,
) -> ErasedPassExecution<Pass::Error>
where
    Pass: EligibilityPass,
{
    Box::pin(pass.run_dispatch_start(session))
}

/// Scheduler-visible queues retired by one completed pass.
struct PassCompletionState<'a> {
    task_sessions: &'a mut HashMap<Id, InFlightPass>,
    in_flight_sessions: &'a mut HashSet<SessionId>,
    ordinary_in_flight: &'a mut usize,
    pending_hints: &'a mut HashMap<SessionId, EligibilityHintPriority>,
    pending_dispatch_starts: &'a mut VecDeque<SessionId>,
    pending_ordinary: &'a mut VecDeque<SessionId>,
    pending_reruns: &'a mut HashMap<SessionId, EligibilityHintPriority>,
    deferred_dispatch_start_retries: &'a mut HashSet<SessionId>,
}

/// Retires one completed pass and requeues whatever that completion leaves owed.
///
/// Returns whether a correlated pass was actually retired, so the caller can
/// decide whether the completion is worth reconsidering admission for.
fn apply_pass_completion<Pass>(
    completed: Result<(Id, PassTaskOutcome<Pass::Error>), JoinError>,
    state: PassCompletionState<'_>,
    observer: &Option<Arc<dyn SchedulerOccupancyObserver>>,
) -> bool
where
    Pass: EligibilityPass,
    Pass::Error: ClassifyOperatorFailure,
{
    let Some(CompletedPass {
        session,
        priority,
        succeeded,
        rerun_allowed,
    }) = observe_pass_completion::<Pass>(
        completed,
        state.task_sessions,
        state.in_flight_sessions,
        observer,
    )
    else {
        return false;
    };
    if priority == EligibilityHintPriority::Ordinary {
        *state.ordinary_in_flight = state.ordinary_in_flight.saturating_sub(1);
    }
    if priority == EligibilityHintPriority::DispatchStart && !succeeded {
        state.deferred_dispatch_start_retries.insert(session);
    }
    if let Some(continuation_priority) = pass_continuation_priority(priority, succeeded) {
        enqueue_pending_hint(
            session,
            continuation_priority,
            state.in_flight_sessions,
            state.pending_reruns,
            state.pending_hints,
            state.pending_dispatch_starts,
            state.pending_ordinary,
        );
    }
    if let Some(mut rerun_priority) = state.pending_reruns.remove(&session)
        && rerun_allowed
    {
        if state.deferred_dispatch_start_retries.remove(&session) {
            rerun_priority = rerun_priority.max(EligibilityHintPriority::DispatchStart);
        }
        enqueue_pending_hint(
            session,
            rerun_priority,
            state.in_flight_sessions,
            state.pending_reruns,
            state.pending_hints,
            state.pending_dispatch_starts,
            state.pending_ordinary,
        );
    }
    true
}

#[allow(
    clippy::too_many_arguments,
    reason = "one admission carries the pass, its reserved-lane priority, and every bound the scheduler enforces over it"
)]
fn spawn_pass<Pass>(
    passes: &mut JoinSet<PassTaskOutcome<Pass::Error>>,
    pass: &mut Pass,
    session: SessionId,
    priority: EligibilityHintPriority,
    counts_toward_occupancy: bool,
    bound: SchedulerPassOccupancyBound,
    shutdown_drain: watch::Receiver<bool>,
    task_sessions: &mut HashMap<Id, InFlightPass>,
    observer: &Option<Arc<dyn SchedulerOccupancyObserver>>,
) where
    Pass: EligibilityPass + Send,
    Pass::Error: Send + 'static,
{
    let expiry_handler = pass.occupancy_expiry_handler();
    let span = session_work_span(session);
    // Heap-erasing the adapter future before composing the task keeps the
    // scheduler's deeply nested concrete adapter type off Tokio's worker
    // stack at the spawn boundary. Both admission lanes erase to the same
    // shape, so the reserved dispatch-start lane costs no extra stack.
    let execution = match priority {
        EligibilityHintPriority::Ordinary => erased_pass_execution(pass, session),
        EligibilityHintPriority::DispatchStart => erased_dispatch_start_execution(pass, session),
    };
    let task = passes.spawn(
        bounded_pass(execution, session, bound, shutdown_drain, expiry_handler).instrument(span),
    );
    task_sessions.insert(
        task.id(),
        InFlightPass {
            session,
            priority,
            started_at: Instant::now(),
            counts_toward_occupancy,
        },
    );
    observe_occupancy(observer, task_sessions);
}

/// Bounds one admitted pass's occupancy, invoking the handoff once it expires.
///
/// Expiry releases the admission slot after the synchronous handoff callback,
/// which is what makes the bound a scheduler-occupancy bound rather than a claim
/// about the turn itself. A deployment that configures no bound runs the pass to
/// completion, and so does a pass that is already draining: once the loop
/// requests drain the pass stops spending its ordinary occupancy deadline and
/// finishes under the composition root's shutdown grace window instead.
async fn bounded_pass<PassError>(
    execution: ErasedPassExecution<PassError>,
    session: SessionId,
    bound: SchedulerPassOccupancyBound,
    mut shutdown_drain: watch::Receiver<bool>,
    expiry_handler: Option<Arc<dyn SchedulerPassExpiryHandler>>,
) -> PassTaskOutcome<PassError>
where
    PassError: Send + 'static,
{
    let Some(duration) = bound.get() else {
        return PassTaskOutcome::Completed(execution.await);
    };
    pin!(execution);
    let deadline = time::sleep(duration);
    pin!(deadline);
    let drain_requested = async move {
        let _ = shutdown_drain.changed().await;
    };
    pin!(drain_requested);
    select! {
        biased;
        result = &mut execution => PassTaskOutcome::Completed(result),
        _ = &mut drain_requested => PassTaskOutcome::Completed(execution.as_mut().await),
        () = &mut deadline => {
            if let Some(handler) = expiry_handler {
                handler.occupancy_expired(session);
            }
            PassTaskOutcome::OccupancyExpired { bound }
        }
    }
}

fn observe_occupancy(
    observer: &Option<Arc<dyn SchedulerOccupancyObserver>>,
    passes: &HashMap<Id, InFlightPass>,
) {
    let Some(observer) = observer else {
        return;
    };
    let oldest = passes
        .values()
        .filter(|pass| pass.counts_toward_occupancy)
        .min_by_key(|pass| pass.started_at)
        .map(|pass| SchedulerOldestInFlightPass::new(pass.session, pass.started_at));
    let occupancy = passes
        .values()
        .filter(|pass| pass.counts_toward_occupancy)
        .count();
    observer.observe(occupancy, oldest);
}

/// One retired pass's scheduler-visible outcome.
struct CompletedPass {
    /// Session whose admission slot the pass held.
    session: SessionId,
    /// Admission lane the retired pass occupied.
    priority: EligibilityHintPriority,
    /// The pass reached a successful authoritative outcome.
    succeeded: bool,
    /// A rerun recorded for this session may be re-admitted now.
    ///
    /// Occupancy expiry hands the turn to the daemon recovery path, so the
    /// scheduler does not itself re-admit that session on this completion.
    rerun_allowed: bool,
}

/// Retires one pass's scheduler correlation and records classified failure.
///
/// Session and optional turn are daemon-minted identities; failure class,
/// cause, and stage are closed typed tokens. The pass error itself is never
/// formatted, so adapter prose and caller content cannot enter any event here.
/// The message makes no retry claim because this generic boundary covers both
/// scheduler-retryable failures and failures that stop the daemon for startup
/// recovery.
fn observe_pass_completion<Pass>(
    completed: Result<(Id, PassTaskOutcome<Pass::Error>), JoinError>,
    task_sessions: &mut HashMap<Id, InFlightPass>,
    in_flight_sessions: &mut HashSet<SessionId>,
    observer: &Option<Arc<dyn SchedulerOccupancyObserver>>,
) -> Option<CompletedPass>
where
    Pass: EligibilityPass,
    Pass::Error: ClassifyOperatorFailure,
{
    let task = match &completed {
        Ok((task, _)) => *task,
        Err(error) => error.id(),
    };
    let Some(in_flight) = task_sessions.remove(&task) else {
        tracing::error!(
            failure_class = ?crate::OperatorFailureClass::CallerOrHubBug,
            "eligibility-pass task completed without its session correlation"
        );
        return None;
    };
    let session = in_flight.session;
    let priority = in_flight.priority;
    in_flight_sessions.remove(&session);
    observe_occupancy(observer, task_sessions);

    let succeeded = match completed {
        Ok((_, PassTaskOutcome::Completed(Ok(())))) => true,
        Ok((_, PassTaskOutcome::Completed(Err(error)))) => {
            let failure_class = error.operator_failure_class();
            let cause_code = error.operator_failure_cause_code();
            let stage = Pass::failure_stage(&error);
            match Pass::failure_turn(&error) {
                Some(turn) => tracing::error!(
                    ?failure_class,
                    cause_code,
                    stage,
                    session_id = %session.as_uuid(),
                    turn_id = %turn.as_uuid(),
                    "authoritative eligibility pass failed"
                ),
                None => tracing::error!(
                    ?failure_class,
                    cause_code,
                    stage,
                    session_id = %session.as_uuid(),
                    turn_id = tracing::field::Empty,
                    "authoritative eligibility pass failed"
                ),
            };
            false
        }
        Ok((_, PassTaskOutcome::OccupancyExpired { bound })) => {
            tracing::error!(
                failure_class = ?crate::OperatorFailureClass::Infrastructure { commit_ambiguous: true },
                cause_code = "scheduler_pass_occupancy_expired",
                stage = "occupancy",
                session_id = %session.as_uuid(),
                occupancy_bound_seconds = bound.get().map(|duration| duration.as_secs()),
                "authoritative eligibility pass exceeded its occupancy bound and released its slot"
            );
            return Some(CompletedPass {
                session,
                priority,
                succeeded: false,
                rerun_allowed: false,
            });
        }
        Err(_) => {
            tracing::error!(
                failure_class = ?crate::OperatorFailureClass::CallerOrHubBug,
                cause_code = "eligibility_pass_task_terminated",
                stage = "task",
                session_id = %session.as_uuid(),
                "authoritative eligibility pass task terminated unexpectedly"
            );
            false
        }
    };
    Some(CompletedPass {
        session,
        priority,
        succeeded,
        rerun_allowed: true,
    })
}

/// Creates the root of one session's scheduler work.
///
/// The stable span name and daemon-minted session identifier let a future
/// OpenTelemetry layer preserve the same hierarchy without reshaping events.
/// No caller content or adapter-provided prose enters the span.
fn session_work_span(session: SessionId) -> tracing::Span {
    tracing::info_span!(
        "session_work",
        session_id = %session.as_uuid(),
    )
}

/// Records one reconciliation-sweep failure before the interval retry.
///
/// The event admits only the shared class and static typed cause token; it has
/// no session payload and never formats the underlying repository error.
fn log_sweep_failure<Error>(error: &Error)
where
    Error: ClassifyOperatorFailure,
{
    let failure_class = error.operator_failure_class();
    let cause_code = error.operator_failure_cause_code();
    tracing::error!(
        ?failure_class,
        cause_code,
        stage = "reconciliation_sweep",
        "eligibility reconciliation sweep failed; the next interval will retry"
    );
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::{HashMap, HashSet, VecDeque},
        fmt,
        future::{Future, pending, ready},
        io::{self, Write},
        num::NonZeroUsize,
        sync::{
            Arc, Mutex, OnceLock,
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
        AdmissionState, ClassifyOperatorFailure, EligibilityHintPriority, EligibilityNudge,
        EligibilityNudgeOutcome, EligibilityPass, EligibilitySweep, EligibilitySweepBatch,
        EligibilityWorkSource, GoalAwareEligibilityPass, GoalAwareEligibilityPassError,
        GoalPassDisposition, InProcessEligibilityWorkSource, InvalidReconciliationSweepInterval,
        MINIMUM_DISPATCH_START_BACKLOG_CAPACITY, PendingHintQueues, ReconciliationSweepInterval,
        SchedulerLoop, SchedulerLoopExit, SchedulerPassOccupancyBound, enqueue_pending_hint,
        erased_pass_execution, ordinary_pass_limit, pass_continuation_priority,
        take_admissible_hint,
    };
    use crate::{
        OperatorFailureClass, StartEligibleTurnIdGenerator, StartEligibleTurnOutcome,
        StartEligibleTurnService, StartEligibleTurnTransaction,
    };

    thread_local! {
        /// Telemetry captured on this thread alone.
        static CAPTURED_TELEMETRY: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    /// Appends every formatted event to the emitting thread's own buffer.
    #[derive(Clone, Copy, Default)]
    struct CapturedTelemetry;

    impl Write for CapturedTelemetry {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            CAPTURED_TELEMETRY.with(|captured| captured.borrow_mut().extend_from_slice(buffer));
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CapturedTelemetry {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            *self
        }
    }

    /// Installs the capturing subscriber once for the whole test process.
    ///
    /// It must be global rather than thread-scoped. `tracing` caches each
    /// callsite's interest process-wide, but `set_default` binds a subscriber
    /// to one thread, so a sibling test that reaches a callsite first on
    /// another thread registers it against no subscriber at all — recording it
    /// as uninteresting for every thread, including the one that installed a
    /// capture. The event then is not merely written late; it is never emitted,
    /// and the assertion reads an empty buffer. A global subscriber is live for
    /// whichever thread registers the callsite, so no registration can resolve
    /// to uninterested and the capture cannot be lost.
    ///
    /// Writes are routed per thread so concurrent tests never read each other's
    /// events, which keeps both the positive and the negative assertions honest.
    fn capture_telemetry_for_this_thread() {
        static INSTALLED: OnceLock<()> = OnceLock::new();

        INSTALLED.get_or_init(|| {
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(CapturedTelemetry)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other global telemetry subscriber is installed");
        });
        CAPTURED_TELEMETRY.with(|captured| captured.borrow_mut().clear());
    }

    /// Returns the telemetry captured on this thread.
    fn captured_telemetry() -> String {
        CAPTURED_TELEMETRY
            .with(|captured| String::from_utf8(captured.borrow().clone()))
            .expect("captured telemetry is UTF-8")
    }

    fn session(value: u128) -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(value))
    }

    /// The one-second reconciliation cadence these fixtures configure.
    ///
    /// The cadence is deployment configuration rather than a compiled constant,
    /// so the fixtures name their own interval instead of importing one.
    fn baseline_sweep_interval() -> ReconciliationSweepInterval {
        ReconciliationSweepInterval::try_new(Duration::from_secs(1))
            .expect("one second is a valid reconciliation interval")
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

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeGoalDispositionError;

    impl fmt::Display for FakeGoalDispositionError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("durable goal disposition failed")
        }
    }

    impl std::error::Error for FakeGoalDispositionError {}

    impl ClassifyOperatorFailure for FakeGoalDispositionError {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            OperatorFailureClass::FailClosedCorruption
        }

        fn operator_failure_cause_code(&self) -> &'static str {
            "goal_disposition_corruption"
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
        let interval = ReconciliationSweepInterval::try_new(Duration::from_secs(1))
            .expect("the test sweep interval is valid");
        let (nudge, mut source) = InProcessEligibilityWorkSource::with_interval(
            FakeSweep::returning([Ok(vec![]), Ok(vec![swept])]),
            interval,
        );

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
    async fn inv069_dispatch_start_nudges_coalesce_and_upgrade_pending_admission() {
        let selected = session(33);
        let (nudge, mut source) =
            InProcessEligibilityWorkSource::new(FakeSweep::returning([Ok(vec![])]));

        assert_eq!(nudge.nudge(selected), EligibilityNudgeOutcome::Enqueued);
        assert_eq!(
            nudge.nudge_dispatch_start(selected),
            EligibilityNudgeOutcome::Coalesced
        );
        assert_eq!(source.next().await, Ok(selected));
        assert!(source.take_returned_dispatch_start(selected));
    }

    #[tokio::test]
    async fn inv069_out_of_band_priority_take_preserves_its_channel_token() {
        let selected = session(34);
        let (nudge, mut source) =
            InProcessEligibilityWorkSource::new(FakeSweep::returning([Ok(vec![])]));

        assert_eq!(
            nudge.nudge_dispatch_start(selected),
            EligibilityNudgeOutcome::Enqueued
        );
        assert_eq!(source.take_pending_dispatch_start(), Some(selected));
        assert_eq!(source.next().await, Ok(selected));
        assert!(!source.take_returned_dispatch_start(selected));
    }

    #[tokio::test]
    async fn inv069_an_equal_nudge_does_not_consume_another_buffer_slot() {
        let first = session(33);
        let second = session(34);
        let (nudge, _source) = InProcessEligibilityWorkSource::with_options(
            FakeSweep::returning([]),
            Some(baseline_sweep_interval()),
            Some(NonZeroUsize::new(1).expect("the test capacity is nonzero")),
        );

        assert_eq!(nudge.nudge(first), EligibilityNudgeOutcome::Enqueued);
        assert_eq!(nudge.nudge(first), EligibilityNudgeOutcome::Coalesced);
        assert_eq!(
            nudge.nudge(second),
            EligibilityNudgeOutcome::DroppedAtCapacity
        );
    }

    #[tokio::test]
    async fn inv069_full_ordinary_buffer_retains_one_complete_dispatch_batch() {
        let ordinary = session(40);
        let dispatch_starts = (0..MINIMUM_DISPATCH_START_BACKLOG_CAPACITY)
            .map(|offset| session(100 + offset as u128))
            .collect::<Vec<_>>();
        let overflow = session(200);
        let (nudge, mut source) = InProcessEligibilityWorkSource::with_options(
            FakeSweep::returning([]),
            Some(baseline_sweep_interval()),
            Some(NonZeroUsize::new(1).expect("the test capacity is nonzero")),
        );

        assert_eq!(nudge.nudge(ordinary), EligibilityNudgeOutcome::Enqueued);
        let enqueue_outcomes = dispatch_starts
            .iter()
            .copied()
            .map(|dispatch_start| nudge.nudge_dispatch_start(dispatch_start))
            .collect::<Vec<_>>();
        assert_eq!(
            enqueue_outcomes,
            vec![EligibilityNudgeOutcome::Enqueued; dispatch_starts.len()]
        );
        assert_eq!(
            nudge.nudge_dispatch_start(overflow),
            EligibilityNudgeOutcome::DroppedAtCapacity
        );

        let retained =
            std::iter::from_fn(|| source.take_pending_dispatch_start()).collect::<HashSet<_>>();
        assert_eq!(
            retained,
            dispatch_starts.into_iter().collect::<HashSet<_>>()
        );
        assert!(!retained.contains(&overflow));
    }

    #[test]
    fn inv069_failed_dispatch_start_pass_waits_for_a_later_hint() {
        assert_eq!(
            pass_continuation_priority(EligibilityHintPriority::DispatchStart, false),
            None
        );
        assert_eq!(
            pass_continuation_priority(EligibilityHintPriority::DispatchStart, true),
            Some(EligibilityHintPriority::Ordinary)
        );
        assert_eq!(
            pass_continuation_priority(EligibilityHintPriority::Ordinary, false),
            None
        );
    }

    #[tokio::test]
    async fn inv069_dropping_a_source_exposes_closed_instead_of_stale_coalescing() {
        let selected = session(35);
        let (nudge, source) = InProcessEligibilityWorkSource::new(FakeSweep::returning([]));

        assert_eq!(
            nudge.nudge_dispatch_start(selected),
            EligibilityNudgeOutcome::Enqueued
        );
        drop(source);
        assert_eq!(
            nudge.nudge_dispatch_start(selected),
            EligibilityNudgeOutcome::WorkSourceClosed
        );
    }

    #[tokio::test]
    async fn inv007_full_nudge_buffer_drops_only_the_hint() {
        let first = session(33);
        let second = session(34);
        let (nudge, _source) = InProcessEligibilityWorkSource::with_options(
            FakeSweep::returning([]),
            Some(
                ReconciliationSweepInterval::try_new(Duration::from_secs(2))
                    .expect("the test interval is valid"),
            ),
            Some(NonZeroUsize::new(1).expect("the test capacity is nonzero")),
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
    async fn scheduler_heap_erases_pass_execution_before_task_construction() {
        let admitted = session(57);
        let (shutdown, _shutdown_receiver) = oneshot::channel();
        let mut pass = FakePass::failing_once(session(58), 1, shutdown);

        assert_eq!(erased_pass_execution(&mut pass, admitted).await, Ok(()));
    }

    #[derive(Clone, Copy, Debug)]
    struct GoalFixturePass {
        result: Result<(), FakeSweepError>,
    }

    impl EligibilityPass for GoalFixturePass {
        type Error = FakeSweepError;

        fn failure_stage(_error: &Self::Error) -> &'static str {
            "execution"
        }

        fn failure_turn(_error: &Self::Error) -> Option<signalbox_domain::TurnId> {
            Some(signalbox_domain::TurnId::from_uuid(Uuid::from_u128(52)))
        }

        fn run(
            &mut self,
            _session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            ready(self.result)
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum GoalDispositionCall {
        Reconcile(SessionId),
        Block(SessionId, signalbox_domain::TurnId),
    }

    #[derive(Clone, Debug, Default)]
    struct RecordingGoalDisposition {
        calls: Arc<Mutex<Vec<GoalDispositionCall>>>,
    }

    impl GoalPassDisposition for RecordingGoalDisposition {
        type Error = FakeSweepError;

        fn reconcile_success(
            &self,
            session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            self.calls
                .lock()
                .expect("goal disposition calls are available")
                .push(GoalDispositionCall::Reconcile(session));
            ready(Ok(()))
        }

        fn block_execution_failure(
            &self,
            session: SessionId,
            turn: signalbox_domain::TurnId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            self.calls
                .lock()
                .expect("goal disposition calls are available")
                .push(GoalDispositionCall::Block(session, turn));
            ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn inv048_successful_pass_reconciles_goal_continuation_once() {
        let selected_session = session(51);
        let disposition = RecordingGoalDisposition::default();
        let calls = Arc::clone(&disposition.calls);
        let mut pass =
            GoalAwareEligibilityPass::new(GoalFixturePass { result: Ok(()) }, disposition);

        assert_eq!(pass.run(selected_session).await, Ok(()));
        assert_eq!(
            *calls.lock().expect("goal disposition calls are available"),
            vec![GoalDispositionCall::Reconcile(selected_session)]
        );
    }

    #[tokio::test]
    async fn inv048_failed_selected_turn_blocks_goal_without_retrying_the_pass() {
        let selected_session = session(51);
        let selected_turn = signalbox_domain::TurnId::from_uuid(Uuid::from_u128(52));
        let disposition = RecordingGoalDisposition::default();
        let calls = Arc::clone(&disposition.calls);
        let mut pass = GoalAwareEligibilityPass::new(
            GoalFixturePass {
                result: Err(FakeSweepError::Unavailable),
            },
            disposition,
        );

        assert_eq!(
            pass.run(selected_session).await,
            Err(GoalAwareEligibilityPassError::Pass {
                source: FakeSweepError::Unavailable,
                blocking: None,
            })
        );
        assert_eq!(
            *calls.lock().expect("goal disposition calls are available"),
            vec![GoalDispositionCall::Block(selected_session, selected_turn)]
        );
    }

    #[test]
    fn goal_pass_error_preserves_secondary_blocking_evidence() {
        let error = GoalAwareEligibilityPassError::Pass {
            source: "authoritative pass failed",
            blocking: Some("durable block failed"),
        };

        assert_eq!(
            error.to_string(),
            "authoritative pass failed; goal execution-failure blocking also failed: durable block failed"
        );
    }

    #[test]
    fn goal_pass_error_reports_secondary_blocking_classification() {
        let blocking = FakeGoalDispositionError;
        let expected_class = blocking.operator_failure_class();
        let expected_cause_code = blocking.operator_failure_cause_code();
        let error = GoalAwareEligibilityPassError::Pass {
            source: FakeSweepError::Unavailable,
            blocking: Some(blocking),
        };

        assert_eq!(error.operator_failure_class(), expected_class);
        assert_eq!(error.operator_failure_cause_code(), expected_cause_code);
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
    struct OccupancyExpiryPass {
        started: Arc<Notify>,
        expired: Arc<Notify>,
        expiration_count: Arc<AtomicUsize>,
        run_count: Arc<AtomicUsize>,
    }

    impl super::SchedulerPassExpiryHandler for OccupancyExpiryPass {
        fn occupancy_expired(&self, _session: SessionId) {
            self.expiration_count.fetch_add(1, Ordering::SeqCst);
            self.expired.notify_one();
        }
    }

    impl EligibilityPass for OccupancyExpiryPass {
        type Error = FakeSweepError;

        fn occupancy_expiry_handler(&self) -> Option<Arc<dyn super::SchedulerPassExpiryHandler>> {
            Some(Arc::new(self.clone()))
        }

        fn run(
            &mut self,
            _session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            let started = Arc::clone(&self.started);
            let run_count = Arc::clone(&self.run_count);
            async move {
                run_count.fetch_add(1, Ordering::SeqCst);
                started.notify_one();
                pending().await
            }
        }
    }

    /// S10 / INV-007: a provider future that never returns cannot retain a
    /// scheduler admission slot past the compiled-or-lowered occupancy bound.
    #[tokio::test(start_paused = true)]
    async fn inv007_scheduler_expires_a_stalled_pass_and_calls_recovery() {
        capture_telemetry_for_this_thread();
        let selected = session(51);
        let started = Arc::new(Notify::new());
        let expired = Arc::new(Notify::new());
        let expiration_count = Arc::new(AtomicUsize::new(0));
        let run_count = Arc::new(AtomicUsize::new(0));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let bound = SchedulerPassOccupancyBound::try_new(Duration::from_secs(1))
            .expect("one second lowers the production ceiling");
        let scheduler = SchedulerLoop::new(
            FakeWorkSource {
                hints: VecDeque::from([Ok(selected), Ok(selected)]),
            },
            OccupancyExpiryPass {
                started: Arc::clone(&started),
                expired: Arc::clone(&expired),
                expiration_count: Arc::clone(&expiration_count),
                run_count: Arc::clone(&run_count),
            },
        )
        .with_occupancy_bound(bound);
        let runtime = tokio::spawn(async move {
            let mut scheduler = scheduler;
            scheduler
                .run_until(async {
                    shutdown_receiver.await.expect("the test requests shutdown");
                })
                .await
        });

        started.notified().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        expired.notified().await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(run_count.load(Ordering::SeqCst), 1);
        shutdown_sender
            .send(())
            .expect("the scheduler still listens for shutdown");
        let exit = runtime.await.expect("scheduler task completes");
        let encoded = captured_telemetry();

        assert_eq!(expiration_count.load(Ordering::SeqCst), 1);
        assert_eq!(exit, SchedulerLoopExit::Shutdown);
        assert!(encoded.contains("scheduler_pass_occupancy_expired"));
        assert!(encoded.contains("occupancy_bound_seconds=1"));
    }

    #[derive(Clone, Debug)]
    struct ShutdownDrainPass {
        started: Arc<Notify>,
        release: Arc<Notify>,
        expiration_count: Arc<AtomicUsize>,
    }

    impl super::SchedulerPassExpiryHandler for ShutdownDrainPass {
        fn occupancy_expired(&self, _session: SessionId) {
            self.expiration_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl EligibilityPass for ShutdownDrainPass {
        type Error = FakeSweepError;

        fn occupancy_expiry_handler(&self) -> Option<Arc<dyn super::SchedulerPassExpiryHandler>> {
            Some(Arc::new(self.clone()))
        }

        fn run(
            &mut self,
            _session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            let started = Arc::clone(&self.started);
            let release = Arc::clone(&self.release);
            async move {
                started.notify_one();
                release.notified().await;
                Ok(())
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_drain_suspends_the_admitted_pass_occupancy_deadline() {
        let selected = session(52);
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let shutdown_observed = Arc::new(Notify::new());
        let expiration_count = Arc::new(AtomicUsize::new(0));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let bound = SchedulerPassOccupancyBound::try_new(Duration::from_secs(1))
            .expect("one second is a valid configured bound");
        let scheduler = SchedulerLoop::new(
            FakeWorkSource {
                hints: VecDeque::from([Ok(selected)]),
            },
            ShutdownDrainPass {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
                expiration_count: Arc::clone(&expiration_count),
            },
        )
        .with_occupancy_bound(bound);
        let observed = Arc::clone(&shutdown_observed);
        let runtime = tokio::spawn(async move {
            let mut scheduler = scheduler;
            scheduler
                .run_until(async {
                    shutdown_receiver.await.expect("the test requests shutdown");
                    observed.notify_one();
                })
                .await
        });

        started.notified().await;
        shutdown_sender
            .send(())
            .expect("the scheduler still listens for shutdown");
        shutdown_observed.notified().await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;

        assert_eq!(expiration_count.load(Ordering::SeqCst), 0);

        release.notify_one();
        assert_eq!(
            runtime.await.expect("scheduler task completes"),
            SchedulerLoopExit::Shutdown
        );
    }

    #[tokio::test(start_paused = true)]
    async fn inv007_paused_scheduler_admits_no_authoritative_passes() {
        let selected = session(50);
        let (unused_pass_shutdown, pass_shutdown_receiver) = oneshot::channel();
        let pass = FakePass::failing_once(selected, 1, unused_pass_shutdown);
        let observed = Arc::clone(&pass.state);
        let mut scheduler = SchedulerLoop::paused(
            FakeWorkSource {
                hints: VecDeque::from([Ok(selected)]),
            },
            pass,
        );
        let outcome = timeout(
            Duration::from_secs(1),
            scheduler.run_until(async {
                pass_shutdown_receiver
                    .await
                    .expect("an admitted fake pass signals shutdown");
            }),
        )
        .await;

        assert!(outcome.is_err());
        assert!(
            observed
                .lock()
                .expect("fake-pass state is not poisoned")
                .observed
                .is_empty()
        );
    }

    /// One deployment-configured admission cap, standing in for a live bound.
    ///
    /// The cap is deployment configuration rather than a compiled constant, so
    /// the admission fixtures name their own value instead of importing one.
    const FIXTURE_PASS_ADMISSION_CAP: usize = 16;

    #[test]
    fn dispatch_start_admission_precedes_ordinary_work_with_available_capacity() {
        let ordinary = session(48);
        let dispatch_start = session(49);
        let mut in_flight = HashSet::from_iter([ordinary]);
        let mut reruns = HashMap::new();
        let mut pending = HashMap::new();
        let mut dispatch_starts = VecDeque::new();
        let mut ordinary_hints = VecDeque::new();
        enqueue_pending_hint(
            dispatch_start,
            EligibilityHintPriority::DispatchStart,
            &in_flight,
            &mut reruns,
            &mut pending,
            &mut dispatch_starts,
            &mut ordinary_hints,
        );
        let filler_count = FIXTURE_PASS_ADMISSION_CAP - 2;
        in_flight.extend((0..filler_count).map(|offset| session(100 + offset as u128)));

        assert_eq!(
            take_admissible_hint(
                PendingHintQueues {
                    dispatch_starts: &mut dispatch_starts,
                    ordinary: &mut ordinary_hints,
                    priorities: &mut pending,
                },
                AdmissionState {
                    total_in_flight: in_flight.len(),
                    ordinary_in_flight: in_flight.len(),
                    max_in_flight_passes: FIXTURE_PASS_ADMISSION_CAP,
                },
            ),
            Some((dispatch_start, EligibilityHintPriority::DispatchStart))
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn ordinary_admission_uses_the_last_available_capacity() {
        let ordinary = session(50);
        let in_flight = HashSet::from_iter(
            (0..ordinary_pass_limit(FIXTURE_PASS_ADMISSION_CAP) - 1)
                .map(|offset| session(200 + offset as u128)),
        );
        let mut reruns = HashMap::new();
        let mut pending = HashMap::new();
        let mut dispatch_starts = VecDeque::new();
        let mut ordinary_hints = VecDeque::new();
        enqueue_pending_hint(
            ordinary,
            EligibilityHintPriority::Ordinary,
            &in_flight,
            &mut reruns,
            &mut pending,
            &mut dispatch_starts,
            &mut ordinary_hints,
        );

        assert_eq!(
            take_admissible_hint(
                PendingHintQueues {
                    dispatch_starts: &mut dispatch_starts,
                    ordinary: &mut ordinary_hints,
                    priorities: &mut pending,
                },
                AdmissionState {
                    total_in_flight: in_flight.len(),
                    ordinary_in_flight: in_flight.len(),
                    max_in_flight_passes: FIXTURE_PASS_ADMISSION_CAP,
                },
            ),
            Some((ordinary, EligibilityHintPriority::Ordinary))
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn inv007_explicit_scheduler_bound_is_used_exactly() {
        let requested = NonZeroUsize::new(19).expect("the fixture bound is positive");
        let scheduler = SchedulerLoop::with_max_in_flight((), (), requested);

        assert_eq!(scheduler.max_in_flight_passes, requested.get());
    }

    #[test]
    fn scheduler_occupancy_bound_accepts_configured_whole_seconds() {
        let configured = SchedulerPassOccupancyBound::try_new(Duration::from_secs(60))
            .expect("one minute is a valid configured bound");

        assert_eq!(configured.get(), Some(Duration::from_secs(60)));
        assert_eq!(SchedulerPassOccupancyBound::unbounded().get(), None);
        assert_eq!(
            SchedulerPassOccupancyBound::try_new(Duration::ZERO),
            Err(super::InvalidSchedulerPassOccupancyBound)
        );
        assert_eq!(
            SchedulerPassOccupancyBound::try_new(Duration::from_secs(901))
                .map(SchedulerPassOccupancyBound::get),
            Ok(Some(Duration::from_secs(901)))
        );
        assert_eq!(
            SchedulerPassOccupancyBound::try_new(Duration::from_millis(500)),
            Err(super::InvalidSchedulerPassOccupancyBound)
        );
    }

    #[tokio::test]
    async fn failed_pass_event_does_not_promise_scheduler_retry() {
        capture_telemetry_for_this_thread();
        let failing_session = session(7);
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let pass = FakePass::failing_once(failing_session, 1, shutdown_sender);
        let mut scheduler = SchedulerLoop::new(
            FakeWorkSource {
                hints: VecDeque::from([Ok(failing_session)]),
            },
            pass,
        );

        let exit = scheduler
            .run_until(async {
                shutdown_receiver
                    .await
                    .expect("fake pass requests shutdown after its failure");
            })
            .await;
        let encoded = captured_telemetry();

        assert_eq!(exit, SchedulerLoopExit::Shutdown);
        assert!(encoded.contains("authoritative eligibility pass failed"));
        assert!(!encoded.contains("a later nudge or sweep will retry"));
    }

    #[derive(Debug, Clone)]
    struct StatefulActivationIds {
        next: u128,
    }

    impl StartEligibleTurnIdGenerator for StatefulActivationIds {
        fn next_model_identity_entry_id(&mut self) -> SemanticTranscriptEntryId {
            let id = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(self.next));
            self.next += 1;
            id
        }

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
    async fn inv069_dispatch_start_wakes_when_the_ordinary_nudge_buffer_is_full() {
        let ordinary = session(53);
        let dispatch_start = session(54);
        let (nudge, mut source) = InProcessEligibilityWorkSource::with_options(
            FakeSweep::returning([Ok(vec![])]),
            Some(baseline_sweep_interval()),
            Some(NonZeroUsize::new(1).expect("the test nudge buffer is nonzero")),
        );

        assert_eq!(nudge.nudge(ordinary), EligibilityNudgeOutcome::Enqueued);
        assert_eq!(
            nudge.nudge_dispatch_start(dispatch_start),
            EligibilityNudgeOutcome::Enqueued
        );
        assert_eq!(
            timeout(Duration::from_secs(1), source.next_pending_dispatch_start())
                .await
                .expect("the priority-only notification wakes promptly"),
            Ok(dispatch_start)
        );
        assert_eq!(source.next().await, Ok(ordinary));
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
