//! Atomic PostgreSQL activation of the earliest eligible accepted-input turn.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{StartEligibleTurnOutcome, StartEligibleTurnTransaction};
use signalbox_domain::{
    AcceptedInputEligibilityFailure, AcceptedInputStartingLineage,
    AcceptedInputTurnActivationIdentities, ActiveTurnPhase, CurrentTurnAttemptState,
    InitialSemanticTranscriptEntryPayload, PreparedAcceptedInputTurnActivation, SessionId,
};
use sqlx::{PgConnection, PgPool, types::Uuid};

use crate::{
    mapping::{input_position_to_numeric, session_id_to_uuid, turn_id_to_uuid},
    session::{SessionCorruption, SessionRepositoryError, load_session_from_connection},
    submit_input::{SubmitInputCorruption, SubmitInputRepositoryError, load_scheduling_projection},
};

/// Which fresh activation identity collided with an existing durable identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartEligibleTurnIdentityCollision {
    /// The proposed semantic origin-entry identity already exists.
    OriginEntry,
    /// The proposed starting context-frontier identity already exists.
    StartingFrontier,
    /// The proposed initial turn-attempt identity already exists.
    InitialAttempt,
}

impl fmt::Display for StartEligibleTurnIdentityCollision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let identity = match self {
            Self::OriginEntry => "origin semantic-entry",
            Self::StartingFrontier => "starting context-frontier",
            Self::InitialAttempt => "initial turn-attempt",
        };
        write!(formatter, "{identity} identity already exists")
    }
}

impl Error for StartEligibleTurnIdentityCollision {}

/// A durable shape that cannot reconstruct or commit one eligibility pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartEligibleTurnCorruption {
    /// One required durable record is absent.
    Missing(&'static str),
    /// Correlated durable records disagree.
    Inconsistent(&'static str),
    /// The current session projection is invalid.
    CurrentSession(SessionCorruption),
    /// Complete scheduling records fail their checked persistence mapping.
    Scheduling(SubmitInputCorruption),
}

impl fmt::Display for StartEligibleTurnCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(record) => write!(formatter, "missing StartEligibleTurn {record}"),
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent StartEligibleTurn {relationship}")
            }
            Self::CurrentSession(error) => {
                write!(
                    formatter,
                    "StartEligibleTurn current Session is invalid: {error}"
                )
            }
            Self::Scheduling(error) => {
                write!(
                    formatter,
                    "StartEligibleTurn scheduling projection is invalid: {error}"
                )
            }
        }
    }
}

impl Error for StartEligibleTurnCorruption {}

/// A database, integrity, or identity-collision failure during eligibility.
#[derive(Debug)]
pub enum StartEligibleTurnRepositoryError {
    /// PostgreSQL could not complete the transaction.
    Database(sqlx::Error),
    /// Durable records cannot reconstruct or commit the accepted domain shape.
    Corruption(StartEligibleTurnCorruption),
    /// A supplied fresh identity already names a durable record.
    IdentityCollision(StartEligibleTurnIdentityCollision),
}

impl fmt::Display for StartEligibleTurnRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(formatter, "StartEligibleTurn database failure: {error}")
            }
            Self::Corruption(error) => error.fmt(formatter),
            Self::IdentityCollision(error) => error.fmt(formatter),
        }
    }
}

impl Error for StartEligibleTurnRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
            Self::IdentityCollision(error) => Some(error),
        }
    }
}

impl From<StartEligibleTurnCorruption> for StartEligibleTurnRepositoryError {
    fn from(error: StartEligibleTurnCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl From<sqlx::Error> for StartEligibleTurnRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        if let Some(collision) = identity_collision(&error) {
            Self::IdentityCollision(collision)
        } else {
            Self::Database(error)
        }
    }
}

enum TransactionDecision {
    Commit(StartEligibleTurnOutcome),
    Rollback(StartEligibleTurnOutcome),
}

/// PostgreSQL implementation of one authoritative session eligibility pass.
#[derive(Clone, Debug)]
pub struct StartEligibleTurnRepository {
    pool: PgPool,
}

impl StartEligibleTurnRepository {
    /// Uses the supplied pool for serialized, atomic eligibility handling.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Locks one session scheduler row, reconstitutes complete scheduling
    /// state, and atomically activates the earliest eligible queued turn.
    pub async fn handle(
        &self,
        session: SessionId,
        identities: AcceptedInputTurnActivationIdentities,
    ) -> Result<StartEligibleTurnOutcome, StartEligibleTurnRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let decision = handle_in_transaction(&mut transaction, session, identities).await;

        match decision {
            Ok(TransactionDecision::Commit(outcome)) => {
                transaction.commit().await?;
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

impl StartEligibleTurnTransaction for StartEligibleTurnRepository {
    type Error = StartEligibleTurnRepositoryError;

    async fn handle(
        &mut self,
        session: SessionId,
        identities: AcceptedInputTurnActivationIdentities,
    ) -> Result<StartEligibleTurnOutcome, Self::Error> {
        StartEligibleTurnRepository::handle(self, session, identities).await
    }
}

async fn handle_in_transaction(
    connection: &mut PgConnection,
    requested_session: SessionId,
    identities: AcceptedInputTurnActivationIdentities,
) -> Result<TransactionDecision, StartEligibleTurnRepositoryError> {
    // Lock inventory for this transaction: the `session_scheduler` row below
    // is its only explicit lock (`FOR UPDATE`); the session row is locked only
    // `KEY SHARE`, implicitly, by the inserts' session foreign keys; and the
    // candidate `turn_lifecycle` row is locked `NO KEY UPDATE` by the guarded
    // activation UPDATE itself (plus `KEY SHARE` from the `turn_attempt`
    // insert's foreign key). Two standing constraints: every turn-lifecycle
    // writer acquires this scheduler lock before touching `turn_lifecycle`
    // rows, and no production path may lock the session row `FOR UPDATE` —
    // see the lock-mode contract beside the session-row lock in
    // `submit_input.rs::prepare_against_locked_state`.
    let session_uuid = session_id_to_uuid(requested_session);
    let (session_exists, scheduler_session) = sqlx::query_as::<_, (bool, Option<Uuid>)>(
        "SELECT
            EXISTS (
                SELECT 1
                  FROM session
                 WHERE session_id = $1
            ),
            (
                SELECT session_id
                  FROM session_scheduler
                 WHERE session_id = $1
                 FOR UPDATE
            )",
    )
    .bind(session_uuid)
    .fetch_one(&mut *connection)
    .await?;

    if scheduler_session.is_none() {
        if session_exists {
            return Err(StartEligibleTurnCorruption::Missing("session scheduler row").into());
        }
        return Ok(TransactionDecision::Rollback(
            StartEligibleTurnOutcome::NoEligibleTurn,
        ));
    }

    let session = match load_session_from_connection(connection, requested_session).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            return Err(
                StartEligibleTurnCorruption::Inconsistent("locked session disappeared").into(),
            );
        }
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(StartEligibleTurnCorruption::CurrentSession(error).into());
        }
    };
    let scheduling = load_scheduling_projection(connection, session)
        .await
        .map_err(map_scheduling_error)?;

    let prepared = match scheduling.prepare_earliest_queued_activation(identities) {
        Ok(prepared) => prepared,
        Err(error) => {
            let outcome = match error.failure() {
                AcceptedInputEligibilityFailure::ActiveTurnPresent { .. }
                | AcceptedInputEligibilityFailure::NoQueuedTurn => {
                    return Ok(TransactionDecision::Rollback(
                        StartEligibleTurnOutcome::NoEligibleTurn,
                    ));
                }
                AcceptedInputEligibilityFailure::OriginEntryIdentityAlreadyExists => {
                    StartEligibleTurnIdentityCollision::OriginEntry
                }
                AcceptedInputEligibilityFailure::StartingFrontierIdentityAlreadyExists => {
                    StartEligibleTurnIdentityCollision::StartingFrontier
                }
                AcceptedInputEligibilityFailure::InitialAttemptIdentityAlreadyExists => {
                    StartEligibleTurnIdentityCollision::InitialAttempt
                }
                AcceptedInputEligibilityFailure::InternalOriginFrontierConstructionFailed => {
                    return Err(StartEligibleTurnCorruption::Inconsistent(
                        "origin frontier construction",
                    )
                    .into());
                }
                AcceptedInputEligibilityFailure::InternalPredecessorTerminalFrontierMissing {
                    ..
                } => {
                    return Err(StartEligibleTurnCorruption::Inconsistent(
                        "predecessor terminal frontier",
                    )
                    .into());
                }
                AcceptedInputEligibilityFailure::InternalStartingFrontierDerivationFailed => {
                    return Err(StartEligibleTurnCorruption::Inconsistent(
                        "starting frontier derivation",
                    )
                    .into());
                }
            };
            return Err(StartEligibleTurnRepositoryError::IdentityCollision(outcome));
        }
    };

    let activated = insert_prepared_activation(connection, prepared).await?;
    Ok(TransactionDecision::Commit(
        StartEligibleTurnOutcome::Activated(Box::new(activated)),
    ))
}

async fn insert_prepared_activation(
    connection: &mut PgConnection,
    prepared: PreparedAcceptedInputTurnActivation,
) -> Result<signalbox_domain::ActivatedAcceptedInputTurn, StartEligibleTurnRepositoryError> {
    let (activated, origin_entry, starting_snapshot) = prepared.into_parts();
    let accepted_input = match origin_entry.payload() {
        InitialSemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input } => {
            accepted_input
        }
        InitialSemanticTranscriptEntryPayload::TurnFailed { .. } => {
            return Err(
                StartEligibleTurnCorruption::Inconsistent("prepared origin-entry payload").into(),
            );
        }
    };
    let session = activated.session();
    if origin_entry.source_session() != session
        || starting_snapshot.frontier().owning_session() != session
    {
        return Err(
            StartEligibleTurnCorruption::Inconsistent("prepared activation ownership").into(),
        );
    }

    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'origin_accepted_input', $3, NULL)",
    )
    .bind(session_id_to_uuid(origin_entry.source_session()))
    .bind(origin_entry.identity().into_uuid())
    .bind(accepted_input.into_uuid())
    .execute(&mut *connection)
    .await?;

    let member_count = u64::try_from(starting_snapshot.entry_count())
        .map_err(|_| StartEligibleTurnCorruption::Inconsistent("starting frontier member count"))?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, $3)",
    )
    .bind(session_id_to_uuid(session))
    .bind(starting_snapshot.frontier().snapshot().into_uuid())
    .bind(Decimal::from(member_count))
    .execute(&mut *connection)
    .await?;
    for (index, entry) in starting_snapshot.ordered_entries().enumerate() {
        let position = u64::try_from(index + 1).map_err(|_| {
            StartEligibleTurnCorruption::Inconsistent("starting frontier member position")
        })?;
        sqlx::query(
            "INSERT INTO context_frontier_member
                (owning_session_id, context_frontier_id, member_position,
                 source_session_id, semantic_entry_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(session_id_to_uuid(session))
        .bind(starting_snapshot.frontier().snapshot().into_uuid())
        .bind(Decimal::from(position))
        .bind(session_id_to_uuid(entry.source_session()))
        .bind(entry.entry().into_uuid())
        .execute(&mut *connection)
        .await?;
    }

    let initial_attempt = match activated.phase() {
        ActiveTurnPhase::Running { current_attempt }
            if current_attempt.state() == &CurrentTurnAttemptState::Prepared =>
        {
            current_attempt.id()
        }
        ActiveTurnPhase::Running { .. }
        | ActiveTurnPhase::AwaitingApproval { .. }
        | ActiveTurnPhase::AwaitingRecoveryDecision { .. } => {
            return Err(
                StartEligibleTurnCorruption::Inconsistent("prepared initial active phase").into(),
            );
        }
    };
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(initial_attempt.into_uuid())
    .bind(turn_id_to_uuid(activated.turn()))
    .bind(session_id_to_uuid(session))
    .execute(&mut *connection)
    .await?;

    let (lineage_kind, predecessor) = match activated.start().lineage() {
        AcceptedInputStartingLineage::FirstInSession => ("first_in_session", None),
        AcceptedInputStartingLineage::After {
            immediate_predecessor,
        } => ("after", Some(turn_id_to_uuid(immediate_predecessor))),
    };
    let updated = sqlx::query(
        "UPDATE turn_lifecycle AS candidate
            SET state_kind = 'active',
                start_lineage_kind = $1,
                immediate_predecessor_turn_id = $2,
                starting_frontier_id = $3,
                active_phase_kind = 'running',
                current_attempt_id = $4
          WHERE candidate.turn_id = $5
            AND candidate.session_id = $6
            AND candidate.origin_accepted_input_id = $7
            AND candidate.acceptance_position = $8
            AND candidate.state_kind = 'queued'
            AND NOT EXISTS (
                SELECT 1
                  FROM turn_lifecycle AS active
                 WHERE active.session_id = candidate.session_id
                   AND active.state_kind = 'active'
            )
            AND NOT EXISTS (
                SELECT 1
                  FROM turn_lifecycle AS earlier
                 WHERE earlier.session_id = candidate.session_id
                   AND earlier.acceptance_position < candidate.acceptance_position
                   AND earlier.state_kind <> 'terminal'
            )
            AND (
                (
                    $1 = 'first_in_session'
                    AND $2::uuid IS NULL
                    AND NOT EXISTS (
                        SELECT 1
                          FROM turn_lifecycle AS earlier
                         WHERE earlier.session_id = candidate.session_id
                           AND earlier.acceptance_position < candidate.acceptance_position
                    )
                )
                OR
                (
                    $1 = 'after'
                    AND $2::uuid = (
                        SELECT earlier.turn_id
                          FROM turn_lifecycle AS earlier
                         WHERE earlier.session_id = candidate.session_id
                           AND earlier.acceptance_position < candidate.acceptance_position
                         ORDER BY earlier.acceptance_position DESC
                         LIMIT 1
                    )
                )
            )",
    )
    .bind(lineage_kind)
    .bind(predecessor)
    .bind(starting_snapshot.frontier().snapshot().into_uuid())
    .bind(initial_attempt.into_uuid())
    .bind(turn_id_to_uuid(activated.turn()))
    .bind(session_id_to_uuid(session))
    .bind(activated.accepted_input().id().into_uuid())
    .bind(input_position_to_numeric(
        activated.order().acceptance_position(),
    ))
    .execute(&mut *connection)
    .await?
    .rows_affected();

    match updated {
        1 => Ok(activated),
        0 => Err(
            StartEligibleTurnCorruption::Inconsistent("guarded activation matched no row").into(),
        ),
        _ => {
            Err(StartEligibleTurnCorruption::Inconsistent("guarded activation cardinality").into())
        }
    }
}

fn map_scheduling_error(error: SubmitInputRepositoryError) -> StartEligibleTurnRepositoryError {
    match error {
        SubmitInputRepositoryError::Database(error) => error.into(),
        SubmitInputRepositoryError::Corruption(error) => {
            StartEligibleTurnCorruption::Scheduling(error).into()
        }
        SubmitInputRepositoryError::DifferentCommandKind { .. } => {
            StartEligibleTurnCorruption::Inconsistent("origin command kind").into()
        }
        SubmitInputRepositoryError::AcceptedInputIdentityCollision { .. } => {
            StartEligibleTurnCorruption::Inconsistent("origin accepted-input identity").into()
        }
        SubmitInputRepositoryError::InterruptApplicationUnavailable { .. } => {
            StartEligibleTurnCorruption::Inconsistent("origin command application").into()
        }
    }
}

fn identity_collision(error: &sqlx::Error) -> Option<StartEligibleTurnIdentityCollision> {
    match error
        .as_database_error()
        .and_then(|database| database.constraint())
    {
        Some("semantic_transcript_entry_pk" | "semantic_transcript_entry_id_global") => {
            Some(StartEligibleTurnIdentityCollision::OriginEntry)
        }
        Some("context_frontier_pk" | "context_frontier_id_global") => {
            Some(StartEligibleTurnIdentityCollision::StartingFrontier)
        }
        Some("turn_attempt_pkey") => Some(StartEligibleTurnIdentityCollision::InitialAttempt),
        _ => None,
    }
}
