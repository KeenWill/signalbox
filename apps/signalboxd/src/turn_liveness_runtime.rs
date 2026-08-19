//! Periodic supervision of turn-level liveness.
//!
//! Component deadlines cover physical operations; this pass covers the gap
//! between them, where a turn holds its session's progressing slot with no
//! operation outstanding at all. It runs on its own timer rather than inside a
//! scheduler pass, because the sessions it exists to reach are exactly the
//! ones no pass is scheduled for.

use signalbox_application::{
    ClassifyOperatorFailure, StaleActiveTurnBound, StaleTurnCandidate, StaleTurnOutcome,
    TurnLivenessLedger, TurnLivenessScanInterval,
};
use signalbox_domain::{
    AcceptedInputTurnFailureIdentities, ContextFrontierId, SemanticTranscriptEntryId, SessionId,
};
use signalbox_persistence::turn_liveness::{
    PostgresTurnLivenessRepository, TurnLivenessRepositoryError,
};
use sqlx::PgPool;
use tokio::{
    select,
    sync::watch,
    time::{Instant, Interval, MissedTickBehavior, interval},
};
use uuid::Uuid;

/// The terminal cause an operator audits a watchdog-ended turn by.
///
/// Distinct from every restart-recovery cause: a turn carrying this ended
/// while its daemon was alive and its session was reachable, which is a
/// different defect from a turn abandoned by a dead process.
const STALE_TURN_TERMINAL_CAUSE: &str = "turn_liveness_watchdog_stale";

/// Why a candidate the scan reported was left alone.
///
/// Distinct from the terminal cause: a search for turns this pass ended must
/// not also return turns it explicitly declined to end.
const STALE_TURN_SUPERSEDED_CAUSE: &str = "turn_liveness_candidate_superseded";

/// Why a terminalization's durable outcome is unknown.
///
/// The audit line the terminal cause promises can no longer be written truly —
/// the commit may or may not have landed — so this reports the same identity
/// under a cause an operator can tell apart from a committed terminalization.
const STALE_TURN_AMBIGUOUS_CAUSE: &str = "turn_liveness_terminalization_ambiguous";

/// Why a wedged turn could not be ended at all.
///
/// Steering pending on a turn must be closed before the turn terminalizes, and
/// the failed-turn transition this pass reuses closes none — so this reports a
/// turn that is wedged and that no present transition can release. It is a
/// warning rather than an informational line because, unlike supersession,
/// nothing about it is expected to resolve on its own.
const STALE_TURN_STEERING_BLOCKED_CAUSE: &str = "turn_liveness_steering_blocks_terminalization";

/// Why one turn-liveness pass produced no decision.
const PASS_FAILURE_CAUSE: &str = "turn_liveness_pass_failed";

/// Why a rotation was abandoned without deciding anything.
const ROTATION_CEILING_CAUSE: &str = "turn_liveness_rotation_ceiling_reached";

/// How many pages one scan may read before abandoning its rotation.
///
/// The rotation terminates on its own — sessions are distinct and the cursor
/// advances strictly — so this is not what ends the loop. It is the fail-safe
/// against a rotation that does not converge, and it is set where reaching it
/// is itself the defect: at the page size it multiplies to roughly a million
/// simultaneously quiescent turns, which the session population cannot reach
/// before something else has failed. A scan that reaches it decides nothing
/// rather than deciding on a partial population.
// numeric-bound: ceiling - bounds one scan's reads against a non-converging rotation
const QUIESCENT_ROTATION_PAGE_CEILING: usize = 4_096;

/// What woke the supervising loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnLivenessWake {
    /// The scan interval elapsed.
    Scan,
    /// Shutdown was requested, or its sender was dropped.
    Shutdown,
}

/// Periodic turn-liveness supervision over the shared daemon pool.
#[derive(Clone, Debug)]
pub struct TurnLivenessRuntime {
    repository: PostgresTurnLivenessRepository,
    staleness_bound: StaleActiveTurnBound,
    scan_interval: TurnLivenessScanInterval,
}

impl TurnLivenessRuntime {
    /// Supervises turn liveness with the supplied bound and cadence.
    ///
    /// The bound is a parameter rather than a reload of the compiled ceiling so
    /// a deployment that validated a shorter one actually runs with it; the
    /// ceiling stays the only maximum, enforced where the bound is built.
    pub fn new(
        pool: PgPool,
        staleness_bound: StaleActiveTurnBound,
        scan_interval: TurnLivenessScanInterval,
    ) -> Self {
        Self {
            repository: PostgresTurnLivenessRepository::new(pool),
            staleness_bound,
            scan_interval,
        }
    }

    /// Scans until shutdown, terminalizing every turn observed boundedly stale.
    ///
    /// A failed pass changes nothing and is retried at the next interval, so
    /// no pass outcome ends this task: the durable rows the next pass reads
    /// are the only state it carries. The cursor rotates across passes so a
    /// quiescent population larger than one page is still covered in full.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(self.scan_interval.get());
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut ledger = TurnLivenessLedger::new(self.staleness_bound);
        loop {
            match next_turn_liveness_wake(&mut shutdown, &mut ticker).await {
                TurnLivenessWake::Shutdown => return,
                TurnLivenessWake::Scan => {
                    reconcile_turn_liveness(&self.repository, &mut ledger, self.staleness_bound)
                        .await;
                }
            }
        }
    }
}

/// Waits for the next scan or for shutdown, whichever comes first.
///
/// Shutdown is biased ahead of the timer so a requested stop is never delayed
/// by a tick that became ready in the same poll.
async fn next_turn_liveness_wake(
    shutdown: &mut watch::Receiver<bool>,
    ticker: &mut Interval,
) -> TurnLivenessWake {
    if *shutdown.borrow() {
        return TurnLivenessWake::Shutdown;
    }
    select! {
        biased;
        changed = shutdown.changed() => {
            if changed.is_err() || *shutdown.borrow() {
                TurnLivenessWake::Shutdown
            } else {
                TurnLivenessWake::Scan
            }
        }
        _ = ticker.tick() => TurnLivenessWake::Scan,
    }
}

/// Folds one durable observation into the ledger and acts on what is due.
///
/// The whole rotation is drained before anything is decided. Paging across
/// wakes instead would tie the time to reach a turn to the population size,
/// and the staleness bound is a property of the binary, not of how many
/// sessions happen to be quiescent.
async fn reconcile_turn_liveness(
    repository: &PostgresTurnLivenessRepository,
    ledger: &mut TurnLivenessLedger,
    staleness_bound: StaleActiveTurnBound,
) {
    // A rotation that could not be drained is not a smaller population; the
    // pass therefore ends without a decision and the ledger keeps what it had,
    // rather than forgetting every turn the unread pages would have carried.
    let Some(quiescent) = drain_quiescent_rotation(repository).await else {
        return;
    };
    let due = ledger.reconcile(&quiescent, Instant::now());
    for candidate in due {
        terminalize_stale_turn(repository, candidate, staleness_bound).await;
    }
}

/// Reads pages until the rotation ends, or reports why it could not.
async fn drain_quiescent_rotation(
    repository: &PostgresTurnLivenessRepository,
) -> Option<Vec<StaleTurnCandidate>> {
    let mut quiescent = Vec::new();
    let mut cursor: Option<SessionId> = None;
    for _ in 0..QUIESCENT_ROTATION_PAGE_CEILING {
        let page = match repository.quiescent_active_turns(cursor).await {
            Ok(page) => page,
            // An unreadable inventory is not evidence that anything is stale.
            Err(error) => {
                report_turn_liveness_failure(&error);
                return None;
            }
        };
        cursor = page.resume_after();
        quiescent.extend(page.into_candidates());
        if cursor.is_none() {
            return Some(quiescent);
        }
    }
    tracing::warn!(
        cause_code = ROTATION_CEILING_CAUSE,
        page_ceiling = QUIESCENT_ROTATION_PAGE_CEILING,
        observed_turns = quiescent.len(),
        "turn-liveness rotation did not end within its page ceiling"
    );
    None
}

async fn terminalize_stale_turn(
    repository: &PostgresTurnLivenessRepository,
    candidate: StaleTurnCandidate,
    staleness_bound: StaleActiveTurnBound,
) {
    let identities = AcceptedInputTurnFailureIdentities::new(
        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
        ContextFrontierId::from_uuid(Uuid::now_v7()),
    );
    match repository
        .terminalize_stale_turn(candidate, identities)
        .await
    {
        Ok(StaleTurnOutcome::Terminalized) => tracing::warn!(
            cause_code = STALE_TURN_TERMINAL_CAUSE,
            session_id = %candidate.session().as_uuid(),
            turn_id = %candidate.turn().as_uuid(),
            staleness_bound_seconds = staleness_bound.as_secs(),
            "active turn terminalized as failed after its durable evidence stood still"
        ),
        Ok(StaleTurnOutcome::Superseded) => tracing::info!(
            cause_code = STALE_TURN_SUPERSEDED_CAUSE,
            session_id = %candidate.session().as_uuid(),
            turn_id = %candidate.turn().as_uuid(),
            "stale active turn changed under its locks and was left alone"
        ),
        Ok(StaleTurnOutcome::BlockedByPendingSteering) => tracing::warn!(
            cause_code = STALE_TURN_STEERING_BLOCKED_CAUSE,
            session_id = %candidate.session().as_uuid(),
            turn_id = %candidate.turn().as_uuid(),
            staleness_bound_seconds = staleness_bound.as_secs(),
            "stale active turn holds pending steering that no present transition can close"
        ),
        // An ambiguous commit is the one failure whose durable effect is
        // unknown from here: if it landed, the turn is terminal and no later
        // scan reports it; if it did not, the candidate is unchanged and a
        // later scan retries it. Nothing here distinguishes those, and the
        // durable `TurnFailed` shape carries no cause, so this line is the only
        // record naming the turn — written without claiming the commit landed.
        Err(
            error @ TurnLivenessRepositoryError::TerminalizationDatabase {
                commit_ambiguous: true,
                ..
            },
        ) => tracing::warn!(
            failure_class = ?error.operator_failure_class(),
            cause_code = STALE_TURN_AMBIGUOUS_CAUSE,
            detail_code = error.operator_failure_cause_code(),
            session_id = %candidate.session().as_uuid(),
            turn_id = %candidate.turn().as_uuid(),
            staleness_bound_seconds = staleness_bound.as_secs(),
            "stale active turn may or may not have terminalized; its commit was not acknowledged"
        ),
        Err(error) => tracing::warn!(
            failure_class = ?error.operator_failure_class(),
            cause_code = PASS_FAILURE_CAUSE,
            detail_code = error.operator_failure_cause_code(),
            session_id = %candidate.session().as_uuid(),
            turn_id = %candidate.turn().as_uuid(),
            "stale active turn was not terminalized"
        ),
    }
}

fn report_turn_liveness_failure(error: &TurnLivenessRepositoryError) {
    tracing::warn!(
        failure_class = ?error.operator_failure_class(),
        cause_code = PASS_FAILURE_CAUSE,
        detail_code = error.operator_failure_cause_code(),
        "turn-liveness pass produced no decision"
    );
}

#[cfg(test)]
mod tests {
    use super::{
        PASS_FAILURE_CAUSE, QUIESCENT_ROTATION_PAGE_CEILING, ROTATION_CEILING_CAUSE,
        STALE_TURN_AMBIGUOUS_CAUSE, STALE_TURN_STEERING_BLOCKED_CAUSE, STALE_TURN_SUPERSEDED_CAUSE,
        STALE_TURN_TERMINAL_CAUSE, TurnLivenessWake, next_turn_liveness_wake,
    };
    use signalbox_application::StaleActiveTurnBound;
    use std::time::Duration;
    use tokio::{
        sync::watch,
        time::{MissedTickBehavior, interval},
    };

    /// The elapsed scan interval is what drives an ordinary pass.
    #[tokio::test(start_paused = true)]
    async fn an_elapsed_interval_wakes_a_scan() {
        let (_shutdown, receiver) = watch::channel(false);
        let mut receiver = receiver;
        let mut ticker = interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let wake = next_turn_liveness_wake(&mut receiver, &mut ticker).await;

        assert_eq!(wake, TurnLivenessWake::Scan);
    }

    /// A requested shutdown is answered without waiting out the interval.
    #[tokio::test(start_paused = true)]
    async fn a_requested_shutdown_wins_over_a_pending_interval() {
        let (shutdown, receiver) = watch::channel(false);
        let mut receiver = receiver;
        let mut ticker = interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let _first = next_turn_liveness_wake(&mut receiver, &mut ticker).await;
        shutdown.send(true).expect("the receiver is still held");

        let wake = next_turn_liveness_wake(&mut receiver, &mut ticker).await;

        assert_eq!(wake, TurnLivenessWake::Shutdown);
    }

    /// A dropped sender is the same stop signal as an explicit request.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_shutdown_sender_stops_the_loop() {
        let (shutdown, receiver) = watch::channel(false);
        let mut receiver = receiver;
        let mut ticker = interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let _first = next_turn_liveness_wake(&mut receiver, &mut ticker).await;
        drop(shutdown);

        let wake = next_turn_liveness_wake(&mut receiver, &mut ticker).await;

        assert_eq!(wake, TurnLivenessWake::Shutdown);
    }

    /// The audited cause codes are stable strings an operator can search.
    #[test]
    fn the_watchdog_cause_codes_are_distinct() {
        assert_eq!(STALE_TURN_TERMINAL_CAUSE, "turn_liveness_watchdog_stale");
        assert_eq!(PASS_FAILURE_CAUSE, "turn_liveness_pass_failed");
        assert_eq!(
            ROTATION_CEILING_CAUSE,
            "turn_liveness_rotation_ceiling_reached"
        );
        assert_eq!(
            STALE_TURN_SUPERSEDED_CAUSE,
            "turn_liveness_candidate_superseded"
        );
        assert_eq!(
            STALE_TURN_AMBIGUOUS_CAUSE,
            "turn_liveness_terminalization_ambiguous"
        );
        assert_eq!(
            STALE_TURN_STEERING_BLOCKED_CAUSE,
            "turn_liveness_steering_blocks_terminalization"
        );
        assert_ne!(STALE_TURN_SUPERSEDED_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_ne!(STALE_TURN_AMBIGUOUS_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_ne!(STALE_TURN_STEERING_BLOCKED_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_ne!(PASS_FAILURE_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_ne!(ROTATION_CEILING_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_eq!(QUIESCENT_ROTATION_PAGE_CEILING, 4_096);
    }

    /// The bound reported beside a terminalized turn is the configured one, so
    /// an operator reading the line sees what actually decided the turn.
    #[test]
    fn the_audited_bound_is_the_configured_one() {
        let shortened = Duration::from_secs(300);
        let lowered = StaleActiveTurnBound::try_lowered(shortened).expect("300s is below 30m");

        assert_eq!(lowered.get(), shortened);
        assert_eq!(StaleActiveTurnBound::hard_ceiling().as_secs(), 1_800);
    }
}
