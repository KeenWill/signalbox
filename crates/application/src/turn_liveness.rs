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

use signalbox_domain::{ModelCallId, SemanticTranscriptEntryId, SessionId, TurnAttemptId, TurnId};
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

    /// Accepts a shorter bound and rejects zero or anything above the ceiling.
    ///
    /// A deployment may make the watchdog react sooner. Raising the bound is
    /// refused here rather than in a caller, because the ceiling is what makes
    /// the maximum wedge duration a property of the binary.
    pub fn try_lowered(bound: Duration) -> Result<Self, TurnLivenessBoundError> {
        if bound.is_zero() {
            Err(TurnLivenessBoundError::Zero)
        } else if bound > STALE_ACTIVE_TURN_BOUND {
            Err(TurnLivenessBoundError::AboveCeiling)
        } else {
            Ok(Self(bound))
        }
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
}

impl fmt::Display for TurnLivenessBoundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::Zero => "must be nonzero",
            Self::AboveCeiling => "cannot exceed the compiled staleness ceiling",
        };
        write!(formatter, "turn-liveness staleness bound {reason}")
    }
}

impl Error for TurnLivenessBoundError {}

/// The durable evidence whose change proves a turn is still progressing.
///
/// The lifecycle tables carry no activity timestamp, so "the latest model call
/// and the latest transcript entry are both older than the bound" is decided
/// by observing this evidence unchanged across the bound rather than by
/// reading a stored clock. Counts move when either sequence is appended to,
/// and the attempt identity moves when orchestration takes a fresh tenure, so
/// any progress at all changes the value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TurnLivenessEvidence {
    current_attempt: TurnAttemptId,
    model_call_count: u64,
    latest_model_call: Option<ModelCallId>,
    transcript_entry_count: u64,
    latest_transcript_entry: Option<SemanticTranscriptEntryId>,
}

impl TurnLivenessEvidence {
    /// Records one complete observation of a session's progress evidence.
    pub const fn new(
        current_attempt: TurnAttemptId,
        model_call_count: u64,
        latest_model_call: Option<ModelCallId>,
        transcript_entry_count: u64,
        latest_transcript_entry: Option<SemanticTranscriptEntryId>,
    ) -> Self {
        Self {
            current_attempt,
            model_call_count,
            latest_model_call,
            transcript_entry_count,
            latest_transcript_entry,
        }
    }

    /// Returns the attempt holding the turn's physical tenure.
    pub const fn current_attempt(self) -> TurnAttemptId {
        self.current_attempt
    }

    /// Returns how many model calls the session has ever authorized.
    pub const fn model_call_count(self) -> u64 {
        self.model_call_count
    }

    /// Returns the session's newest model-call identity, if any exists.
    pub const fn latest_model_call(self) -> Option<ModelCallId> {
        self.latest_model_call
    }

    /// Returns how many semantic transcript entries the session holds.
    pub const fn transcript_entry_count(self) -> u64 {
        self.transcript_entry_count
    }

    /// Returns the session's newest transcript-entry identity, if any exists.
    pub const fn latest_transcript_entry(self) -> Option<SemanticTranscriptEntryId> {
        self.latest_transcript_entry
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

/// Remembers how long each quiescent turn has stood still.
///
/// The ledger is in-process state with no authority: losing it costs one more
/// staleness bound before a wedged turn is reached, which is the safe
/// direction. Nothing here can terminalize a turn that a scan did not first
/// report as quiescent.
#[derive(Clone, Debug, Default)]
pub struct TurnLivenessLedger {
    quiescent: HashMap<TurnId, QuiescentObservation>,
}

#[derive(Clone, Copy, Debug)]
struct QuiescentObservation {
    evidence: TurnLivenessEvidence,
    since: Instant,
}

impl TurnLivenessLedger {
    /// Starts with no observed turn.
    pub fn new() -> Self {
        Self {
            quiescent: HashMap::new(),
        }
    }

    /// Returns how many turns are currently being watched for staleness.
    pub fn watched_turn_count(&self) -> usize {
        self.quiescent.len()
    }

    /// Folds one scan into the ledger and returns the turns now due.
    ///
    /// A turn whose evidence differs from the last observation restarts its
    /// bound, and a turn absent from this scan is forgotten. Both directions
    /// are deliberate: progress must reset the clock, and a turn that left the
    /// quiescent shape must not carry credit into a later quiescent period.
    pub fn reconcile(
        &mut self,
        quiescent: &[StaleTurnCandidate],
        now: Instant,
    ) -> Box<[StaleTurnCandidate]> {
        let bound = StaleActiveTurnBound::hard_ceiling().get();
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
    use signalbox_domain::{ModelCallId, SessionId, TurnAttemptId, TurnId};
    use std::time::Duration;
    use tokio::time::Instant;

    fn session() -> SessionId {
        SessionId::from_uuid(uuid::Uuid::from_u128(0x5e_5510))
    }

    fn turn() -> TurnId {
        TurnId::from_uuid(uuid::Uuid::from_u128(0x7a_0001))
    }

    fn attempt() -> TurnAttemptId {
        TurnAttemptId::from_uuid(uuid::Uuid::from_u128(0xa7_0001))
    }

    fn evidence(model_call_count: u64) -> TurnLivenessEvidence {
        TurnLivenessEvidence::new(
            attempt(),
            model_call_count,
            Some(ModelCallId::from_uuid(uuid::Uuid::from_u128(0xca11_0001))),
            2,
            None,
        )
    }

    fn candidate(model_call_count: u64) -> StaleTurnCandidate {
        StaleTurnCandidate::new(session(), turn(), evidence(model_call_count))
    }

    /// A deployment may react sooner than the compiled ceiling but never later.
    #[test]
    fn the_staleness_bound_lowers_and_never_raises() {
        let lowered = StaleActiveTurnBound::try_lowered(Duration::from_secs(60));
        let raised = StaleActiveTurnBound::try_lowered(Duration::from_secs(60 * 60));
        let zero = StaleActiveTurnBound::try_lowered(Duration::ZERO);

        assert_eq!(
            lowered.map(StaleActiveTurnBound::get),
            Ok(Duration::from_secs(60))
        );
        assert_eq!(raised, Err(TurnLivenessBoundError::AboveCeiling));
        assert_eq!(zero, Err(TurnLivenessBoundError::Zero));
    }

    /// The compiled constants are the production values this page states.
    #[test]
    fn the_compiled_constants_are_thirty_minutes_and_one_minute() {
        assert_eq!(
            StaleActiveTurnBound::hard_ceiling().get(),
            Duration::from_secs(1_800)
        );
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
        let mut ledger = TurnLivenessLedger::new();

        let due = ledger.reconcile(&[candidate(1)], Instant::now());

        assert_eq!(due.len(), 0);
        assert_eq!(ledger.watched_turn_count(), 1);
    }

    /// Unchanged evidence across the bound is what makes a turn due.
    #[tokio::test(start_paused = true)]
    async fn unchanged_evidence_across_the_bound_becomes_due() {
        let mut ledger = TurnLivenessLedger::new();
        let first = ledger.reconcile(&[candidate(1)], Instant::now());
        tokio::time::advance(Duration::from_secs(1_800)).await;

        let second = ledger.reconcile(&[candidate(1)], Instant::now());

        assert_eq!(first.len(), 0);
        assert_eq!(second.len(), 1);
        assert_eq!(second.first().copied(), Some(candidate(1)));
    }

    /// Progress restarts the bound, so a turn that produced one more model
    /// call is not terminalized on the strength of its earlier silence.
    #[tokio::test(start_paused = true)]
    async fn changed_evidence_restarts_the_bound() {
        let mut ledger = TurnLivenessLedger::new();
        let _ = ledger.reconcile(&[candidate(1)], Instant::now());
        tokio::time::advance(Duration::from_secs(1_799)).await;
        let _ = ledger.reconcile(&[candidate(2)], Instant::now());
        tokio::time::advance(Duration::from_secs(1_799)).await;

        let due = ledger.reconcile(&[candidate(2)], Instant::now());

        assert_eq!(due.len(), 0);
    }

    /// A turn that leaves the quiescent shape carries no credit back into it.
    #[tokio::test(start_paused = true)]
    async fn an_absent_turn_is_forgotten() {
        let mut ledger = TurnLivenessLedger::new();
        let _ = ledger.reconcile(&[candidate(1)], Instant::now());
        tokio::time::advance(Duration::from_secs(1_799)).await;
        let _ = ledger.reconcile(&[], Instant::now());
        tokio::time::advance(Duration::from_secs(1_799)).await;

        let due = ledger.reconcile(&[candidate(1)], Instant::now());

        assert_eq!(due.len(), 0);
        assert_eq!(ledger.watched_turn_count(), 1);
    }
}
