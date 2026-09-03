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

use std::{error::Error, fmt, future::Future, num::NonZeroU64, time::Duration};

use signalbox_application::{
    ClassifyOperatorFailure, DurableTurnLivenessObservation, OperatorFailureClass,
    StaleTurnCandidate, StaleTurnOutcome, StartupScanSessionOutcome, TurnLivenessEvidence,
    TurnLivenessGuardKind, TurnLivenessScanInterval,
};
use signalbox_domain::{
    AcceptedInputTurnFailureFailure, AcceptedInputTurnFailureIdentities, ModelCallId, SessionId,
    TurnAttemptId, TurnTerminalCause,
};
use sqlx::{PgConnection, PgPool, Row, types::Decimal, types::Uuid};
use tokio::time::timeout;

use crate::mapping::{
    session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid,
};
use crate::session::{SessionRepositoryError, load_session_from_connection};
use crate::startup::{
    StartupScanCorruption, StartupScanIdentityCollision, StartupScanRepositoryError,
    TransactionDecision, insert_prepared_failure, lost_failure_identities, map_scheduling_error,
    recover_observed_slot_held_in_transaction,
};
use crate::submit_input::load_scheduling_projection;

/// How many quiescent turns one inventory read returns.
///
/// This bounds the size of one statement's result, not how much of the
/// population a scan reaches: the caller drains its whole rotation before
/// deciding anything, so every turn is observed on every scan and the staleness
/// bound holds whatever the population is.
const QUIESCENT_INVENTORY_PAGE_SIZE: i64 = 256;

/// Deployment policy for liveness-terminalization database waits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnLivenessPersistenceBounds {
    lock_wait: Option<Duration>,
    acquire_wait: Option<Duration>,
    write_lock_wait: Option<Duration>,
}

impl TurnLivenessPersistenceBounds {
    /// Binds every terminalization wait to validated daemon configuration.
    pub const fn new(
        lock_wait: Option<Duration>,
        acquire_wait: Option<Duration>,
        write_lock_wait: Option<Duration>,
    ) -> Self {
        Self {
            lock_wait,
            acquire_wait,
            write_lock_wait,
        }
    }
}

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
    /// Recording a complete durable observation population failed.
    Observation {
        /// Whether the failure leaves the commit's outcome unknown.
        commit_ambiguous: bool,
        /// The originating driver failure.
        source: sqlx::Error,
    },
    /// A required terminalization row stayed locked past the attempt's wait.
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
    /// Classifies an unambiguous driver failure while recording observations.
    fn observation(error: sqlx::Error) -> Self {
        Self::Observation {
            commit_ambiguous: false,
            source: error,
        }
    }

    /// Classifies the commit of an observation transaction.
    fn observation_commit(error: sqlx::Error) -> Self {
        Self::Observation {
            commit_ambiguous: crate::commit_failure_is_ambiguous(&error),
            source: error,
        }
    }

    /// Classifies an unambiguous driver failure on the terminalization path.
    fn terminalization(error: sqlx::Error) -> Self {
        Self::TerminalizationDatabase {
            commit_ambiguous: false,
            source: error,
        }
    }

    /// Classifies a failure of the statement that takes the scheduler row.
    ///
    /// The shared transition conversion applies the same classification to a
    /// refusal from a later guarded lock site. This direct constructor keeps a
    /// different driver failure on the scheduler statement ordinary.
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
            Self::Observation { source, .. } => {
                write!(
                    formatter,
                    "durable turn-liveness observation failed: {source}"
                )
            }
            Self::TerminalizationLockUnavailable(source) => {
                write!(
                    formatter,
                    "stale-turn terminalization could not acquire a required row lock: {source}"
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
            | Self::Observation { source, .. }
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
            Self::Observation {
                commit_ambiguous, ..
            } => OperatorFailureClass::Infrastructure {
                commit_ambiguous: *commit_ambiguous,
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
            Self::Observation { .. } => "turn_liveness_observation_failed",
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
        match error {
            StartupScanRepositoryError::Database {
                source,
                commit_ambiguous: false,
            } if lock_wait_expired(&source) => Self::TerminalizationLockUnavailable(source),
            error => Self::Terminalization(error),
        }
    }
}

/// PostgreSQL inventory and terminalization adapter for turn liveness.
#[derive(Clone, Debug)]
pub struct PostgresTurnLivenessRepository {
    pool: PgPool,
    bounds: TurnLivenessPersistenceBounds,
}

impl PostgresTurnLivenessRepository {
    /// Uses the supplied shared pool for liveness supervision.
    pub fn new(pool: PgPool, bounds: TurnLivenessPersistenceBounds) -> Self {
        Self { pool, bounds }
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
        let fetched = read_slot_held_active_turns(&mut connection, None, after)
            .await
            .map_err(TurnLivenessRepositoryError::Inventory)?;
        Ok(QuiescentActiveTurnPage::new(fetched))
    }

    /// Advances one guard's durable repeated-observation ledger atomically.
    ///
    /// `candidates` is the complete population for the guard. Rows absent from
    /// it are removed in the same transaction, so a turn that leaves and later
    /// re-enters the predicate starts again at ordinal one.
    pub async fn record_complete_observation(
        &self,
        guard: TurnLivenessGuardKind,
        scan_interval: TurnLivenessScanInterval,
        candidates: &[StaleTurnCandidate],
    ) -> Result<Box<[DurableTurnLivenessObservation]>, TurnLivenessRepositoryError> {
        self.record_complete_observation_with_progress(guard, scan_interval, candidates, true)
            .await
    }

    /// Records a restart's complete population without advancing existing rows.
    pub async fn record_restart_complete_observation(
        &self,
        guard: TurnLivenessGuardKind,
        scan_interval: TurnLivenessScanInterval,
        candidates: &[StaleTurnCandidate],
    ) -> Result<Box<[DurableTurnLivenessObservation]>, TurnLivenessRepositoryError> {
        self.record_complete_observation_with_progress(guard, scan_interval, candidates, false)
            .await
    }

    /// Clears observation continuity while stale-turn supervision is disabled.
    pub async fn clear_guard_observations(&self) -> Result<(), TurnLivenessRepositoryError> {
        sqlx::query("DELETE FROM turn_liveness_observation")
            .execute(&self.pool)
            .await
            .map_err(TurnLivenessRepositoryError::observation)?;
        Ok(())
    }

    async fn record_complete_observation_with_progress(
        &self,
        guard: TurnLivenessGuardKind,
        scan_interval: TurnLivenessScanInterval,
        candidates: &[StaleTurnCandidate],
        advance_existing: bool,
    ) -> Result<Box<[DurableTurnLivenessObservation]>, TurnLivenessRepositoryError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(TurnLivenessRepositoryError::observation)?;
        let turns = candidates
            .iter()
            .map(|candidate| turn_id_to_uuid(candidate.turn()))
            .collect::<Vec<_>>();
        let sessions = candidates
            .iter()
            .map(|candidate| session_id_to_uuid(candidate.session()))
            .collect::<Vec<_>>();
        let attempts = candidates
            .iter()
            .map(|candidate| candidate.evidence().current_attempt().into_uuid())
            .collect::<Vec<_>>();
        let frontiers = candidates
            .iter()
            .map(|candidate| {
                candidate
                    .evidence()
                    .outbox_frontier()
                    .map_or_else(|| "none".to_owned(), |frontier| frontier.to_string())
            })
            .collect::<Vec<_>>();
        let rows = sqlx::query(
            "WITH incoming AS (
                SELECT *
                  FROM UNNEST($2::uuid[], $3::uuid[], $4::uuid[], $5::text[])
                       AS item(turn_id, session_id, current_attempt_id, outbox_frontier_token)
             )
             INSERT INTO turn_liveness_observation AS observation
                (guard_kind, turn_id, session_id, current_attempt_id,
                 outbox_frontier_token, scan_interval_seconds,
                 scan_interval_subsec_nanos, observation_ordinal)
             SELECT $1, turn_id, session_id, current_attempt_id,
                    outbox_frontier_token, $6, $7, 1
               FROM incoming
             ON CONFLICT (guard_kind, turn_id) DO UPDATE
                SET session_id = EXCLUDED.session_id,
                    current_attempt_id = EXCLUDED.current_attempt_id,
                    outbox_frontier_token = EXCLUDED.outbox_frontier_token,
                    scan_interval_seconds = EXCLUDED.scan_interval_seconds,
                    scan_interval_subsec_nanos = EXCLUDED.scan_interval_subsec_nanos,
                    observation_ordinal = CASE
                        WHEN ROW(
                            observation.current_attempt_id,
                            observation.outbox_frontier_token
                        ) IS DISTINCT FROM ROW(
                            EXCLUDED.current_attempt_id,
                            EXCLUDED.outbox_frontier_token
                        )
                        THEN 1
                        WHEN ROW(
                            observation.scan_interval_seconds,
                            observation.scan_interval_subsec_nanos
                        ) IS DISTINCT FROM ROW($6::numeric, $7::integer)
                        THEN 1
                        WHEN NOT $8::boolean
                        THEN observation.observation_ordinal
                        ELSE CASE
                            WHEN observation.observation_ordinal < 9223372036854775807
                            THEN observation.observation_ordinal + 1
                            ELSE observation.observation_ordinal
                        END
                    END
             RETURNING turn_id, session_id, current_attempt_id,
                       outbox_frontier_token, observation_ordinal",
        )
        .bind(guard.as_str())
        .bind(&turns)
        .bind(&sessions)
        .bind(&attempts)
        .bind(&frontiers)
        .bind(Decimal::from(scan_interval.get().as_secs()))
        .bind(
            i32::try_from(scan_interval.get().subsec_nanos()).map_err(|source| {
                TurnLivenessRepositoryError::observation(sqlx::Error::Decode(Box::new(source)))
            })?,
        )
        .bind(advance_existing)
        .fetch_all(&mut *transaction)
        .await
        .map_err(TurnLivenessRepositoryError::observation)?;
        sqlx::query(
            "DELETE FROM turn_liveness_observation
              WHERE guard_kind = $1
                AND NOT (turn_id = ANY($2::uuid[]))",
        )
        .bind(guard.as_str())
        .bind(&turns)
        .execute(&mut *transaction)
        .await
        .map_err(TurnLivenessRepositoryError::observation)?;
        transaction
            .commit()
            .await
            .map_err(TurnLivenessRepositoryError::observation_commit)?;
        let mut observations = rows
            .into_iter()
            .map(decode_durable_observation)
            .collect::<Result<Vec<_>, _>>()?;
        observations.sort_unstable_by_key(|observation| observation.candidate().session());
        Ok(observations.into_boxed_slice())
    }

    /// Reads the current slot-held observation for one exact session.
    pub async fn observed_slot_held_turn(
        &self,
        session: SessionId,
    ) -> Result<Option<StaleTurnCandidate>, TurnLivenessRepositoryError> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(TurnLivenessRepositoryError::Inventory)?;
        read_exact_slot_held_candidate(&mut connection, session)
            .await
            .map_err(TurnLivenessRepositoryError::Inventory)
    }

    /// Reconciles one exact slot-held observation under the session locks.
    ///
    /// `None` means the turn or its progress evidence changed before the lock
    /// was acquired. That is an ordinary supersession, never authority to
    /// recover whichever turn happens to be active now.
    pub async fn recover_observed_slot_held_turn<Generator>(
        &self,
        candidate: StaleTurnCandidate,
        identities: AcceptedInputTurnFailureIdentities,
        ids: &mut Generator,
    ) -> Result<Option<StartupScanSessionOutcome>, TurnLivenessRepositoryError>
    where
        Generator: signalbox_application::StartupScanIdGenerator + Send,
    {
        let mut transaction = optional_timeout(self.bounds.acquire_wait, self.pool.begin())
            .await
            .unwrap_or(Err(sqlx::Error::PoolTimedOut))
            .map_err(TurnLivenessRepositoryError::terminalization)?;
        sqlx::query("SELECT set_config('lock_timeout', $1, true)")
            .bind(postgres_lock_timeout(self.bounds.lock_wait))
            .execute(&mut *transaction)
            .await
            .map_err(TurnLivenessRepositoryError::terminalization)?;
        let decision = recover_observed_slot_held_in_transaction(
            &mut transaction,
            candidate,
            identities,
            self.bounds.write_lock_wait,
            ids,
        )
        .await
        .map_err(TurnLivenessRepositoryError::from)?;
        match decision {
            None => {
                transaction
                    .rollback()
                    .await
                    .map_err(TurnLivenessRepositoryError::terminalization)?;
                Ok(None)
            }
            Some(TransactionDecision::Commit(outcome)) => {
                transaction.commit().await.map_err(|error| {
                    TurnLivenessRepositoryError::TerminalizationDatabase {
                        commit_ambiguous: crate::commit_failure_is_ambiguous(&error),
                        source: error,
                    }
                })?;
                Ok(Some(outcome))
            }
            Some(TransactionDecision::Rollback(outcome)) => {
                transaction
                    .rollback()
                    .await
                    .map_err(TurnLivenessRepositoryError::terminalization)?;
                Ok(Some(outcome))
            }
        }
    }

    /// Recovers the compaction an expired pre-activation pass left behind.
    ///
    /// The budgets are the pair the slot-held sibling installs, in the same
    /// order and for the same reason: a caller's wall-clock deadline cannot
    /// cancel a statement already waiting in the backend, so abandoning the
    /// future would leave that wait running on a checked-out pooled connection.
    /// Bounding the acquisition and both lock waits server-side is what makes
    /// the caller's deadline a backstop rather than the only bound.
    ///
    /// `abandoned_call` names the exact compaction the expired window made
    /// durable. The session alone would not distinguish it from a compaction a
    /// later admitted pass is running now, which a delayed attempt would
    /// otherwise terminalize.
    ///
    /// `Ok(None)` means that call no longer holds the boundary — it committed
    /// before the pass future was dropped, or a prior attempt of this same
    /// handoff already terminalized it — and nothing was touched.
    pub async fn recover_abandoned_compaction(
        &self,
        session: SessionId,
        abandoned_call: ModelCallId,
    ) -> Result<Option<StartupScanSessionOutcome>, TurnLivenessRepositoryError> {
        let mut transaction = optional_timeout(self.bounds.acquire_wait, self.pool.begin())
            .await
            .unwrap_or(Err(sqlx::Error::PoolTimedOut))
            .map_err(TurnLivenessRepositoryError::terminalization)?;
        sqlx::query("SELECT set_config('lock_timeout', $1, true)")
            .bind(postgres_lock_timeout(self.bounds.lock_wait))
            .execute(&mut *transaction)
            .await
            .map_err(TurnLivenessRepositoryError::terminalization)?;
        let recovered = crate::startup::recover_abandoned_compaction_in_transaction(
            &mut transaction,
            session,
            abandoned_call,
            self.bounds.write_lock_wait,
        )
        .await
        .map_err(TurnLivenessRepositoryError::from)?;
        match recovered {
            None => {
                transaction
                    .rollback()
                    .await
                    .map_err(TurnLivenessRepositoryError::terminalization)?;
                Ok(None)
            }
            Some(outcome) => {
                transaction.commit().await.map_err(|error| {
                    TurnLivenessRepositoryError::TerminalizationDatabase {
                        commit_ambiguous: crate::commit_failure_is_ambiguous(&error),
                        source: error,
                    }
                })?;
                Ok(Some(outcome))
            }
        }
    }

    /// Terminalizes one observed-stale turn as failed under the session locks.
    ///
    /// The observation is revalidated inside the transaction, so a turn that
    /// resumed between the scan and this call is left untouched and reported
    /// [`StaleTurnOutcome::Superseded`].
    pub async fn terminalize_stale_turn<Generator>(
        &self,
        candidate: StaleTurnCandidate,
        identities: AcceptedInputTurnFailureIdentities,
        ids: &mut Generator,
    ) -> Result<StaleTurnOutcome, TurnLivenessRepositoryError>
    where
        Generator: signalbox_application::StartupScanIdGenerator + Send,
    {
        let mut transaction = optional_timeout(self.bounds.acquire_wait, self.pool.begin())
            .await
            .unwrap_or(Err(sqlx::Error::PoolTimedOut))
            .map_err(TurnLivenessRepositoryError::terminalization)?;
        // Bounded before anything is read or written, so the only statement it
        // can interrupt is the one waiting for the scheduler row. A bound that
        // could fire later — over the whole statement, or over the future —
        // might interrupt the commit instead, and this pass would then not know
        // whether the turn ended.
        sqlx::query("SELECT set_config('lock_timeout', $1, true)")
            .bind(postgres_lock_timeout(self.bounds.lock_wait))
            .execute(&mut *transaction)
            .await
            .map_err(TurnLivenessRepositoryError::terminalization)?;
        let outcome = terminalize_in_transaction(
            &mut transaction,
            candidate,
            identities,
            self.bounds.write_lock_wait,
            ids,
        )
        .await;
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
            // A superseded candidate wrote nothing, so it rolls back.
            Ok(outcome @ StaleTurnOutcome::Superseded) => {
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

fn decode_durable_observation(
    row: sqlx::postgres::PgRow,
) -> Result<DurableTurnLivenessObservation, TurnLivenessRepositoryError> {
    let token: String = row
        .try_get("outbox_frontier_token")
        .map_err(TurnLivenessRepositoryError::observation)?;
    let frontier = if token == "none" {
        None
    } else {
        Some(token.parse::<u64>().map_err(|source| {
            TurnLivenessRepositoryError::observation(sqlx::Error::Decode(Box::new(source)))
        })?)
    };
    let ordinal: i64 = row
        .try_get("observation_ordinal")
        .map_err(TurnLivenessRepositoryError::observation)?;
    let ordinal = u64::try_from(ordinal)
        .ok()
        .and_then(NonZeroU64::new)
        .ok_or_else(|| {
            TurnLivenessRepositoryError::observation(sqlx::Error::Decode(Box::new(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid durable turn-liveness ordinal",
                ),
            )))
        })?;
    Ok(DurableTurnLivenessObservation::new(
        StaleTurnCandidate::new(
            session_id_from_uuid(
                row.try_get("session_id")
                    .map_err(TurnLivenessRepositoryError::observation)?,
            ),
            turn_id_from_uuid(
                row.try_get("turn_id")
                    .map_err(TurnLivenessRepositoryError::observation)?,
            ),
            TurnLivenessEvidence::new(
                TurnAttemptId::from_uuid(
                    row.try_get("current_attempt_id")
                        .map_err(TurnLivenessRepositoryError::observation)?,
                ),
                frontier,
            ),
        ),
        ordinal,
    ))
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
///
/// A parked session is not a candidate. Parking suspends its turn in place —
/// the turn keeps its phase and nothing about it proceeds — so a watchdog that
/// still saw it would read a deliberately held turn as a stalled one and reap
/// exactly the work an operator is holding.
///
/// Ownership is deliberately not a second conjunct. Turn-liveness recovery
/// applies to every turn whoever owns the session; ownership governs lifecycle
/// driving — retry, park, escalation, auto-resume — not liveness. A dead turn
/// left active in a conversation would block its next input, and injection is
/// available in every non-terminal state.
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
                    'runner_state_transition',
                    'session_state_changed',
                    'session_terminal',
                    'goal_changed',
                    'command_settled',
                    'injection_settled',
                    'session_ownership_changed'
                )
                AND NOT (
                    newest.event_kind = 'turn_terminal'
                    AND newest.turn_disposition = 'retired'
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
              FROM session_lifecycle AS parked
             WHERE parked.session_id = active.session_id
               AND parked.state_kind = 'parked'
        )
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
///
/// A parked session is not a candidate. Parking suspends its turn in place —
/// the turn keeps its phase and nothing about it proceeds — so a watchdog that
/// still saw it would read a deliberately held turn as a stalled one and reap
/// exactly the work an operator is holding.
///
/// Ownership is deliberately not a second conjunct. Turn-liveness recovery
/// applies to every turn whoever owns the session; ownership governs lifecycle
/// driving — retry, park, escalation, auto-resume — not liveness. A dead turn
/// left active in a conversation would block its next input, and injection is
/// available in every non-terminal state.
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
                    'runner_state_transition',
                    'session_state_changed',
                    'session_terminal',
                    'goal_changed',
                    'command_settled',
                    'injection_settled',
                    'session_ownership_changed'
                )
                AND NOT (
                    newest.event_kind = 'turn_terminal'
                    AND newest.turn_disposition = 'retired'
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
        AND NOT EXISTS (
            SELECT 1
              FROM session_lifecycle AS parked
             WHERE parked.session_id = active.session_id
               AND parked.state_kind = 'parked'
        )
      ORDER BY active.session_id
      LIMIT $3";

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
) -> Result<FetchedPage, sqlx::Error> {
    let rows = sqlx::query(SLOT_HELD_ACTIVE_TURNS)
        .bind(session.map(session_id_to_uuid))
        .bind(after.map(session_id_to_uuid))
        .bind(QUIESCENT_INVENTORY_PAGE_SIZE)
        .fetch_all(connection)
        .await?;
    decode_candidate_page(rows)
}

pub(crate) async fn read_exact_slot_held_candidate(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<StaleTurnCandidate>, sqlx::Error> {
    let page = read_slot_held_active_turns(connection, Some(session), None).await?;
    Ok(page.candidates.first().copied())
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

async fn terminalize_in_transaction<Generator>(
    connection: &mut PgConnection,
    candidate: StaleTurnCandidate,
    identities: AcceptedInputTurnFailureIdentities,
    write_lock_wait: Option<Duration>,
    ids: &mut Generator,
) -> Result<StaleTurnOutcome, TurnLivenessRepositoryError>
where
    Generator: signalbox_application::StartupScanIdGenerator + Send,
{
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
        .bind(postgres_lock_timeout(write_lock_wait))
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
    let identities = lost_failure_identities(identities, &scheduling, ids);
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
                AcceptedInputTurnFailureFailure::PendingSteeringReclassificationMismatch
                | AcceptedInputTurnFailureFailure::ActiveAttemptCannotEndLost
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
    insert_prepared_failure(connection, prepared, TurnTerminalCause::WatchdogStaleTurn).await?;
    Ok(StaleTurnOutcome::Terminalized)
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

pub(crate) fn postgres_lock_timeout(bound: Option<Duration>) -> String {
    match bound {
        Some(bound) => format!("{}us", bound.as_micros().max(1)),
        None => String::from("0"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClassifyOperatorFailure, FetchedPage, QUIESCENT_INVENTORY_PAGE_SIZE,
        QuiescentActiveTurnPage, TurnLivenessRepositoryError, postgres_lock_timeout,
    };
    use signalbox_application::{OperatorFailureClass, StaleTurnCandidate, TurnLivenessEvidence};
    use signalbox_domain::{SessionId, TurnAttemptId, TurnId};
    use sqlx::types::Uuid;

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

    #[test]
    fn postgres_lock_waits_preserve_bounded_and_unbounded_policy() {
        assert_eq!(
            postgres_lock_timeout(Some(std::time::Duration::from_micros(7))),
            "7us"
        );
        assert_eq!(postgres_lock_timeout(None), "0");
    }

    /// Only PostgreSQL's lock-refusal code reports contention, so a different
    /// driver failure remains ordinary infrastructure.
    #[test]
    fn a_non_lock_driver_failure_is_an_ordinary_one() {
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

    /// Losing the commit response preserves its ambiguity for operator policy.
    #[test]
    fn an_observation_commit_without_a_response_is_ambiguous() {
        let failure =
            TurnLivenessRepositoryError::observation_commit(sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "observation commit response was lost",
            )));

        assert_eq!(
            failure.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            }
        );
        assert_eq!(
            failure.operator_failure_cause_code(),
            "turn_liveness_observation_failed"
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
