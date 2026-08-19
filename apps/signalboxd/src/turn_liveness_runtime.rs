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
    AcceptedInputTurnFailureIdentities, ContextFrontierId, SemanticTranscriptEntryId,
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
    scan_interval: TurnLivenessScanInterval,
}

impl TurnLivenessRuntime {
    /// Supervises turn liveness through the supplied shared pool.
    pub fn new(pool: PgPool) -> Self {
        Self {
            repository: PostgresTurnLivenessRepository::new(pool),
            scan_interval: TurnLivenessScanInterval::baseline(),
        }
    }

    /// Scans until shutdown, terminalizing every turn observed boundedly stale.
    ///
    /// A failed pass changes nothing and is retried at the next interval, so
    /// no pass outcome ends this task: the durable rows the next pass reads
    /// are the only state it carries.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(self.scan_interval.get());
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut ledger = TurnLivenessLedger::new();
        loop {
            match next_turn_liveness_wake(&mut shutdown, &mut ticker).await {
                TurnLivenessWake::Shutdown => return,
                TurnLivenessWake::Scan => {
                    reconcile_turn_liveness(&self.repository, &mut ledger).await;
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
async fn reconcile_turn_liveness(
    repository: &PostgresTurnLivenessRepository,
    ledger: &mut TurnLivenessLedger,
) {
    let quiescent = match repository.quiescent_active_turns().await {
        Ok(quiescent) => quiescent,
        // An unreadable inventory is not evidence that anything is stale, so
        // the pass ends without a decision and the ledger keeps what it had.
        Err(error) => return report_turn_liveness_failure(&error),
    };
    let due = ledger.reconcile(&quiescent, Instant::now());
    for candidate in due {
        terminalize_stale_turn(repository, candidate).await;
    }
}

async fn terminalize_stale_turn(
    repository: &PostgresTurnLivenessRepository,
    candidate: StaleTurnCandidate,
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
            staleness_bound_seconds = stale_turn_bound_seconds(),
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

fn stale_turn_bound_seconds() -> u64 {
    StaleActiveTurnBound::hard_ceiling().get().as_secs()
}

#[cfg(test)]
mod tests {
    use super::{
        PASS_FAILURE_CAUSE, STALE_TURN_TERMINAL_CAUSE, TurnLivenessWake, next_turn_liveness_wake,
        stale_turn_bound_seconds,
    };
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
        assert_eq!(stale_turn_bound_seconds(), 1_800);
    }
}
