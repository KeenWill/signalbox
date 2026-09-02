//! Periodic supervision of turn-level liveness.
//!
//! Component deadlines cover physical operations; this pass covers the gap
//! between them, where a turn holds its session's progressing slot with no
//! operation outstanding at all. It runs on its own timer rather than inside a
//! scheduler pass, because the sessions it exists to reach are exactly the
//! ones no pass is scheduled for.

use std::{collections::VecDeque, future::Future, num::NonZeroU32};

use signalbox_application::{
    AutomaticReconciliationOperation, AutomaticReconciliationOutcome,
    ClaimedAutomaticReconciliation, ClassifyOperatorFailure, DurableTurnLivenessObservation,
    StaleActiveTurnBound, StaleTurnCandidate, StaleTurnOutcome, TurnLivenessGuardKind,
    TurnLivenessLedger, TurnLivenessScanInterval, UuidV7StartupScanIdGenerator,
};
use signalbox_domain::{
    AcceptedInputTurnFailureIdentities, ContextFrontierId, SemanticTranscriptEntryId, SessionId,
};
use signalbox_persistence::{
    automatic_reconciliation::{
        AutomaticReconciliationRepositoryError, PostgresAutomaticReconciliationRepository,
        reconciliation_deadline,
    },
    turn_liveness::{
        PostgresTurnLivenessRepository, TurnLivenessPersistenceBounds, TurnLivenessRepositoryError,
    },
};
use sqlx::PgPool;
use tokio::{
    select,
    sync::watch,
    time::{Duration, Interval, MissedTickBehavior, interval, timeout},
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

/// Why turns that came due were left for a later scan.
const TERMINALIZATION_DEFERRED_CAUSE: &str = "turn_liveness_terminalization_deferred";

/// Why one attempt ended without reaching the turn at all.
///
/// Informational rather than a warning: another transaction holding the session
/// is ordinary, and is evidence the turn may not be wedged after all.
const STALE_TURN_LOCK_UNAVAILABLE_CAUSE: &str = "turn_liveness_scheduler_row_busy";

/// Why one turn-liveness pass produced no decision.
const PASS_FAILURE_CAUSE: &str = "turn_liveness_pass_failed";

/// Why a slot-held inventory read decided nothing within its bound.
const SLOT_HELD_PAGE_TIMEOUT_CAUSE: &str = "turn_liveness_slot_held_page_timed_out";

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
// numeric-bound: guard - prevents a non-converging liveness inventory scan
const QUIESCENT_ROTATION_PAGE_CEILING: usize = 4_096;

/// One operation class's scan admission and transaction bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutomaticReconciliationNumericBounds {
    reconciliations_per_scan: Option<usize>,
    attempt_bound: Option<Duration>,
}

impl AutomaticReconciliationNumericBounds {
    /// Binds one operation class to its configured scan and transaction limits.
    pub const fn new(
        reconciliations_per_scan: Option<usize>,
        attempt_bound: Option<Duration>,
    ) -> Self {
        Self {
            reconciliations_per_scan,
            attempt_bound,
        }
    }
}

/// Declared slow-substrate conditions and their staleness multiplier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlowSubstrateNumericBounds {
    backup_enabled: bool,
    restart_enabled: bool,
    lock_convoy_enabled: bool,
    factor: NonZeroU32,
}

impl SlowSubstrateNumericBounds {
    /// Binds every declared condition and the multiplier to configuration.
    pub const fn new(
        backup_enabled: bool,
        restart_enabled: bool,
        lock_convoy_enabled: bool,
        factor: NonZeroU32,
    ) -> Self {
        Self {
            backup_enabled,
            restart_enabled,
            lock_convoy_enabled,
            factor,
        }
    }
}

/// Deployment policy for one turn-liveness scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnLivenessNumericBounds {
    terminalizations_per_scan: Option<usize>,
    slot_held_recovery_attempt_bound: Option<Duration>,
    automatic_model_call_reconciliations_per_scan: Option<usize>,
    automatic_tool_reconciliations_per_scan: Option<usize>,
    automatic_model_call_reconciliation_attempt_bound: Option<Duration>,
    automatic_tool_reconciliation_attempt_bound: Option<Duration>,
    slow_substrate_backup_enabled: bool,
    slow_substrate_restart_enabled: bool,
    slow_substrate_lock_convoy_enabled: bool,
    slow_substrate_factor: NonZeroU32,
    persistence: TurnLivenessPersistenceBounds,
}

impl TurnLivenessNumericBounds {
    /// Binds every scan limit to the validated daemon configuration.
    ///
    /// The two attempt bounds are separate because their consumers are. A
    /// slot-held recovery transaction is applied once per candidate across the
    /// fair window, so its bound multiplies into how long a stalled database
    /// delays the next watchdog wake and has to stay well inside the
    /// scheduler-pass ceiling this watchdog backstops. An automatic
    /// reconciliation transaction crosses the shared outbox and
    /// deferred-validation convoy instead, which is ordinary traffic that a
    /// ten-second deadline would refuse.
    pub const fn new(
        terminalizations_per_scan: Option<usize>,
        slot_held_recovery_attempt_bound: Option<Duration>,
        model_call: AutomaticReconciliationNumericBounds,
        tool: AutomaticReconciliationNumericBounds,
        slow_substrate: SlowSubstrateNumericBounds,
        persistence: TurnLivenessPersistenceBounds,
    ) -> Self {
        Self {
            terminalizations_per_scan,
            slot_held_recovery_attempt_bound,
            automatic_model_call_reconciliations_per_scan: model_call.reconciliations_per_scan,
            automatic_tool_reconciliations_per_scan: tool.reconciliations_per_scan,
            automatic_model_call_reconciliation_attempt_bound: model_call.attempt_bound,
            automatic_tool_reconciliation_attempt_bound: tool.attempt_bound,
            slow_substrate_backup_enabled: slow_substrate.backup_enabled,
            slow_substrate_restart_enabled: slow_substrate.restart_enabled,
            slow_substrate_lock_convoy_enabled: slow_substrate.lock_convoy_enabled,
            slow_substrate_factor: slow_substrate.factor,
            persistence,
        }
    }

    const fn automatic_reconciliation_attempt_bound(
        self,
        operation: AutomaticReconciliationOperation,
    ) -> Option<Duration> {
        match operation {
            AutomaticReconciliationOperation::ModelCall(_) => {
                self.automatic_model_call_reconciliation_attempt_bound
            }
            AutomaticReconciliationOperation::ToolAttempt(_) => {
                self.automatic_tool_reconciliation_attempt_bound
            }
        }
    }

    const fn slow_substrate_factor_for(
        self,
        backup_running: bool,
        restart_in_progress: bool,
        lock_convoy_detected: bool,
    ) -> NonZeroU32 {
        if (backup_running && self.slow_substrate_backup_enabled)
            || (restart_in_progress && self.slow_substrate_restart_enabled)
            || (lock_convoy_detected && self.slow_substrate_lock_convoy_enabled)
        {
            self.slow_substrate_factor
        } else {
            NonZeroU32::MIN
        }
    }
}

/// What one scan's terminalization phase actually did.
///
/// Attempting a turn is not ending one: a candidate can be superseded under the
/// lock, refused because steering is pending on it, or fail its transaction. An
/// operator counting watchdog work needs those apart, so the phase reports each
/// rather than reporting its window's length.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TerminalizationTally {
    terminalized: usize,
    superseded: usize,
    lock_unavailable: usize,
    failed: usize,
}

impl TerminalizationTally {
    fn record(&mut self, outcome: AttemptOutcome) {
        match outcome {
            AttemptOutcome::Decided(StaleTurnOutcome::Terminalized) => self.terminalized += 1,
            AttemptOutcome::Decided(StaleTurnOutcome::Superseded) => self.superseded += 1,
            AttemptOutcome::LockUnavailable => self.lock_unavailable += 1,
            AttemptOutcome::Failed => self.failed += 1,
        }
    }

    const fn attempted(self) -> usize {
        self.terminalized + self.superseded + self.lock_unavailable + self.failed
    }
}

/// What one attempt on one turn came to.
///
/// A refused lock is neither a decision nor a fault: it means another
/// transaction held the session while this one waited its bound. Keeping it
/// apart from failure is what stops ordinary contention reading as a defect,
/// and keeping it apart from a decision is what makes the turn wait for its
/// lap rather than be counted as handled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttemptOutcome {
    Decided(StaleTurnOutcome),
    LockUnavailable,
    Failed,
}

/// The capped, rotating window of due turns one scan terminalizes.
///
/// The cap alone is not fair. A turn can be due forever without ever ending —
/// one holding pending steering is refused every time it is attempted — so a
/// window taken from the front of the due order would hand those turns every
/// slot on every scan, and a turn behind them would wait for one that never
/// comes.
///
/// The window therefore works in laps, and a lap is a *membership* fixed when
/// it opens: the sessions due at that moment, in order. Successive scans take
/// the next `capacity` of them that are still due, and the lap ends when its
/// members are exhausted, whereupon the next scan opens a fresh lap over
/// whatever is due then. Every turn due when a lap opens is attempted within
/// `⌈members ÷ capacity⌉` scans of it, and nothing about the turns arriving
/// meanwhile can change that.
///
/// Two narrower versions of this window failed to keep that guarantee, both by
/// deciding membership from what was due at each scan rather than at the lap's
/// opening. Following the due order alone let arrivals, which carry
/// time-ordered session identities and so sort last, be served forever ahead of
/// the turns behind the cursor. Bounding the lap by the greatest session it
/// opened with fixed that but not its converse: a session that already existed
/// below the bound and became due mid-lap still joined, pushing the lap's own
/// members back. A membership has neither failure because it is not a
/// predicate over what is due — it is a list.
///
/// A member that stops being due before its slot comes is skipped rather than
/// waited for, since it is no longer this pass's business.
struct TerminalizationWindow {
    capacity: Option<usize>,
    lap: VecDeque<SessionId>,
}

impl TerminalizationWindow {
    fn new(capacity: Option<usize>) -> Self {
        Self {
            capacity,
            lap: VecDeque::new(),
        }
    }

    /// Returns this scan's turns, consuming that many members of the lap.
    ///
    /// `due` is in ascending session order and holds one turn per session, so a
    /// member is found by binary search rather than by scanning a cohort that
    /// may be very large.
    fn take(&mut self, due: &[StaleTurnCandidate]) -> Vec<StaleTurnCandidate> {
        if self.lap.is_empty() {
            self.lap = due.iter().map(|candidate| candidate.session()).collect();
        }
        let mut window =
            Vec::with_capacity(self.capacity.unwrap_or(self.lap.len()).min(self.lap.len()));
        while self.capacity.is_none_or(|capacity| window.len() < capacity) {
            let Some(member) = self.lap.pop_front() else {
                break;
            };
            if let Ok(found) = due.binary_search_by_key(&member, |candidate| candidate.session()) {
                window.push(due[found]);
            }
        }
        window
    }
}

/// One inventory read, as the rotation consumes it.
///
/// The rotation cares about three things a page reports: what it observed, how
/// many rows it was drawn from, and whether anything follows it. The row count
/// is separate from the observations because a row whose evidence cannot be
/// read is dropped, and every question about the rotation's extent is a
/// question about the statement rather than about what this pass could read.
/// Naming them here is what lets the drain be exercised without a database,
/// which several boundary defects in it have earned.
struct InventoryPage {
    candidates: Box<[StaleTurnCandidate]>,
    rows: usize,
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
        self.terminalize_stale_turn(candidate, identities, &mut UuidV7StartupScanIdGenerator)
            .await
    }
}

/// Reads one page of the quiescent inventory.
trait QuiescentInventory {
    fn read_page(
        &self,
        after: Option<SessionId>,
    ) -> impl Future<Output = Result<InventoryPage, TurnLivenessRepositoryError>> + Send;
}

/// Persists one complete guard population and returns its advanced ordinals.
trait DurableObservationRecorder {
    fn record_observation(
        &self,
        guard: TurnLivenessGuardKind,
        candidates: &[StaleTurnCandidate],
    ) -> impl Future<
        Output = Result<Box<[DurableTurnLivenessObservation]>, TurnLivenessRepositoryError>,
    > + Send;
}

impl DurableObservationRecorder for PostgresTurnLivenessRepository {
    async fn record_observation(
        &self,
        guard: TurnLivenessGuardKind,
        candidates: &[StaleTurnCandidate],
    ) -> Result<Box<[DurableTurnLivenessObservation]>, TurnLivenessRepositoryError> {
        self.record_complete_observation(guard, candidates).await
    }
}

impl QuiescentInventory for PostgresTurnLivenessRepository {
    async fn read_page(
        &self,
        after: Option<SessionId>,
    ) -> Result<InventoryPage, TurnLivenessRepositoryError> {
        let page = self.quiescent_active_turns(after).await?;
        let resume_after = page.resume_after();
        let rows = page.rows();
        Ok(InventoryPage {
            candidates: page.into_candidates(),
            rows,
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

/// One operation class's durable attempt and backoff policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutomaticReconciliationRuntimePolicy {
    attempt_budget: u32,
    base_backoff: Option<Duration>,
    backoff_cap: Option<Duration>,
}

impl AutomaticReconciliationRuntimePolicy {
    /// Binds one operation class's durable retry policy to configuration.
    pub const fn new(
        attempt_budget: u32,
        base_backoff: Option<Duration>,
        backoff_cap: Option<Duration>,
    ) -> Self {
        Self {
            attempt_budget,
            base_backoff,
            backoff_cap,
        }
    }
}

/// Periodic turn-liveness supervision over the shared daemon pool.
#[derive(Clone, Debug)]
pub struct TurnLivenessRuntime {
    repository: PostgresTurnLivenessRepository,
    automatic_reconciliation: PostgresAutomaticReconciliationRepository,
    quiescent_staleness_bound: Option<StaleActiveTurnBound>,
    slot_held_staleness_bound: Option<StaleActiveTurnBound>,
    scan_interval: Option<TurnLivenessScanInterval>,
    automatic_model_call_reconciliation_attempt_budget: u32,
    automatic_tool_reconciliation_attempt_budget: u32,
    numeric_bounds: TurnLivenessNumericBounds,
}

impl TurnLivenessRuntime {
    /// Supervises turn liveness with the supplied bound and cadence.
    ///
    /// The independent staleness bounds govern quiescent and slot-held
    /// observation. The required deployment configuration may disable stale-turn
    /// terminalization with `none` while leaving ambiguity reconciliation
    /// active.
    pub fn new(
        pool: PgPool,
        quiescent_staleness_bound: Option<StaleActiveTurnBound>,
        slot_held_staleness_bound: Option<StaleActiveTurnBound>,
        scan_interval: Option<TurnLivenessScanInterval>,
        model_call: AutomaticReconciliationRuntimePolicy,
        tool: AutomaticReconciliationRuntimePolicy,
        numeric_bounds: TurnLivenessNumericBounds,
    ) -> Self {
        Self {
            repository: PostgresTurnLivenessRepository::new(
                pool.clone(),
                numeric_bounds.persistence,
            ),
            automatic_reconciliation: PostgresAutomaticReconciliationRepository::new(pool.clone())
                .with_class_policies(
                    model_call.attempt_budget,
                    model_call.base_backoff,
                    model_call.backoff_cap,
                    tool.attempt_budget,
                    tool.base_backoff,
                    tool.backoff_cap,
                ),
            quiescent_staleness_bound,
            slot_held_staleness_bound,
            scan_interval,
            automatic_model_call_reconciliation_attempt_budget: model_call.attempt_budget,
            automatic_tool_reconciliation_attempt_budget: tool.attempt_budget,
            numeric_bounds,
        }
    }

    /// Scans until shutdown, terminalizing every turn observed boundedly stale.
    ///
    /// A failed pass changes nothing and is retried at the next interval, so no
    /// pass outcome ends this task. Each pass reads its whole rotation before
    /// deciding, paging only within itself — nothing about that paging survives
    /// a pass; paging across passes would make an incomplete population look
    /// complete. Repeated-observation state survives only in the database.
    pub async fn run(self, shutdown: watch::Receiver<bool>) {
        let Some(scan_interval) = self.scan_interval else {
            let mut shutdown = shutdown;
            while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
            return;
        };
        let numeric_bounds = self.numeric_bounds;
        let Some(quiescent_staleness_bound) = self.quiescent_staleness_bound else {
            run_ambiguous_operation_watchdog(
                self.automatic_reconciliation,
                scan_interval,
                self.automatic_model_call_reconciliation_attempt_budget,
                self.automatic_tool_reconciliation_attempt_budget,
                numeric_bounds,
                shutdown,
            )
            .await;
            return;
        };
        let Some(slot_held_staleness_bound) = self.slot_held_staleness_bound else {
            run_ambiguous_operation_watchdog(
                self.automatic_reconciliation,
                scan_interval,
                self.automatic_model_call_reconciliation_attempt_budget,
                self.automatic_tool_reconciliation_attempt_budget,
                numeric_bounds,
                shutdown,
            )
            .await;
            return;
        };
        let quiescent_shutdown = shutdown.clone();
        let slot_held_shutdown = shutdown.clone();
        let quiescent = run_quiescent_watchdog(
            self.repository.clone(),
            quiescent_staleness_bound,
            scan_interval,
            numeric_bounds,
            quiescent_shutdown,
        );
        let slot_held = run_slot_held_watchdog(
            self.repository,
            slot_held_staleness_bound,
            scan_interval,
            numeric_bounds,
            slot_held_shutdown,
        );
        let ambiguous_operations = run_ambiguous_operation_watchdog(
            self.automatic_reconciliation,
            scan_interval,
            self.automatic_model_call_reconciliation_attempt_budget,
            self.automatic_tool_reconciliation_attempt_budget,
            numeric_bounds,
            shutdown,
        );
        tokio::join!(quiescent, slot_held, ambiguous_operations);
    }
}

async fn run_quiescent_watchdog(
    repository: PostgresTurnLivenessRepository,
    staleness_bound: StaleActiveTurnBound,
    scan_interval: TurnLivenessScanInterval,
    numeric_bounds: TurnLivenessNumericBounds,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = interval(scan_interval.get());
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let ledger = TurnLivenessLedger::new(staleness_bound, scan_interval);
    let mut window = TerminalizationWindow::new(numeric_bounds.terminalizations_per_scan);
    let mut restart_in_progress = numeric_bounds.slow_substrate_restart_enabled;
    loop {
        match next_turn_liveness_wake(&mut shutdown, &mut ticker).await {
            TurnLivenessWake::Shutdown => return,
            TurnLivenessWake::Scan => {
                let (backup_running, lock_convoy_detected) =
                    match repository.slow_substrate_conditions().await {
                        Ok(conditions) => conditions,
                        Err(error) => {
                            report_turn_liveness_failure(&error);
                            continue;
                        }
                    };
                let _ = reconcile_turn_liveness(
                    &repository,
                    ledger,
                    TurnLivenessGuardKind::Quiescent,
                    numeric_bounds.slow_substrate_factor_for(
                        backup_running,
                        restart_in_progress,
                        lock_convoy_detected,
                    ),
                    staleness_bound,
                    &mut window,
                )
                .await;
                restart_in_progress = false;
            }
        }
    }
}

async fn run_slot_held_watchdog(
    repository: PostgresTurnLivenessRepository,
    staleness_bound: StaleActiveTurnBound,
    scan_interval: TurnLivenessScanInterval,
    numeric_bounds: TurnLivenessNumericBounds,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = interval(scan_interval.get());
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let ledger = TurnLivenessLedger::new(staleness_bound, scan_interval);
    let mut window = TerminalizationWindow::new(numeric_bounds.terminalizations_per_scan);
    let mut restart_in_progress = numeric_bounds.slow_substrate_restart_enabled;
    loop {
        match next_turn_liveness_wake(&mut shutdown, &mut ticker).await {
            TurnLivenessWake::Shutdown => return,
            TurnLivenessWake::Scan => {
                let (backup_running, lock_convoy_detected) =
                    match repository.slow_substrate_conditions().await {
                        Ok(conditions) => conditions,
                        Err(error) => {
                            report_turn_liveness_failure(&error);
                            continue;
                        }
                    };
                reconcile_slot_held_turns(
                    &repository,
                    ledger,
                    numeric_bounds.slow_substrate_factor_for(
                        backup_running,
                        restart_in_progress,
                        lock_convoy_detected,
                    ),
                    &mut window,
                    numeric_bounds.slot_held_recovery_attempt_bound,
                    &mut shutdown,
                )
                .await;
                restart_in_progress = false;
            }
        }
    }
}

async fn run_ambiguous_operation_watchdog(
    repository: PostgresAutomaticReconciliationRepository,
    scan_interval: TurnLivenessScanInterval,
    automatic_model_call_reconciliation_attempt_budget: u32,
    automatic_tool_reconciliation_attempt_budget: u32,
    numeric_bounds: TurnLivenessNumericBounds,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = interval(scan_interval.get());
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        match next_turn_liveness_wake(&mut shutdown, &mut ticker).await {
            TurnLivenessWake::Shutdown => return,
            TurnLivenessWake::Scan => {
                reconcile_ambiguous_operations(
                    &repository,
                    automatic_model_call_reconciliation_attempt_budget,
                    automatic_tool_reconciliation_attempt_budget,
                    numeric_bounds,
                    &mut shutdown,
                )
                .await;
            }
        }
    }
}

async fn reconcile_slot_held_turns(
    inventory: &PostgresTurnLivenessRepository,
    ledger: TurnLivenessLedger,
    slow_substrate_factor: NonZeroU32,
    window: &mut TerminalizationWindow,
    recovery_attempt_bound: Option<Duration>,
    shutdown: &mut watch::Receiver<bool>,
) {
    let Some(active) = drain_slot_held_rotation(inventory, recovery_attempt_bound).await else {
        return;
    };
    let observations = match inventory
        .record_complete_observation(TurnLivenessGuardKind::SlotHeld, &active)
        .await
    {
        Ok(observations) => observations,
        Err(error) => {
            report_turn_liveness_failure(&error);
            return;
        }
    };
    let due = ledger.reconcile(&observations, slow_substrate_factor);
    let attempted = window.take(&due);
    for candidate in attempted {
        let identities = AcceptedInputTurnFailureIdentities::new(
            SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
            ContextFrontierId::from_uuid(Uuid::now_v7()),
        );
        let mut ids = UuidV7StartupScanIdGenerator;
        let attempt = optional_timeout(
            recovery_attempt_bound,
            inventory.recover_observed_slot_held_turn(candidate, identities, &mut ids),
        );
        let Some(outcome) = complete_before_shutdown(shutdown, attempt).await else {
            return;
        };
        match outcome {
            Ok(Ok(Some(outcome))) => tracing::warn!(
                cause_code = "turn_liveness_slot_held_recovered",
                session_id = %candidate.session().as_uuid(),
                turn_id = %candidate.turn().as_uuid(),
                recovery_outcome = ?outcome,
                "slot-held turn exceeded the liveness bound and was handed to durable startup recovery"
            ),
            Ok(Ok(None)) => tracing::info!(
                cause_code = "turn_liveness_slot_held_superseded",
                session_id = %candidate.session().as_uuid(),
                turn_id = %candidate.turn().as_uuid(),
                "slot-held turn or its progress evidence changed under the lock and was left alone"
            ),
            Ok(Err(error)) => report_slot_held_recovery_failure(candidate, &error),
            Err(_) => tracing::error!(
                failure_class = ?signalbox_application::OperatorFailureClass::Infrastructure { commit_ambiguous: true },
                cause_code = "turn_liveness_slot_held_recovery_timed_out",
                session_id = %candidate.session().as_uuid(),
                turn_id = %candidate.turn().as_uuid(),
                attempt_bound_seconds = ?recovery_attempt_bound.map(|bound| bound.as_secs()),
                "slot-held turn recovery exceeded its bound; unchanged evidence remains due"
            ),
        }
    }
}

async fn drain_slot_held_rotation(
    inventory: &PostgresTurnLivenessRepository,
    recovery_attempt_bound: Option<Duration>,
) -> Option<Vec<StaleTurnCandidate>> {
    let mut active = Vec::new();
    let mut cursor = None;
    for _ in 0..QUIESCENT_ROTATION_PAGE_CEILING {
        let page =
            read_slot_held_inventory_page(inventory, cursor, recovery_attempt_bound, "paging")
                .await?;
        cursor = page.resume_after();
        active.extend(page.into_candidates());
        if cursor.is_none() {
            return Some(active);
        }
    }
    let probe = read_slot_held_inventory_page(
        inventory,
        cursor,
        recovery_attempt_bound,
        "rotation_ceiling_probe",
    )
    .await?;
    if probe.rows() == 0 {
        return Some(active);
    }
    tracing::warn!(
        cause_code = "turn_liveness_slot_held_rotation_ceiling_reached",
        page_ceiling = QUIESCENT_ROTATION_PAGE_CEILING,
        observed_turns = active.len(),
        "slot-held turn rotation exceeded the population its scan can drain"
    );
    None
}

async fn read_slot_held_inventory_page(
    inventory: &PostgresTurnLivenessRepository,
    cursor: Option<SessionId>,
    recovery_attempt_bound: Option<Duration>,
    phase: &'static str,
) -> Option<signalbox_persistence::turn_liveness::QuiescentActiveTurnPage> {
    match optional_timeout(
        recovery_attempt_bound,
        inventory.slot_held_active_turns(cursor),
    )
    .await
    {
        Ok(Ok(page)) => Some(page),
        Ok(Err(error)) => {
            report_turn_liveness_failure(&error);
            None
        }
        Err(_) => {
            report_slot_held_page_timeout(phase, recovery_attempt_bound);
            None
        }
    }
}

fn report_slot_held_recovery_failure(
    candidate: StaleTurnCandidate,
    error: &TurnLivenessRepositoryError,
) {
    let failure_class = error.operator_failure_class();
    tracing::error!(
        ?failure_class,
        cause_code = "turn_liveness_slot_held_recovery_failed",
        session_id = %candidate.session().as_uuid(),
        turn_id = %candidate.turn().as_uuid(),
        "slot-held turn recovery failed; unchanged durable evidence remains due"
    );
}

/// Reports whether the batch may commission another reconciliation.
///
/// A requested stop ends the batch here rather than at its per-scan ceiling: a
/// scan may commission as many transactions as that ceiling allows, each with
/// its own wall-clock deadline, so waiting behind the remaining population is
/// how a drain outlives its grace window.
fn batch_admits_another_reconciliation(
    shutdown: &watch::Receiver<bool>,
    reconciliations: usize,
    ceiling: Option<usize>,
) -> bool {
    !*shutdown.borrow() && ceiling.is_none_or(|limit| reconciliations < limit)
}

/// Claims and applies one bounded window of durable ambiguous-call work.
///
/// Every stage observes shutdown ahead of its own deadline, exactly as the
/// slot-held watchdog's sequential recovery transactions do. A stage abandoned
/// that way queues an ordinary rollback while its database-side budget — which
/// expires first by construction — releases the backend, and the claimed
/// attempt's own recorded deadline stays the recovery authority.
async fn reconcile_ambiguous_operations(
    repository: &PostgresAutomaticReconciliationRepository,
    automatic_model_call_reconciliation_attempt_budget: u32,
    automatic_tool_reconciliation_attempt_budget: u32,
    numeric_bounds: TurnLivenessNumericBounds,
    shutdown: &mut watch::Receiver<bool>,
) {
    let mut reconciliations = 0usize;
    let mut model_call_reconciliations = 0usize;
    let mut tool_reconciliations = 0usize;
    let mut model_call_empty = false;
    let mut tool_empty = false;
    let mut prefer_model_call = true;
    let inventory_attempt_bound = match (
        numeric_bounds.automatic_model_call_reconciliation_attempt_bound,
        numeric_bounds.automatic_tool_reconciliation_attempt_bound,
    ) {
        (Some(model), Some(tool)) => Some(model.max(tool)),
        (None, _) | (_, None) => None,
    };
    loop {
        let model_call_admitted = !model_call_empty
            && batch_admits_another_reconciliation(
                shutdown,
                model_call_reconciliations,
                numeric_bounds.automatic_model_call_reconciliations_per_scan,
            );
        let tool_admitted = !tool_empty
            && batch_admits_another_reconciliation(
                shutdown,
                tool_reconciliations,
                numeric_bounds.automatic_tool_reconciliations_per_scan,
            );
        let claim_model_call = match (model_call_admitted, tool_admitted, prefer_model_call) {
            (true, true, preference) => preference,
            (true, false, _) => true,
            (false, true, _) => false,
            (false, false, _) => break,
        };
        prefer_model_call = !claim_model_call;
        let claim = timeout(reconciliation_deadline(inventory_attempt_bound), async {
            if claim_model_call {
                repository.claim_due_model_call().await
            } else {
                repository.claim_due_tool_attempt().await
            }
        });
        let Some(claim_outcome) = complete_before_shutdown(shutdown, claim).await else {
            return;
        };
        let batch = match claim_outcome {
            Ok(Ok(batch)) => batch,
            Ok(Err(error)) => {
                report_automatic_reconciliation_failure("inventory", None, &error);
                return;
            }
            Err(_) => {
                report_automatic_reconciliation_timeout("inventory", None, inventory_attempt_bound);
                return;
            }
        };
        for exhausted in batch.exhausted() {
            let (operation_kind, operation_id) = operation_log_fields(exhausted.operation());
            let attempt_budget = match exhausted.operation() {
                AutomaticReconciliationOperation::ModelCall(_) => {
                    automatic_model_call_reconciliation_attempt_budget
                }
                AutomaticReconciliationOperation::ToolAttempt(_) => {
                    automatic_tool_reconciliation_attempt_budget
                }
            };
            tracing::warn!(
                cause_code = "automatic_reconciliation_exhausted",
                session_id = %exhausted.session().as_uuid(),
                turn_id = %exhausted.turn().as_uuid(),
                operation_kind,
                operation_id = %operation_id,
                attempt_budget,
                "automatic operation reconciliation exhausted; the turn remains visibly parked for an operator"
            );
        }
        let Some(claimed) = batch.claimed().first().copied() else {
            if claim_model_call {
                model_call_empty = true;
            } else {
                tool_empty = true;
            }
            continue;
        };
        let attempt_bound =
            numeric_bounds.automatic_reconciliation_attempt_bound(claimed.operation());
        let (operation_kind, operation_id) = operation_log_fields(claimed.operation());
        let attempt = timeout(
            reconciliation_deadline(attempt_bound),
            repository.reconcile(claimed),
        );
        let Some(attempt_outcome) = complete_before_shutdown(shutdown, attempt).await else {
            return;
        };
        match attempt_outcome {
            Ok(Ok(AutomaticReconciliationOutcome::Reconciled)) => tracing::warn!(
                cause_code = "model_call_automatically_reconciled",
                session_id = %claimed.session().as_uuid(),
                turn_id = %claimed.turn().as_uuid(),
                operation_kind,
                operation_id = %operation_id,
                attempt = claimed.attempt().get(),
                "ambiguous-operation turn terminalized through automatic reconciliation"
            ),
            Ok(Ok(AutomaticReconciliationOutcome::Superseded)) => tracing::info!(
                cause_code = "automatic_reconciliation_superseded",
                session_id = %claimed.session().as_uuid(),
                turn_id = %claimed.turn().as_uuid(),
                operation_kind,
                operation_id = %operation_id,
                attempt = claimed.attempt().get(),
                "automatic reconciliation found that the ambiguity had moved on"
            ),
            Ok(Err(error)) => {
                report_automatic_reconciliation_failure("attempt", Some(claimed), &error);
                if !matches!(
                    error.operator_failure_class(),
                    signalbox_application::OperatorFailureClass::Infrastructure {
                        commit_ambiguous: true
                    }
                ) {
                    let record_failure = timeout(
                        reconciliation_deadline(attempt_bound),
                        repository.record_failure(claimed, error.failure_kind()),
                    );
                    let Some(record_outcome) =
                        complete_before_shutdown(shutdown, record_failure).await
                    else {
                        return;
                    };
                    match record_outcome {
                        Ok(Ok(())) => {}
                        Ok(Err(record_error)) => report_automatic_reconciliation_failure(
                            "failure_record",
                            Some(claimed),
                            &record_error,
                        ),
                        Err(_) => report_automatic_reconciliation_timeout(
                            "failure_record",
                            Some(claimed),
                            attempt_bound,
                        ),
                    }
                }
            }
            Err(_) => {
                report_automatic_reconciliation_timeout("attempt", Some(claimed), attempt_bound)
            }
        }
        reconciliations = reconciliations.saturating_add(1);
        match claimed.operation() {
            AutomaticReconciliationOperation::ModelCall(_) => {
                model_call_reconciliations = model_call_reconciliations.saturating_add(1);
            }
            AutomaticReconciliationOperation::ToolAttempt(_) => {
                tool_reconciliations = tool_reconciliations.saturating_add(1);
            }
        }
    }
    if *shutdown.borrow() {
        tracing::info!(
            cause_code = "automatic_reconciliation_batch_preempted",
            reconciliations,
            model_call_reconciliations,
            tool_reconciliations,
            model_attempt_ceiling = ?numeric_bounds.automatic_model_call_reconciliations_per_scan,
            tool_attempt_ceiling = ?numeric_bounds.automatic_tool_reconciliations_per_scan,
            "shutdown ended the automatic reconciliation batch; due work remains discoverable"
        );
        return;
    }
    tracing::info!(
        cause_code = "automatic_reconciliation_scan_ceiling_reached",
        model_call_reconciliations,
        tool_reconciliations,
        model_attempt_ceiling = ?numeric_bounds.automatic_model_call_reconciliations_per_scan,
        tool_attempt_ceiling = ?numeric_bounds.automatic_tool_reconciliations_per_scan,
        "automatic reconciliation reached its per-scan ceiling; due work remains discoverable"
    );
}

fn report_automatic_reconciliation_timeout(
    stage: &'static str,
    claimed: Option<ClaimedAutomaticReconciliation>,
    recovery_attempt_bound: Option<Duration>,
) {
    match claimed {
        Some(claimed) => {
            let (operation_kind, operation_id) = operation_log_fields(claimed.operation());
            tracing::error!(
            failure_class = ?signalbox_application::OperatorFailureClass::Infrastructure { commit_ambiguous: true },
            cause_code = "automatic_reconciliation_timed_out",
            stage,
            session_id = %claimed.session().as_uuid(),
            turn_id = %claimed.turn().as_uuid(),
            operation_kind,
            operation_id = %operation_id,
            attempt = claimed.attempt().get(),
            attempt_bound_seconds = ?recovery_attempt_bound.map(|bound| bound.as_secs()),
            "automatic operation reconciliation exceeded its bound; the durable attempt remains recoverable"
            )
        }
        None => tracing::error!(
            failure_class = ?signalbox_application::OperatorFailureClass::Infrastructure { commit_ambiguous: true },
            cause_code = "automatic_reconciliation_timed_out",
            stage,
            attempt_bound_seconds = ?recovery_attempt_bound.map(|bound| bound.as_secs()),
            "automatic operation reconciliation inventory exceeded its bound"
        ),
    }
}

fn report_automatic_reconciliation_failure(
    stage: &'static str,
    claimed: Option<ClaimedAutomaticReconciliation>,
    error: &AutomaticReconciliationRepositoryError,
) {
    let failure_class = error.operator_failure_class();
    let cause_code = error.operator_failure_cause_code();
    match claimed {
        Some(claimed) => {
            let (operation_kind, operation_id) = operation_log_fields(claimed.operation());
            tracing::error!(
            ?failure_class,
            cause_code,
            stage,
            session_id = %claimed.session().as_uuid(),
            turn_id = %claimed.turn().as_uuid(),
            operation_kind,
            operation_id = %operation_id,
            attempt = claimed.attempt().get(),
            "automatic operation reconciliation failed; durable backoff remains authoritative"
            )
        }
        None => tracing::error!(
            ?failure_class,
            cause_code,
            stage,
            "automatic operation reconciliation inventory failed; the next watchdog scan retries"
        ),
    }
}

fn operation_log_fields(operation: AutomaticReconciliationOperation) -> (&'static str, Uuid) {
    match operation {
        AutomaticReconciliationOperation::ModelCall(call) => ("model_call", *call.as_uuid()),
        AutomaticReconciliationOperation::ToolAttempt(attempt) => {
            ("tool_attempt", *attempt.as_uuid())
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

/// Completes one recovery stage unless daemon shutdown wins first.
///
/// The future stays pinned while false-valued watch notifications are ignored,
/// so observing a configuration-neutral notification never cancels a database
/// transaction and reissues it.
async fn complete_before_shutdown<Output>(
    shutdown: &mut watch::Receiver<bool>,
    future: impl Future<Output = Output>,
) -> Option<Output> {
    tokio::pin!(future);
    loop {
        if *shutdown.borrow() {
            return None;
        }
        select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return None;
                }
            }
            output = &mut future => return Some(output),
        }
    }
}

async fn optional_timeout<F>(
    bound: Option<Duration>,
    future: F,
) -> Result<F::Output, tokio::time::error::Elapsed>
where
    F: Future,
{
    match bound {
        Some(bound) => timeout(bound, future).await,
        None => Ok(future.await),
    }
}

/// Folds one durable observation into the ledger and acts on what is due.
///
/// The whole rotation is drained before anything is decided. Paging across
/// wakes instead would tie the time to reach a turn to the population size,
/// and the staleness bound is a property of the binary, not of how many
/// sessions happen to be quiescent.
///
/// That leaves the read phase uncapped where the terminalization phase is
/// capped, which is deliberate rather than an oversight: the ledger is fed
/// complete populations, so a read phase that stopped partway would report the
/// turns it never reached as having left the quiescent shape and restart their
/// bounds. The phases differ in cost as well as in kind — a page is one indexed
/// statement, a terminalization is a locked transaction — and the count is
/// `⌊population ÷ 256⌋ + 1` — the probe is needed only when the population is an
/// exact multiple of the page size, since any other ends on an underfilled page
/// — which is one read for an idle deployment and reaches the ceiling only for
/// a population that decides nothing anyway. A
/// scan whose reads outlast the interval delays the next tick rather than
/// overlapping it, so the pass degrades to observing less often.
async fn reconcile_turn_liveness<Repository>(
    repository: &Repository,
    ledger: TurnLivenessLedger,
    guard: TurnLivenessGuardKind,
    slow_substrate_factor: NonZeroU32,
    staleness_bound: StaleActiveTurnBound,
    window: &mut TerminalizationWindow,
) -> TerminalizationTally
where
    Repository: QuiescentInventory + DurableObservationRecorder + StaleTurnTerminalizer,
{
    // A rotation that could not be drained is not a smaller population; the
    // pass therefore ends without changing durable observation evidence.
    let Some(quiescent) =
        drain_quiescent_rotation(repository, QUIESCENT_ROTATION_PAGE_CEILING).await
    else {
        return TerminalizationTally::default();
    };
    let observations = match repository.record_observation(guard, &quiescent).await {
        Ok(observations) => observations,
        Err(error) => {
            report_turn_liveness_failure(&error);
            return TerminalizationTally::default();
        }
    };
    let due = ledger.reconcile(&observations, slow_substrate_factor);
    let attempted = window.take(&due);
    let mut tally = TerminalizationTally::default();
    for candidate in &attempted {
        tally.record(terminalize_stale_turn(repository, *candidate, staleness_bound).await);
    }
    // A cohort larger than the window takes a scan per windowful to attempt in
    // full, so what deferral costs is laps, not one interval. The alternative
    // is worse in kind rather than degree: draining a cohort in one scan delays
    // the next observation of every other session by however long the cohort
    // takes, which is the property this pass is built to keep.
    let deferred = due.len() - attempted.len();
    if deferred > 0 {
        tracing::info!(
            cause_code = TERMINALIZATION_DEFERRED_CAUSE,
            deferred_turns = deferred,
            attempted_turns = tally.attempted(),
            terminalized_turns = tally.terminalized,
            superseded_turns = tally.superseded,
            lock_unavailable_turns = tally.lock_unavailable,
            failed_turns = tally.failed,
            "more turns came due than one scan attempts; the rest wait for a later scan"
        );
    }
    tally
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
    // The probe asks whether the statement has anything left, which is a
    // question about its rows rather than about what this pass could read from
    // them: a probe whose rows were all dropped is still a page past the
    // ceiling, and treating it as the end would report a truncated population
    // as the whole of one.
    let probe = read_inventory_page(inventory, cursor).await?;
    if probe.rows == 0 {
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

/// Attempts one turn, reporting what it decided and `None` if it could not.
async fn terminalize_stale_turn<Terminalizer>(
    repository: &Terminalizer,
    candidate: StaleTurnCandidate,
    staleness_bound: StaleActiveTurnBound,
) -> AttemptOutcome
where
    Terminalizer: StaleTurnTerminalizer,
{
    let identities = AcceptedInputTurnFailureIdentities::new(
        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
        ContextFrontierId::from_uuid(Uuid::now_v7()),
    );
    match repository.terminalize(candidate, identities).await {
        Ok(outcome @ StaleTurnOutcome::Terminalized) => {
            tracing::warn!(
            cause_code = STALE_TURN_TERMINAL_CAUSE,
            session_id = %candidate.session().as_uuid(),
            turn_id = %candidate.turn().as_uuid(),
            staleness_bound_seconds = staleness_bound.as_secs(),
            "active turn terminalized as failed after its durable evidence stood still"
            );
            AttemptOutcome::Decided(outcome)
        }
        Ok(outcome @ StaleTurnOutcome::Superseded) => {
            tracing::info!(
            cause_code = STALE_TURN_SUPERSEDED_CAUSE,
            session_id = %candidate.session().as_uuid(),
            turn_id = %candidate.turn().as_uuid(),
            "stale active turn changed under its locks and was left alone"
            );
            AttemptOutcome::Decided(outcome)
        }
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
        ) => {
            tracing::warn!(
            failure_class = ?error.operator_failure_class(),
            cause_code = STALE_TURN_AMBIGUOUS_CAUSE,
            detail_code = error.operator_failure_cause_code(),
            session_id = %candidate.session().as_uuid(),
            turn_id = %candidate.turn().as_uuid(),
            staleness_bound_seconds = staleness_bound.as_secs(),
            "stale active turn may or may not have terminalized; its commit was not acknowledged"
            );
            AttemptOutcome::Failed
        }
        Err(error @ TurnLivenessRepositoryError::TerminalizationLockUnavailable(_)) => {
            tracing::info!(
                failure_class = ?error.operator_failure_class(),
                cause_code = STALE_TURN_LOCK_UNAVAILABLE_CAUSE,
                detail_code = error.operator_failure_cause_code(),
                session_id = %candidate.session().as_uuid(),
                turn_id = %candidate.turn().as_uuid(),
                "stale active turn was busy under another transaction and was left for its next lap"
            );
            AttemptOutcome::LockUnavailable
        }
        Err(error) => {
            tracing::warn!(
            failure_class = ?error.operator_failure_class(),
            cause_code = PASS_FAILURE_CAUSE,
            detail_code = error.operator_failure_cause_code(),
            session_id = %candidate.session().as_uuid(),
            turn_id = %candidate.turn().as_uuid(),
            "stale active turn was not terminalized"
            );
            AttemptOutcome::Failed
        }
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

/// Reports a slot-held inventory read that exceeded its bound.
///
/// The slot-held scan is the durable backstop for a turn whose scheduler pass
/// expired, so a read that keeps timing out silently would retire that backstop
/// invisibly: `reconcile_slot_held_turns` returns at its first statement on
/// every scan and no turn is ever reached. Every sibling bound in this file and
/// in the scheduler-expiry path emits a cause code, and so does this one.
fn report_slot_held_page_timeout(phase: &'static str, recovery_attempt_bound: Option<Duration>) {
    tracing::error!(
        failure_class = ?signalbox_application::OperatorFailureClass::Infrastructure { commit_ambiguous: false },
        cause_code = SLOT_HELD_PAGE_TIMEOUT_CAUSE,
        phase,
        attempt_bound_seconds = ?recovery_attempt_bound.map(|bound| bound.as_secs()),
        "slot-held inventory read exceeded its bound; the slot-held backstop made no progress this scan"
    );
}

#[cfg(test)]
mod tests {
    use signalbox_persistence::automatic_reconciliation::RECONCILIATION_LOCK_WAIT;

    use super::{
        AutomaticReconciliationNumericBounds, InventoryPage, PASS_FAILURE_CAUSE,
        QUIESCENT_ROTATION_PAGE_CEILING, QuiescentInventory, ROTATION_CEILING_CAUSE,
        SLOT_HELD_PAGE_TIMEOUT_CAUSE, STALE_TURN_AMBIGUOUS_CAUSE,
        STALE_TURN_LOCK_UNAVAILABLE_CAUSE, STALE_TURN_SUPERSEDED_CAUSE, STALE_TURN_TERMINAL_CAUSE,
        SlowSubstrateNumericBounds, StaleTurnTerminalizer, TERMINALIZATION_DEFERRED_CAUSE,
        TerminalizationWindow, TurnLivenessNumericBounds, TurnLivenessWake,
        batch_admits_another_reconciliation, complete_before_shutdown, drain_quiescent_rotation,
        next_turn_liveness_wake, reconcile_turn_liveness, reconciliation_deadline,
    };
    use signalbox_application::{
        DurableTurnLivenessObservation, StaleActiveTurnBound, StaleTurnCandidate, StaleTurnOutcome,
        TurnLivenessEvidence, TurnLivenessGuardKind, TurnLivenessLedger, TurnLivenessScanInterval,
    };
    use signalbox_domain::{AcceptedInputTurnFailureIdentities, SessionId, TurnAttemptId, TurnId};
    use signalbox_persistence::turn_liveness::{
        TurnLivenessPersistenceBounds, TurnLivenessRepositoryError,
    };
    use std::{
        num::{NonZeroU32, NonZeroU64},
        sync::{Mutex, atomic::AtomicU64, atomic::AtomicUsize, atomic::Ordering},
        time::Duration,
    };
    use uuid::Uuid;

    fn fixture_staleness_bound() -> StaleActiveTurnBound {
        StaleActiveTurnBound::try_new(Duration::from_secs(37))
            .expect("fixture staleness bound is valid")
    }

    fn fixture_ledger(bound: StaleActiveTurnBound) -> TurnLivenessLedger {
        TurnLivenessLedger::new(
            bound,
            TurnLivenessScanInterval::try_new(bound.get())
                .expect("fixture interval is timer-representable"),
        )
    }

    fn example_numeric_bounds() -> TurnLivenessNumericBounds {
        let configured = crate::configuration::checked_in_example_configuration()
            .expect("checked-in example parses");
        let bounds = configured.numeric_bounds();
        TurnLivenessNumericBounds::new(
            bounds
                .integer("terminalizations_per_liveness_scan")
                .flatten()
                .and_then(|value| usize::try_from(value).ok()),
            bounds
                .duration("slot_held_turn_recovery_attempt_bound")
                .flatten(),
            AutomaticReconciliationNumericBounds::new(
                bounds
                    .integer("automatic_model_call_reconciliations_per_liveness_scan")
                    .flatten()
                    .and_then(|value| usize::try_from(value).ok()),
                bounds
                    .duration("automatic_model_call_reconciliation_attempt_bound")
                    .flatten(),
            ),
            AutomaticReconciliationNumericBounds::new(
                bounds
                    .integer("automatic_tool_reconciliations_per_liveness_scan")
                    .flatten()
                    .and_then(|value| usize::try_from(value).ok()),
                bounds
                    .duration("automatic_tool_reconciliation_attempt_bound")
                    .flatten(),
            ),
            SlowSubstrateNumericBounds::new(
                bounds.integer("slow_substrate_backup_enabled").flatten() == Some(1),
                bounds.integer("slow_substrate_restart_enabled").flatten() == Some(1),
                bounds
                    .integer("slow_substrate_lock_convoy_enabled")
                    .flatten()
                    == Some(1),
                bounds
                    .integer("slow_substrate_staleness_factor")
                    .flatten()
                    .and_then(|value| u32::try_from(value).ok())
                    .and_then(NonZeroU32::new)
                    .expect("the example slow-substrate factor is positive"),
            ),
            TurnLivenessPersistenceBounds::new(
                bounds.duration("terminalization_lock_wait").flatten(),
                bounds.duration("terminalization_acquire_wait").flatten(),
                bounds.duration("terminalization_write_lock_wait").flatten(),
            ),
        )
    }

    fn candidate(seed: u128) -> StaleTurnCandidate {
        StaleTurnCandidate::new(
            SessionId::from_uuid(Uuid::from_u128(seed)),
            TurnId::from_uuid(Uuid::from_u128(0xa_0000 + seed)),
            TurnLivenessEvidence::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(0xb_0000 + seed)),
                Some(7),
            ),
        )
    }

    /// A page that filled, so the rotation continues past its last session.
    fn full_page(seed: u128) -> InventoryPage {
        InventoryPage {
            candidates: Box::new([candidate(seed)]),
            rows: 1,
            resume_after: Some(SessionId::from_uuid(Uuid::from_u128(seed))),
        }
    }

    /// A page that did not fill, so it is where the rotation ends.
    fn last_page(seed: u128) -> InventoryPage {
        InventoryPage {
            candidates: Box::new([candidate(seed)]),
            rows: 1,
            resume_after: None,
        }
    }

    fn empty_page() -> InventoryPage {
        InventoryPage {
            candidates: Box::new([]),
            rows: 0,
            resume_after: None,
        }
    }

    /// A page whose every row this pass could not read: no candidates, but
    /// rows all the same.
    fn wholly_dropped_page() -> InventoryPage {
        InventoryPage {
            candidates: Box::new([]),
            rows: 1,
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
        busy: Vec<TurnId>,
        terminalized: AtomicUsize,
        observation_ordinal: AtomicU64,
    }

    impl CountingRepository {
        fn new(candidates: Vec<StaleTurnCandidate>) -> Self {
            Self {
                remaining: Mutex::new(candidates),
                busy: Vec::new(),
                terminalized: AtomicUsize::new(0),
                observation_ordinal: AtomicU64::new(0),
            }
        }

        /// Every attempt on these turns finds the session held by another
        /// transaction and gives up on its wait.
        fn with_busy(candidates: Vec<StaleTurnCandidate>, busy: Vec<TurnId>) -> Self {
            Self {
                busy,
                ..Self::new(candidates)
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
                rows: candidates.len(),
                candidates: candidates.into_boxed_slice(),
                resume_after: None,
            })
        }
    }

    impl super::DurableObservationRecorder for CountingRepository {
        async fn record_observation(
            &self,
            _guard: TurnLivenessGuardKind,
            candidates: &[StaleTurnCandidate],
        ) -> Result<Box<[DurableTurnLivenessObservation]>, TurnLivenessRepositoryError> {
            let ordinal = self
                .observation_ordinal
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            let ordinal = NonZeroU64::new(ordinal).expect("fixture ordinal stays positive");
            Ok(candidates
                .iter()
                .copied()
                .map(|candidate| DurableTurnLivenessObservation::new(candidate, ordinal))
                .collect::<Vec<_>>()
                .into_boxed_slice())
        }
    }

    impl StaleTurnTerminalizer for CountingRepository {
        async fn terminalize(
            &self,
            candidate: StaleTurnCandidate,
            _identities: AcceptedInputTurnFailureIdentities,
        ) -> Result<StaleTurnOutcome, TurnLivenessRepositoryError> {
            if self.busy.contains(&candidate.turn()) {
                return Err(TurnLivenessRepositoryError::TerminalizationLockUnavailable(
                    sqlx::Error::PoolTimedOut,
                ));
            }
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
        let bound = fixture_staleness_bound();
        let cohort = vec![candidate(1), candidate(2), candidate(3)];
        let capacity = cohort.len() - 1;
        let repository = CountingRepository::new(cohort.clone());
        let ledger = fixture_ledger(bound);
        let mut window = TerminalizationWindow::new(Some(capacity));
        let _ = reconcile_turn_liveness(
            &repository,
            ledger,
            TurnLivenessGuardKind::Quiescent,
            NonZeroU32::MIN,
            bound,
            &mut window,
        )
        .await;

        let _ = reconcile_turn_liveness(
            &repository,
            ledger,
            TurnLivenessGuardKind::Quiescent,
            NonZeroU32::MIN,
            bound,
            &mut window,
        )
        .await;

        assert_eq!(repository.terminalized(), capacity);
        assert_eq!(repository.still_active(), cohort.len() - capacity);
    }

    /// What one scan defers the next one ends: nothing about a deferred turn
    /// changed, so it is observed unchanged and comes due again.
    #[tokio::test(start_paused = true)]
    async fn the_next_scan_ends_what_the_cap_deferred() {
        let bound = fixture_staleness_bound();
        let cohort = vec![candidate(1), candidate(2), candidate(3)];
        let capacity = cohort.len() - 1;
        let repository = CountingRepository::new(cohort.clone());
        let ledger = fixture_ledger(bound);
        let mut window = TerminalizationWindow::new(Some(capacity));
        let _ = reconcile_turn_liveness(
            &repository,
            ledger,
            TurnLivenessGuardKind::Quiescent,
            NonZeroU32::MIN,
            bound,
            &mut window,
        )
        .await;
        let _ = reconcile_turn_liveness(
            &repository,
            ledger,
            TurnLivenessGuardKind::Quiescent,
            NonZeroU32::MIN,
            bound,
            &mut window,
        )
        .await;

        let _ = reconcile_turn_liveness(
            &repository,
            ledger,
            TurnLivenessGuardKind::Quiescent,
            NonZeroU32::MIN,
            bound,
            &mut window,
        )
        .await;

        assert_eq!(repository.terminalized(), cohort.len());
        assert_eq!(repository.still_active(), 0);
    }

    /// A lap serves the members it opened with. Sessions carry time-ordered
    /// identities, so one becoming due mid-lap sorts above everything the lap
    /// holds; serving it first would mean never returning to the rest.
    #[test]
    fn a_lap_serves_its_own_members_though_newer_sessions_keep_arriving() {
        let first = candidate(1);
        let second = candidate(2);
        let arrival = candidate(3);
        let mut window = TerminalizationWindow::new(Some(1));

        let opening = window.take(&[first, second]);
        let closing = window.take(&[first, second, arrival]);
        let next_lap = window.take(&[first, second, arrival]);

        assert_eq!(opening, vec![first]);
        assert_eq!(closing, vec![second]);
        assert_eq!(
            next_lap,
            vec![first],
            "the lap's members were exhausted, so the next lap opens over what is due now"
        );
    }

    /// A session that already existed and becomes due mid-lap waits for the
    /// next one, however far below the lap's members it sorts. Otherwise a busy
    /// band of older sessions could push a lap's own members back indefinitely.
    #[test]
    fn a_session_becoming_due_mid_lap_waits_for_the_next_one() {
        let opener = candidate(2);
        let tail = candidate(4);
        let older = candidate(1);
        let between = candidate(3);
        let mut window = TerminalizationWindow::new(Some(1));

        let opening = window.take(&[opener, tail]);
        let closing = window.take(&[older, opener, between, tail]);
        let next_lap = window.take(&[older, opener, between, tail]);

        assert_eq!(opening, vec![opener]);
        assert_eq!(
            closing,
            vec![tail],
            "the lap's second member is served before sessions that joined after it opened"
        );
        assert_eq!(next_lap, vec![older]);
    }

    /// A member that stops being due before its slot comes is skipped, not
    /// waited for: the scan spends its window on turns that are still due.
    #[test]
    fn a_member_that_stopped_being_due_is_skipped() {
        let departed = candidate(1);
        let remains = candidate(2);
        let mut window = TerminalizationWindow::new(Some(1));

        let opening = window.take(&[departed, remains]);
        let closing = window.take(&[remains]);

        assert_eq!(opening, vec![departed]);
        assert_eq!(closing, vec![remains]);
    }

    /// A session held by another transaction is counted apart from a failure,
    /// and its turn stays due for the lap to reach again — ordinary contention
    /// is not a fault, and it is evidence the turn may not be wedged.
    #[tokio::test(start_paused = true)]
    async fn a_busy_session_is_counted_apart_from_a_failure() {
        let bound = fixture_staleness_bound();
        let busy = candidate(1);
        let ends = candidate(2);
        let repository = CountingRepository::with_busy(vec![busy, ends], vec![busy.turn()]);
        let ledger = fixture_ledger(bound);
        let mut window = TerminalizationWindow::new(Some(2));
        let _ = reconcile_turn_liveness(
            &repository,
            ledger,
            TurnLivenessGuardKind::Quiescent,
            NonZeroU32::MIN,
            bound,
            &mut window,
        )
        .await;

        let tally = reconcile_turn_liveness(
            &repository,
            ledger,
            TurnLivenessGuardKind::Quiescent,
            NonZeroU32::MIN,
            bound,
            &mut window,
        )
        .await;

        assert_eq!(tally.lock_unavailable, 1);
        assert_eq!(tally.failed, 0);
        assert_eq!(tally.terminalized, 1);
        assert_eq!(repository.still_active(), 1);
    }

    /// The deployed cap is the value the checked-in example states.
    #[test]
    fn one_scan_uses_the_configured_terminalization_limit() {
        assert!(example_numeric_bounds().terminalizations_per_scan.is_some());
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

    /// A probe whose rows were all dropped is still a page past the ceiling,
    /// so it decides nothing rather than proving the rotation ended. Counting
    /// its candidates instead would report a truncated population as a whole
    /// one.
    #[tokio::test]
    async fn a_probe_of_wholly_unreadable_rows_decides_nothing() {
        let inventory = ScriptedInventory::new([full_page(1), full_page(2), wholly_dropped_page()]);

        let drained = drain_quiescent_rotation(&inventory, 2).await;

        assert!(drained.is_none());
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
            TERMINALIZATION_DEFERRED_CAUSE,
            "turn_liveness_terminalization_deferred"
        );
        assert_eq!(
            STALE_TURN_LOCK_UNAVAILABLE_CAUSE,
            "turn_liveness_scheduler_row_busy"
        );
        assert_eq!(
            SLOT_HELD_PAGE_TIMEOUT_CAUSE,
            "turn_liveness_slot_held_page_timed_out"
        );
        assert_ne!(STALE_TURN_SUPERSEDED_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_ne!(STALE_TURN_AMBIGUOUS_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_ne!(PASS_FAILURE_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_ne!(ROTATION_CEILING_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_ne!(TERMINALIZATION_DEFERRED_CAUSE, STALE_TURN_TERMINAL_CAUSE);
        assert_ne!(STALE_TURN_LOCK_UNAVAILABLE_CAUSE, STALE_TURN_TERMINAL_CAUSE);
    }

    /// The bound reported beside a terminalized turn is the configured one, so
    /// an operator reading the line sees what actually decided the turn.
    #[test]
    fn the_audited_bound_is_the_configured_one() {
        let shortened = Duration::from_secs(23);
        let lowered = StaleActiveTurnBound::try_new(shortened).expect("fixture bound is valid");

        assert_eq!(lowered.get(), shortened);
    }

    /// The example keeps recovery bounded while production may choose `none`.
    ///
    /// The configured bound is now the reconciliation transaction's last-resort
    /// client deadline; the database-side budgets it must sit above are pinned
    /// in `signalbox_persistence::automatic_reconciliation`.
    #[test]
    fn recovery_attempts_use_the_configured_bound() {
        assert!(
            example_numeric_bounds()
                .slot_held_recovery_attempt_bound
                .is_some()
        );
        assert_eq!(
            reconciliation_deadline(Some(Duration::from_secs(60))),
            Duration::from_secs(60)
        );
    }

    /// The slot-held recovery ceiling and the reconciliation ceiling are
    /// configured apart, and the slot-held one multiplied across its fair
    /// window still lands inside the scheduler-pass ceiling this watchdog
    /// backstops. Sharing one value between the two consumers is exactly what
    /// carried that product past the ceiling.
    #[test]
    fn the_slot_held_fair_window_stays_inside_the_scheduler_pass_ceiling() {
        let bounds = example_numeric_bounds();
        let recovery = bounds
            .slot_held_recovery_attempt_bound
            .expect("the example bounds slot-held recovery");
        let reconciliation = bounds
            .automatic_model_call_reconciliation_attempt_bound
            .expect("the example bounds automatic reconciliation");
        let window = u32::try_from(
            bounds
                .terminalizations_per_scan
                .expect("the example bounds the fair window"),
        )
        .expect("the example fair window fits a scan multiplier");
        let occupancy_ceiling = crate::configuration::checked_in_example_configuration()
            .expect("checked-in example parses")
            .numeric_bounds()
            .duration("scheduler_pass_occupancy_bound")
            .flatten()
            .expect("the example bounds scheduler pass occupancy");

        assert!(recovery < reconciliation);
        assert!(recovery.saturating_mul(window) < occupancy_ceiling);
    }

    #[test]
    fn one_scan_uses_the_configured_reconciliation_limit() {
        assert!(
            example_numeric_bounds()
                .automatic_model_call_reconciliations_per_scan
                .is_some()
        );
    }

    /// A requested stop ends the batch where it stands rather than waiting
    /// behind the population the per-scan ceiling still admits.
    #[test]
    fn a_requested_shutdown_ends_the_batch_before_its_remaining_population() {
        let (sender, shutdown) = tokio::sync::watch::channel(false);
        sender
            .send(true)
            .expect("the shutdown receiver remains live");

        assert!(!batch_admits_another_reconciliation(
            &shutdown,
            0,
            example_numeric_bounds().automatic_model_call_reconciliations_per_scan
        ));
    }

    /// A ceiling small enough to read the gate's arithmetic off directly. It
    /// is this test's own number, deliberately not the configured one.
    const FIXTURE_RECONCILIATION_CEILING: usize = 2;

    /// The same gate still admits work while no stop has been requested, so
    /// the preemption narrows nothing an ordinary scan does.
    #[test]
    fn a_clear_shutdown_admits_the_next_reconciliation_within_the_ceiling() {
        let (_sender, shutdown) = tokio::sync::watch::channel(false);

        assert!(batch_admits_another_reconciliation(
            &shutdown,
            FIXTURE_RECONCILIATION_CEILING - 1,
            Some(FIXTURE_RECONCILIATION_CEILING)
        ));
    }

    /// The ceiling still ends the batch on its own once no stop is pending.
    #[test]
    fn the_per_scan_ceiling_still_ends_a_batch_no_shutdown_preempts() {
        let (_sender, shutdown) = tokio::sync::watch::channel(false);

        assert!(!batch_admits_another_reconciliation(
            &shutdown,
            FIXTURE_RECONCILIATION_CEILING,
            Some(FIXTURE_RECONCILIATION_CEILING)
        ));
    }

    #[tokio::test]
    async fn shutdown_interrupts_a_recovery_stage() {
        let (sender, mut shutdown) = tokio::sync::watch::channel(false);
        sender
            .send(true)
            .expect("the shutdown receiver remains live");

        let outcome = complete_before_shutdown(&mut shutdown, std::future::pending::<()>()).await;

        assert_eq!(outcome, None);
    }

    #[tokio::test]
    async fn a_completed_recovery_stage_returns_while_shutdown_is_clear() {
        let (_sender, mut shutdown) = tokio::sync::watch::channel(false);

        let outcome = complete_before_shutdown(&mut shutdown, std::future::ready(7_u8)).await;

        assert_eq!(outcome, Some(7));
    }

    /// The reconciliation stages are bounded above their database-side lock
    /// budget, so a contended row ends as `55P03` under the caller's timer
    /// rather than as a dropped future that leaves the backend still waiting.
    ///
    /// Stated here as well as in the compile-time assertion because the two
    /// answer different questions: that one rejects a margin that has vanished,
    /// this one records which side of the pair the margin belongs to.
    #[test]
    fn the_reconciliation_bound_outlasts_its_database_budget() {
        assert_eq!(RECONCILIATION_LOCK_WAIT, Duration::from_secs(1));
        assert!(
            reconciliation_deadline(None) > RECONCILIATION_LOCK_WAIT,
            "the database-side budget has to be the one that expires first"
        );
        assert!(
            reconciliation_deadline(Some(Duration::from_millis(1))) > RECONCILIATION_LOCK_WAIT,
            "a configured bound below the floor is raised, never honoured as-is"
        );
    }
}
