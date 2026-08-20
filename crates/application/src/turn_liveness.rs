//! Turn-level liveness: deciding when an active turn is boundedly stale.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md owns the active slot and the
//! terminal transitions this module's decision feeds. Component deadlines
//! cover each physical operation; nothing else covers a turn that holds the
//! slot while no operation is outstanding at all. This module owns only the
//! decision. Reading durable evidence and committing the failed-turn
//! transition belong to the persistence adapter, and the periodic pass that
//! drives both belongs to the daemon runtime.

use std::{collections::HashMap, error::Error, fmt, time::Duration};

use signalbox_domain::{SessionId, TurnAttemptId, TurnId};
use tokio::time::Instant;

/// How long a turn's durable evidence may stand still before the turn is
/// treated as wedged rather than working.
///
/// Set at the real production danger point: the longest legitimate tool
/// execution and the longest legitimate provider call both complete far
/// inside it, while an operator waiting on a wedged session notices well
/// before the next half hour passes.
// numeric-bound: ceiling - protects against a wedged turn holding its session slot forever
const STALE_ACTIVE_TURN_BOUND: Duration = Duration::from_secs(30 * 60);
/// How often turn liveness is reconsidered.
// numeric-bound: tunable - controls the turn-liveness reconsideration cadence
const BASELINE_TURN_LIVENESS_SCAN_INTERVAL: Duration = Duration::from_secs(60);

/// The staleness bound after which a quiescent active turn is terminalized.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct StaleActiveTurnBound(Duration);

impl StaleActiveTurnBound {
    /// Returns the hard safety ceiling compiled into this binary.
    pub const fn hard_ceiling() -> Self {
        Self(STALE_ACTIVE_TURN_BOUND)
    }

    /// Accepts a shorter whole-second bound, rejecting anything else.
    ///
    /// A deployment may make the watchdog react sooner. Raising the bound is
    /// refused here rather than in a caller, because the ceiling is what makes
    /// the maximum wedge duration a property of the binary. Sub-second
    /// precision is refused rather than truncated: nothing could act on it —
    /// staleness is decided from evidence sampled once per scan interval — and
    /// admitting it would make the audited bound disagree with the bound in
    /// force.
    pub fn try_lowered(bound: Duration) -> Result<Self, TurnLivenessBoundError> {
        if bound.is_zero() {
            Err(TurnLivenessBoundError::Zero)
        } else if bound > STALE_ACTIVE_TURN_BOUND {
            Err(TurnLivenessBoundError::AboveCeiling)
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
    /// Returns the one-minute baseline compiled into this binary.
    pub const fn baseline() -> Self {
        Self(BASELINE_TURN_LIVENESS_SCAN_INTERVAL)
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
    /// The proposal exceeds the hard safety ceiling, which only lowers.
    AboveCeiling,
    /// The proposal carries precision finer than a whole second.
    Subsecond,
}

impl fmt::Display for TurnLivenessBoundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::Zero => "must be nonzero",
            Self::AboveCeiling => "cannot exceed the compiled staleness ceiling",
            Self::Subsecond => "must be a whole number of seconds",
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
    /// [`StaleActiveTurnBound::try_lowered`] actually gets it.
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
        StaleActiveTurnBound, StaleTurnCandidate, TurnLivenessBoundError, TurnLivenessEvidence,
        TurnLivenessLedger, TurnLivenessScanInterval,
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
        TurnLivenessLedger::new(StaleActiveTurnBound::hard_ceiling())
    }

    /// A deployment may react sooner than the compiled ceiling but never later.
    #[test]
    fn the_staleness_bound_lowers_and_never_raises() {
        let shorter = Duration::from_secs(60);
        let lowered = StaleActiveTurnBound::try_lowered(shorter);
        let raised = StaleActiveTurnBound::try_lowered(Duration::from_secs(60 * 60));
        let zero = StaleActiveTurnBound::try_lowered(Duration::ZERO);

        assert_eq!(lowered.map(StaleActiveTurnBound::get), Ok(shorter));
        assert_eq!(raised, Err(TurnLivenessBoundError::AboveCeiling));
        assert_eq!(zero, Err(TurnLivenessBoundError::Zero));
    }

    /// A bound the audit record could not report exactly is refused outright,
    /// so the reported bound and the bound in force can never disagree.
    #[test]
    fn a_subsecond_bound_is_refused_rather_than_truncated() {
        let millis = StaleActiveTurnBound::try_lowered(Duration::from_millis(500));
        let fractional = StaleActiveTurnBound::try_lowered(Duration::from_millis(1_500));
        let whole = StaleActiveTurnBound::try_lowered(Duration::from_secs(1));

        assert_eq!(millis, Err(TurnLivenessBoundError::Subsecond));
        assert_eq!(fractional, Err(TurnLivenessBoundError::Subsecond));
        assert_eq!(whole.map(StaleActiveTurnBound::as_secs), Ok(1));
    }

    /// The compiled constants are the production values this page states.
    #[test]
    fn the_compiled_constants_are_thirty_minutes_and_one_minute() {
        assert_eq!(StaleActiveTurnBound::hard_ceiling().get(), BOUND);
        assert_eq!(
            TurnLivenessScanInterval::baseline().get(),
            Duration::from_secs(60)
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

    /// The validated lowered bound is the one the ledger decides by, so a
    /// deployment that shortened it does not silently keep the ceiling.
    #[tokio::test(start_paused = true)]
    async fn a_lowered_bound_is_the_one_the_ledger_applies() {
        let lowered =
            StaleActiveTurnBound::try_lowered(Duration::from_secs(60)).expect("60s is below 30m");
        let mut ledger = TurnLivenessLedger::new(lowered);
        let _ = ledger.reconcile(&[candidate(1)], Instant::now());
        tokio::time::advance(Duration::from_secs(60)).await;

        let due = ledger.reconcile(&[candidate(1)], Instant::now());

        assert_eq!(ledger.bound(), lowered);
        assert_eq!(due.first().copied(), Some(candidate(1)));
    }
}
