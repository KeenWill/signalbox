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

use std::{error::Error, fmt};

use signalbox_application::{
    ClassifyOperatorFailure, OperatorFailureClass, StaleTurnCandidate, StaleTurnOutcome,
    TurnLivenessEvidence,
};
use signalbox_domain::{
    AcceptedInputTurnFailureFailure, AcceptedInputTurnFailureIdentities, ModelCallId,
    SemanticTranscriptEntryId, SessionId, TurnAttemptId,
};
use sqlx::{PgConnection, PgPool, Row, types::Uuid};

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
/// A wedge is rare and the pass repeats every scan interval, so a small page
/// keeps the periodic read cheap while still draining any realistic backlog
/// within a few passes.
const QUIESCENT_INVENTORY_PAGE_SIZE: i64 = 32;

/// Infrastructure or integrity failure while supervising turn liveness.
#[derive(Debug)]
pub enum TurnLivenessRepositoryError {
    /// Reading the quiescent active-turn inventory failed.
    Inventory(sqlx::Error),
    /// The shared failed-turn transition could not be committed.
    Terminalization(StartupScanRepositoryError),
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
            Self::Terminalization(source) => source.fmt(formatter),
        }
    }
}

impl Error for TurnLivenessRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inventory(source) => Some(source),
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
            Self::Terminalization(source) => source.operator_failure_class(),
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Inventory(_) => "turn_liveness_inventory_failed",
            Self::Terminalization(source) => source.operator_failure_cause_code(),
        }
    }
}

impl From<sqlx::Error> for TurnLivenessRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Inventory(error)
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
    pub async fn quiescent_active_turns(
        &self,
    ) -> Result<Box<[StaleTurnCandidate]>, TurnLivenessRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        quiescent_active_turns(&mut connection, None).await
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
        let mut transaction = self.pool.begin().await?;
        let outcome = terminalize_in_transaction(&mut transaction, candidate, identities).await;
        match outcome {
            Ok(StaleTurnOutcome::Terminalized) => {
                transaction.commit().await.map_err(|error| {
                    TurnLivenessRepositoryError::Terminalization(
                        StartupScanRepositoryError::Database {
                            commit_ambiguous: crate::commit_failure_is_ambiguous(&error),
                            source: error,
                        },
                    )
                })?;
                Ok(StaleTurnOutcome::Terminalized)
            }
            Ok(StaleTurnOutcome::Superseded) => {
                transaction.rollback().await?;
                Ok(StaleTurnOutcome::Superseded)
            }
            Err(error) => {
                if let Err(rollback_error) = transaction.rollback().await {
                    return Err(rollback_error.into());
                }
                Err(error)
            }
        }
    }
}

/// The complete no-work-in-flight predicate, in one statement.
///
/// Every conjunct is an absence this statement proves for itself. A shape it
/// cannot see is a shape it does not clear: the phase filter admits only
/// `running`, so every durable wait — approval parking included — is outside
/// the result rather than judged by it.
const QUIESCENT_ACTIVE_TURNS: &str = "SELECT active.session_id,
            active.turn_id,
            active.current_attempt_id,
            (SELECT count(*)
               FROM model_call AS counted
              WHERE counted.session_id = active.session_id) AS model_call_count,
            (SELECT max(newest.model_call_id)
               FROM model_call AS newest
              WHERE newest.session_id = active.session_id) AS latest_model_call_id,
            (SELECT count(*)
               FROM semantic_transcript_entry AS counted
              WHERE counted.source_session_id = active.session_id) AS transcript_entry_count,
            (SELECT max(newest.semantic_entry_id)
               FROM semantic_transcript_entry AS newest
              WHERE newest.source_session_id = active.session_id) AS latest_transcript_entry_id
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
        AND tenure.state_kind = 'running'
        AND ($1::uuid IS NULL OR active.session_id = $1)
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
        AND NOT EXISTS (
            SELECT 1
              FROM accepted_input AS steering
             WHERE steering.session_id = active.session_id
               AND steering.disposition_kind = 'pending_steering'
        )
      ORDER BY active.session_id
      LIMIT $2";

async fn quiescent_active_turns(
    connection: &mut PgConnection,
    session: Option<SessionId>,
) -> Result<Box<[StaleTurnCandidate]>, TurnLivenessRepositoryError> {
    let rows = sqlx::query(QUIESCENT_ACTIVE_TURNS)
        .bind(session.map(session_id_to_uuid))
        .bind(QUIESCENT_INVENTORY_PAGE_SIZE)
        .fetch_all(connection)
        .await?;
    let mut candidates = Vec::with_capacity(rows.len());
    for row in rows {
        let session: Uuid = row.try_get("session_id")?;
        let turn: Uuid = row.try_get("turn_id")?;
        let attempt: Uuid = row.try_get("current_attempt_id")?;
        let model_call_count: i64 = row.try_get("model_call_count")?;
        let latest_model_call: Option<Uuid> = row.try_get("latest_model_call_id")?;
        let transcript_entry_count: i64 = row.try_get("transcript_entry_count")?;
        let latest_transcript_entry: Option<Uuid> = row.try_get("latest_transcript_entry_id")?;
        candidates.push(StaleTurnCandidate::new(
            session_id_from_uuid(session),
            turn_id_from_uuid(turn),
            TurnLivenessEvidence::new(
                TurnAttemptId::from_uuid(attempt),
                model_call_count.unsigned_abs(),
                latest_model_call.map(ModelCallId::from_uuid),
                transcript_entry_count.unsigned_abs(),
                latest_transcript_entry.map(SemanticTranscriptEntryId::from_uuid),
            ),
        ));
    }
    Ok(candidates.into_boxed_slice())
}

async fn terminalize_in_transaction(
    connection: &mut PgConnection,
    candidate: StaleTurnCandidate,
    identities: AcceptedInputTurnFailureIdentities,
) -> Result<StaleTurnOutcome, TurnLivenessRepositoryError> {
    let locks = sqlx::query(crate::lock_inventory::STARTUP_RECOVERY)
        .bind(session_id_to_uuid(candidate.session()))
        .fetch_one(&mut *connection)
        .await?;
    let session_exists: bool = locks.try_get(0)?;
    let scheduler_session: Option<Uuid> = locks.try_get(1)?;
    let active_turn: Option<Uuid> = locks.try_get(2)?;
    if !session_exists
        || scheduler_session.is_none()
        || active_turn != Some(turn_id_to_uuid(candidate.turn()))
    {
        return Ok(StaleTurnOutcome::Superseded);
    }
    // The scan ran without the scheduler lock, so the whole predicate is
    // re-decided here against rows no concurrent pass can now be changing.
    let locked = quiescent_active_turns(connection, Some(candidate.session())).await?;
    if locked.first().copied() != Some(candidate) {
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
    let prepared = match scheduling.prepare_active_turn_lost_failure(identities) {
        Ok(prepared) => prepared,
        // Every refusal is a shape the locked predicate said was absent, so
        // none of them can be answered by forcing the transition. Reporting
        // the turn as superseded leaves it for the next pass, which is the
        // direction that cannot end live work.
        Err(error) => match error.failure() {
            AcceptedInputTurnFailureFailure::NoActiveTurn
            | AcceptedInputTurnFailureFailure::PendingSteering { .. }
            | AcceptedInputTurnFailureFailure::ActiveAttemptCannotEndLost
            | AcceptedInputTurnFailureFailure::ActiveStartMissing
            | AcceptedInputTurnFailureFailure::StartingSnapshotMissing
            | AcceptedInputTurnFailureFailure::TerminalFrontierCannotAppend => {
                return Ok(StaleTurnOutcome::Superseded);
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
