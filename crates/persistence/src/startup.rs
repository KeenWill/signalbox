//! Atomic PostgreSQL recovery of prior-process active attempts.

use std::{collections::BTreeSet, error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{
    ClassifyOperatorFailure, OperatorFailureClass, StartupScanRepository, StartupScanSessionOutcome,
};
use signalbox_domain::{
    AcceptedInputId, AcceptedInputTurnFailureFailure, AcceptedInputTurnFailureIdentities,
    AttemptEnd, CurrentModelCallState, FailedModelCallTurnIdentities, ModelCallTerminalOutcome,
    PendingSteeringReclassificationIdentity, PreparedAcceptedInputTurnFailure,
    SemanticTranscriptEntryPayload as InitialSemanticTranscriptEntryPayload, SessionId,
    TurnDisposition, TurnId, UnstoppedAttemptDisposition,
};
use sqlx::{PgConnection, PgPool, types::Uuid};

use crate::{
    mapping::{
        input_position_to_numeric, session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid,
        turn_id_to_uuid,
    },
    model_execution::{
        ModelCallCorruption, ModelCallIdentityCollision, ModelCallRepositoryError,
        persist_terminal_outcome, require_live_execution_for_restart,
    },
    outbox,
    session::{SessionCorruption, SessionRepositoryError, load_session_from_connection},
    submit_input::{SubmitInputCorruption, SubmitInputRepositoryError, load_scheduling_projection},
};

/// Which fresh startup-recovery identity collided durably.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupScanIdentityCollision {
    /// The proposed `TurnFailed` entry identity already exists.
    FailureEntry,
    /// The proposed terminal context-frontier identity already exists.
    TerminalFrontier,
    /// A proposed reclassified successor-turn identity already exists.
    ReclassifiedTurn,
}

impl fmt::Display for StartupScanIdentityCollision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let identity = match self {
            Self::FailureEntry => "failure semantic-entry",
            Self::TerminalFrontier => "terminal context-frontier",
            Self::ReclassifiedTurn => "reclassified successor-turn",
        };
        write!(formatter, "{identity} identity already exists")
    }
}

impl Error for StartupScanIdentityCollision {}

/// A durable shape that cannot reconstruct or commit startup recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartupScanCorruption {
    /// One required durable record is absent.
    Missing(&'static str),
    /// Correlated durable records disagree.
    Inconsistent(&'static str),
    /// The current session projection is invalid.
    CurrentSession(SessionCorruption),
    /// Complete scheduling records fail checked persistence mapping.
    Scheduling(SubmitInputCorruption),
    /// Complete model-call records fail checked persistence mapping.
    ModelCall(ModelCallCorruption),
}

impl fmt::Display for StartupScanCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(record) => write!(formatter, "missing startup-scan {record}"),
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent startup-scan {relationship}")
            }
            Self::CurrentSession(error) => {
                write!(
                    formatter,
                    "startup-scan current Session is invalid: {error}"
                )
            }
            Self::Scheduling(error) => {
                write!(
                    formatter,
                    "startup-scan scheduling projection is invalid: {error}"
                )
            }
            Self::ModelCall(error) => error.fmt(formatter),
        }
    }
}

impl Error for StartupScanCorruption {}

/// Database, integrity, or identity-collision failure during startup scan.
#[derive(Debug)]
pub enum StartupScanRepositoryError {
    /// PostgreSQL could not complete the operation.
    Database {
        /// The underlying SQLx failure.
        source: sqlx::Error,
        /// Whether failure occurred while awaiting commit.
        commit_ambiguous: bool,
    },
    /// Durable records cannot reconstruct or commit the accepted shape.
    Corruption {
        /// The invalid durable shape.
        source: StartupScanCorruption,
        /// The active durable turn observed for the scoped session.
        turn: Option<TurnId>,
    },
    /// A supplied fresh identity already names a durable record.
    IdentityCollision(StartupScanIdentityCollision),
}

impl fmt::Display for StartupScanRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { source, .. } => write!(formatter, "startup scan failed: {source}"),
            Self::Corruption { source, .. } => source.fmt(formatter),
            Self::IdentityCollision(error) => error.fmt(formatter),
        }
    }
}

impl Error for StartupScanRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Corruption { source, .. } => Some(source),
            Self::IdentityCollision(error) => Some(error),
        }
    }
}

impl ClassifyOperatorFailure for StartupScanRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database {
                commit_ambiguous, ..
            } => OperatorFailureClass::Infrastructure {
                commit_ambiguous: *commit_ambiguous,
            },
            Self::Corruption { .. } => OperatorFailureClass::FailClosedCorruption,
            Self::IdentityCollision(_) => OperatorFailureClass::IdentityCollision,
        }
    }
}

impl From<StartupScanCorruption> for StartupScanRepositoryError {
    fn from(error: StartupScanCorruption) -> Self {
        Self::Corruption {
            source: error,
            turn: None,
        }
    }
}

impl From<sqlx::Error> for StartupScanRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::from_database(error, false)
    }
}

impl StartupScanRepositoryError {
    /// Returns the relevant durable turn for corruption scoped to one active
    /// turn.
    pub const fn corruption_turn(&self) -> Option<TurnId> {
        match self {
            Self::Corruption { turn, .. } => *turn,
            Self::Database { .. } | Self::IdentityCollision(_) => None,
        }
    }

    fn from_database(error: sqlx::Error, commit_ambiguous: bool) -> Self {
        if let Some(collision) = identity_collision(&error) {
            Self::IdentityCollision(collision)
        } else {
            Self::Database {
                source: error,
                commit_ambiguous,
            }
        }
    }

    fn with_corruption_turn(self, turn: Option<TurnId>) -> Self {
        match self {
            Self::Corruption { source, turn: None } => Self::Corruption { source, turn },
            error => error,
        }
    }
}

enum TransactionDecision {
    Commit(StartupScanSessionOutcome),
    Rollback(StartupScanSessionOutcome),
}

/// PostgreSQL inventory and authoritative per-session recovery adapter.
#[derive(Clone, Debug)]
pub struct PostgresStartupScanRepository {
    pool: PgPool,
}

impl PostgresStartupScanRepository {
    /// Uses the supplied shared pool for startup recovery.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Reads the finite active-session inventory in deterministic order.
    pub async fn active_sessions(&self) -> Result<Box<[SessionId]>, StartupScanRepositoryError> {
        let rows = sqlx::query_scalar::<_, Uuid>(
            "SELECT session_id
               FROM turn_lifecycle
              WHERE state_kind = 'active'
              ORDER BY session_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(session_id_from_uuid)
            .collect::<Vec<_>>()
            .into_boxed_slice())
    }

    /// Locks one session and atomically terminalizes its prior-process attempt.
    pub async fn recover<NextTurn>(
        &self,
        session: SessionId,
        identities: AcceptedInputTurnFailureIdentities,
        next_reclassified_turn: NextTurn,
    ) -> Result<StartupScanSessionOutcome, StartupScanRepositoryError>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        let mut transaction = self.pool.begin().await?;
        let decision = recover_in_transaction(
            &mut transaction,
            session,
            identities,
            next_reclassified_turn,
        )
        .await;

        match decision {
            Ok(TransactionDecision::Commit(outcome)) => {
                transaction.commit().await.map_err(|error| {
                    let commit_ambiguous = commit_failure_is_ambiguous(&error);
                    StartupScanRepositoryError::from_database(error, commit_ambiguous)
                })?;
                Ok(outcome)
            }
            Ok(TransactionDecision::Rollback(outcome)) => {
                transaction.rollback().await?;
                Ok(outcome)
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

impl StartupScanRepository for PostgresStartupScanRepository {
    type Error = StartupScanRepositoryError;

    async fn active_sessions(&mut self) -> Result<Box<[SessionId]>, Self::Error> {
        PostgresStartupScanRepository::active_sessions(self).await
    }

    async fn recover<NextTurn>(
        &mut self,
        session: SessionId,
        identities: AcceptedInputTurnFailureIdentities,
        next_reclassified_turn: NextTurn,
    ) -> Result<StartupScanSessionOutcome, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        PostgresStartupScanRepository::recover(self, session, identities, next_reclassified_turn)
            .await
    }
}

async fn recover_in_transaction<NextTurn>(
    connection: &mut PgConnection,
    requested_session: SessionId,
    identities: AcceptedInputTurnFailureIdentities,
    next_reclassified_turn: NextTurn,
) -> Result<TransactionDecision, StartupScanRepositoryError>
where
    NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
{
    // This is the same scheduler-row lock ordering used by every lifecycle
    // writer. Reconstitution and all guarded writes happen while it is held.
    let session_uuid = session_id_to_uuid(requested_session);
    let (session_exists, scheduler_session, active_turn) =
        sqlx::query_as::<_, (bool, Option<Uuid>, Option<Uuid>)>(
            crate::lock_inventory::STARTUP_RECOVERY,
        )
        .bind(session_uuid)
        .fetch_one(&mut *connection)
        .await?;

    recover_locked_session(
        connection,
        requested_session,
        identities,
        session_exists,
        scheduler_session,
        next_reclassified_turn,
    )
    .await
    .map_err(|error| error.with_corruption_turn(active_turn.map(turn_id_from_uuid)))
}

async fn recover_locked_session<NextTurn>(
    connection: &mut PgConnection,
    requested_session: SessionId,
    identities: AcceptedInputTurnFailureIdentities,
    session_exists: bool,
    scheduler_session: Option<Uuid>,
    mut next_reclassified_turn: NextTurn,
) -> Result<TransactionDecision, StartupScanRepositoryError>
where
    NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
{
    if scheduler_session.is_none() {
        if session_exists {
            return Err(StartupScanCorruption::Missing("session scheduler row").into());
        }
        return Ok(TransactionDecision::Rollback(
            StartupScanSessionOutcome::NoActiveTurn,
        ));
    }

    let session = match load_session_from_connection(connection, requested_session).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            return Err(StartupScanCorruption::Inconsistent("locked session disappeared").into());
        }
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(StartupScanCorruption::CurrentSession(error).into());
        }
    };
    let scheduling = load_scheduling_projection(connection, session)
        .await
        .map_err(map_scheduling_error)?;

    let Some(active_turn) = scheduling.active_turn_execution() else {
        return Ok(TransactionDecision::Rollback(
            StartupScanSessionOutcome::NoActiveTurn,
        ));
    };
    if !matches!(
        active_turn.phase(),
        signalbox_domain::ActiveTurnPhase::Running { .. }
    ) {
        return Ok(TransactionDecision::Rollback(
            StartupScanSessionOutcome::NoActiveTurn,
        ));
    }
    let pending_steering = active_turn
        .pending_steering()
        .first()
        .map(signalbox_domain::PendingSteeringInput::accepted_input);

    let model_execution = require_live_execution_for_restart(connection, requested_session)
        .await
        .map_err(map_model_call_error)?;
    if let Some(call_state) = model_execution.current_call().map(|call| call.state()) {
        let mut failure_identities = FailedModelCallTurnIdentities::new(
            identities.failure_entry(),
            identities.terminal_frontier(),
        );
        if call_state == CurrentModelCallState::Prepared {
            let mut proposed_turns = BTreeSet::new();
            let mut reclassifications = Vec::new();
            for pending in model_execution.active_turn().pending_steering() {
                let accepted_input = pending.accepted_input();
                let proposed_turn = next_reclassified_turn(accepted_input);
                record_reclassified_turn_candidate(
                    model_execution.turn(),
                    proposed_turn,
                    &mut proposed_turns,
                )?;
                reclassifications.push(PendingSteeringReclassificationIdentity::new(
                    accepted_input,
                    proposed_turn,
                ));
            }
            failure_identities =
                failure_identities.with_pending_steering_reclassifications(reclassifications);
        }
        let outcome = model_execution
            .recover_after_restart(failure_identities)
            .map_err(|_| {
                StartupScanCorruption::Inconsistent("model-call restart classification")
            })?;
        if !matches!(
            outcome,
            ModelCallTerminalOutcome::Failed(_) | ModelCallTerminalOutcome::AwaitingRecovery(_)
        ) {
            return Err(StartupScanCorruption::Inconsistent("model-call restart outcome").into());
        }
        persist_terminal_outcome(connection, &outcome)
            .await
            .map_err(map_model_call_error)?;
        return Ok(TransactionDecision::Commit(
            StartupScanSessionOutcome::RecoveredModelCall(Box::new(outcome)),
        ));
    }

    if pending_steering.is_some() {
        let mut proposed_turns = BTreeSet::new();
        let mut reclassifications = Vec::new();
        for pending in model_execution.active_turn().pending_steering() {
            let accepted_input = pending.accepted_input();
            let proposed_turn = next_reclassified_turn(accepted_input);
            record_reclassified_turn_candidate(
                model_execution.turn(),
                proposed_turn,
                &mut proposed_turns,
            )?;
            reclassifications.push(PendingSteeringReclassificationIdentity::new(
                accepted_input,
                proposed_turn,
            ));
        }
        let failure_identities = FailedModelCallTurnIdentities::new(
            identities.failure_entry(),
            identities.terminal_frontier(),
        )
        .with_pending_steering_reclassifications(reclassifications);
        let failed = model_execution
            .recover_evidence_free_after_restart(failure_identities)
            .map_err(|_| {
                StartupScanCorruption::Inconsistent("evidence-free restart classification")
            })?;
        let outcome = ModelCallTerminalOutcome::Failed(failed);
        persist_terminal_outcome(connection, &outcome)
            .await
            .map_err(map_model_call_error)?;
        return Ok(TransactionDecision::Commit(
            StartupScanSessionOutcome::RecoveredModelCall(Box::new(outcome)),
        ));
    }

    let prepared = match scheduling.prepare_active_turn_lost_failure(identities) {
        Ok(prepared) => prepared,
        Err(error) => match error.failure() {
            AcceptedInputTurnFailureFailure::NoActiveTurn => {
                return Ok(TransactionDecision::Rollback(
                    StartupScanSessionOutcome::NoActiveTurn,
                ));
            }
            AcceptedInputTurnFailureFailure::PendingSteering { accepted_input } => {
                return Ok(TransactionDecision::Rollback(
                    StartupScanSessionOutcome::DeferredPendingSteering { accepted_input },
                ));
            }
            AcceptedInputTurnFailureFailure::FailureEntryIdentityAlreadyExists => {
                return Err(StartupScanRepositoryError::IdentityCollision(
                    StartupScanIdentityCollision::FailureEntry,
                ));
            }
            AcceptedInputTurnFailureFailure::TerminalFrontierIdentityAlreadyExists => {
                return Err(StartupScanRepositoryError::IdentityCollision(
                    StartupScanIdentityCollision::TerminalFrontier,
                ));
            }
            AcceptedInputTurnFailureFailure::ActiveAttemptCannotEndLost
            | AcceptedInputTurnFailureFailure::ActiveStartMissing
            | AcceptedInputTurnFailureFailure::StartingSnapshotMissing
            | AcceptedInputTurnFailureFailure::TerminalFrontierCannotAppend => {
                return Err(
                    StartupScanCorruption::Inconsistent("active failure preparation").into(),
                );
            }
        },
    };

    let failed = insert_prepared_failure(connection, prepared).await?;
    Ok(TransactionDecision::Commit(
        StartupScanSessionOutcome::Recovered(Box::new(failed)),
    ))
}

async fn insert_prepared_failure(
    connection: &mut PgConnection,
    prepared: PreparedAcceptedInputTurnFailure,
) -> Result<signalbox_domain::FailedAcceptedInputTurn, StartupScanRepositoryError> {
    let (failed, failure_entry, terminal_snapshot) = prepared.into_parts();
    let session = failed.session();
    let turn = failed.turn();
    if failure_entry.source_session() != session
        || failure_entry.payload() != &(InitialSemanticTranscriptEntryPayload::TurnFailed { turn })
        || terminal_snapshot.frontier().owning_session() != session
        || terminal_snapshot.frontier().snapshot() != failed.terminal_frontier()
        || failed.disposition() != &TurnDisposition::Failed
    {
        return Err(StartupScanCorruption::Inconsistent("prepared failure ownership").into());
    }
    let attempt = failed.ended_attempt();
    if attempt.end()
        != &(AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Lost,
        })
    {
        return Err(StartupScanCorruption::Inconsistent("prepared Lost attempt end").into());
    }

    let ended = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'lost'
          WHERE turn_attempt_id = $1
            AND turn_id = $2
            AND session_id = $3
            AND state_kind IN ('prepared', 'running')
            AND end_variant IS NULL
            AND end_disposition IS NULL",
    )
    .bind(attempt.id().into_uuid())
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    if ended != 1 {
        return Err(StartupScanCorruption::Inconsistent("guarded attempt end cardinality").into());
    }

    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session_id_to_uuid(session))
    .bind(failure_entry.identity().into_uuid())
    .bind(turn_id_to_uuid(turn))
    .execute(&mut *connection)
    .await?;

    let member_count = u64::try_from(terminal_snapshot.entry_count())
        .map_err(|_| StartupScanCorruption::Inconsistent("terminal frontier member count"))?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, $3)",
    )
    .bind(session_id_to_uuid(session))
    .bind(terminal_snapshot.frontier().snapshot().into_uuid())
    .bind(Decimal::from(member_count))
    .execute(&mut *connection)
    .await?;
    for (index, entry) in terminal_snapshot.ordered_entries().enumerate() {
        let position = u64::try_from(index + 1)
            .map_err(|_| StartupScanCorruption::Inconsistent("terminal member position"))?;
        sqlx::query(
            "INSERT INTO context_frontier_member
                (owning_session_id, context_frontier_id, member_position,
                 source_session_id, semantic_entry_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(session_id_to_uuid(session))
        .bind(terminal_snapshot.frontier().snapshot().into_uuid())
        .bind(Decimal::from(position))
        .bind(session_id_to_uuid(entry.source_session()))
        .bind(entry.entry().into_uuid())
        .execute(&mut *connection)
        .await?;
    }

    let updated = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = $1,
                active_phase_kind = NULL,
                terminal_attempt_id = current_attempt_id,
                current_attempt_id = NULL,
                terminal_model_call_id = NULL,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $2
            AND session_id = $3
            AND origin_accepted_input_id = $4
            AND acceptance_position = $5
            AND state_kind = 'active'
            AND starting_frontier_id = $6
            AND active_phase_kind = 'running'
            AND current_attempt_id = $7",
    )
    .bind(terminal_snapshot.frontier().snapshot().into_uuid())
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .bind(failed.accepted_input().id().into_uuid())
    .bind(input_position_to_numeric(
        failed.order().acceptance_position(),
    ))
    .bind(failed.start().frontier().snapshot().into_uuid())
    .bind(attempt.id().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    if updated != 1 {
        return Err(
            StartupScanCorruption::Inconsistent("guarded lifecycle terminalization").into(),
        );
    }

    outbox::append(
        connection,
        outbox::OutboxEvent::TurnFailed {
            session,
            turn,
            failure_entry: failure_entry.identity(),
            terminal_frontier: terminal_snapshot.frontier().snapshot(),
        },
    )
    .await?;

    Ok(failed)
}

fn map_scheduling_error(error: SubmitInputRepositoryError) -> StartupScanRepositoryError {
    match error {
        SubmitInputRepositoryError::Database(error) => error.into(),
        SubmitInputRepositoryError::Corruption(error) => {
            StartupScanCorruption::Scheduling(error).into()
        }
        SubmitInputRepositoryError::DifferentCommandKind { .. } => {
            StartupScanCorruption::Inconsistent("origin command kind").into()
        }
        SubmitInputRepositoryError::AcceptedInputIdentityCollision { .. } => {
            StartupScanCorruption::Inconsistent("origin accepted-input identity").into()
        }
        SubmitInputRepositoryError::InterruptApplicationUnavailable { .. } => {
            StartupScanCorruption::Inconsistent("origin command application").into()
        }
    }
}

fn map_model_call_error(error: ModelCallRepositoryError) -> StartupScanRepositoryError {
    match error {
        ModelCallRepositoryError::Database { source, .. } => source.into(),
        ModelCallRepositoryError::Corruption(source) => {
            StartupScanCorruption::ModelCall(source).into()
        }
        ModelCallRepositoryError::IdentityCollision(ModelCallIdentityCollision::SemanticEntry) => {
            StartupScanRepositoryError::IdentityCollision(
                StartupScanIdentityCollision::FailureEntry,
            )
        }
        ModelCallRepositoryError::IdentityCollision(
            ModelCallIdentityCollision::TerminalFrontier,
        ) => StartupScanRepositoryError::IdentityCollision(
            StartupScanIdentityCollision::TerminalFrontier,
        ),
        ModelCallRepositoryError::IdentityCollision(
            ModelCallIdentityCollision::ReclassifiedTurn,
        ) => StartupScanRepositoryError::IdentityCollision(
            StartupScanIdentityCollision::ReclassifiedTurn,
        ),
        ModelCallRepositoryError::IdentityCollision(ModelCallIdentityCollision::ModelCall)
        | ModelCallRepositoryError::NoLiveExecution
        | ModelCallRepositoryError::InvalidTransition(_) => {
            StartupScanCorruption::Inconsistent("model-call recovery transition").into()
        }
    }
}

fn record_reclassified_turn_candidate(
    source_turn: TurnId,
    proposed_turn: TurnId,
    proposed_turns: &mut BTreeSet<TurnId>,
) -> Result<(), StartupScanRepositoryError> {
    if proposed_turn == source_turn || !proposed_turns.insert(proposed_turn) {
        return Err(StartupScanRepositoryError::IdentityCollision(
            StartupScanIdentityCollision::ReclassifiedTurn,
        ));
    }
    Ok(())
}

fn identity_collision(error: &sqlx::Error) -> Option<StartupScanIdentityCollision> {
    match error
        .as_database_error()
        .and_then(|database| database.constraint())
    {
        Some("semantic_transcript_entry_pk" | "semantic_transcript_entry_id_global") => {
            Some(StartupScanIdentityCollision::FailureEntry)
        }
        Some("context_frontier_pk" | "context_frontier_id_global") => {
            Some(StartupScanIdentityCollision::TerminalFrontier)
        }
        _ => None,
    }
}

fn commit_failure_is_ambiguous(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(database) => {
            matches!(database.code().as_deref(), Some("08007" | "40003"))
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, collections::BTreeSet, error::Error, fmt, io};

    use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
    use signalbox_domain::TurnId;
    use sqlx::error::{DatabaseError, ErrorKind};
    use sqlx::types::Uuid;

    use super::{
        StartupScanCorruption, StartupScanIdentityCollision, StartupScanRepositoryError,
        commit_failure_is_ambiguous, record_reclassified_turn_candidate,
    };

    /// INV-034: a generated source-turn identity is a retryable collision, not
    /// durable corruption.
    #[test]
    fn inv034_generated_successor_source_candidate_is_a_retryable_collision() {
        let source = TurnId::from_uuid(Uuid::from_u128(1));
        let mut proposed = BTreeSet::new();

        assert!(matches!(
            record_reclassified_turn_candidate(source, source, &mut proposed),
            Err(StartupScanRepositoryError::IdentityCollision(
                StartupScanIdentityCollision::ReclassifiedTurn
            ))
        ));
    }

    /// INV-034: a duplicate generated successor is a retryable collision, not
    /// durable corruption.
    #[test]
    fn inv034_generated_successor_duplicate_is_a_retryable_collision() {
        let source = TurnId::from_uuid(Uuid::from_u128(1));
        let successor = TurnId::from_uuid(Uuid::from_u128(2));
        let mut proposed = BTreeSet::new();

        record_reclassified_turn_candidate(source, successor, &mut proposed)
            .expect("the first source-safe successor is accepted");
        assert!(matches!(
            record_reclassified_turn_candidate(source, successor, &mut proposed),
            Err(StartupScanRepositoryError::IdentityCollision(
                StartupScanIdentityCollision::ReclassifiedTurn
            ))
        ));
    }

    #[derive(Debug)]
    struct ServerCommitFailure {
        code: &'static str,
    }

    impl fmt::Display for ServerCommitFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("server reported commit failure")
        }
    }

    impl Error for ServerCommitFailure {}

    impl DatabaseError for ServerCommitFailure {
        fn message(&self) -> &str {
            "server reported commit failure"
        }

        fn as_error(&self) -> &(dyn Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn Error + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.code))
        }
    }

    #[test]
    fn corruption_retains_the_scoped_durable_turn() {
        let turn = TurnId::from_uuid(Uuid::from_u128(1));
        let error =
            StartupScanRepositoryError::from(StartupScanCorruption::Missing("active turn record"))
                .with_corruption_turn(Some(turn));

        assert_eq!(error.corruption_turn(), Some(turn));
        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::FailClosedCorruption
        );
    }

    #[test]
    fn precommit_database_failure_is_not_commit_ambiguous() {
        let error = StartupScanRepositoryError::from_database(sqlx::Error::PoolClosed, false);
        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false
            }
        );
    }

    #[test]
    fn lost_commit_response_is_commit_ambiguous() {
        let error = sqlx::Error::Io(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "commit response was lost",
        ));
        let commit_ambiguous = commit_failure_is_ambiguous(&error);

        assert!(commit_ambiguous);
        let error = StartupScanRepositoryError::from_database(error, commit_ambiguous);
        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            }
        );
    }

    #[test]
    fn server_rejected_commit_is_not_ambiguous() {
        let error = sqlx::Error::Database(Box::new(ServerCommitFailure { code: "23514" }));
        let commit_ambiguous = commit_failure_is_ambiguous(&error);

        assert!(!commit_ambiguous);
        let error = StartupScanRepositoryError::from_database(error, commit_ambiguous);
        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false
            }
        );
    }

    #[test]
    fn server_reported_transaction_resolution_unknown_is_ambiguous() {
        let error = sqlx::Error::Database(Box::new(ServerCommitFailure { code: "08007" }));
        let commit_ambiguous = commit_failure_is_ambiguous(&error);

        assert!(commit_ambiguous);
        let classified = StartupScanRepositoryError::from_database(error, commit_ambiguous);
        assert_eq!(
            classified.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            }
        );
    }

    #[test]
    fn server_reported_statement_completion_unknown_is_ambiguous() {
        let error = sqlx::Error::Database(Box::new(ServerCommitFailure { code: "40003" }));
        let commit_ambiguous = commit_failure_is_ambiguous(&error);

        assert!(commit_ambiguous);
        let classified = StartupScanRepositoryError::from_database(error, commit_ambiguous);
        assert_eq!(
            classified.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            }
        );
    }
}
