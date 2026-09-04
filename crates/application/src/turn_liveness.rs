//! Turn-level liveness: deciding when an active turn is boundedly stale.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md owns the active slot and the
//! terminal transitions this module's decision feeds. Component deadlines
//! cover each physical operation; nothing else covers a turn that holds the
//! slot while no operation is outstanding at all. This module owns only the
//! decision. Reading durable evidence and committing the failed-turn
//! transition belong to the persistence adapter, and the periodic pass that
//! drives both belongs to the daemon runtime.

use std::{error::Error, fmt, num::NonZeroU32, num::NonZeroU64, time::Duration};

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
}

/// The independently observed watchdog predicate for one turn.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TurnLivenessGuardKind {
    /// The turn has no physical operation in flight.
    Quiescent,
    /// The turn still holds its slot after a component deadline should have settled it.
    SlotHeld,
}

impl TurnLivenessGuardKind {
    /// Returns the durable storage token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Quiescent => "quiescent",
            Self::SlotHeld => "slot_held",
        }
    }
}

/// One commit-ordered observation after persistence advanced its ordinal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurableTurnLivenessObservation {
    candidate: StaleTurnCandidate,
    ordinal: NonZeroU64,
}

impl DurableTurnLivenessObservation {
    /// Reconstitutes one persisted observation.
    pub const fn new(candidate: StaleTurnCandidate, ordinal: NonZeroU64) -> Self {
        Self { candidate, ordinal }
    }

    /// Returns the observed turn and progress evidence.
    pub const fn candidate(self) -> StaleTurnCandidate {
        self.candidate
    }

    /// Returns the number of consecutive equal scan observations.
    pub const fn ordinal(self) -> NonZeroU64 {
        self.ordinal
    }
}

/// Decides staleness from durable repeated-observation ordinals.
#[derive(Clone, Copy, Debug)]
pub struct TurnLivenessLedger {
    bound: StaleActiveTurnBound,
    scan_interval: TurnLivenessScanInterval,
}

impl TurnLivenessLedger {
    /// Binds a staleness decision to its configured scan cadence.
    pub const fn new(bound: StaleActiveTurnBound, scan_interval: TurnLivenessScanInterval) -> Self {
        Self {
            bound,
            scan_interval,
        }
    }

    /// Returns the staleness bound this ledger decides by.
    pub const fn bound(&self) -> StaleActiveTurnBound {
        self.bound
    }

    /// Returns the configured scan interval used to interpret ordinals.
    pub const fn scan_interval(&self) -> TurnLivenessScanInterval {
        self.scan_interval
    }

    /// Returns candidates whose persisted repeated observations cover the bound.
    ///
    /// The first observation establishes the frontier. Each later equal
    /// observation contributes one configured scan interval, so system-clock
    /// adjustment cannot add elapsed time.
    pub fn reconcile(
        self,
        observations: &[DurableTurnLivenessObservation],
    ) -> Box<[StaleTurnCandidate]> {
        let interval_nanos = self.scan_interval.get().as_nanos();
        let bound_nanos = self.bound.get().as_nanos();
        let intervals =
            bound_nanos.saturating_add(interval_nanos.saturating_sub(1)) / interval_nanos;
        let required = intervals.saturating_add(1);
        observations
            .iter()
            .filter(|observation| u128::from(observation.ordinal().get()) >= required)
            .map(|observation| observation.candidate())
            .collect::<Vec<_>>()
            .into_boxed_slice()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AutomaticReconciliationAttempt, DurableTurnLivenessObservation, StaleActiveTurnBound,
        StaleTurnCandidate, TurnLivenessBoundError, TurnLivenessEvidence, TurnLivenessLedger,
        TurnLivenessScanInterval,
    };
    use signalbox_domain::{SessionId, TurnAttemptId, TurnId};
    use std::{num::NonZeroU64, time::Duration};

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

    fn ledger() -> TurnLivenessLedger {
        TurnLivenessLedger::new(
            StaleActiveTurnBound::try_new(BOUND).expect("fixture bound is valid"),
            TurnLivenessScanInterval::try_new(Duration::from_secs(60))
                .expect("fixture interval is valid"),
        )
    }

    fn observation(frontier: u64, ordinal: u64) -> DurableTurnLivenessObservation {
        DurableTurnLivenessObservation::new(
            candidate(frontier),
            NonZeroU64::new(ordinal).expect("fixture ordinal is positive"),
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

    /// A first persisted observation establishes evidence but carries no elapsed scans.
    #[test]
    fn a_first_observation_is_never_due() {
        let due = ledger().reconcile(&[observation(1, 1)]);

        assert!(due.is_empty());
    }

    /// Thirty elapsed minute scans plus the establishing observation cover a 30-minute bound.
    #[test]
    fn unchanged_evidence_across_the_bound_becomes_due() {
        let due = ledger().reconcile(&[observation(1, 31)]);

        assert_eq!(due.as_ref(), &[candidate(1)]);
    }

    /// Persistence resets the ordinal when commit-ordered progress changes.
    #[test]
    fn changed_evidence_restarts_the_bound() {
        let due = ledger().reconcile(&[observation(2, 2)]);

        assert!(due.is_empty());
    }

    /// A forward wall-clock jump supplies no input to the ordinal decision.
    #[test]
    fn forward_clock_jump_does_not_stale_live_work() {
        let due = ledger().reconcile(&[observation(2, 1)]);

        assert!(due.is_empty());
    }

    /// The validated configured bound is the one the ledger decides by.
    #[test]
    fn a_lowered_bound_is_the_one_the_ledger_applies() {
        let lowered = StaleActiveTurnBound::try_new(Duration::from_secs(60)).expect("60s is valid");
        let ledger = TurnLivenessLedger::new(
            lowered,
            TurnLivenessScanInterval::try_new(Duration::from_secs(60))
                .expect("fixture interval is valid"),
        );
        let due = ledger.reconcile(&[observation(1, 2)]);

        assert_eq!(ledger.bound(), lowered);
        assert_eq!(due.as_ref(), &[candidate(1)]);
    }
}
