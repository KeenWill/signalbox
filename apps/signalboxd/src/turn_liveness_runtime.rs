//! Periodic supervision of turn-level liveness.
//!
//! Component deadlines cover physical operations; this pass covers the gap
//! between them, where a turn holds its session's progressing slot with no
//! operation outstanding at all. It runs on its own timer rather than inside a
//! scheduler pass, because the sessions it exists to reach are exactly the
//! ones no pass is scheduled for.

use std::future::Future;

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

/// Why turns that came due were left for a later scan.
const TERMINALIZATION_DEFERRED_CAUSE: &str = "turn_liveness_terminalization_deferred";

/// Why one turn-liveness pass produced no decision.
const PASS_FAILURE_CAUSE: &str = "turn_liveness_pass_failed";

/// How many turns one scan terminalizes before leaving the rest.
///
/// Terminalizations run one at a time, each a short transaction under the
/// session's scheduler lock, and the next scan cannot start until the phase
/// ends — so an unbounded phase would let a large stale cohort push the next
/// observation arbitrarily far past the scan interval, which is the
/// population-independent behaviour this pass exists to have. At a pessimistic
/// fifth of a second per transaction this cap is a phase of about thirteen
/// seconds against a one-minute interval, leaving the cadence intact under
/// conditions well worse than any observed. Turns over the cap are not lost:
/// nothing about them changed, so the next scan finds them due again.
// numeric-bound: tunable - bounds one scan's terminalization phase
const TERMINALIZATIONS_PER_SCAN: usize = 64;

/// Why a rotation was abandoned without deciding anything.
const ROTATION_CEILING_CAUSE: &str = "turn_liveness_rotation_ceiling_reached";

/// How many candidate-bearing pages one scan may read.
///
/// The rotation terminates on its own — sessions are distinct and the cursor
/// advances strictly — so this is not what ends the loop. It is the fail-safe
/// against a rotation that does not converge, and it is set where reaching it
/// is itself the defect: multiplied by the page size it is a capacity of
/// 1,048,576 simultaneously quiescent turns, which the session population
/// cannot reach before something else has failed. A scan whose rotation exceeds
/// that decides nothing rather than deciding on a partial population.
///
/// The capacity is that product exactly, not one page short of it. A population
/// that is an exact multiple of the page size fills its last candidate-bearing
/// page, so the rotation's end is learned from one further read that returns
/// nothing; the loop allows that probe past this ceiling and counts no
/// candidates from it.
// numeric-bound: ceiling - bounds one scan's reads against a non-converging rotation
const QUIESCENT_ROTATION_PAGE_CEILING: usize = 4_096;

/// One inventory read, as the rotation consumes it.
///
/// The rotation cares about two things a page reports: what it observed, and
/// whether anything follows it. Naming them here is what lets the drain be
/// exercised without a database, which two boundary defects in it have earned.
struct InventoryPage {
    candidates: Box<[StaleTurnCandidate]>,
    resume_after: Option<SessionId>,
}

/// Ends one turn observed boundedly stale.
trait StaleTurnTerminalizer {
    fn terminalize(
        &self,
        candidate: StaleTurnCandidate,
        identities: AcceptedInputTurnFailureIdentities,
    ) -> impl Future<Output = Result<StaleTurnOutcome, TurnLivenessRepositoryError>> + Send;
}

impl StaleTurnTerminalizer for PostgresTurnLivenessRepository {
    async fn terminalize(
        &self,
        candidate: StaleTurnCandidate,
        identities: AcceptedInputTurnFailureIdentities,
    ) -> Result<StaleTurnOutcome, TurnLivenessRepositoryError> {
        self.terminalize_stale_turn(candidate, identities).await
    }
}

/// Reads one page of the quiescent inventory.
trait QuiescentInventory {
    fn read_page(
        &self,
        after: Option<SessionId>,
    ) -> impl Future<Output = Result<InventoryPage, TurnLivenessRepositoryError>> + Send;
}

impl QuiescentInventory for PostgresTurnLivenessRepository {
    async fn read_page(
        &self,
        after: Option<SessionId>,
    ) -> Result<InventoryPage, TurnLivenessRepositoryError> {
        let page = self.quiescent_active_turns(after).await?;
        let resume_after = page.resume_after();
        Ok(InventoryPage {
            candidates: page.into_candidates(),
            resume_after,
        })
    }
}

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
                    reconcile_turn_liveness(
                        &self.repository,
                        &mut ledger,
                        self.staleness_bound,
                        TERMINALIZATIONS_PER_SCAN,
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
/// The whole rotation is drained before anything is decided. Paging across
/// wakes instead would tie the time to reach a turn to the population size,
/// and the staleness bound is a property of the binary, not of how many
/// sessions happen to be quiescent.
async fn reconcile_turn_liveness<Repository>(
    repository: &Repository,
    ledger: &mut TurnLivenessLedger,
    staleness_bound: StaleActiveTurnBound,
    terminalization_cap: usize,
) where
    Repository: QuiescentInventory + StaleTurnTerminalizer,
{
    // A rotation that could not be drained is not a smaller population; the
    // pass therefore ends without a decision and the ledger keeps what it had,
    // rather than forgetting every turn the unread pages would have carried.
    let Some(quiescent) =
        drain_quiescent_rotation(repository, QUIESCENT_ROTATION_PAGE_CEILING).await
    else {
        return;
    };
    let due = ledger.reconcile(&quiescent, Instant::now());
    for candidate in due.iter().take(terminalization_cap) {
        terminalize_stale_turn(repository, *candidate, staleness_bound).await;
    }
    // Deferring costs a scan interval and nothing else: a turn left here had
    // nothing change, so the next scan observes the same evidence and finds it
    // due again. Draining the whole cohort instead would delay that scan by as
    // long as the cohort takes.
    if let Some(deferred) = due
        .len()
        .checked_sub(terminalization_cap)
        .filter(|count| *count > 0)
    {
        tracing::info!(
            cause_code = TERMINALIZATION_DEFERRED_CAUSE,
            deferred_turns = deferred,
            terminalization_cap,
            "more turns came due than one scan terminalizes; the rest wait for the next scan"
        );
    }
}

/// Reads pages until the rotation ends, or reports why it could not.
///
/// `page_ceiling` bounds the candidate-bearing pages one scan may read, so the
/// capacity is exactly that many pages' worth of turns. A page that does not
/// fill ends the rotation where it stands. A ceiling reached on a full page ends
/// it only if one further read — the probe — returns nothing: a full page never
/// proves the end, and a probe carrying candidates proves the opposite, that the
/// population is past the capacity. Accepting those candidates would raise the
/// ceiling by up to one page without saying so, so the probe ends the rotation
/// or fails it, and never contributes to it.
async fn drain_quiescent_rotation<Inventory>(
    inventory: &Inventory,
    page_ceiling: usize,
) -> Option<Vec<StaleTurnCandidate>>
where
    Inventory: QuiescentInventory,
{
    let mut quiescent = Vec::new();
    let mut cursor: Option<SessionId> = None;
    for _ in 0..page_ceiling {
        let page = read_inventory_page(inventory, cursor).await?;
        cursor = page.resume_after;
        quiescent.extend(page.candidates);
        if cursor.is_none() {
            return Some(quiescent);
        }
    }
    let probe = read_inventory_page(inventory, cursor).await?;
    if probe.candidates.is_empty() {
        return Some(quiescent);
    }
    tracing::warn!(
        cause_code = ROTATION_CEILING_CAUSE,
        page_ceiling,
        observed_turns = quiescent.len(),
        "turn-liveness rotation exceeded the quiescent population its scan can drain"
    );
    None
}

/// Reads one page, reporting an unreadable inventory as no decision.
///
/// An unreadable inventory is not evidence that anything is stale.
async fn read_inventory_page<Inventory>(
    inventory: &Inventory,
    after: Option<SessionId>,
) -> Option<InventoryPage>
where
    Inventory: QuiescentInventory,
{
    match inventory.read_page(after).await {
        Ok(page) => Some(page),
        Err(error) => {
            report_turn_liveness_failure(&error);
            None
        }
    }
}

async fn terminalize_stale_turn<Terminalizer>(
    repository: &Terminalizer,
    candidate: StaleTurnCandidate,
    staleness_bound: StaleActiveTurnBound,
) where
    Terminalizer: StaleTurnTerminalizer,
{
    let identities = AcceptedInputTurnFailureIdentities::new(
        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
        ContextFrontierId::from_uuid(Uuid::now_v7()),
    );
    match repository.terminalize(candidate, identities).await {
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
        InventoryPage, PASS_FAILURE_CAUSE, QUIESCENT_ROTATION_PAGE_CEILING, QuiescentInventory,
        ROTATION_CEILING_CAUSE, STALE_TURN_AMBIGUOUS_CAUSE, STALE_TURN_STEERING_BLOCKED_CAUSE,
        STALE_TURN_SUPERSEDED_CAUSE, STALE_TURN_TERMINAL_CAUSE, StaleTurnTerminalizer,
        TERMINALIZATION_DEFERRED_CAUSE, TERMINALIZATIONS_PER_SCAN, TurnLivenessWake,
        drain_quiescent_rotation, next_turn_liveness_wake, reconcile_turn_liveness,
    };
    use signalbox_application::{
        StaleActiveTurnBound, StaleTurnCandidate, StaleTurnOutcome, TurnLivenessEvidence,
        TurnLivenessLedger,
    };
    use signalbox_domain::{AcceptedInputTurnFailureIdentities, SessionId, TurnAttemptId, TurnId};
    use signalbox_persistence::turn_liveness::TurnLivenessRepositoryError;
    use std::{
        sync::{Mutex, atomic::AtomicUsize, atomic::Ordering},
        time::Duration,
    };
    use uuid::Uuid;

    fn candidate(seed: u128) -> StaleTurnCandidate {
        StaleTurnCandidate::new(
            SessionId::from_uuid(Uuid::from_u128(seed)),
            TurnId::from_uuid(Uuid::from_u128(0xa_0000 + seed)),
            TurnLivenessEvidence::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(0xb_0000 + seed)),
                None,
            ),
        )
    }

    /// A page that filled, so the rotation continues past its last session.
    fn full_page(seed: u128) -> InventoryPage {
        InventoryPage {
            candidates: Box::new([candidate(seed)]),
            resume_after: Some(SessionId::from_uuid(Uuid::from_u128(seed))),
        }
    }

    /// A page that did not fill, so it is where the rotation ends.
    fn last_page(seed: u128) -> InventoryPage {
        InventoryPage {
            candidates: Box::new([candidate(seed)]),
            resume_after: None,
        }
    }

    fn empty_page() -> InventoryPage {
        InventoryPage {
            candidates: Box::new([]),
            resume_after: None,
        }
    }

    struct ScriptedInventory {
        pages: Mutex<std::vec::IntoIter<InventoryPage>>,
        reads: AtomicUsize,
    }

    impl ScriptedInventory {
        fn new<const PAGES: usize>(pages: [InventoryPage; PAGES]) -> Self {
            Self {
                pages: Mutex::new(pages.into_iter().collect::<Vec<_>>().into_iter()),
                reads: AtomicUsize::new(0),
            }
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::Relaxed)
        }
    }

    /// Returns every candidate it still holds as one page, and drops a turn
    /// when it terminalizes — which is what the real inventory does, since a
    /// terminal turn is no longer active.
    struct CountingRepository {
        remaining: Mutex<Vec<StaleTurnCandidate>>,
        terminalized: AtomicUsize,
    }

    impl CountingRepository {
        fn new(candidates: Vec<StaleTurnCandidate>) -> Self {
            Self {
                remaining: Mutex::new(candidates),
                terminalized: AtomicUsize::new(0),
            }
        }

        fn terminalized(&self) -> usize {
            self.terminalized.load(Ordering::Relaxed)
        }

        fn still_active(&self) -> usize {
            self.remaining
                .lock()
                .expect("the fixture is not poisoned")
                .len()
        }
    }

    impl QuiescentInventory for CountingRepository {
        async fn read_page(
            &self,
            _after: Option<SessionId>,
        ) -> Result<InventoryPage, TurnLivenessRepositoryError> {
            let candidates = self
                .remaining
                .lock()
                .expect("the fixture is not poisoned")
                .clone();
            Ok(InventoryPage {
                candidates: candidates.into_boxed_slice(),
                resume_after: None,
            })
        }
    }

    impl StaleTurnTerminalizer for CountingRepository {
        async fn terminalize(
            &self,
            candidate: StaleTurnCandidate,
            _identities: AcceptedInputTurnFailureIdentities,
        ) -> Result<StaleTurnOutcome, TurnLivenessRepositoryError> {
            self.terminalized.fetch_add(1, Ordering::Relaxed);
            self.remaining
                .lock()
                .expect("the fixture is not poisoned")
                .retain(|held| held.turn() != candidate.turn());
            Ok(StaleTurnOutcome::Terminalized)
        }
    }

    impl QuiescentInventory for ScriptedInventory {
        async fn read_page(
            &self,
            _after: Option<SessionId>,
        ) -> Result<InventoryPage, TurnLivenessRepositoryError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let page = self
                .pages
                .lock()
                .expect("the script is not poisoned")
                .next()
                .expect("the script supplies every read the drain takes");
            Ok(page)
        }
    }
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

    /// A cohort larger than the cap is terminalized up to it and no further,
    /// so the phase cannot push the next scan past the interval.
    #[tokio::test(start_paused = true)]
    async fn one_scan_terminalizes_no_more_than_its_cap() {
        let bound = StaleActiveTurnBound::hard_ceiling();
        let repository = CountingRepository::new(vec![candidate(1), candidate(2), candidate(3)]);
        let mut ledger = TurnLivenessLedger::new(bound);
        reconcile_turn_liveness(&repository, &mut ledger, bound, 2).await;
        tokio::time::advance(bound.get()).await;

        reconcile_turn_liveness(&repository, &mut ledger, bound, 2).await;

        assert_eq!(repository.terminalized(), 2);
        assert_eq!(repository.still_active(), 1);
    }

    /// What one scan defers the next one ends: nothing about a deferred turn
    /// changed, so it is observed unchanged and comes due again.
    #[tokio::test(start_paused = true)]
    async fn the_next_scan_ends_what_the_cap_deferred() {
        let bound = StaleActiveTurnBound::hard_ceiling();
        let repository = CountingRepository::new(vec![candidate(1), candidate(2), candidate(3)]);
        let mut ledger = TurnLivenessLedger::new(bound);
        reconcile_turn_liveness(&repository, &mut ledger, bound, 2).await;
        tokio::time::advance(bound.get()).await;
        reconcile_turn_liveness(&repository, &mut ledger, bound, 2).await;

        reconcile_turn_liveness(&repository, &mut ledger, bound, 2).await;

        assert_eq!(repository.terminalized(), 3);
        assert_eq!(repository.still_active(), 0);
    }

    /// The compiled cap is the value the page states.
    #[test]
    fn one_scan_terminalizes_at_most_sixty_four_turns() {
        assert_eq!(TERMINALIZATIONS_PER_SCAN, 64);
    }

    /// The compiled ceiling is the capacity the page states.
    #[test]
    fn the_page_ceiling_is_four_thousand_and_ninety_six() {
        assert_eq!(QUIESCENT_ROTATION_PAGE_CEILING, 4_096);
    }

    /// A population that ends inside the ceiling drains where it ends, and no
    /// probe is read because no page before it filled.
    #[tokio::test]
    async fn a_rotation_ending_inside_the_ceiling_drains() {
        let inventory = ScriptedInventory::new([full_page(1), last_page(2)]);

        let drained = drain_quiescent_rotation(&inventory, 2).await;

        assert_eq!(drained.map(|turns| turns.len()), Some(2));
        assert_eq!(inventory.reads(), 2);
    }

    /// A population filling every allowed page still drains: the probe returns
    /// nothing, which is what proves the rotation ended on the last full page.
    #[tokio::test]
    async fn an_empty_probe_proves_a_full_last_page_ended_the_rotation() {
        let inventory = ScriptedInventory::new([full_page(1), full_page(2), empty_page()]);

        let drained = drain_quiescent_rotation(&inventory, 2).await;

        assert_eq!(drained.map(|turns| turns.len()), Some(2));
        assert_eq!(inventory.reads(), 3);
    }

    /// One turn past the capacity — every allowed page full and the probe
    /// carrying a candidate — decides nothing. Folding that probe in would
    /// raise the advertised ceiling by up to a page without saying so.
    #[tokio::test]
    async fn a_candidate_bearing_probe_decides_nothing() {
        let inventory = ScriptedInventory::new([full_page(1), full_page(2), last_page(3)]);

        let drained = drain_quiescent_rotation(&inventory, 2).await;

        assert!(drained.is_none());
        assert_eq!(inventory.reads(), 3);
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
        assert_eq!(
            TERMINALIZATION_DEFERRED_CAUSE,
            "turn_liveness_terminalization_deferred"
        );
        assert_ne!(STALE_TURN_SUPERSEDED_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_ne!(STALE_TURN_AMBIGUOUS_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_ne!(STALE_TURN_STEERING_BLOCKED_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_ne!(PASS_FAILURE_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_ne!(ROTATION_CEILING_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_ne!(TERMINALIZATION_DEFERRED_CAUSE, STALE_TURN_TERMINAL_CAUSE);
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
