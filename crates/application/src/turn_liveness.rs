//! Turn-level liveness: deciding when an active turn is boundedly stale.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md owns the active slot and the
//! terminal transitions this module's decision feeds. Component deadlines
//! cover each physical operation; nothing else covers a turn that holds the
//! slot while no operation is outstanding at all. This module owns only the
//! decision. Reading durable evidence and committing the failed-turn
//! transition belong to the persistence adapter, and the periodic pass that
//! drives both belongs to the daemon runtime.

use std::{collections::HashMap, error::Error, fmt, num::NonZeroU32, time::Duration};

use signalbox_domain::{ModelCallId, SessionId, ToolAttemptId, TurnAttemptId, TurnId};
use tokio::time::Instant;

/// One durable automatic reconciliation attempt.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AutomaticReconciliationAttempt(NonZeroU32);

impl AutomaticReconciliationAttempt {
    /// Returns the first attempt.
    pub const fn first() -> Self {
        Self(NonZeroU32::MIN)
    }

    /// Reconstitutes a structurally valid attempt ordinal.
    pub const fn try_from_u32(value: u32) -> Option<Self> {
        match NonZeroU32::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the one-based attempt ordinal.
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// Returns the configured delay after this failed attempt before another is due.
    pub fn retry_backoff(self, base: Duration, cap: Option<Duration>) -> Duration {
        let backoff = base.saturating_mul(2_u32.saturating_pow(self.get() - 1));
        cap.map_or(backoff, |cap| backoff.min(cap))
    }

    /// Returns the next structurally representable attempt.
    pub const fn next(self) -> Option<Self> {
        Self::try_from_u32(self.get().saturating_add(1))
    }

    /// Reports whether this attempt is admitted by the deployment budget.
    pub const fn is_within_budget(self, budget: Option<u32>) -> bool {
        match budget {
            Some(budget) => self.get() <= budget,
            None => true,
        }
    }
}

/// The exact physical operation whose ambiguity owns a durable wait.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AutomaticReconciliationOperation {
    /// One physical provider call.
    ModelCall(ModelCallId),
    /// One physical tool attempt.
    ToolAttempt(ToolAttemptId),
}

/// One claimed durable ambiguity reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClaimedAutomaticReconciliation {
    session: SessionId,
    turn: TurnId,
    operation: AutomaticReconciliationOperation,
    attempt: AutomaticReconciliationAttempt,
}

/// One ambiguity whose automatic attempt budget has just been exhausted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExhaustedAutomaticReconciliation {
    session: SessionId,
    turn: TurnId,
    operation: AutomaticReconciliationOperation,
}

impl ExhaustedAutomaticReconciliation {
    /// Records the exact exhausted wait.
    pub const fn new(
        session: SessionId,
        turn: TurnId,
        operation: AutomaticReconciliationOperation,
    ) -> Self {
        Self {
            session,
            turn,
            operation,
        }
    }

    /// Returns the owning session.
    pub const fn session(self) -> SessionId {
        self.session
    }

    /// Returns the still-active turn.
    pub const fn turn(self) -> TurnId {
        self.turn
    }

    /// Returns the exact ambiguous operation.
    pub const fn operation(self) -> AutomaticReconciliationOperation {
        self.operation
    }
}

/// One bounded discovery pass's claimed attempts and newly exhausted waits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomaticReconciliationBatch {
    claimed: Box<[ClaimedAutomaticReconciliation]>,
    exhausted: Box<[ExhaustedAutomaticReconciliation]>,
}

impl AutomaticReconciliationBatch {
    /// Combines the durable outcomes of one discovery transaction.
    pub fn new(
        claimed: Box<[ClaimedAutomaticReconciliation]>,
        exhausted: Box<[ExhaustedAutomaticReconciliation]>,
    ) -> Self {
        Self { claimed, exhausted }
    }

    /// Borrows claimed attempts.
    pub fn claimed(&self) -> &[ClaimedAutomaticReconciliation] {
        &self.claimed
    }

    /// Borrows waits newly parked for operator action.
    pub fn exhausted(&self) -> &[ExhaustedAutomaticReconciliation] {
        &self.exhausted
    }
}

/// Closed durable failure class for one automatic reconciliation attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomaticReconciliationFailureKind {
    /// PostgreSQL prevented the attempt from reaching a durable decision.
    Infrastructure,
    /// Durable rows could not reconstruct the expected ambiguity exactly.
    Integrity,
}

/// Durable result of applying one claimed automatic reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomaticReconciliationOutcome {
    /// The exact ambiguous wait terminalized as reconciliation-required.
    Reconciled,
    /// Another authoritative transition changed or ended the wait first.
    Superseded,
}

impl AutomaticReconciliationFailureKind {
    /// Returns the closed storage token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Infrastructure => "infrastructure_failure",
            Self::Integrity => "integrity_failure",
        }
    }
}

impl ClaimedAutomaticReconciliation {
    /// Reconstitutes one repository-claimed attempt.
    pub const fn new(
        session: SessionId,
        turn: TurnId,
        operation: AutomaticReconciliationOperation,
        attempt: AutomaticReconciliationAttempt,
    ) -> Self {
        Self {
            session,
            turn,
            operation,
            attempt,
        }
    }

    /// Returns the owning session.
    pub const fn session(self) -> SessionId {
        self.session
    }

    /// Returns the active turn.
    pub const fn turn(self) -> TurnId {
        self.turn
    }

    /// Returns the exact ambiguous operation.
    pub const fn operation(self) -> AutomaticReconciliationOperation {
        self.operation
    }

    /// Returns the durable attempt ordinal.
    pub const fn attempt(self) -> AutomaticReconciliationAttempt {
        self.attempt
    }
}

/// The staleness bound after which a quiescent active turn is terminalized.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct StaleActiveTurnBound(Duration);

impl StaleActiveTurnBound {
    /// Accepts a configured nonzero whole-second bound.
    pub fn try_new(bound: Duration) -> Result<Self, TurnLivenessBoundError> {
        if bound.is_zero() {
            Err(TurnLivenessBoundError::Zero)
        } else if bound.subsec_nanos() != 0 {
            Err(TurnLivenessBoundError::Subsecond)
        } else {
            Ok(Self(bound))
        }
    }

    /// Returns the validated bound in whole seconds, losing nothing.
    pub const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }

    /// Returns the validated duration.
    pub const fn get(self) -> Duration {
        self.0
    }
}

/// The cadence at which turn liveness is reconsidered.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TurnLivenessScanInterval(Duration);

impl TurnLivenessScanInterval {
    /// Accepts a configured nonzero timer-representable interval.
    pub fn try_new(interval: Duration) -> Result<Self, TurnLivenessBoundError> {
        if interval.is_zero() {
            Err(TurnLivenessBoundError::Zero)
        } else if interval.subsec_nanos() != 0 {
            Err(TurnLivenessBoundError::Subsecond)
        } else if Instant::now().checked_add(interval).is_none() {
            Err(TurnLivenessBoundError::TimerRange)
        } else {
            Ok(Self(interval))
        }
    }

    /// Returns the validated duration.
    pub const fn get(self) -> Duration {
        self.0
    }
}

/// Why a proposed turn-liveness bound was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnLivenessBoundError {
    /// A zero bound would terminalize a turn the moment it was first observed.
    Zero,
    /// The proposal carries precision finer than a whole second.
    Subsecond,
    /// The proposal cannot be represented by the runtime timer.
    TimerRange,
}

impl fmt::Display for TurnLivenessBoundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::Zero => "must be nonzero",
            Self::Subsecond => "must be a whole number of seconds",
            Self::TimerRange => "does not fit the runtime timer range",
        };
        write!(formatter, "turn-liveness staleness bound {reason}")
    }
}

impl Error for TurnLivenessBoundError {}

/// The durable evidence whose change proves a turn is still progressing.
///
/// The lifecycle tables carry no activity timestamp, so "the latest model call
/// and the latest transcript entry are both older than the bound" is decided by
/// observing this evidence unchanged across the bound rather than by reading a
/// stored clock.
///
/// Progress is read from the session's *turn-progress* outbox frontier. A
/// durable transition of a turn — a model call changing state, a tool batch
/// moving, a turn activating or completing — appends an outbox event carrying a
/// sequence assigned in commit order, and the greatest such sequence is this
/// observation.
///
/// Not every outbox event counts, and a caller constructing this value must
/// apply the same exclusion the adapter does. Accepting queued input, resolving
/// a turn's model settings, replacing session defaults, retiring a goal turn,
/// creating a session, and a runner state transition all advance a session's
/// unfiltered frontier while its active turn does nothing — so an observation
/// built from that frontier would let a user hold a wedged turn open
/// indefinitely by submitting input. The adapter reads the filtered frontier
/// from a partial index defined on exactly those exclusions.
///
/// That sequence is what makes the comparison sound. It is minted by the
/// outbox, not derived from a clock, so no adjustment of the system clock and
/// no skew between processes can produce an append that leaves the frontier
/// unchanged — which would read as silence on a turn that had in fact
/// progressed, and is the one error this pass must never make.
///
/// Both members are also chosen so that reading them costs the same whatever a
/// session's history weighs: the attempt is a column of the lifecycle row
/// already being read, and the frontier is one backward index lookup on
/// `outbox_event (session_id, event_sequence)`. Aggregates over a session's
/// history would have made a once-a-minute pass cost more the longer a session
/// lived, which is the opposite of what a watchdog may do.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TurnLivenessEvidence {
    current_attempt: TurnAttemptId,
    outbox_frontier: Option<u64>,
}

impl TurnLivenessEvidence {
    /// Records one complete observation of a session's progress evidence.
    pub const fn new(current_attempt: TurnAttemptId, outbox_frontier: Option<u64>) -> Self {
        Self {
            current_attempt,
            outbox_frontier,
        }
    }

    /// Returns the attempt holding the turn's physical tenure.
    pub const fn current_attempt(self) -> TurnAttemptId {
        self.current_attempt
    }

    /// Returns the session's newest outbox sequence, if it has emitted one.
    pub const fn outbox_frontier(self) -> Option<u64> {
        self.outbox_frontier
    }
}

/// One active turn whose durable shape shows no work in flight.
///
/// Producing this value is the adapter's assertion that every in-flight
/// conjunct was checked and none held. Staleness is not part of it: the
/// ledger decides that from repeated observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StaleTurnCandidate {
    session: SessionId,
    turn: TurnId,
    evidence: TurnLivenessEvidence,
}

impl StaleTurnCandidate {
    /// Records one quiescent active turn and the evidence observed with it.
    pub const fn new(session: SessionId, turn: TurnId, evidence: TurnLivenessEvidence) -> Self {
        Self {
            session,
            turn,
            evidence,
        }
    }

    /// Returns the owning session.
    pub const fn session(self) -> SessionId {
        self.session
    }

    /// Returns the active turn holding the session's progressing slot.
    pub const fn turn(self) -> TurnId {
        self.turn
    }

    /// Returns the evidence observed with this candidate.
    pub const fn evidence(self) -> TurnLivenessEvidence {
        self.evidence
    }
}

/// What one attempted stale-turn terminalization committed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StaleTurnOutcome {
    /// The turn terminalized as failed and released the session's slot.
    Terminalized,
    /// Durable state no longer matched the observation, so nothing changed.
    Superseded,
    /// Steering is pending on the turn, which no present transition can end.
    ///
    /// Terminalizing a turn requires closing every steering input pending on
    /// it, and the failed-turn transition this pass reuses cannot do that. The
    /// turn stays wedged, so this outcome exists to report it by identity
    /// rather than let it pass as an ordinary race — once per lap of the
    /// terminalization window, which for a large population is far apart.
    BlockedByPendingSteering,
}

/// Remembers how long each quiescent turn has stood still.
///
/// The ledger is in-process state with no authority: losing it costs one more
/// staleness bound before a wedged turn is reached, which is the safe
/// direction. Nothing here can terminalize a turn that a scan did not first
/// report as quiescent.
#[derive(Clone, Debug)]
pub struct TurnLivenessLedger {
    bound: StaleActiveTurnBound,
    quiescent: HashMap<TurnId, QuiescentObservation>,
}

#[derive(Clone, Copy, Debug)]
struct QuiescentObservation {
    evidence: TurnLivenessEvidence,
    since: Instant,
}

impl TurnLivenessLedger {
    /// Starts with no observed turn, deciding staleness by the given bound.
    ///
    /// The bound is supplied rather than reloaded from the ceiling so a
    /// deployment that validated a shorter one through
    /// [`StaleActiveTurnBound::try_new`] actually gets it.
    pub fn new(bound: StaleActiveTurnBound) -> Self {
        Self {
            bound,
            quiescent: HashMap::new(),
        }
    }

    /// Returns the staleness bound this ledger decides by.
    pub const fn bound(&self) -> StaleActiveTurnBound {
        self.bound
    }

    /// Returns how many turns are currently being watched for staleness.
    pub fn watched_turn_count(&self) -> usize {
        self.quiescent.len()
    }

    /// Folds one complete observation into the ledger and returns what is due.
    ///
    /// `quiescent` is the whole quiescent population, not a page of it: the
    /// caller drains its rotation before reconciling, because a ledger fed
    /// pages could neither forget a departed turn nor accumulate a bound for a
    /// turn outside the current page. A turn whose evidence differs from the
    /// last observation restarts its bound — progress must reset the clock —
    /// and a turn absent from this scan is forgotten, since it left the
    /// quiescent shape and must carry no credit into a later quiescent period.
    pub fn reconcile(
        &mut self,
        quiescent: &[StaleTurnCandidate],
        now: Instant,
    ) -> Box<[StaleTurnCandidate]> {
        let bound = self.bound.get();
        let mut retained = HashMap::with_capacity(quiescent.len());
        let mut due = Vec::new();
        for candidate in quiescent {
            let evidence = candidate.evidence();
            let since = match self.quiescent.get(&candidate.turn()) {
                Some(previous) if previous.evidence == evidence => previous.since,
                Some(_) | None => now,
            };
            retained.insert(candidate.turn(), QuiescentObservation { evidence, since });
            if now.saturating_duration_since(since) >= bound {
                due.push(*candidate);
            }
        }
        self.quiescent = retained;
        due.into_boxed_slice()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AutomaticReconciliationAttempt, StaleActiveTurnBound, StaleTurnCandidate,
        TurnLivenessBoundError, TurnLivenessEvidence, TurnLivenessLedger, TurnLivenessScanInterval,
    };
    use signalbox_domain::{SessionId, TurnAttemptId, TurnId};
    use std::time::Duration;
    use tokio::time::Instant;

    const BOUND: Duration = Duration::from_secs(1_800);

    fn session() -> SessionId {
        SessionId::from_uuid(uuid::Uuid::from_u128(0x5e_5510))
    }

    fn turn(index: u128) -> TurnId {
        TurnId::from_uuid(uuid::Uuid::from_u128(0x7a_0000 + index))
    }

    fn attempt() -> TurnAttemptId {
        TurnAttemptId::from_uuid(uuid::Uuid::from_u128(0xa7_0001))
    }

    fn evidence(frontier: u64) -> TurnLivenessEvidence {
        TurnLivenessEvidence::new(attempt(), Some(frontier))
    }

    fn candidate(frontier: u64) -> StaleTurnCandidate {
        StaleTurnCandidate::new(session(), turn(1), evidence(frontier))
    }

    fn other_candidate() -> StaleTurnCandidate {
        StaleTurnCandidate::new(session(), turn(2), evidence(1))
    }

    fn ledger() -> TurnLivenessLedger {
        TurnLivenessLedger::new(
            StaleActiveTurnBound::try_new(BOUND).expect("fixture bound is valid"),
        )
    }

    /// The deployment may select any positive whole-second staleness bound.
    #[test]
    fn the_staleness_bound_accepts_configured_whole_seconds() {
        let shorter = Duration::from_secs(60);
        let longer = Duration::from_secs(60 * 60);
        let configured_shorter = StaleActiveTurnBound::try_new(shorter);
        let configured_longer = StaleActiveTurnBound::try_new(longer);
        let zero = StaleActiveTurnBound::try_new(Duration::ZERO);

        assert_eq!(
            configured_shorter.map(StaleActiveTurnBound::get),
            Ok(shorter)
        );
        assert_eq!(configured_longer.map(StaleActiveTurnBound::get), Ok(longer));
        assert_eq!(zero, Err(TurnLivenessBoundError::Zero));
    }

    /// A bound the audit record could not report exactly is refused outright,
    /// so the reported bound and the bound in force can never disagree.
    #[test]
    fn a_subsecond_bound_is_refused_rather_than_truncated() {
        let millis = StaleActiveTurnBound::try_new(Duration::from_millis(500));
        let fractional = StaleActiveTurnBound::try_new(Duration::from_millis(1_500));
        let whole = StaleActiveTurnBound::try_new(Duration::from_secs(1));

        assert_eq!(millis, Err(TurnLivenessBoundError::Subsecond));
        assert_eq!(fractional, Err(TurnLivenessBoundError::Subsecond));
        assert_eq!(whole.map(StaleActiveTurnBound::as_secs), Ok(1));
    }

    /// The scan interval uses the same explicit deployment validation.
    #[test]
    fn the_scan_interval_accepts_a_configured_duration() {
        assert_eq!(
            TurnLivenessScanInterval::try_new(Duration::from_secs(60))
                .map(TurnLivenessScanInterval::get),
            Ok(Duration::from_secs(60))
        );
    }

    /// Automatic reconciliation applies the supplied budget and backoff policy.
    #[test]
    fn ambiguous_model_call_reconciliation_uses_configured_retry_policy() {
        let base = Duration::from_secs(120);
        let cap = Some(Duration::from_secs(1_800));
        let first = AutomaticReconciliationAttempt::first();
        let second = first.next().expect("the second attempt is admitted");
        let third = second.next().expect("the third attempt is admitted");
        let fourth = third.next().expect("the fourth attempt is admitted");
        let fifth = fourth.next().expect("the fifth attempt is admitted");

        assert!(fifth.is_within_budget(Some(5)));
        assert!(
            !fifth
                .next()
                .expect("sixth is representable")
                .is_within_budget(Some(5))
        );
        assert_eq!(first.retry_backoff(base, cap), Duration::from_secs(120));
        assert_eq!(second.retry_backoff(base, cap), Duration::from_secs(240));
        assert_eq!(third.retry_backoff(base, cap), Duration::from_secs(480));
        assert_eq!(fourth.retry_backoff(base, cap), Duration::from_secs(960));
        assert_eq!(fifth.retry_backoff(base, cap), Duration::from_secs(1_800));
        assert_eq!(AutomaticReconciliationAttempt::try_from_u32(0), None);
        assert_eq!(
            AutomaticReconciliationAttempt::try_from_u32(6)
                .map(AutomaticReconciliationAttempt::get),
            Some(6)
        );
    }

    /// A turn seen for the first time is never due, however long the daemon
    /// has been running: the bound is measured from observation, not startup.
    #[tokio::test(start_paused = true)]
    async fn a_first_observation_is_never_due() {
        tokio::time::advance(Duration::from_secs(60 * 60)).await;
        let mut ledger = ledger();

        let due = ledger.reconcile(&[candidate(1)], Instant::now());

        assert_eq!(due.len(), 0);
        assert_eq!(ledger.watched_turn_count(), 1);
    }

    /// Unchanged evidence across the bound is what makes a turn due.
    #[tokio::test(start_paused = true)]
    async fn unchanged_evidence_across_the_bound_becomes_due() {
        let mut ledger = ledger();
        let first = ledger.reconcile(&[candidate(1)], Instant::now());
        tokio::time::advance(BOUND).await;

        let second = ledger.reconcile(&[candidate(1)], Instant::now());

        assert_eq!(first.len(), 0);
        assert_eq!(second.len(), 1);
        assert_eq!(second.first().copied(), Some(candidate(1)));
    }

    /// Progress restarts the bound, so a turn that produced one more model
    /// call is not terminalized on the strength of its earlier silence.
    #[tokio::test(start_paused = true)]
    async fn changed_evidence_restarts_the_bound() {
        let mut ledger = ledger();
        let _ = ledger.reconcile(&[candidate(1)], Instant::now());
        tokio::time::advance(BOUND - Duration::from_secs(1)).await;
        let _ = ledger.reconcile(&[candidate(2)], Instant::now());
        tokio::time::advance(BOUND - Duration::from_secs(1)).await;

        let due = ledger.reconcile(&[candidate(2)], Instant::now());

        assert_eq!(due.len(), 0);
    }

    /// A turn that a complete scan no longer reports has left the quiescent
    /// shape, so it carries no credit back into it.
    #[tokio::test(start_paused = true)]
    async fn a_complete_scan_forgets_an_absent_turn() {
        let mut ledger = ledger();
        let _ = ledger.reconcile(&[candidate(1)], Instant::now());
        tokio::time::advance(BOUND - Duration::from_secs(1)).await;
        let _ = ledger.reconcile(&[], Instant::now());
        tokio::time::advance(BOUND - Duration::from_secs(1)).await;

        let due = ledger.reconcile(&[candidate(1)], Instant::now());

        assert_eq!(due.len(), 0);
        assert_eq!(ledger.watched_turn_count(), 1);
    }

    /// Every turn in the scan accrues its own bound, and only the turns this
    /// scan no longer reports are pruned — one scan carries the whole
    /// population, so neither outcome depends on how large it is.
    #[tokio::test(start_paused = true)]
    async fn each_scanned_turn_accrues_its_own_bound_and_departures_are_pruned() {
        let mut ledger = ledger();
        let _ = ledger.reconcile(&[candidate(1), other_candidate()], Instant::now());
        tokio::time::advance(BOUND).await;

        let due = ledger.reconcile(&[candidate(1)], Instant::now());

        assert_eq!(due.first().copied(), Some(candidate(1)));
        assert_eq!(due.len(), 1);
        assert_eq!(ledger.watched_turn_count(), 1);
    }

    /// The validated configured bound is the one the ledger decides by.
    #[tokio::test(start_paused = true)]
    async fn a_lowered_bound_is_the_one_the_ledger_applies() {
        let lowered = StaleActiveTurnBound::try_new(Duration::from_secs(60)).expect("60s is valid");
        let mut ledger = TurnLivenessLedger::new(lowered);
        let _ = ledger.reconcile(&[candidate(1)], Instant::now());
        tokio::time::advance(Duration::from_secs(60)).await;

        let due = ledger.reconcile(&[candidate(1)], Instant::now());

        assert_eq!(ledger.bound(), lowered);
        assert_eq!(due.first().copied(), Some(candidate(1)));
    }
}
