//! Durable evidence and terminalization for boundedly stale active turns.
//!
//! The inventory query is the whole in-flight predicate: a turn reaches the
//! result only when every durable shape that could mean live work is absent.
//! Terminalization then re-runs that same query for the one session under the
//! scheduler lock and commits the shared failed-turn transition
//! ([`crate::startup`]) rather than editing lifecycle rows here, so a stale
//! turn ends exactly as a recovered one does and every trigger that watches
//! for a terminal turn — repository-watch dispatch release included — fires
//! without this module naming any of them.

use std::{error::Error, fmt, time::Duration};

use signalbox_application::{
    ClassifyOperatorFailure, OperatorFailureClass, StaleTurnCandidate, StaleTurnOutcome,
    TurnLivenessEvidence,
};
use signalbox_domain::{
    AcceptedInputTurnFailureFailure, AcceptedInputTurnFailureIdentities, SessionId, TurnAttemptId,
};
use sqlx::{PgConnection, PgPool, Row, types::Decimal, types::Uuid};
use tokio::time::timeout;

use crate::mapping::{
    session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid,
};
use crate::session::{SessionRepositoryError, load_session_from_connection};
use crate::startup::{
    StartupScanCorruption, StartupScanIdentityCollision, StartupScanRepositoryError,
    insert_prepared_failure, map_scheduling_error,
};
use crate::submit_input::load_scheduling_projection;

/// How many quiescent turns one inventory read returns.
///
/// This bounds the size of one statement's result, not how much of the
/// population a scan reaches: the caller drains its whole rotation before
/// deciding anything, so every turn is observed on every scan and the staleness
/// bound holds whatever the population is.
const QUIESCENT_INVENTORY_PAGE_SIZE: i64 = 256;

/// How long one terminalization waits for the session's scheduler row.
///
/// The attempt takes that row under an exclusive row lock, and the transactions holding it —
/// activation, submission, startup recovery — are short. A wait longer than
/// this means the session is busy, which is itself evidence against the turn
/// being wedged, so failing fast costs nothing: the turn stays due and its lap
/// reaches it again. What an unbounded wait would cost is the whole serial
/// phase, which one stuck transaction could hold for as long as it lived.
///
/// The value is what keeps the phase inside its interval in the worst case: a
/// windowful of attempts that all wait the full bound is sixteen seconds, the
/// same budget a windowful of committing transactions is estimated at.
// numeric-bound: tunable - bounds one terminalization's wait for the scheduler row
const TERMINALIZATION_LOCK_WAIT: &str = "250ms";

/// The same wait, applied to reaching a connection at all.
///
/// The shared pool's own acquisition timeout is thirty seconds, which a
/// windowful of attempts would multiply into half an hour of a phase that is
/// supposed to fit inside a one-minute interval. Cancelling an acquisition is
/// safe in a way cancelling later work would not be: no transaction has begun,
/// so there is nothing whose fate could be unknown.
// numeric-bound: tunable - bounds one terminalization's wait for a pooled connection
const TERMINALIZATION_ACQUIRE_WAIT: Duration = Duration::from_millis(250);

/// How long the rest of the attempt waits for any lock, once the row is held.
///
/// The write phase needs its own budget rather than the acquisition one or none
/// at all. Not the acquisition budget: appending to the outbox takes the shared
/// `outbox_sequence_state` row, which every writer in the daemon holds from its
/// first append until it commits, so a wait of a few hundred milliseconds there
/// is ordinary traffic rather than a stall, and refusing on it would make the
/// pass fail whenever the daemon was busy. And not `0`, which is what this
/// previously reset to: an unbounded wait lets one indefinite holder of that
/// row stall the whole reconciliation loop, which is the failure the
/// acquisition budget exists to prevent, reintroduced one statement later.
///
/// A second is two orders of magnitude above a brief hold and still detects a
/// stalled holder within one interval. Tripping it costs a retry rather than a
/// turn: the attempt has written nothing durable, the transaction rolls back,
/// and the lap reaches the turn again.
// numeric-bound: tunable - bounds the write phase's wait for any contended row
const TERMINALIZATION_WRITE_LOCK_WAIT: &str = "1s";

/// Infrastructure or integrity failure while supervising turn liveness.
///
/// The two database arms are separate because they mean different things to an
/// operator: a failed inventory read is a pass that made no decision at all,
/// while a failed terminalization is a decision that could not be carried out
/// on a turn already judged stale. No blanket `From<sqlx::Error>` exists, so
/// neither path can silently borrow the other's cause code.
#[derive(Debug)]
pub enum TurnLivenessRepositoryError {
    /// Reading the quiescent active-turn inventory failed.
    Inventory(sqlx::Error),
    /// The session's scheduler row stayed locked past the attempt's wait.
    TerminalizationLockUnavailable(sqlx::Error),
    /// A database operation on the terminalization path failed.
    TerminalizationDatabase {
        /// Whether the failure leaves the commit's outcome unknown.
        commit_ambiguous: bool,
        /// The originating driver failure.
        source: sqlx::Error,
    },
    /// The shared failed-turn transition could not be committed.
    Terminalization(StartupScanRepositoryError),
}

impl TurnLivenessRepositoryError {
    /// Classifies an unambiguous driver failure on the terminalization path.
    fn terminalization(error: sqlx::Error) -> Self {
        Self::TerminalizationDatabase {
            commit_ambiguous: false,
            source: error,
        }
    }

    /// Classifies a failure of the statement that takes the scheduler row.
    ///
    /// Only this site can report a refused lock, rather than any statement that
    /// happens to raise the code: a refusal means "someone else is working on
    /// this session", which is true of the row this statement waits for and of
    /// nothing else the transaction goes on to touch.
    fn terminalization_lock(error: sqlx::Error) -> Self {
        if lock_wait_expired(&error) {
            return Self::TerminalizationLockUnavailable(error);
        }
        Self::terminalization(error)
    }
}

impl fmt::Display for TurnLivenessRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inventory(source) => {
                write!(
                    formatter,
                    "quiescent active-turn inventory failed: {source}"
                )
            }
            Self::TerminalizationLockUnavailable(source) => {
                write!(
                    formatter,
                    "stale-turn terminalization could not lock the session scheduler row: {source}"
                )
            }
            Self::TerminalizationDatabase { source, .. } => {
                write!(formatter, "stale-turn terminalization failed: {source}")
            }
            Self::Terminalization(source) => source.fmt(formatter),
        }
    }
}

impl Error for TurnLivenessRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inventory(source)
            | Self::TerminalizationLockUnavailable(source)
            | Self::TerminalizationDatabase { source, .. } => Some(source),
            Self::Terminalization(source) => Some(source),
        }
    }
}

impl ClassifyOperatorFailure for TurnLivenessRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Inventory(_) => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::TerminalizationLockUnavailable(_) => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::TerminalizationDatabase {
                commit_ambiguous, ..
            } => OperatorFailureClass::Infrastructure {
                commit_ambiguous: *commit_ambiguous,
            },
            Self::Terminalization(source) => source.operator_failure_class(),
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Inventory(_) => "turn_liveness_inventory_failed",
            Self::TerminalizationLockUnavailable(_) => {
                "turn_liveness_terminalization_lock_unavailable"
            }
            Self::TerminalizationDatabase { .. } => "turn_liveness_terminalization_failed",
            Self::Terminalization(source) => source.operator_failure_cause_code(),
        }
    }
}

impl From<StartupScanRepositoryError> for TurnLivenessRepositoryError {
    fn from(error: StartupScanRepositoryError) -> Self {
        Self::Terminalization(error)
    }
}

/// PostgreSQL inventory and terminalization adapter for turn liveness.
#[derive(Clone, Debug)]
pub struct PostgresTurnLivenessRepository {
    pool: PgPool,
}

impl PostgresTurnLivenessRepository {
    /// Uses the supplied shared pool for liveness supervision.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Reads one bounded page of active turns with no work in flight.
    ///
    /// `after` resumes a rotation: passing the last session of a saturated page
    /// continues past it, and passing `None` starts again from the first. The
    /// returned coverage says whether the page was the whole population, which
    /// is the only condition under which absence means a turn stopped being
    /// quiescent rather than merely falling outside this page.
    pub async fn quiescent_active_turns(
        &self,
        after: Option<SessionId>,
    ) -> Result<QuiescentActiveTurnPage, TurnLivenessRepositoryError> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(TurnLivenessRepositoryError::Inventory)?;
        let fetched = read_quiescent_active_turns(&mut connection, None, after)
            .await
            .map_err(TurnLivenessRepositoryError::Inventory)?;
        Ok(QuiescentActiveTurnPage::new(fetched))
    }

    /// Reads one bounded page of running turns regardless of their currently
    /// live operation, for the outer slot-held staleness watchdog.
    pub async fn slot_held_active_turns(
        &self,
        after: Option<SessionId>,
    ) -> Result<QuiescentActiveTurnPage, TurnLivenessRepositoryError> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(TurnLivenessRepositoryError::Inventory)?;
        let fetched = read_slot_held_active_turns(
            &mut connection,
            None,
            after,
            QUIESCENT_INVENTORY_PAGE_SIZE,
        )
        .await
        .map_err(TurnLivenessRepositoryError::Inventory)?;
        Ok(QuiescentActiveTurnPage::new(fetched))
    }

    /// Reads the slot-held observation for one exact session.
    pub async fn slot_held_active_turn(
        &self,
        session: SessionId,
    ) -> Result<Option<StaleTurnCandidate>, TurnLivenessRepositoryError> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(TurnLivenessRepositoryError::Inventory)?;
        let fetched = read_slot_held_active_turns(&mut connection, Some(session), None, 1)
            .await
            .map_err(TurnLivenessRepositoryError::Inventory)?;
        Ok(fetched.candidates.first().copied())
    }

    /// Reads one exact running active turn for scheduler-pass expiry recovery.
    ///
    /// Unlike the slot-held watchdog inventory, this includes the quiescent
    /// shape immediately after activation. The expiry path separately binds
    /// the returned observation to the exact turn from the expired pass.
    pub async fn recoverable_active_turn(
        &self,
        session: SessionId,
    ) -> Result<Option<StaleTurnCandidate>, TurnLivenessRepositoryError> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(TurnLivenessRepositoryError::Inventory)?;
        let fetched = read_recoverable_active_turn(&mut connection, session)
            .await
            .map_err(TurnLivenessRepositoryError::Inventory)?;
        Ok(fetched.candidates.first().copied())
    }

    /// Terminalizes one observed-stale turn as failed under the session locks.
    ///
    /// The observation is revalidated inside the transaction, so a turn that
    /// resumed between the scan and this call is left untouched and reported
    /// [`StaleTurnOutcome::Superseded`].
    pub async fn terminalize_stale_turn(
        &self,
        candidate: StaleTurnCandidate,
        identities: AcceptedInputTurnFailureIdentities,
    ) -> Result<StaleTurnOutcome, TurnLivenessRepositoryError> {
        let mut transaction = timeout(TERMINALIZATION_ACQUIRE_WAIT, self.pool.begin())
            .await
            .unwrap_or(Err(sqlx::Error::PoolTimedOut))
            .map_err(TurnLivenessRepositoryError::terminalization)?;
        // Bounded before anything is read or written, so the only statement it
        // can interrupt is the one waiting for the scheduler row. A bound that
        // could fire later — over the whole statement, or over the future —
        // might interrupt the commit instead, and this pass would then not know
        // whether the turn ended.
        sqlx::query("SELECT set_config('lock_timeout', $1, true)")
            .bind(TERMINALIZATION_LOCK_WAIT)
            .execute(&mut *transaction)
            .await
            .map_err(TurnLivenessRepositoryError::terminalization)?;
        let outcome = terminalize_in_transaction(&mut transaction, candidate, identities).await;
        match outcome {
            Ok(StaleTurnOutcome::Terminalized) => {
                transaction.commit().await.map_err(|error| {
                    TurnLivenessRepositoryError::TerminalizationDatabase {
                        commit_ambiguous: crate::commit_failure_is_ambiguous(&error),
                        source: error,
                    }
                })?;
                Ok(StaleTurnOutcome::Terminalized)
            }
            // Neither decided outcome wrote anything, so both roll back.
            Ok(
                outcome @ (StaleTurnOutcome::Superseded
                | StaleTurnOutcome::BlockedByPendingSteering),
            ) => {
                transaction
                    .rollback()
                    .await
                    .map_err(TurnLivenessRepositoryError::terminalization)?;
                Ok(outcome)
            }
            // The originating failure is the one that classifies this outcome,
            // and it may be corruption — which an operator must not read as a
            // transient infrastructure fault merely because a rollback also
            // failed. Nothing is rolled back explicitly here: dropping the
            // transaction rolls it back, so there is no second failure to weigh
            // against the first in the first place.
            Err(error) => Err(error),
        }
    }
}

/// One page of the quiescent inventory, and where the rotation continues.
#[derive(Clone, Debug)]
pub struct QuiescentActiveTurnPage {
    candidates: Box<[StaleTurnCandidate]>,
    rows: usize,
    resume_after: Option<SessionId>,
}

impl QuiescentActiveTurnPage {
    /// Derives the continuation cursor from one page.
    ///
    /// A page that filled may have rows behind it, so it carries the cursor to
    /// resume past; a page that did not fill is the end of the rotation and
    /// carries none. Sessions in the result are distinct, because the schema
    /// admits one active turn per session, and the statement resumes strictly
    /// past the cursor — so a rotation driven by these cursors advances on
    /// every page and terminates.
    ///
    /// Both are decided from the rows the statement returned rather than from
    /// the candidates kept, because a row this pass cannot read is dropped
    /// without being answered for. Counting kept candidates would let one such
    /// row make a full page look like the end of the rotation, stopping every
    /// scan at the same row and leaving everything behind it unobserved.
    fn new(fetched: FetchedPage) -> Self {
        let filled =
            i64::try_from(fetched.rows).unwrap_or(i64::MAX) >= QUIESCENT_INVENTORY_PAGE_SIZE;
        let resume_after = filled.then_some(fetched.furthest_session).flatten();
        Self {
            candidates: fetched.candidates,
            rows: fetched.rows,
            resume_after,
        }
    }

    /// Returns the quiescent turns this page observed.
    pub fn candidates(&self) -> &[StaleTurnCandidate] {
        &self.candidates
    }

    /// Returns how many rows the statement returned, dropped ones included.
    ///
    /// Whether a rotation has ended is a question about the statement, not
    /// about what this pass could read: a page of rows it dropped entirely is
    /// still a page of rows behind the cursor.
    pub const fn rows(&self) -> usize {
        self.rows
    }

    /// Consumes the page, yielding the turns it observed.
    pub fn into_candidates(self) -> Box<[StaleTurnCandidate]> {
        self.candidates
    }

    /// Returns the cursor the rotation continues from, `None` at its end.
    pub const fn resume_after(&self) -> Option<SessionId> {
        self.resume_after
    }
}

/// The complete no-work-in-flight predicate, in one statement.
///
/// Every conjunct is an absence this statement proves for itself. A shape it
/// cannot see is a shape it does not clear: the phase filter admits only
/// `running`, so every durable wait — approval parking included — is outside
/// the result rather than judged by it.
///
/// Pending steering is deliberately not one of these conjuncts. Nothing
/// consumes a steering input without a model call to consume it at a safe
/// point, and every conjunct here already proves that no call, attempt, or tool
/// round is live — so a steered turn that is otherwise quiescent is wedged
/// rather than working, and excluding it made it permanently invisible to the
/// one pass that looks for wedges. Admitting it is what lets the turn be
/// reported by identity; ending it needs a transition that closes the steering,
/// which `docs/open-questions.md` records as undecided.
///
/// The attempt arm admits `prepared` as well as `running`. An attempt becomes
/// `running` only when a model call is authorized on it, so `prepared` is a
/// turn activated but never dispatched — and nothing else reaches that shape:
/// the eligibility sweep's active arm requires a live tool round, so it does
/// not re-drive a turn that has none. Excluding `prepared` left exactly that
/// wedge unowned. `stop_requested` and `ended` stay out, the first because an
/// interrupt is in flight and the second because the attempt already closed.
///
/// The progress observation is the session's newest turn-progress outbox event.
/// `event_sequence` is assigned by the outbox in commit order, so it rises on
/// every durable transition and cannot be moved backwards by a clock — unlike
/// an identity ordering, where a backward adjustment or a future-skewed mint
/// would let a fresh row sort below the recorded frontier and read as silence
/// on a session that had progressed.
///
/// The excluded kinds are the ones that happen *to* a session while its active
/// turn sits still, traced to their writers rather than inferred from their
/// names: session creation (`create_session`, and the imported-frontier
/// variant), session-defaults replacement (`replace_session_defaults`), goal
/// turn retirement (`goal`), runner state transitions (`runner_protocol`), and
/// the pair a submission writes — model-settings resolution
/// (`model_settings_resolution`) and input acceptance (`submit_input`, and goal
/// dispatch). Reading any of them as progress would let a user keep a wedged
/// turn alive indefinitely by submitting input, since one submission writes two
/// of them.
///
/// Those last two have a second writer inside `model_execution`, which mints a
/// successor turn from pending steering while the source turn terminalizes.
/// Excluding them loses nothing there: that transaction is a terminal
/// transition, so it appends the source turn's own terminal event too, and that
/// kind is included.
///
/// Everything else in the vocabulary is emitted by a transition of a turn, its
/// model calls, its tool rounds, or its approvals — activation by
/// `start_eligible_turn`, the terminal shapes by `model_execution` and
/// `startup`, calls by `model_execution`, rounds by `tool_loop`, approvals by
/// `approval_judge` and `tool_loop`, compaction by `context_compaction` — so
/// none of it can advance while the turn does nothing.
///
/// Delegation is outside this statement rather than excluded from it: its
/// updates and wakes are appended to `delegation_outbox_event`, a separate
/// table, so they never reach this frontier. That is the right answer for the
/// same reason `input_accepted` is excluded — a message arriving for a session
/// queues work rather than advancing the turn holding its slot — and a turn
/// actually waiting on a child sits in `awaiting_child`, which the phase filter
/// excludes before any of this is consulted. Naming what to exclude rather than what to include means a kind
/// added later reads as progress until someone decides otherwise, which delays
/// a terminalization rather than risking a live turn.
///
/// It is read with `ORDER BY … DESC LIMIT 1` over
/// `outbox_event_turn_progress_by_session`, whose partial predicate is this
/// exclusion — so the scan descends past no excluded row and the read stays one
/// index lookup whatever a session's history weighs. Every observation this
/// statement reports is bounded that way: the `LIMIT` caps returned rows, and
/// nothing per row scans a history.
const QUIESCENT_ACTIVE_TURNS: &str = "SELECT active.session_id,
            active.turn_id,
            active.current_attempt_id,
            (SELECT newest.event_sequence
               FROM outbox_event AS newest
              WHERE newest.session_id = active.session_id
                AND newest.event_kind NOT IN (
                    'session_created',
                    'session_model_settings_changed',
                    'turn_model_settings_resolved',
                    'input_accepted',
                    'goal_turn_retired',
                    'runner_state_transition'
                )
              ORDER BY newest.event_sequence DESC
              LIMIT 1) AS outbox_frontier
       FROM turn_lifecycle AS active
       JOIN turn_attempt AS tenure
         ON tenure.turn_attempt_id = active.current_attempt_id
        AND tenure.turn_id = active.turn_id
        AND tenure.session_id = active.session_id
      WHERE active.state_kind = 'active'
        AND active.origin_kind = 'accepted_input'
        AND NOT active.delegation_runtime_terminal
        AND active.active_phase_kind = 'running'
        AND active.active_tool_round_call_id IS NULL
        AND active.approval_tool_request_id IS NULL
        AND active.recovery_tool_attempt_id IS NULL
        AND tenure.state_kind IN ('prepared', 'running')
        AND tenure.end_variant IS NULL
        AND tenure.end_disposition IS NULL
        AND ($1::uuid IS NULL OR active.session_id = $1)
        AND ($2::uuid IS NULL OR active.session_id > $2)
        AND NOT EXISTS (
            SELECT 1
              FROM model_call AS live
             WHERE live.session_id = active.session_id
               AND live.state_kind <> 'terminal'
        )
        AND NOT EXISTS (
            SELECT 1
              FROM context_compaction_model_call AS live
             WHERE live.session_id = active.session_id
               AND live.state_kind <> 'terminal'
        )
        AND NOT EXISTS (
            SELECT 1
              FROM tool_attempt AS live
             WHERE live.session_id = active.session_id
               AND live.state_kind <> 'terminal'
        )
      ORDER BY active.session_id
      LIMIT $3";

/// Outer watchdog inventory for a pass that still holds an active running
/// turn after every component deadline should have ended. Approval and other
/// durable waits are excluded by the phase predicate; live calls and tools are
/// intentionally retained because their containing pass has its own tighter
/// occupancy ceiling.
const SLOT_HELD_ACTIVE_TURNS: &str = "SELECT active.session_id,
            active.turn_id,
            active.current_attempt_id,
            (SELECT newest.event_sequence
               FROM outbox_event AS newest
              WHERE newest.session_id = active.session_id
                AND newest.event_kind NOT IN (
                    'session_created',
                    'session_model_settings_changed',
                    'turn_model_settings_resolved',
                    'input_accepted',
                    'goal_turn_retired',
                    'runner_state_transition'
                )
              ORDER BY newest.event_sequence DESC
              LIMIT 1) AS outbox_frontier
       FROM turn_lifecycle AS active
       JOIN turn_attempt AS tenure
         ON tenure.turn_attempt_id = active.current_attempt_id
        AND tenure.turn_id = active.turn_id
        AND tenure.session_id = active.session_id
      WHERE active.state_kind = 'active'
        AND NOT active.delegation_runtime_terminal
        AND active.active_phase_kind = 'running'
        AND tenure.state_kind IN ('prepared', 'running', 'stop_requested')
        AND tenure.end_variant IS NULL
        AND tenure.end_disposition IS NULL
        AND (
            tenure.state_kind = 'stop_requested'
            OR active.origin_kind = 'delegation'
            OR EXISTS (
                SELECT 1
                  FROM model_call AS live
                 WHERE live.session_id = active.session_id
                   AND live.state_kind <> 'terminal'
            )
            OR EXISTS (
                SELECT 1
                  FROM context_compaction_model_call AS live
                 WHERE live.session_id = active.session_id
                   AND live.state_kind <> 'terminal'
            )
            OR EXISTS (
                SELECT 1
                  FROM tool_attempt AS live
                 WHERE live.session_id = active.session_id
                   AND live.state_kind <> 'terminal'
            )
        )
        AND ($1::uuid IS NULL OR active.session_id = $1)
        AND ($2::uuid IS NULL OR active.session_id > $2)
      ORDER BY active.session_id
      LIMIT $3";

/// Exact-session inventory for a scheduler pass that expired while executing
/// a known turn. It admits both quiescent and slot-held running shapes while
/// excluding durable waits through the active-phase predicate.
const RECOVERABLE_ACTIVE_TURN: &str = "SELECT active.session_id,
            active.turn_id,
            active.current_attempt_id,
            (SELECT newest.event_sequence
               FROM outbox_event AS newest
              WHERE newest.session_id = active.session_id
                AND newest.event_kind NOT IN (
                    'session_created',
                    'session_model_settings_changed',
                    'turn_model_settings_resolved',
                    'input_accepted',
                    'goal_turn_retired',
                    'runner_state_transition'
                )
              ORDER BY newest.event_sequence DESC
              LIMIT 1) AS outbox_frontier
       FROM turn_lifecycle AS active
       JOIN turn_attempt AS tenure
         ON tenure.turn_attempt_id = active.current_attempt_id
        AND tenure.turn_id = active.turn_id
        AND tenure.session_id = active.session_id
      WHERE active.session_id = $1
        AND active.state_kind = 'active'
        AND NOT active.delegation_runtime_terminal
        AND active.active_phase_kind = 'running'
        AND tenure.state_kind IN ('prepared', 'running', 'stop_requested')
        AND tenure.end_variant IS NULL
        AND tenure.end_disposition IS NULL
      LIMIT 1";

/// Reads one page, leaving classification of any failure to the caller.
///
/// The same statement serves the periodic scan and the locked revalidation, so
/// it returns the driver's error unclassified: the scan reports an inventory
/// failure and the revalidation a terminalization failure.
/// One statement's result: the candidates it yielded, and what it returned.
///
/// The two are not the same count. A row whose evidence this pass cannot read
/// is dropped, and pagination is answered from what the statement returned so
/// that dropping one changes which turns are watched and nothing else.
struct FetchedPage {
    candidates: Box<[StaleTurnCandidate]>,
    rows: usize,
    furthest_session: Option<SessionId>,
}

async fn read_quiescent_active_turns(
    connection: &mut PgConnection,
    session: Option<SessionId>,
    after: Option<SessionId>,
) -> Result<FetchedPage, sqlx::Error> {
    let rows = sqlx::query(QUIESCENT_ACTIVE_TURNS)
        .bind(session.map(session_id_to_uuid))
        .bind(after.map(session_id_to_uuid))
        .bind(QUIESCENT_INVENTORY_PAGE_SIZE)
        .fetch_all(connection)
        .await?;
    decode_candidate_page(rows)
}

fn decode_candidate_page(rows: Vec<sqlx::postgres::PgRow>) -> Result<FetchedPage, sqlx::Error> {
    let fetched_rows = rows.len();
    let mut furthest_session = None;
    let mut candidates = Vec::with_capacity(fetched_rows);
    for row in rows {
        let session: Uuid = row.try_get("session_id")?;
        // Recorded before any decision to drop the row: the statement orders by
        // session, so the last one it returned is where the next page resumes
        // whether or not this pass could read it.
        furthest_session = Some(session_id_from_uuid(session));
        let turn: Uuid = row.try_get("turn_id")?;
        let attempt: Uuid = row.try_get("current_attempt_id")?;
        let stored_frontier: Option<Decimal> = row.try_get("outbox_frontier")?;
        let frontier = match stored_frontier {
            // No turn-progress event yet is an ordinary shape, and stays an
            // observation: two scans that both see none saw no progress.
            None => None,
            Some(sequence) => match decode_outbox_frontier(sequence) {
                Some(frontier) => Some(frontier),
                // A frontier this pass cannot read is not evidence of silence.
                // Dropping the row keeps the turn out of the inventory
                // entirely, so no bound accrues against it — where reporting it
                // as absent would compare equal to the next unreadable
                // observation and end the turn on evidence never understood.
                None => continue,
            },
        };
        candidates.push(StaleTurnCandidate::new(
            session_id_from_uuid(session),
            turn_id_from_uuid(turn),
            TurnLivenessEvidence::new(TurnAttemptId::from_uuid(attempt), frontier),
        ));
    }
    Ok(FetchedPage {
        candidates: candidates.into_boxed_slice(),
        rows: fetched_rows,
        furthest_session,
    })
}

async fn read_slot_held_active_turns(
    connection: &mut PgConnection,
    session: Option<SessionId>,
    after: Option<SessionId>,
    limit: i64,
) -> Result<FetchedPage, sqlx::Error> {
    let rows = sqlx::query(SLOT_HELD_ACTIVE_TURNS)
        .bind(session.map(session_id_to_uuid))
        .bind(after.map(session_id_to_uuid))
        .bind(limit)
        .fetch_all(connection)
        .await?;
    decode_candidate_page(rows)
}

async fn read_recoverable_active_turn(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<FetchedPage, sqlx::Error> {
    let rows = sqlx::query(RECOVERABLE_ACTIVE_TURN)
        .bind(session_id_to_uuid(session))
        .fetch_all(connection)
        .await?;
    decode_candidate_page(rows)
}

/// Revalidates one exact slot-held observation on a connection whose scheduler
/// row is already locked by the caller.
///
/// The unlocked inventory and locked recovery check deliberately share the
/// statement and decoder, preventing either path from adopting weaker evidence.
pub(crate) async fn slot_held_candidate_matches(
    connection: &mut PgConnection,
    candidate: StaleTurnCandidate,
) -> Result<bool, sqlx::Error> {
    let fetched =
        read_slot_held_active_turns(connection, Some(candidate.session()), None, 1).await?;
    Ok(fetched.candidates.first().copied() == Some(candidate))
}

/// Revalidates one exact running-turn observation under the scheduler lock.
pub(crate) async fn recoverable_candidate_matches(
    connection: &mut PgConnection,
    candidate: StaleTurnCandidate,
) -> Result<bool, sqlx::Error> {
    let fetched = read_recoverable_active_turn(connection, candidate.session()).await?;
    Ok(fetched.candidates.first().copied() == Some(candidate))
}

/// Reads one outbox sequence as the token the ledger compares, or nothing if
/// the stored value is not one.
///
/// `outbox_event_sequence_positive_u64` constrains the column to `1..=u64::MAX`,
/// so failing is unreachable. It is reported rather than defaulted because the
/// ledger compares whole observations for equality: a value that read as absent
/// would compare equal to the next one that did, and the turn would come due on
/// evidence this pass never understood. The caller drops such a row instead.
/// Whether the database refused a lock rather than failing at one.
///
/// `55P03` is `lock_not_available`, which is what `lock_timeout` raises: the
/// row was held past the wait, and nothing was read or written.
fn lock_wait_expired(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(database)
        if database.code().as_deref() == Some("55P03"))
}

fn decode_outbox_frontier(sequence: Decimal) -> Option<u64> {
    if sequence.fract().is_zero() {
        u64::try_from(sequence).ok()
    } else {
        None
    }
}

async fn terminalize_in_transaction(
    connection: &mut PgConnection,
    candidate: StaleTurnCandidate,
    identities: AcceptedInputTurnFailureIdentities,
) -> Result<StaleTurnOutcome, TurnLivenessRepositoryError> {
    let locks = sqlx::query(crate::lock_inventory::STARTUP_RECOVERY)
        .bind(session_id_to_uuid(candidate.session()))
        .fetch_one(&mut *connection)
        .await
        .map_err(TurnLivenessRepositoryError::terminalization_lock)?;
    // The acquisition budget has done its work: the row is held, and the
    // question of whether this session is busy is settled. The write phase
    // takes over with a budget of its own — wide enough that the outbox's
    // shared sequence row, which every writer holds until it commits, is not
    // mistaken for a stall, and finite so that one indefinite holder of it
    // cannot stall this loop.
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(TERMINALIZATION_WRITE_LOCK_WAIT)
        .execute(&mut *connection)
        .await
        .map_err(TurnLivenessRepositoryError::terminalization)?;
    let session_exists: bool = locks
        .try_get(0)
        .map_err(TurnLivenessRepositoryError::terminalization)?;
    let scheduler_session: Option<Uuid> = locks
        .try_get(1)
        .map_err(TurnLivenessRepositoryError::terminalization)?;
    let active_turn: Option<Uuid> = locks
        .try_get(2)
        .map_err(TurnLivenessRepositoryError::terminalization)?;
    // A session that is gone, or whose active turn is no longer this one, is an
    // ordinary concurrent departure. A session that exists without its
    // scheduler row is not: the two are one-to-one and neither is deleted, so
    // the same lock result is corruption in startup recovery and is corruption
    // here. Reporting it as supersession would retry the wedged turn every
    // scan and log the inconsistency as an informational race.
    if !session_exists || active_turn != Some(turn_id_to_uuid(candidate.turn())) {
        return Ok(StaleTurnOutcome::Superseded);
    }
    if scheduler_session.is_none() {
        return Err(
            StartupScanRepositoryError::from(StartupScanCorruption::Missing(
                "session scheduler row",
            ))
            .into(),
        );
    }
    // The scan ran without the scheduler lock, so the whole predicate is
    // re-decided here against rows no concurrent pass can now be changing.
    let locked = read_quiescent_active_turns(connection, Some(candidate.session()), None)
        .await
        .map_err(TurnLivenessRepositoryError::terminalization)?;
    if locked.candidates.first().copied() != Some(candidate) {
        return Ok(StaleTurnOutcome::Superseded);
    }

    let session = match load_session_from_connection(connection, candidate.session()).await {
        Ok(Some(session)) => session,
        Ok(None) => return Ok(StaleTurnOutcome::Superseded),
        Err(SessionRepositoryError::Database(error)) => {
            return Err(StartupScanRepositoryError::from(error).into());
        }
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(
                StartupScanRepositoryError::from(StartupScanCorruption::CurrentSession(error))
                    .into(),
            );
        }
    };
    let scheduling = load_scheduling_projection(connection, session)
        .await
        .map_err(map_scheduling_error)?;
    let prepared =
        match scheduling.prepare_active_turn_lost_failure(identities) {
            Ok(prepared) => prepared,
            // This is the identical transition startup recovery prepares, so it is
            // classified identically: only a turn that is no longer active is a
            // concurrent departure. Every other refusal contradicts something the
            // locked predicate proved absent moments earlier, in the same
            // transaction, under the same locks — a projection that disagrees with
            // the rows it was built from is inconsistent durable state, not a race.
            // Reporting those as supersession would retry them silently forever
            // while the slot stayed wedged, which is exactly the failure this pass
            // exists to end.
            Err(error) => match error.failure() {
                AcceptedInputTurnFailureFailure::NoActiveTurn => {
                    return Ok(StaleTurnOutcome::Superseded);
                }
                // The candidate query no longer proves steering absent, so this
                // refusal is an expected shape rather than an impossible one:
                // the schema requires every steering row pending on a turn to
                // be closed before it terminalizes, and this transition closes
                // none. Reporting it as its own outcome keeps the wedge visible
                // without claiming inconsistent durable state.
                AcceptedInputTurnFailureFailure::PendingSteering { .. } => {
                    return Ok(StaleTurnOutcome::BlockedByPendingSteering);
                }
                AcceptedInputTurnFailureFailure::ActiveAttemptCannotEndLost
                | AcceptedInputTurnFailureFailure::ActiveStartMissing
                | AcceptedInputTurnFailureFailure::StartingSnapshotMissing
                | AcceptedInputTurnFailureFailure::TerminalFrontierCannotAppend => {
                    return Err(StartupScanRepositoryError::from(
                        StartupScanCorruption::Inconsistent("active failure preparation"),
                    )
                    .into());
                }
                AcceptedInputTurnFailureFailure::FailureEntryIdentityAlreadyExists => {
                    return Err(StartupScanRepositoryError::IdentityCollision(
                        StartupScanIdentityCollision::FailureEntry,
                    )
                    .into());
                }
                AcceptedInputTurnFailureFailure::TerminalFrontierIdentityAlreadyExists => {
                    return Err(StartupScanRepositoryError::IdentityCollision(
                        StartupScanIdentityCollision::TerminalFrontier,
                    )
                    .into());
                }
            },
        };
    insert_prepared_failure(connection, prepared).await?;
    Ok(StaleTurnOutcome::Terminalized)
}

#[cfg(test)]
mod tests {
    use super::{
        ClassifyOperatorFailure, FetchedPage, QUIESCENT_INVENTORY_PAGE_SIZE,
        QuiescentActiveTurnPage, RECOVERABLE_ACTIVE_TURN, SLOT_HELD_ACTIVE_TURNS,
        TERMINALIZATION_LOCK_WAIT, TERMINALIZATION_WRITE_LOCK_WAIT, TurnLivenessRepositoryError,
    };
    use signalbox_application::{StaleTurnCandidate, TurnLivenessEvidence};
    use signalbox_domain::{SessionId, TurnAttemptId, TurnId};
    use sqlx::types::Uuid;

    #[test]
    fn expiry_inventory_admits_quiescent_and_slot_held_running_shapes() {
        assert!(RECOVERABLE_ACTIVE_TURN.contains("active.active_phase_kind = 'running'"));
        assert!(RECOVERABLE_ACTIVE_TURN.contains("'prepared', 'running', 'stop_requested'"));
        assert!(!RECOVERABLE_ACTIVE_TURN.contains("EXISTS ("));
        assert!(!RECOVERABLE_ACTIVE_TURN.contains("active_tool_round_call_id IS NULL"));
    }

    #[test]
    fn outer_watchdog_admits_quiescent_delegated_running_turns() {
        assert!(SLOT_HELD_ACTIVE_TURNS.contains("active.origin_kind = 'delegation'"));
    }

    fn candidate(session: u128) -> StaleTurnCandidate {
        StaleTurnCandidate::new(
            SessionId::from_uuid(Uuid::from_u128(session)),
            TurnId::from_uuid(Uuid::from_u128(0xa_0000 + session)),
            TurnLivenessEvidence::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(0xb_0000 + session)),
                None,
            ),
        )
    }

    fn fetched(rows: usize, kept: usize) -> FetchedPage {
        FetchedPage {
            candidates: (1..=kept)
                .map(|session| candidate(session as u128))
                .collect(),
            rows,
            furthest_session: (rows > 0)
                .then(|| SessionId::from_uuid(Uuid::from_u128(rows as u128))),
        }
    }

    fn page(rows: usize) -> QuiescentActiveTurnPage {
        QuiescentActiveTurnPage::new(fetched(rows, rows))
    }

    /// A page that did not fill is the end of the rotation, so a scan that
    /// reads it has the whole population and needs no further read.
    #[test]
    fn an_underfilled_page_ends_the_rotation() {
        let short = page(3);
        let empty = page(0);

        assert_eq!(short.resume_after(), None);
        assert_eq!(short.candidates().len(), 3);
        assert_eq!(empty.resume_after(), None);
    }

    /// An attempt bounds two different waits, on two different rows, for two
    /// different questions — so the budgets are not the same number.
    #[test]
    fn an_attempt_carries_two_lock_budgets() {
        assert_eq!(TERMINALIZATION_LOCK_WAIT, "250ms");
        assert_eq!(TERMINALIZATION_WRITE_LOCK_WAIT, "1s");
        assert_ne!(TERMINALIZATION_LOCK_WAIT, TERMINALIZATION_WRITE_LOCK_WAIT);
    }

    /// Only the statement taking the scheduler row reports contention, so a
    /// failure raised anywhere else is an ordinary one whatever it carries.
    #[test]
    fn a_failure_away_from_the_lock_site_is_an_ordinary_one() {
        let failure = TurnLivenessRepositoryError::terminalization(sqlx::Error::PoolTimedOut);

        assert!(matches!(
            failure,
            TurnLivenessRepositoryError::TerminalizationDatabase {
                commit_ambiguous: false,
                ..
            }
        ));
        assert_eq!(
            failure.operator_failure_cause_code(),
            "turn_liveness_terminalization_failed"
        );
    }

    /// A full page may have rows behind it, so it carries the cursor the next
    /// read resumes strictly past — which is what makes the rotation advance.
    #[test]
    fn a_full_page_resumes_past_its_last_session() {
        let rows = usize::try_from(QUIESCENT_INVENTORY_PAGE_SIZE).expect("the page size is small");
        let full = page(rows);

        assert_eq!(
            full.resume_after(),
            Some(candidate(rows as u128).session()),
            "the cursor is the greatest session this page returned"
        );
    }

    /// Dropping a row this pass cannot read changes which turns are watched and
    /// nothing else: the page is still full, and still resumes past the last
    /// session the statement returned rather than the last one kept.
    #[test]
    fn a_dropped_row_does_not_end_the_rotation() {
        let rows = usize::try_from(QUIESCENT_INVENTORY_PAGE_SIZE).expect("the page size is small");
        let short_one = QuiescentActiveTurnPage::new(fetched(rows, rows - 1));

        assert_eq!(short_one.candidates().len(), rows - 1);
        assert_eq!(
            short_one.resume_after(),
            Some(candidate(rows as u128).session()),
            "the cursor comes from the rows returned, not the candidates kept"
        );
    }
}
