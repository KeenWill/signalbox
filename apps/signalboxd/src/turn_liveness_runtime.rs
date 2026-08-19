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

/// Why one turn-liveness pass produced no decision.
const PASS_FAILURE_CAUSE: &str = "turn_liveness_pass_failed";

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
        let mut cursor: Option<SessionId> = None;
        loop {
            match next_turn_liveness_wake(&mut shutdown, &mut ticker).await {
                TurnLivenessWake::Shutdown => return,
                TurnLivenessWake::Scan => {
                    cursor = reconcile_turn_liveness(
                        &self.repository,
                        &mut ledger,
                        cursor,
                        self.staleness_bound,
                    )
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
/// Returns the cursor the next pass resumes from: a saturated page leaves the
/// rotation partway through the population, and any other page restarts it.
async fn reconcile_turn_liveness(
    repository: &PostgresTurnLivenessRepository,
    ledger: &mut TurnLivenessLedger,
    cursor: Option<SessionId>,
    staleness_bound: StaleActiveTurnBound,
) -> Option<SessionId> {
    let page = match repository.quiescent_active_turns(cursor).await {
        Ok(page) => page,
        // An unreadable inventory is not evidence that anything is stale, so
        // the pass ends without a decision, the ledger keeps what it had, and
        // the rotation restarts rather than skipping past unread turns.
        Err(error) => {
            report_turn_liveness_failure(&error);
            return None;
        }
    };
    let due = ledger.reconcile(page.candidates(), page.coverage(), Instant::now());
    for candidate in due {
        terminalize_stale_turn(repository, candidate, staleness_bound).await;
    }
    page.resume_after()
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
            staleness_bound_seconds = staleness_bound.get().as_secs(),
            "active turn terminalized as failed after its durable evidence stood still"
        ),
        Ok(StaleTurnOutcome::Superseded) => tracing::info!(
            cause_code = STALE_TURN_TERMINAL_CAUSE,
            session_id = %candidate.session().as_uuid(),
            turn_id = %candidate.turn().as_uuid(),
            "stale active turn changed under its locks and was left alone"
        ),
        Err(error) => report_turn_liveness_failure(&error),
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
        PASS_FAILURE_CAUSE, STALE_TURN_TERMINAL_CAUSE, TurnLivenessWake, next_turn_liveness_wake,
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
    }

    /// The bound reported beside a terminalized turn is the configured one, so
    /// an operator reading the line sees what actually decided the turn.
    #[test]
    fn the_audited_bound_is_the_configured_one() {
        let lowered =
            StaleActiveTurnBound::try_lowered(Duration::from_secs(300)).expect("300s is below 30m");

        assert_eq!(lowered.get().as_secs(), 300);
        assert_eq!(StaleActiveTurnBound::hard_ceiling().get().as_secs(), 1_800);
    }
}
