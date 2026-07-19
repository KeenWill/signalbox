//! Atomic PostgreSQL persistence and replay for durable input acceptance.

use std::collections::{BTreeMap, BTreeSet};
use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{SubmitInputOutcome, SubmitInputTransaction};
use signalbox_domain::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputSchedulingProjection, AcceptedInputSchedulingReconstitutionFailure,
    AcceptedInputSchedulingReconstitutionInput, AcceptedInputStartingLineage,
    AcceptedInputTurnSchedulingRecord, AcceptedInputTurnSchedulingRecordState,
    ActiveTurnSchedulingReconstitutionInput, Actor, ContextFrontierId, DeliveryRequest,
    DirectModelSelection, DurableCommandId, FrozenAliasDefinition, FrozenModelSelection,
    InitialSemanticTranscriptEntryPayload, ModelAlias, ModelSelectionOverride,
    ModelSelectionRequest, NonEmptyUnicodeTextFailure, PerInputConfigurationChoices,
    PreparedSubmitInput, ReconstitutedSubmitInput, ResolvedContextFrontierReconstitutionInput,
    SemanticTranscriptEntryId, SemanticTranscriptEntryReconstitutionInput,
    SemanticTranscriptEntryRef, Session, SessionAcceptanceTailEntryReconstitutionInput,
    SessionAcceptanceTailReconstitutionInput, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionId, SessionInputPosition, SteeringBinding,
    SubmitInput, SubmitInputAppliedResult, SubmitInputPreparationFailure,
    SubmitInputReconstitutionFailure, SubmitInputReconstitutionInput, SubmitInputRejectedResult,
    SubmitInputResult, SubmitInputTurnOriginReconstitutionInput, ToolRequestId, TurnAttemptId,
    TurnId, UserContent,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    command_registry::{
        self, CommandKind, RegistryCorruption, RegistryInspectionError, SUBMIT_INPUT_KIND,
    },
    mapping::{
        PositiveOrdinalMappingError, accepted_input_id_from_uuid, accepted_input_id_to_uuid,
        defaults_version_from_numeric, defaults_version_to_numeric, durable_command_id_from_uuid,
        durable_command_id_to_uuid, input_position_from_numeric, input_position_to_numeric,
        session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid,
    },
    session::{SessionCorruption, SessionRepositoryError, load_session_from_connection},
};

const STORAGE_VERSION: i16 = 1;
const APPLIED: &str = "applied";
const REJECTED: &str = "rejected";

type StoredTurnOriginKey = (Uuid, Uuid);

#[derive(Clone, Copy)]
struct StoredTurnOriginLink {
    command_id: DurableCommandId,
    predecessor: Option<StoredTurnOriginKey>,
    accepted_input: AcceptedInputId,
    queue_order: AcceptedInputQueueOrder,
}

/// The committed outcome of handling one canonical input submission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubmitInputHandlingOutcome {
    /// First handling or equal replay returns the complete recorded result.
    Recorded(SubmitInputResult),
    /// The identifier already names another kind or structural payload.
    ConflictingReuse {
        /// The owner-global identifier whose earlier meaning is retained.
        command_id: DurableCommandId,
    },
}

/// A durable shape that cannot reconstruct one complete input handling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubmitInputCorruption {
    /// One required row or field is absent.
    Missing(&'static str),
    /// A closed discriminator or representation version is unsupported.
    Unsupported {
        /// The record field that could not be decoded.
        field: &'static str,
        /// The durable spelling that was observed.
        value: String,
    },
    /// Typed records or variant-specific fields disagree.
    Inconsistent(&'static str),
    /// A stored positive ordinal cannot construct the domain value.
    InvalidOrdinal {
        /// The ordinal-bearing field.
        field: &'static str,
        /// Why its numeric representation is invalid.
        reason: PositiveOrdinalMappingError,
    },
    /// Exact stored text cannot construct baseline user content.
    InvalidContent {
        /// The content-bearing field.
        field: &'static str,
        /// Why the exact stored text is outside the baseline.
        failure: NonEmptyUnicodeTextFailure,
    },
    /// The current session projection required for first handling is invalid.
    CurrentSession(SessionCorruption),
    /// Checked stored values fail domain-owned receipt correlation.
    Domain(SubmitInputReconstitutionFailure),
    /// Complete scheduling facts fail domain-owned aggregate reconstruction.
    Scheduling(AcceptedInputSchedulingReconstitutionFailure),
}

impl fmt::Display for SubmitInputCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing durable SubmitInput {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported SubmitInput {field}: {value}")
            }
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent SubmitInput {relationship}")
            }
            Self::InvalidOrdinal { field, reason } => {
                write!(formatter, "invalid SubmitInput {field}: {reason}")
            }
            Self::InvalidContent { field, failure } => {
                write!(formatter, "invalid SubmitInput {field}: {failure:?}")
            }
            Self::CurrentSession(error) => {
                write!(formatter, "SubmitInput current Session is invalid: {error}")
            }
            Self::Domain(failure) => {
                write!(
                    formatter,
                    "SubmitInput domain reconstitution failed: {failure:?}"
                )
            }
            Self::Scheduling(failure) => {
                write!(
                    formatter,
                    "SubmitInput scheduling reconstitution failed: {failure:?}"
                )
            }
        }
    }
}

impl Error for SubmitInputCorruption {}

/// A database failure, wrong purpose-specific load, or integrity failure.
#[derive(Debug)]
pub enum SubmitInputRepositoryError {
    /// PostgreSQL could not complete the operation.
    Database(sqlx::Error),
    /// A purpose-specific load named a valid command of another admitted kind.
    DifferentCommandKind {
        /// The owner-global identifier that names another kind.
        command_id: DurableCommandId,
    },
    /// A generated accepted-input candidate reused the active turn's origin.
    AcceptedInputIdentityCollision {
        /// The unclaimed durable command.
        command_id: DurableCommandId,
        /// The authoritative active turn.
        active_turn: TurnId,
        /// The colliding accepted-input candidate and active origin.
        accepted_input: AcceptedInputId,
    },
    /// Durable records cannot reconstruct the requested domain value.
    Corruption(SubmitInputCorruption),
    /// A matching interrupt reached the intentionally unavailable transition.
    InterruptApplicationUnavailable {
        /// The unclaimed durable-command identity.
        command_id: DurableCommandId,
        /// The exact authoritative active turn that matched the command.
        active_turn: TurnId,
    },
}

impl fmt::Display for SubmitInputRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "SubmitInput database failure: {error}"),
            Self::DifferentCommandKind { command_id } => {
                write!(
                    formatter,
                    "durable command {command_id:?} does not name SubmitInput"
                )
            }
            Self::AcceptedInputIdentityCollision {
                command_id,
                active_turn,
                accepted_input,
            } => write!(
                formatter,
                "SubmitInput command {command_id:?} proposed accepted input {accepted_input:?}, which is already the origin of active turn {active_turn:?}"
            ),
            Self::Corruption(error) => error.fmt(formatter),
            Self::InterruptApplicationUnavailable {
                command_id,
                active_turn,
            } => write!(
                formatter,
                "SubmitInput command {command_id:?} matched active turn {active_turn:?}, but interrupt application is unavailable"
            ),
        }
    }
}

impl Error for SubmitInputRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::DifferentCommandKind { .. }
            | Self::AcceptedInputIdentityCollision { .. }
            | Self::InterruptApplicationUnavailable { .. } => None,
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for SubmitInputRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<SubmitInputCorruption> for SubmitInputRepositoryError {
    fn from(error: SubmitInputCorruption) -> Self {
        Self::Corruption(error)
    }
}

enum TransactionDecision {
    Commit(SubmitInputHandlingOutcome),
    Rollback(SubmitInputHandlingOutcome),
}

/// PostgreSQL implementation of atomic durable input acceptance.
#[derive(Clone, Debug)]
pub struct SubmitInputRepository {
    pool: PgPool,
}

impl SubmitInputRepository {
    /// Uses the supplied pool for atomic handling and fail-closed loads.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Handles an unseen command or resolves its immutable recorded meaning.
    ///
    /// Registry inspection or claim is always first. An unseen command then
    /// locks the session and its current-defaults pointer before reading
    /// state, serializes position assignment on the session row, and commits
    /// the typed terminal result with all applied effects.
    pub async fn handle(
        &self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
    ) -> Result<SubmitInputHandlingOutcome, SubmitInputRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let decision = handle_in_transaction(&mut transaction, command, accepted_input, turn).await;

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

    /// Loads one complete handling, or `None` only for an unseen identifier.
    pub async fn load(
        &self,
        command_id: DurableCommandId,
    ) -> Result<Option<ReconstitutedSubmitInput>, SubmitInputRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        match inspect_registry(&mut connection, command_id).await? {
            None => Ok(None),
            Some(CommandKind::SubmitInput) => {
                load_from_connection(&mut connection, command_id).await
            }
            Some(CommandKind::CreateSession | CommandKind::ReplaceSessionDefaults) => {
                Err(Self::wrong_kind(command_id))
            }
        }
    }

    fn wrong_kind(command_id: DurableCommandId) -> SubmitInputRepositoryError {
        SubmitInputRepositoryError::DifferentCommandKind { command_id }
    }
}

impl SubmitInputTransaction for SubmitInputRepository {
    type Error = SubmitInputRepositoryError;

    async fn handle(
        &mut self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
    ) -> Result<SubmitInputOutcome, Self::Error> {
        let outcome = SubmitInputRepository::handle(self, command, accepted_input, turn).await?;

        Ok(match outcome {
            SubmitInputHandlingOutcome::Recorded(result) => SubmitInputOutcome::Recorded(result),
            SubmitInputHandlingOutcome::ConflictingReuse { command_id } => {
                SubmitInputOutcome::ConflictingReuse { command_id }
            }
        })
    }
}

async fn handle_in_transaction(
    connection: &mut PgConnection,
    command: SubmitInput,
    accepted_input: AcceptedInputId,
    turn: Option<TurnId>,
) -> Result<TransactionDecision, SubmitInputRepositoryError> {
    let command_id = command.command_id();
    match inspect_registry(connection, command_id).await? {
        Some(CommandKind::SubmitInput) => {
            return Ok(TransactionDecision::Rollback(existing_outcome(
                &command,
                require_recorded(connection, command_id).await?,
            )));
        }
        Some(CommandKind::CreateSession | CommandKind::ReplaceSessionDefaults) => {
            return Ok(TransactionDecision::Rollback(
                SubmitInputHandlingOutcome::ConflictingReuse { command_id },
            ));
        }
        None => {}
    }

    let claimed = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, $2, $3, transaction_timestamp())
         ON CONFLICT DO NOTHING",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .bind(SUBMIT_INPUT_KIND)
    .bind(STORAGE_VERSION)
    .execute(&mut *connection)
    .await?
    .rows_affected()
        == 1;

    if !claimed {
        return match inspect_registry(connection, command_id).await? {
            Some(CommandKind::SubmitInput) => Ok(TransactionDecision::Rollback(existing_outcome(
                &command,
                require_recorded(connection, command_id).await?,
            ))),
            Some(CommandKind::CreateSession | CommandKind::ReplaceSessionDefaults) => {
                Ok(TransactionDecision::Rollback(
                    SubmitInputHandlingOutcome::ConflictingReuse { command_id },
                ))
            }
            None => Err(SubmitInputCorruption::Inconsistent("winner claim disappeared").into()),
        };
    }

    let prepared = prepare_against_locked_state(connection, command, accepted_input, turn).await?;
    let recorded = prepared.result().clone();
    insert_prepared(connection, prepared).await?;
    Ok(TransactionDecision::Commit(
        SubmitInputHandlingOutcome::Recorded(recorded),
    ))
}

async fn require_recorded(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<ReconstitutedSubmitInput, SubmitInputRepositoryError> {
    load_from_connection(connection, command_id)
        .await?
        .ok_or_else(|| SubmitInputCorruption::Inconsistent("registry entry disappeared").into())
}

async fn require_recorded_batch(
    connection: &mut PgConnection,
    command_ids: &[DurableCommandId],
) -> Result<BTreeMap<DurableCommandId, ReconstitutedSubmitInput>, SubmitInputRepositoryError> {
    let requested = command_ids
        .iter()
        .copied()
        .map(|command_id| (durable_command_id_to_uuid(command_id), command_id))
        .collect::<BTreeMap<_, _>>();
    let requested_uuids = requested.keys().copied().collect::<Vec<_>>();
    let rows = load_complete_rows(connection, &requested_uuids).await?;
    let mut rows_by_command = BTreeMap::new();
    let mut related_turns = BTreeSet::new();
    for row in rows {
        let command_uuid: Uuid = required(&row, "registry_command_id")?;
        if !requested.contains_key(&command_uuid) {
            return Err(
                SubmitInputCorruption::Inconsistent("unexpected batched command identity").into(),
            );
        }
        if let Some(related_turn) = related_turn_origin_key(&row)? {
            related_turns.insert(related_turn);
        }
        if rows_by_command.insert(command_uuid, row).is_some() {
            return Err(
                SubmitInputCorruption::Inconsistent("duplicate batched command row").into(),
            );
        }
    }
    if rows_by_command.len() != requested.len() {
        return Err(SubmitInputCorruption::Missing("batched origin command").into());
    }

    let related_origins = load_turn_origin_graph(connection, &related_turns).await?;
    let mut recorded = BTreeMap::new();
    for (command_uuid, command_id) in requested {
        let row = rows_by_command
            .remove(&command_uuid)
            .ok_or(SubmitInputCorruption::Missing("batched origin command"))?;
        let related_turn_origin = related_turn_origin_key(&row)?
            .map(|key| {
                related_origins
                    .get(&key)
                    .cloned()
                    .ok_or(SubmitInputCorruption::Missing("related turn origin"))
            })
            .transpose()?;
        let reconstructed = decode_complete(row, command_id, related_turn_origin)?;
        if recorded.insert(command_id, reconstructed).is_some() {
            return Err(
                SubmitInputCorruption::Inconsistent("duplicate batched command row").into(),
            );
        }
    }
    Ok(recorded)
}

fn existing_outcome(
    command: &SubmitInput,
    recorded: ReconstitutedSubmitInput,
) -> SubmitInputHandlingOutcome {
    if command == recorded.command() {
        SubmitInputHandlingOutcome::Recorded(recorded.result().clone())
    } else {
        SubmitInputHandlingOutcome::ConflictingReuse {
            command_id: command.command_id(),
        }
    }
}

async fn prepare_against_locked_state(
    connection: &mut PgConnection,
    command: SubmitInput,
    accepted_input: AcceptedInputId,
    turn: Option<TurnId>,
) -> Result<PreparedSubmitInput, SubmitInputRepositoryError> {
    // Lock-mode constraint: this session-row lock must be `FOR NO KEY
    // UPDATE`, not `FOR UPDATE`. Submit orders the session row before the
    // scheduler row and current-defaults pointer row, while a concurrent
    // defaults replacement holds the pointer row (its compare-and-set) when its
    // `session_defaults_version` insert requests `FOR KEY SHARE` on this
    // session row through the non-deferrable session foreign key.
    // `FOR UPDATE` conflicts with `FOR KEY SHARE` and closes that lock-order
    // cycle into a deadlock (40P01); `FOR NO KEY UPDATE` does not conflict
    // with referential-integrity `KEY SHARE` locks while remaining
    // self-exclusive, so per-session position assignment stays serialized.
    let session_exists = sqlx::query_scalar::<_, Uuid>(
        "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE",
    )
    .bind(session_id_to_uuid(command.session()))
    .fetch_optional(&mut *connection)
    .await?
    .is_some();
    if !session_exists {
        return Ok(command.prepare_session_not_found());
    }

    let scheduler_exists = sqlx::query_scalar::<_, Uuid>(
        "SELECT session_id
           FROM session_scheduler
          WHERE session_id = $1
          FOR UPDATE",
    )
    .bind(session_id_to_uuid(command.session()))
    .fetch_optional(&mut *connection)
    .await?
    .is_some();
    if !scheduler_exists {
        return Err(
            SubmitInputCorruption::CurrentSession(SessionCorruption::Missing("scheduler row"))
                .into(),
        );
    }

    let pointer_exists = sqlx::query_scalar::<_, Decimal>(
        "SELECT current_version
           FROM session_current_defaults
          WHERE session_id = $1
          FOR UPDATE",
    )
    .bind(session_id_to_uuid(command.session()))
    .fetch_optional(&mut *connection)
    .await?
    .is_some();
    if !pointer_exists {
        return Err(
            SubmitInputCorruption::CurrentSession(SessionCorruption::Missing(
                "current defaults pointer",
            ))
            .into(),
        );
    }

    let session = match load_session_from_connection(connection, command.session()).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            return Err(SubmitInputCorruption::Inconsistent("locked session disappeared").into());
        }
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(SubmitInputCorruption::CurrentSession(error).into());
        }
    };

    let scheduling = load_scheduling_projection(connection, session.clone()).await?;
    let active_turn_id = scheduling.active_turn().map(|active| active.turn());
    let prepared = if active_turn_id.is_some() {
        command.prepare_with_active_turn(&scheduling, accepted_input, turn, |_| None)
    } else {
        let previous_position = sqlx::query_scalar::<_, Decimal>(
            "SELECT acceptance_position
               FROM accepted_input
              WHERE session_id = $1
              ORDER BY acceptance_position DESC
              LIMIT 1",
        )
        .bind(session_id_to_uuid(command.session()))
        .fetch_optional(&mut *connection)
        .await?
        .map(|value| {
            input_position_from_numeric(value).map_err(|reason| {
                SubmitInputRepositoryError::Corruption(SubmitInputCorruption::InvalidOrdinal {
                    field: "previous acceptance_position",
                    reason,
                })
            })
        })
        .transpose()?;
        command.prepare_when_no_active_turn(
            &session,
            accepted_input,
            turn,
            previous_position,
            |_| None,
        )
    };

    prepared.map_err(|error| match error.failure() {
        SubmitInputPreparationFailure::SessionMismatch { .. } => {
            SubmitInputCorruption::Inconsistent("current session ownership").into()
        }
        SubmitInputPreparationFailure::TurnCandidateMismatch => {
            SubmitInputCorruption::Inconsistent("delivery turn candidate").into()
        }
        SubmitInputPreparationFailure::AcceptedInputCandidateReusesActiveOrigin {
            active_turn,
            accepted_input,
        } => SubmitInputRepositoryError::AcceptedInputIdentityCollision {
            command_id: error.command().command_id(),
            active_turn,
            accepted_input,
        },
        SubmitInputPreparationFailure::ActiveTurnProjectionMissing => {
            SubmitInputCorruption::Inconsistent("selected active scheduling state").into()
        }
        SubmitInputPreparationFailure::InterruptApplicationUnavailable => {
            SubmitInputRepositoryError::InterruptApplicationUnavailable {
                command_id: error.command().command_id(),
                active_turn: active_turn_id
                    .expect("interrupt unavailability requires a checked active projection"),
            }
        }
    })
}

async fn load_scheduling_projection(
    connection: &mut PgConnection,
    session: Session,
) -> Result<AcceptedInputSchedulingProjection, SubmitInputRepositoryError> {
    let session_id = session.id();
    let (queue_count, lifecycle_count): (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM queued_input_origin
              WHERE session_id = $1),
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1)",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_one(&mut *connection)
    .await?;
    if queue_count != lifecycle_count {
        return Err(
            SubmitInputCorruption::Inconsistent("complete scheduling turn inventory").into(),
        );
    }

    let rows = sqlx::query(
        "SELECT
            queued.turn_id AS queued_turn_id,
            queued.accepted_input_id AS queued_accepted_input_id,
            queued.session_id AS queued_session_id,
            queued.acceptance_position AS queued_position,
            queued.priority_kind,
            accepted.accepting_command_id,
            accepted.accepted_input_id,
            accepted.session_id AS accepted_session_id,
            accepted.disposition_kind,
            accepted.origin_turn_id,
            turn.turn_id AS lifecycle_turn_id,
            turn.session_id AS lifecycle_session_id,
            turn.state_kind AS lifecycle_state_kind,
            turn.start_lineage_kind,
            turn.immediate_predecessor_turn_id,
            turn.starting_frontier_id,
            turn.terminal_frontier_id,
            turn.active_phase_kind,
            turn.current_attempt_id,
            turn.terminal_disposition_kind,
            attempt.turn_attempt_id,
            attempt.turn_id AS attempt_turn_id,
            attempt.session_id AS attempt_session_id,
            attempt.continued_from_attempt_id,
            attempt.state_kind AS attempt_state_kind,
            attempt.end_variant,
            attempt.end_disposition
         FROM queued_input_origin AS queued
         LEFT JOIN accepted_input AS accepted
           ON accepted.accepted_input_id = queued.accepted_input_id
         LEFT JOIN turn_lifecycle AS turn
           ON turn.turn_id = queued.turn_id
         LEFT JOIN turn_attempt AS attempt
           ON attempt.turn_attempt_id = turn.current_attempt_id
        WHERE queued.session_id = $1
        ORDER BY queued.acceptance_position",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    let mut accepting_commands = Vec::with_capacity(rows.len());
    for row in &rows {
        let command_uuid: Uuid = required(row, "accepting_command_id")?;
        accepting_commands.push(
            durable_command_id_from_uuid(command_uuid)
                .map_err(|_| SubmitInputCorruption::Inconsistent("accepting command identity"))?,
        );
    }
    let recorded_commands = require_recorded_batch(connection, &accepting_commands).await?;

    let mut turns = Vec::with_capacity(rows.len());
    let mut required_frontiers = BTreeSet::new();
    for (row, accepting_command) in rows.into_iter().zip(accepting_commands) {
        let queued_turn = turn_id_from_uuid(required(&row, "queued_turn_id")?);
        let queued_accepted =
            accepted_input_id_from_uuid(required(&row, "queued_accepted_input_id")?);
        let queued_session = session_id_from_uuid(required(&row, "queued_session_id")?);
        let queued_position = decode_position(&row, "queued_position")?;
        require_spelling(&row, "priority_kind", "ordinary")?;

        let accepted_input = accepted_input_id_from_uuid(required(&row, "accepted_input_id")?);
        let accepted_session = session_id_from_uuid(required(&row, "accepted_session_id")?);
        require_spelling(&row, "disposition_kind", "origin_of")?;
        let origin_turn = turn_id_from_uuid(required(&row, "origin_turn_id")?);

        let lifecycle_turn = turn_id_from_uuid(required(&row, "lifecycle_turn_id")?);
        let lifecycle_session = session_id_from_uuid(required(&row, "lifecycle_session_id")?);
        if queued_accepted != accepted_input
            || queued_turn != origin_turn
            || lifecycle_turn != queued_turn
        {
            return Err(SubmitInputCorruption::Inconsistent(
                "scheduling turn identity correlation",
            )
            .into());
        }

        let recorded = recorded_commands
            .get(&accepting_command)
            .ok_or(SubmitInputCorruption::Missing("batched origin receipt"))?;
        let (origin_delivery, origin_configuration) = match recorded.result() {
            SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied))
                if applied.accepted_input() == accepted_input
                    && applied.session() == accepted_session
                    && applied.turn() == queued_turn =>
            {
                (
                    recorded.command().delivery(),
                    applied.origin_configuration().clone(),
                )
            }
            _ => {
                return Err(SubmitInputCorruption::Inconsistent(
                    "scheduling origin command result",
                )
                .into());
            }
        };

        let state_kind: String = required(&row, "lifecycle_state_kind")?;
        let lineage_kind: Option<String> = row.try_get("start_lineage_kind")?;
        let predecessor: Option<Uuid> = row.try_get("immediate_predecessor_turn_id")?;
        let starting_frontier: Option<Uuid> = row.try_get("starting_frontier_id")?;
        let terminal_frontier: Option<Uuid> = row.try_get("terminal_frontier_id")?;
        let active_phase: Option<String> = row.try_get("active_phase_kind")?;
        let current_attempt: Option<Uuid> = row.try_get("current_attempt_id")?;
        let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
        let state = match state_kind.as_str() {
            "queued" => {
                if lineage_kind.is_some()
                    || predecessor.is_some()
                    || starting_frontier.is_some()
                    || terminal_frontier.is_some()
                    || active_phase.is_some()
                    || current_attempt.is_some()
                    || terminal_disposition.is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("queued scheduling lifecycle").into(),
                    );
                }
                AcceptedInputTurnSchedulingRecordState::Queued
            }
            "active" => {
                if active_phase.as_deref() != Some("running")
                    || terminal_frontier.is_some()
                    || terminal_disposition.is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("active scheduling lifecycle").into(),
                    );
                }
                let attempt_id = TurnAttemptId::from_uuid(
                    current_attempt.ok_or(SubmitInputCorruption::Missing("current_attempt_id"))?,
                );
                let stored_attempt_id =
                    TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?);
                let attempt_turn = turn_id_from_uuid(required(&row, "attempt_turn_id")?);
                let attempt_session = session_id_from_uuid(required(&row, "attempt_session_id")?);
                let continued_from: Option<Uuid> = row.try_get("continued_from_attempt_id")?;
                let attempt_state: String = required(&row, "attempt_state_kind")?;
                let end_variant: Option<String> = row.try_get("end_variant")?;
                let end_disposition: Option<String> = row.try_get("end_disposition")?;
                if stored_attempt_id != attempt_id
                    || attempt_turn != lifecycle_turn
                    || attempt_session != lifecycle_session
                    || continued_from.is_some()
                    || end_variant.is_some()
                    || end_disposition.is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("active current attempt").into(),
                    );
                }
                let phase = match attempt_state.as_str() {
                    "prepared" => ActiveTurnSchedulingReconstitutionInput::prepared(
                        lifecycle_turn,
                        attempt_id,
                    ),
                    "running" => {
                        ActiveTurnSchedulingReconstitutionInput::running(lifecycle_turn, attempt_id)
                    }
                    value => {
                        return Err(SubmitInputCorruption::Unsupported {
                            field: "active attempt state_kind",
                            value: value.to_owned(),
                        }
                        .into());
                    }
                };
                let starting_frontier = starting_frontier
                    .ok_or(SubmitInputCorruption::Missing("starting_frontier_id"))?;
                required_frontiers.insert(starting_frontier);
                AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: decode_starting_lineage(lineage_kind, predecessor)?,
                    starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                    phase,
                }
            }
            "terminal" => {
                if active_phase.is_some()
                    || current_attempt.is_some()
                    || terminal_disposition.as_deref() != Some("failed")
                {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "terminal scheduling lifecycle",
                    )
                    .into());
                }
                let starting_frontier = starting_frontier
                    .ok_or(SubmitInputCorruption::Missing("starting_frontier_id"))?;
                let terminal_frontier = terminal_frontier
                    .ok_or(SubmitInputCorruption::Missing("terminal_frontier_id"))?;
                required_frontiers.insert(starting_frontier);
                required_frontiers.insert(terminal_frontier);
                AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                    starting_lineage: decode_starting_lineage(lineage_kind, predecessor)?,
                    starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                    terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                }
            }
            value => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "turn lifecycle state_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };

        turns.push(AcceptedInputTurnSchedulingRecord::new(
            lifecycle_session,
            lifecycle_turn,
            accepted_session,
            AcceptedInputLifecycle::new(
                accepted_input,
                AcceptedInputDisposition::OriginOf(origin_turn),
            ),
            queued_session,
            queued_turn,
            AcceptedInputQueueOrder::ordinary(queued_position),
            origin_delivery,
            origin_configuration,
            state,
        ));
    }

    let active_acceptance_tail =
        load_active_acceptance_tail(connection, session_id, &turns).await?;

    let required_frontier_ids = required_frontiers.iter().copied().collect::<Vec<_>>();
    let frontier_rows = sqlx::query(
        "SELECT context_frontier_id, member_count
           FROM context_frontier
          WHERE owning_session_id = $1
            AND context_frontier_id = ANY($2)
          ORDER BY context_frontier_id",
    )
    .bind(session_id_to_uuid(session_id))
    .bind(&required_frontier_ids)
    .fetch_all(&mut *connection)
    .await?;
    let member_rows = sqlx::query(
        "SELECT
            context_frontier_id,
            member_position,
            source_session_id,
            semantic_entry_id
           FROM context_frontier_member
          WHERE owning_session_id = $1
            AND context_frontier_id = ANY($2)
          ORDER BY context_frontier_id, member_position",
    )
    .bind(session_id_to_uuid(session_id))
    .bind(&required_frontier_ids)
    .fetch_all(&mut *connection)
    .await?;
    let mut members_by_frontier =
        BTreeMap::<Uuid, Vec<(Decimal, SessionId, SemanticTranscriptEntryId)>>::new();
    let mut required_semantic_entries = BTreeSet::new();
    for member_row in member_rows {
        let source_session = required(&member_row, "source_session_id")?;
        let semantic_entry = required(&member_row, "semantic_entry_id")?;
        required_semantic_entries.insert((source_session, semantic_entry));
        members_by_frontier
            .entry(required(&member_row, "context_frontier_id")?)
            .or_default()
            .push((
                required(&member_row, "member_position")?,
                session_id_from_uuid(source_session),
                SemanticTranscriptEntryId::from_uuid(semantic_entry),
            ));
    }

    let semantic_source_sessions = required_semantic_entries
        .iter()
        .map(|(source_session, _)| *source_session)
        .collect::<Vec<_>>();
    let semantic_entry_ids = required_semantic_entries
        .iter()
        .map(|(_, semantic_entry)| *semantic_entry)
        .collect::<Vec<_>>();
    let semantic_rows = sqlx::query(
        "SELECT
            source_session_id,
            semantic_entry_id,
            payload_kind,
            origin_accepted_input_id,
            failed_turn_id
         FROM semantic_transcript_entry
        WHERE (source_session_id, semantic_entry_id) IN (
            SELECT required.source_session_id, required.semantic_entry_id
              FROM UNNEST($1::uuid[], $2::uuid[])
                AS required(source_session_id, semantic_entry_id)
        )
        ORDER BY source_session_id, semantic_entry_id",
    )
    .bind(&semantic_source_sessions)
    .bind(&semantic_entry_ids)
    .fetch_all(&mut *connection)
    .await?;
    let mut semantic_entries = Vec::with_capacity(semantic_rows.len());
    let mut loaded_semantic_entries = BTreeSet::new();
    for row in semantic_rows {
        let source_session_uuid: Uuid = required(&row, "source_session_id")?;
        let entry_uuid: Uuid = required(&row, "semantic_entry_id")?;
        if !loaded_semantic_entries.insert((source_session_uuid, entry_uuid)) {
            return Err(SubmitInputCorruption::Inconsistent("duplicate semantic entry").into());
        }
        let source_session = session_id_from_uuid(source_session_uuid);
        let entry = SemanticTranscriptEntryId::from_uuid(entry_uuid);
        let payload_kind: String = required(&row, "payload_kind")?;
        let origin: Option<Uuid> = row.try_get("origin_accepted_input_id")?;
        let failed_turn: Option<Uuid> = row.try_get("failed_turn_id")?;
        let payload = match (payload_kind.as_str(), origin, failed_turn) {
            ("origin_accepted_input", Some(origin), None) => {
                InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                    accepted_input: accepted_input_id_from_uuid(origin),
                }
            }
            ("turn_failed", None, Some(turn)) => {
                InitialSemanticTranscriptEntryPayload::TurnFailed {
                    turn: turn_id_from_uuid(turn),
                }
            }
            ("origin_accepted_input" | "turn_failed", _, _) => {
                return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
            }
            (value, _, _) => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "semantic entry payload_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
            entry,
            source_session,
            payload,
        ));
    }
    if loaded_semantic_entries != required_semantic_entries {
        return Err(SubmitInputCorruption::Missing("context frontier semantic entry").into());
    }

    let mut snapshots = Vec::with_capacity(frontier_rows.len());
    for frontier_row in frontier_rows {
        let frontier_uuid: Uuid = required(&frontier_row, "context_frontier_id")?;
        let declared_count: Decimal = required(&frontier_row, "member_count")?;
        let member_rows = members_by_frontier
            .remove(&frontier_uuid)
            .unwrap_or_default();
        let actual_count = u64::try_from(member_rows.len())
            .expect("PostgreSQL result cardinality fits the u64 schema bound");
        if declared_count != Decimal::from(actual_count) {
            return Err(SubmitInputCorruption::Inconsistent(
                "context frontier declared membership",
            )
            .into());
        }
        let mut members = Vec::with_capacity(member_rows.len());
        for (index, (position, source_session, semantic_entry)) in
            member_rows.into_iter().enumerate()
        {
            let expected_position = u64::try_from(index + 1)
                .expect("PostgreSQL result cardinality fits the u64 schema bound");
            if position != Decimal::from(expected_position) {
                return Err(SubmitInputCorruption::Inconsistent(
                    "context frontier contiguous membership",
                )
                .into());
            }
            members.push(SemanticTranscriptEntryRef::from_source(
                source_session,
                semantic_entry,
            ));
        }
        snapshots.push(ResolvedContextFrontierReconstitutionInput::new(
            session_id,
            ContextFrontierId::from_uuid(frontier_uuid),
            members,
        ));
    }
    if !members_by_frontier.is_empty() {
        return Err(
            SubmitInputCorruption::Inconsistent("context frontier member without header").into(),
        );
    }
    if snapshots.len() != required_frontiers.len() {
        return Err(SubmitInputCorruption::Missing("scheduling context frontier").into());
    }

    AcceptedInputSchedulingReconstitutionInput::new(
        session,
        turns,
        semantic_entries,
        snapshots,
        active_acceptance_tail,
    )
    .reconstitute()
    .map_err(|error| {
        let (_, failure) = error.into_parts();
        SubmitInputCorruption::Scheduling(failure).into()
    })
}

async fn load_active_acceptance_tail(
    connection: &mut PgConnection,
    session: SessionId,
    turns: &[AcceptedInputTurnSchedulingRecord],
) -> Result<Option<SessionAcceptanceTailReconstitutionInput>, SubmitInputRepositoryError> {
    let Some(active) = turns.iter().find(|record| {
        matches!(
            record.state(),
            AcceptedInputTurnSchedulingRecordState::Active { .. }
        )
    }) else {
        return Ok(None);
    };

    let rows = sqlx::query(
        "SELECT
            accepted_input_id,
            session_id,
            acceptance_position,
            disposition_kind,
            origin_turn_id,
            delivery_kind,
            expected_active_turn_id,
            expected_defaults_version,
            model_override_kind,
            replacement_model_kind,
            replacement_direct_model_selection_id,
            replacement_model_alias_id
           FROM accepted_input
          WHERE session_id = $1
            AND acceptance_position >= $2
          ORDER BY acceptance_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(input_position_to_numeric(
        active.order().acceptance_position(),
    ))
    .fetch_all(&mut *connection)
    .await?;

    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let accepted_input = accepted_input_id_from_uuid(required(&row, "accepted_input_id")?);
        let entry_session = session_id_from_uuid(required(&row, "session_id")?);
        let position = decode_position(&row, "acceptance_position")?;
        let expected_active_turn: Option<Uuid> = row.try_get("expected_active_turn_id")?;
        let delivery = decode_delivery(
            required(&row, "delivery_kind")?,
            expected_active_turn,
            row.try_get("expected_defaults_version")?,
            row.try_get("model_override_kind")?,
            row.try_get("replacement_model_kind")?,
            row.try_get("replacement_direct_model_selection_id")?,
            row.try_get("replacement_model_alias_id")?,
            "active acceptance-tail delivery",
        )?;
        let disposition_kind: String = required(&row, "disposition_kind")?;
        let origin_turn: Option<Uuid> = row.try_get("origin_turn_id")?;
        let disposition = match (disposition_kind.as_str(), origin_turn, delivery) {
            ("origin_of", Some(origin), _) => {
                AcceptedInputDisposition::OriginOf(turn_id_from_uuid(origin))
            }
            (
                "pending_steering",
                None,
                DeliveryRequest::NextSafePoint {
                    expected_active_turn,
                },
            ) => AcceptedInputDisposition::PendingSteering {
                binding: SteeringBinding::new(expected_active_turn),
            },
            ("origin_of" | "pending_steering", _, _) => {
                return Err(SubmitInputCorruption::Inconsistent(
                    "active acceptance-tail disposition",
                )
                .into());
            }
            (value, _, _) => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "active acceptance-tail disposition_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        entries.push(SessionAcceptanceTailEntryReconstitutionInput::new(
            entry_session,
            AcceptedInputLifecycle::new(accepted_input, disposition),
            position,
            delivery,
        ));
    }

    let observed_last_position = entries
        .last()
        .map(SessionAcceptanceTailEntryReconstitutionInput::position)
        .ok_or(SubmitInputCorruption::Missing(
            "active acceptance-tail origin",
        ))?;
    Ok(Some(SessionAcceptanceTailReconstitutionInput::new(
        session,
        active.accepted_input().id(),
        observed_last_position,
        entries,
    )))
}

fn decode_starting_lineage(
    kind: Option<String>,
    predecessor: Option<Uuid>,
) -> Result<AcceptedInputStartingLineage, SubmitInputRepositoryError> {
    match (kind.as_deref(), predecessor) {
        (Some("first_in_session"), None) => Ok(AcceptedInputStartingLineage::FirstInSession),
        (Some("after"), Some(predecessor)) => Ok(AcceptedInputStartingLineage::After {
            immediate_predecessor: turn_id_from_uuid(predecessor),
        }),
        (Some("first_in_session" | "after"), _) | (None, _) => {
            Err(SubmitInputCorruption::Inconsistent("starting lineage").into())
        }
        (Some(value), _) => Err(SubmitInputCorruption::Unsupported {
            field: "start_lineage_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

async fn insert_prepared(
    connection: &mut PgConnection,
    prepared: PreparedSubmitInput,
) -> Result<(), SubmitInputRepositoryError> {
    let command = prepared.command();
    let actor = encode_actor(command.actor());
    let delivery = encode_delivery(command.delivery());
    let result = encode_result(prepared.result(), command.delivery());

    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text,
             delivery_kind, expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_actual_active_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
             $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25,
             $26, $27, $28)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(SUBMIT_INPUT_KIND)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(command.session()))
    .bind(actor.kind)
    .bind(actor.turn)
    .bind(actor.tool_request)
    .bind("text")
    .bind(command.content().text().as_str())
    .bind(delivery.kind)
    .bind(delivery.expected_active_turn)
    .bind(delivery.expected_defaults_version)
    .bind(delivery.model_override_kind)
    .bind(delivery.replacement.kind)
    .bind(delivery.replacement.direct)
    .bind(delivery.replacement.alias)
    .bind(result.kind)
    .bind(result.rejection_kind)
    .bind(session_id_to_uuid(result.session))
    .bind(result.accepted_input)
    .bind(result.turn)
    .bind(result.actual_active_turn)
    .bind(result.expected_active_turn)
    .bind(result.expected_defaults_version)
    .bind(result.current_defaults_version)
    .bind(result.unknown_alias)
    .bind(result.selected_defaults_version)
    .bind(result.last_position)
    .execute(&mut *connection)
    .await?;

    if let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
        prepared.result()
    {
        let origin = applied.origin_configuration();
        let requested = encode_selection(origin.requested().model());
        let frozen = encode_frozen_model(origin.effective().model());
        let position = applied.acceptance_position();

        sqlx::query(
            "INSERT INTO accepted_input
                (accepted_input_id, accepting_command_id, session_id,
                 content_kind, content_text, delivery_kind,
                 expected_active_turn_id, expected_defaults_version,
                 model_override_kind, replacement_model_kind,
                 replacement_direct_model_selection_id, replacement_model_alias_id,
                 acceptance_position, disposition_kind, origin_turn_id)
             VALUES
                ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                 $14, $15)",
        )
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(durable_command_id_to_uuid(command.command_id()))
        .bind(session_id_to_uuid(applied.session()))
        .bind("text")
        .bind(command.content().text().as_str())
        .bind(delivery.kind)
        .bind(delivery.expected_active_turn)
        .bind(delivery.expected_defaults_version)
        .bind(delivery.model_override_kind)
        .bind(delivery.replacement.kind)
        .bind(delivery.replacement.direct)
        .bind(delivery.replacement.alias)
        .bind(input_position_to_numeric(position))
        .bind("origin_of")
        .bind(turn_id_to_uuid(applied.turn()))
        .execute(&mut *connection)
        .await?;

        sqlx::query(
            "INSERT INTO queued_input_origin
                (turn_id, accepted_input_id, session_id, acceptance_position,
                 priority_kind, defaults_version,
                 requested_model_kind, requested_direct_model_selection_id,
                 requested_model_alias_id, frozen_model_kind,
                 frozen_direct_model_selection_id, frozen_model_alias_id,
                 frozen_alias_selected_direct_id, model_parameters,
                 known_provider_failure_retry, model_fallback)
             VALUES
                ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                 $14, $15, $16)",
        )
        .bind(turn_id_to_uuid(applied.turn()))
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(session_id_to_uuid(applied.session()))
        .bind(input_position_to_numeric(position))
        .bind("ordinary")
        .bind(defaults_version_to_numeric(
            origin.session_defaults_version(),
        ))
        .bind(requested.kind)
        .bind(requested.direct)
        .bind(requested.alias)
        .bind(frozen.kind)
        .bind(frozen.direct)
        .bind(frozen.alias)
        .bind(frozen.alias_selected)
        .bind("provider_defaults")
        .bind("disabled")
        .bind("disabled")
        .execute(&mut *connection)
        .await?;

        sqlx::query(
            "INSERT INTO turn_lifecycle
                (turn_id, session_id, origin_accepted_input_id,
                 acceptance_position, state_kind)
             VALUES ($1, $2, $3, $4, 'queued')",
        )
        .bind(turn_id_to_uuid(applied.turn()))
        .bind(session_id_to_uuid(applied.session()))
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(input_position_to_numeric(position))
        .execute(&mut *connection)
        .await?;
    }

    if let SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(applied)) =
        prepared.result()
    {
        sqlx::query(
            "INSERT INTO accepted_input
                (accepted_input_id, accepting_command_id, session_id,
                 content_kind, content_text, delivery_kind,
                 expected_active_turn_id, expected_defaults_version,
                 model_override_kind, replacement_model_kind,
                 replacement_direct_model_selection_id, replacement_model_alias_id,
                 acceptance_position, disposition_kind, origin_turn_id)
             VALUES
                ($1, $2, $3, 'text', $4, 'next_safe_point',
                 $5, NULL, NULL, NULL, NULL, NULL, $6, 'pending_steering', NULL)",
        )
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(durable_command_id_to_uuid(command.command_id()))
        .bind(session_id_to_uuid(applied.session()))
        .bind(command.content().text().as_str())
        .bind(turn_id_to_uuid(applied.binding().source_turn()))
        .bind(input_position_to_numeric(applied.acceptance_position()))
        .execute(&mut *connection)
        .await?;
    }

    Ok(())
}

struct EncodedActor {
    kind: &'static str,
    turn: Option<Uuid>,
    tool_request: Option<Uuid>,
}

fn encode_actor(actor: Actor) -> EncodedActor {
    match actor {
        Actor::Owner => EncodedActor {
            kind: "owner",
            turn: None,
            tool_request: None,
        },
        Actor::Model { turn } => EncodedActor {
            kind: "model",
            turn: Some(turn.into_uuid()),
            tool_request: None,
        },
        Actor::Recovery => EncodedActor {
            kind: "recovery",
            turn: None,
            tool_request: None,
        },
        Actor::Tool { request } => EncodedActor {
            kind: "tool",
            turn: None,
            tool_request: Some(request.into_uuid()),
        },
    }
}

#[derive(Clone, Copy)]
struct EncodedSelection {
    kind: Option<&'static str>,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
}

impl EncodedSelection {
    const fn absent() -> Self {
        Self {
            kind: None,
            direct: None,
            alias: None,
        }
    }
}

fn encode_selection(selection: ModelSelectionRequest) -> EncodedSelection {
    match selection {
        ModelSelectionRequest::Direct(selection) => EncodedSelection {
            kind: Some("direct"),
            direct: Some(selection.into_uuid()),
            alias: None,
        },
        ModelSelectionRequest::Alias(alias) => EncodedSelection {
            kind: Some("alias"),
            direct: None,
            alias: Some(alias.into_uuid()),
        },
    }
}

struct EncodedFrozenModel {
    kind: &'static str,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    alias_selected: Option<Uuid>,
}

fn encode_frozen_model(model: &FrozenModelSelection) -> EncodedFrozenModel {
    match model {
        FrozenModelSelection::Direct(selection) => EncodedFrozenModel {
            kind: "direct",
            direct: Some(selection.into_uuid()),
            alias: None,
            alias_selected: None,
        },
        FrozenModelSelection::FrozenAlias { alias, definition } => EncodedFrozenModel {
            kind: "frozen_alias",
            direct: None,
            alias: Some(alias.into_uuid()),
            alias_selected: Some(definition.selected().into_uuid()),
        },
    }
}

#[derive(Clone, Copy)]
struct EncodedDelivery {
    kind: &'static str,
    expected_active_turn: Option<Uuid>,
    expected_defaults_version: Option<Decimal>,
    model_override_kind: Option<&'static str>,
    replacement: EncodedSelection,
}

fn encode_delivery(delivery: DeliveryRequest) -> EncodedDelivery {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration } => {
            encode_configured_delivery("start_when_no_active_turn", None, configuration)
        }
        DeliveryRequest::Interrupt {
            expected_active_turn,
            configuration,
        } => encode_configured_delivery(
            "interrupt",
            Some(expected_active_turn.into_uuid()),
            configuration,
        ),
        DeliveryRequest::NextSafePoint {
            expected_active_turn,
        } => EncodedDelivery {
            kind: "next_safe_point",
            expected_active_turn: Some(expected_active_turn.into_uuid()),
            expected_defaults_version: None,
            model_override_kind: None,
            replacement: EncodedSelection::absent(),
        },
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            configuration,
        } => encode_configured_delivery(
            "after_current_turn",
            Some(expected_active_turn.into_uuid()),
            configuration,
        ),
    }
}

fn encode_configured_delivery(
    kind: &'static str,
    expected_active_turn: Option<Uuid>,
    configuration: PerInputConfigurationChoices,
) -> EncodedDelivery {
    let (model_override_kind, replacement) = match configuration.model() {
        ModelSelectionOverride::UseSessionDefault => {
            ("use_session_default", EncodedSelection::absent())
        }
        ModelSelectionOverride::ReplaceWith(selection) => {
            ("replace_with", encode_selection(selection))
        }
    };
    EncodedDelivery {
        kind,
        expected_active_turn,
        expected_defaults_version: Some(defaults_version_to_numeric(
            configuration.expected_session_defaults_version(),
        )),
        model_override_kind: Some(model_override_kind),
        replacement,
    }
}

struct EncodedResult {
    kind: &'static str,
    rejection_kind: Option<&'static str>,
    session: SessionId,
    accepted_input: Option<Uuid>,
    turn: Option<Uuid>,
    actual_active_turn: Option<Uuid>,
    expected_active_turn: Option<Uuid>,
    expected_defaults_version: Option<Decimal>,
    current_defaults_version: Option<Decimal>,
    unknown_alias: Option<Uuid>,
    selected_defaults_version: Option<Decimal>,
    last_position: Option<Decimal>,
}

fn encode_result(result: &SubmitInputResult, delivery: DeliveryRequest) -> EncodedResult {
    match result {
        SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(result)) => EncodedResult {
            kind: APPLIED,
            rejection_kind: None,
            session: result.session(),
            accepted_input: Some(accepted_input_id_to_uuid(result.accepted_input())),
            turn: Some(turn_id_to_uuid(result.turn())),
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
        },
        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(result)) => {
            EncodedResult {
                kind: APPLIED,
                rejection_kind: None,
                session: result.session(),
                accepted_input: Some(accepted_input_id_to_uuid(result.accepted_input())),
                turn: None,
                actual_active_turn: Some(turn_id_to_uuid(result.binding().source_turn())),
                expected_active_turn: None,
                expected_defaults_version: None,
                current_defaults_version: None,
                unknown_alias: None,
                selected_defaults_version: None,
                last_position: None,
            }
        }
        SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
            session,
            active_turn,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("active_turn_present"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*active_turn)),
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
            session,
            expected_active_turn,
            actual_active_turn,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("active_turn_mismatch"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*actual_active_turn)),
            expected_active_turn: Some(turn_id_to_uuid(*expected_active_turn)),
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::SessionNotFound { session }) => {
            EncodedResult {
                kind: REJECTED,
                rejection_kind: Some("session_not_found"),
                session: *session,
                accepted_input: None,
                turn: None,
                actual_active_turn: None,
                expected_active_turn: None,
                expected_defaults_version: None,
                current_defaults_version: None,
                unknown_alias: None,
                selected_defaults_version: None,
                last_position: None,
            }
        }
        SubmitInputResult::Rejected(SubmitInputRejectedResult::NoActiveTurn {
            session,
            expected_active_turn,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("no_active_turn"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: Some(turn_id_to_uuid(*expected_active_turn)),
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
        },
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                session,
                expected,
                current,
            },
        ) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("session_defaults_version_mismatch"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: Some(defaults_version_to_numeric(*expected)),
            current_defaults_version: Some(defaults_version_to_numeric(*current)),
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::UnknownModelAlias {
            session,
            alias,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("unknown_model_alias"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: Some(alias.into_uuid()),
            selected_defaults_version: configured_defaults_version(delivery)
                .map(defaults_version_to_numeric),
            last_position: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::AcceptancePositionExhausted {
            session,
            last,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("acceptance_position_exhausted"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: Some(input_position_to_numeric(*last)),
        },
    }
}

fn configured_defaults_version(
    delivery: DeliveryRequest,
) -> Option<SessionConfigurationDefaultsVersion> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration }
        | DeliveryRequest::Interrupt { configuration, .. }
        | DeliveryRequest::AfterCurrentTurn { configuration, .. } => {
            Some(configuration.expected_session_defaults_version())
        }
        DeliveryRequest::NextSafePoint { .. } => None,
    }
}

async fn load_complete_rows(
    connection: &mut PgConnection,
    command_ids: &[Uuid],
) -> Result<Vec<PgRow>, SubmitInputRepositoryError> {
    let rows = sqlx::query(
        "SELECT
            registry.command_id AS registry_command_id,
            registry.command_kind AS registry_kind,
            registry.storage_version AS registry_version,
            typed.command_id AS typed_command_id,
            typed.command_kind AS typed_kind,
            typed.storage_version AS typed_version,
            typed.session_id AS command_session_id,
            typed.actor_kind,
            typed.actor_turn_id,
            typed.actor_tool_request_id,
            typed.content_kind AS command_content_kind,
            typed.content_text AS command_content_text,
            typed.delivery_kind AS command_delivery_kind,
            typed.expected_active_turn_id AS command_expected_active_turn_id,
            typed.expected_defaults_version AS command_expected_defaults_version,
            typed.model_override_kind AS command_model_override_kind,
            typed.replacement_model_kind AS command_replacement_model_kind,
            typed.replacement_direct_model_selection_id AS command_replacement_direct_id,
            typed.replacement_model_alias_id AS command_replacement_alias_id,
            typed.result_kind,
            typed.rejection_kind,
            typed.result_session_id,
            typed.result_accepted_input_id,
            typed.result_turn_id,
            typed.result_actual_active_turn_id,
            typed.result_expected_active_turn_id,
            typed.result_expected_defaults_version,
            typed.result_current_defaults_version,
            typed.result_unknown_alias_id,
            typed.result_selected_defaults_version,
            typed.result_last_position,
            accepted.accepting_command_id,
            accepted.accepted_input_id,
            accepted.session_id AS accepted_session_id,
            accepted.content_kind AS accepted_content_kind,
            accepted.content_text AS accepted_content_text,
            accepted.delivery_kind AS accepted_delivery_kind,
            accepted.expected_active_turn_id AS accepted_expected_active_turn_id,
            accepted.expected_defaults_version AS accepted_expected_defaults_version,
            accepted.model_override_kind AS accepted_model_override_kind,
            accepted.replacement_model_kind AS accepted_replacement_model_kind,
            accepted.replacement_direct_model_selection_id AS accepted_replacement_direct_id,
            accepted.replacement_model_alias_id AS accepted_replacement_alias_id,
            accepted.acceptance_position AS accepted_position,
            accepted.disposition_kind,
            accepted.origin_turn_id,
            queued.turn_id AS queued_turn_id,
            queued.accepted_input_id AS queued_accepted_input_id,
            queued.session_id AS queued_session_id,
            queued.acceptance_position AS queued_position,
            queued.priority_kind,
            queued.defaults_version AS queued_defaults_version,
            queued.requested_model_kind,
            queued.requested_direct_model_selection_id,
            queued.requested_model_alias_id,
            queued.frozen_model_kind,
            queued.frozen_direct_model_selection_id,
            queued.frozen_model_alias_id,
            queued.frozen_alias_selected_direct_id,
            queued.model_parameters,
            queued.known_provider_failure_retry,
            queued.model_fallback,
            defaults.session_id AS defaults_session_id,
            defaults.version AS defaults_version,
            defaults.model_selection_kind AS defaults_model_kind,
            defaults.direct_model_selection_id AS defaults_direct_id,
            defaults.model_alias_id AS defaults_alias_id,
            (
                SELECT count(*)
                  FROM accepted_input AS effect
                 WHERE effect.accepting_command_id = typed.command_id
            ) AS accepted_effect_count,
            (
                SELECT count(*)
                  FROM queued_input_origin AS effect_queue
                  JOIN accepted_input AS effect_input
                    ON effect_input.accepted_input_id = effect_queue.accepted_input_id
                 WHERE effect_input.accepting_command_id = typed.command_id
            ) AS queued_effect_count
         FROM durable_command AS registry
         LEFT JOIN submit_input_command AS typed
           ON typed.command_id = registry.command_id
         LEFT JOIN accepted_input AS accepted
           ON accepted.accepted_input_id = typed.result_accepted_input_id
         LEFT JOIN queued_input_origin AS queued
           ON queued.accepted_input_id = accepted.accepted_input_id
         LEFT JOIN session_defaults_version AS defaults
           ON defaults.session_id = typed.result_session_id
          AND defaults.version = COALESCE(
                queued.defaults_version,
                typed.result_selected_defaults_version
              )
         WHERE registry.command_id = ANY($1)",
    )
    .bind(command_ids)
    .fetch_all(&mut *connection)
    .await?;

    Ok(rows)
}

async fn load_from_connection(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<ReconstitutedSubmitInput>, SubmitInputRepositoryError> {
    let command_uuid = durable_command_id_to_uuid(command_id);
    let mut rows = load_complete_rows(connection, &[command_uuid]).await?;
    let Some(row) = rows.pop() else {
        return Ok(None);
    };
    if !rows.is_empty() {
        return Err(SubmitInputCorruption::Inconsistent("duplicate complete command rows").into());
    }
    let related_turn_origin = load_related_turn_origin(connection, &row).await?;
    decode_complete(row, command_id, related_turn_origin).map(Some)
}

async fn load_related_turn_origin(
    connection: &mut PgConnection,
    row: &PgRow,
) -> Result<Option<SubmitInputTurnOriginReconstitutionInput>, SubmitInputRepositoryError> {
    let Some(key) = related_turn_origin_key(row)? else {
        return Ok(None);
    };
    let mut origins = load_turn_origin_graph(connection, &BTreeSet::from([key])).await?;
    origins
        .remove(&key)
        .map(Some)
        .ok_or_else(|| SubmitInputCorruption::Missing("related turn origin").into())
}

fn related_turn_origin_key(
    row: &PgRow,
) -> Result<Option<StoredTurnOriginKey>, SubmitInputRepositoryError> {
    let result_kind: Option<String> = row.try_get("result_kind")?;
    let rejection_kind: Option<String> = row.try_get("rejection_kind")?;
    let delivery_kind: Option<String> = row.try_get("command_delivery_kind")?;
    let source_turn = match (
        result_kind.as_deref(),
        rejection_kind.as_deref(),
        delivery_kind.as_deref(),
    ) {
        (Some(APPLIED), None, Some("after_current_turn" | "next_safe_point")) => {
            required(row, "command_expected_active_turn_id")?
        }
        (Some(REJECTED), Some("active_turn_present" | "active_turn_mismatch"), _) => {
            required(row, "result_actual_active_turn_id")?
        }
        (
            Some(REJECTED),
            Some(
                "session_defaults_version_mismatch"
                | "unknown_model_alias"
                | "acceptance_position_exhausted",
            ),
            Some("after_current_turn" | "next_safe_point"),
        ) => required(row, "command_expected_active_turn_id")?,
        _ => return Ok(None),
    };
    Ok(Some((required(row, "result_session_id")?, source_turn)))
}

async fn load_turn_origin_graph(
    connection: &mut PgConnection,
    roots: &BTreeSet<StoredTurnOriginKey>,
) -> Result<
    BTreeMap<StoredTurnOriginKey, SubmitInputTurnOriginReconstitutionInput>,
    SubmitInputRepositoryError,
> {
    if roots.is_empty() {
        return Ok(BTreeMap::new());
    }

    let source_sessions = roots
        .iter()
        .map(|(session, _)| *session)
        .collect::<Vec<_>>();
    let source_turns = roots.iter().map(|(_, turn)| *turn).collect::<Vec<_>>();
    let link_rows = sqlx::query(
        "WITH RECURSIVE origin_turn(session_id, turn_id) AS (
            SELECT root.session_id, root.turn_id
              FROM UNNEST($1::uuid[], $2::uuid[]) AS root(session_id, turn_id)
            UNION
            SELECT current.session_id, command.expected_active_turn_id
              FROM origin_turn AS current
              JOIN turn_lifecycle AS turn
                ON turn.turn_id = current.turn_id
               AND turn.session_id = current.session_id
              JOIN queued_input_origin AS queued
                ON queued.turn_id = turn.turn_id
               AND queued.session_id = turn.session_id
               AND queued.accepted_input_id = turn.origin_accepted_input_id
              JOIN accepted_input AS accepted
                ON accepted.accepted_input_id = queued.accepted_input_id
               AND accepted.session_id = turn.session_id
               AND accepted.origin_turn_id = turn.turn_id
               AND accepted.disposition_kind = 'origin_of'
              JOIN submit_input_command AS command
                ON command.command_id = accepted.accepting_command_id
             WHERE command.delivery_kind = 'after_current_turn'
               AND command.expected_active_turn_id IS NOT NULL
        )
        SELECT
            current.session_id AS origin_session_id,
            current.turn_id AS origin_turn_id,
            accepted.accepting_command_id AS origin_command_id,
            accepted.accepted_input_id AS origin_accepted_input_id,
            queued.acceptance_position AS origin_acceptance_position,
            queued.priority_kind AS origin_priority_kind,
            command.delivery_kind AS origin_delivery_kind,
            command.expected_active_turn_id AS origin_predecessor_turn_id
          FROM origin_turn AS current
          JOIN turn_lifecycle AS turn
            ON turn.turn_id = current.turn_id
           AND turn.session_id = current.session_id
          JOIN queued_input_origin AS queued
            ON queued.turn_id = turn.turn_id
           AND queued.session_id = turn.session_id
           AND queued.accepted_input_id = turn.origin_accepted_input_id
          JOIN accepted_input AS accepted
            ON accepted.accepted_input_id = queued.accepted_input_id
           AND accepted.session_id = turn.session_id
           AND accepted.origin_turn_id = turn.turn_id
           AND accepted.disposition_kind = 'origin_of'
          JOIN submit_input_command AS command
            ON command.command_id = accepted.accepting_command_id
         ORDER BY current.session_id, current.turn_id",
    )
    .bind(&source_sessions)
    .bind(&source_turns)
    .fetch_all(&mut *connection)
    .await?;

    let mut links = BTreeMap::new();
    let mut commands = BTreeMap::new();
    for row in link_rows {
        let key = (
            required(&row, "origin_session_id")?,
            required(&row, "origin_turn_id")?,
        );
        let command_uuid: Uuid = required(&row, "origin_command_id")?;
        let command_id = durable_command_id_from_uuid(command_uuid)
            .map_err(|_| SubmitInputCorruption::Inconsistent("turn origin command identity"))?;
        let accepted_input =
            accepted_input_id_from_uuid(required(&row, "origin_accepted_input_id")?);
        let queue_position = decode_position(&row, "origin_acceptance_position")?;
        require_spelling(&row, "origin_priority_kind", "ordinary")?;
        let delivery_kind: String = required(&row, "origin_delivery_kind")?;
        let predecessor_turn: Option<Uuid> = row.try_get("origin_predecessor_turn_id")?;
        let predecessor = match (delivery_kind.as_str(), predecessor_turn) {
            ("start_when_no_active_turn", None) => None,
            ("after_current_turn", Some(turn)) => Some((key.0, turn)),
            ("start_when_no_active_turn" | "after_current_turn", _) => {
                return Err(
                    SubmitInputCorruption::Inconsistent("turn origin predecessor shape").into(),
                );
            }
            _ => {
                return Err(
                    SubmitInputCorruption::Inconsistent("turn origin command delivery").into(),
                );
            }
        };
        if links
            .insert(
                key,
                StoredTurnOriginLink {
                    command_id,
                    predecessor,
                    accepted_input,
                    queue_order: AcceptedInputQueueOrder::ordinary(queue_position),
                },
            )
            .is_some()
        {
            return Err(SubmitInputCorruption::Inconsistent("duplicate turn origin").into());
        }
        if commands.insert(command_uuid, key).is_some() {
            return Err(
                SubmitInputCorruption::Inconsistent("turn origin command reused by turns").into(),
            );
        }
    }

    for root in roots {
        if !links.contains_key(root) {
            return Err(SubmitInputCorruption::Missing("related turn origin").into());
        }
    }
    for link in links.values() {
        if let Some(predecessor) = link.predecessor
            && !links.contains_key(&predecessor)
        {
            return Err(SubmitInputCorruption::Missing("turn origin predecessor").into());
        }
    }

    let command_uuids = commands.keys().copied().collect::<Vec<_>>();
    let complete_rows = load_complete_rows(connection, &command_uuids).await?;
    let mut rows_by_command = BTreeMap::new();
    for row in complete_rows {
        let command_uuid: Uuid = required(&row, "registry_command_id")?;
        if !commands.contains_key(&command_uuid) {
            return Err(
                SubmitInputCorruption::Inconsistent("unexpected turn origin command").into(),
            );
        }
        if rows_by_command.insert(command_uuid, row).is_some() {
            return Err(
                SubmitInputCorruption::Inconsistent("duplicate turn origin command rows").into(),
            );
        }
    }
    if rows_by_command.len() != commands.len() {
        return Err(SubmitInputCorruption::Missing("turn origin command").into());
    }

    let mut decoded = BTreeMap::new();
    while !links.is_empty() {
        let Some(ready) = links.iter().find_map(|(key, link)| {
            link.predecessor
                .is_none_or(|predecessor| decoded.contains_key(&predecessor))
                .then_some(*key)
        }) else {
            return Err(
                SubmitInputCorruption::Inconsistent("turn origin predecessor cycle").into(),
            );
        };
        let link = links
            .remove(&ready)
            .expect("the selected turn origin link remains present");
        let command_uuid = durable_command_id_to_uuid(link.command_id);
        let row = rows_by_command
            .remove(&command_uuid)
            .ok_or(SubmitInputCorruption::Missing("turn origin command"))?;
        let predecessor = link
            .predecessor
            .map(|key| {
                decoded
                    .get(&key)
                    .cloned()
                    .ok_or(SubmitInputCorruption::Missing("turn origin predecessor"))
            })
            .transpose()?;
        let origin = decode_complete(row, link.command_id, predecessor)?;
        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
            origin.result()
        else {
            return Err(SubmitInputCorruption::Inconsistent("turn origin command result").into());
        };
        if session_id_to_uuid(applied.session()) != ready.0
            || turn_id_to_uuid(applied.turn()) != ready.1
        {
            return Err(SubmitInputCorruption::Inconsistent("turn origin correlation").into());
        }
        decoded.insert(
            ready,
            SubmitInputTurnOriginReconstitutionInput::new(
                origin,
                AcceptedInputLifecycle::new(
                    link.accepted_input,
                    AcceptedInputDisposition::OriginOf(turn_id_from_uuid(ready.1)),
                ),
                link.accepted_input,
                session_id_from_uuid(ready.0),
                turn_id_from_uuid(ready.1),
                link.queue_order,
            ),
        );
    }

    Ok(decoded)
}

fn decode_complete(
    row: PgRow,
    command_id: DurableCommandId,
    related_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
) -> Result<ReconstitutedSubmitInput, SubmitInputRepositoryError> {
    require_spelling(&row, "registry_kind", SUBMIT_INPUT_KIND)?;
    require_version(&row, "registry_version", STORAGE_VERSION)?;
    let typed_id: Uuid = required(&row, "typed_command_id")?;
    if typed_id != durable_command_id_to_uuid(command_id) {
        return Err(SubmitInputCorruption::Inconsistent("typed command identity").into());
    }
    require_spelling(&row, "typed_kind", SUBMIT_INPUT_KIND)?;
    require_version(&row, "typed_version", STORAGE_VERSION)?;

    // Decode-level checks reject unknown or malformed actor spellings here;
    // comparing the decoded actor against the canonical command's actor is
    // domain-owned semantics and happens inside reconstitution.
    let actor = decode_actor(
        required(&row, "actor_kind")?,
        row.try_get("actor_turn_id")?,
        row.try_get("actor_tool_request_id")?,
    )?;
    let command = SubmitInput::new(
        command_id,
        session_id_from_uuid(required(&row, "command_session_id")?),
        decode_content(
            required(&row, "command_content_kind")?,
            required(&row, "command_content_text")?,
            "command content",
        )?,
        decode_delivery(
            required(&row, "command_delivery_kind")?,
            row.try_get("command_expected_active_turn_id")?,
            row.try_get("command_expected_defaults_version")?,
            row.try_get("command_model_override_kind")?,
            row.try_get("command_replacement_model_kind")?,
            row.try_get("command_replacement_direct_id")?,
            row.try_get("command_replacement_alias_id")?,
            "command delivery",
        )?,
    );

    let result_kind: String = required(&row, "result_kind")?;
    let rejection_kind: Option<String> = row.try_get("rejection_kind")?;
    let result_session = session_id_from_uuid(required(&row, "result_session_id")?);
    let result_accepted: Option<Uuid> = row.try_get("result_accepted_input_id")?;
    let result_turn: Option<Uuid> = row.try_get("result_turn_id")?;
    let result_actual_turn: Option<Uuid> = row.try_get("result_actual_active_turn_id")?;
    let result_expected_turn: Option<Uuid> = row.try_get("result_expected_active_turn_id")?;
    let result_expected_defaults: Option<Decimal> =
        row.try_get("result_expected_defaults_version")?;
    let result_current_defaults: Option<Decimal> =
        row.try_get("result_current_defaults_version")?;
    let result_unknown_alias: Option<Uuid> = row.try_get("result_unknown_alias_id")?;
    let result_selected_defaults: Option<Decimal> =
        row.try_get("result_selected_defaults_version")?;
    let result_last_position: Option<Decimal> = row.try_get("result_last_position")?;
    let accepted_effect_count: i64 = required(&row, "accepted_effect_count")?;
    let queued_effect_count: i64 = required(&row, "queued_effect_count")?;

    let input = match (result_kind.as_str(), rejection_kind.as_deref()) {
        (APPLIED, None) => {
            if result_expected_turn.is_some()
                || result_expected_defaults.is_some()
                || result_current_defaults.is_some()
                || result_unknown_alias.is_some()
                || result_selected_defaults.is_some()
                || result_last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent("applied result fields").into());
            }
            if accepted_effect_count != 1 {
                return Err(
                    SubmitInputCorruption::Inconsistent("applied effect cardinality").into(),
                );
            }
            let result_accepted = accepted_input_id_from_uuid(
                result_accepted
                    .ok_or(SubmitInputCorruption::Missing("result_accepted_input_id"))?,
            );
            match (result_turn, result_actual_turn) {
                (Some(result_turn), None) if queued_effect_count == 1 => {
                    decode_applied_turn_origin(
                        &row,
                        command,
                        actor,
                        result_session,
                        result_accepted,
                        turn_id_from_uuid(result_turn),
                        related_turn_origin,
                    )?
                }
                (None, Some(source_turn)) if queued_effect_count == 0 => {
                    let source_turn_origin = related_turn_origin.ok_or(
                        SubmitInputCorruption::Missing("pending steering source turn origin"),
                    )?;
                    decode_applied_pending_steering(
                        &row,
                        command,
                        actor,
                        result_session,
                        result_accepted,
                        turn_id_from_uuid(source_turn),
                        source_turn_origin,
                    )?
                }
                _ => {
                    return Err(
                        SubmitInputCorruption::Inconsistent("applied variant correlation").into(),
                    );
                }
            }
        }
        (REJECTED, Some(kind)) => {
            if accepted_effect_count != 0 || queued_effect_count != 0 {
                return Err(
                    SubmitInputCorruption::Inconsistent("rejected command has effects").into(),
                );
            }
            if result_accepted.is_some() || result_turn.is_some() {
                return Err(
                    SubmitInputCorruption::Inconsistent("rejected applied identities").into(),
                );
            }
            decode_rejected(
                &row,
                command,
                actor,
                result_session,
                related_turn_origin,
                kind,
                result_actual_turn,
                result_expected_turn,
                result_expected_defaults,
                result_current_defaults,
                result_unknown_alias,
                result_selected_defaults,
                result_last_position,
            )?
        }
        (APPLIED, Some(_)) | (REJECTED, None) => {
            return Err(SubmitInputCorruption::Inconsistent("terminal result shape").into());
        }
        (value, _) => {
            return Err(SubmitInputCorruption::Unsupported {
                field: "result_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };

    input
        .reconstitute()
        .map_err(|error| SubmitInputCorruption::Domain(error.failure()).into())
}

fn decode_applied_turn_origin(
    row: &PgRow,
    command: SubmitInput,
    stored_actor: Actor,
    result_session: SessionId,
    result_accepted_input: AcceptedInputId,
    result_turn: TurnId,
    predecessor_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
) -> Result<SubmitInputReconstitutionInput, SubmitInputRepositoryError> {
    let accepting_command_uuid: Uuid = required(row, "accepting_command_id")?;
    let accepting_command = durable_command_id_from_uuid(accepting_command_uuid)
        .map_err(|_| SubmitInputCorruption::Inconsistent("accepting command identity"))?;
    let accepted_input = accepted_input_id_from_uuid(required(row, "accepted_input_id")?);
    let accepted_session = session_id_from_uuid(required(row, "accepted_session_id")?);
    let accepted_content = decode_content(
        required(row, "accepted_content_kind")?,
        required(row, "accepted_content_text")?,
        "accepted content",
    )?;
    let accepted_delivery = decode_delivery(
        required(row, "accepted_delivery_kind")?,
        row.try_get("accepted_expected_active_turn_id")?,
        row.try_get("accepted_expected_defaults_version")?,
        row.try_get("accepted_model_override_kind")?,
        row.try_get("accepted_replacement_model_kind")?,
        row.try_get("accepted_replacement_direct_id")?,
        row.try_get("accepted_replacement_alias_id")?,
        "accepted delivery",
    )?;
    let accepted_position = decode_position(row, "accepted_position")?;
    require_spelling(row, "disposition_kind", "origin_of")?;
    let accepted_origin_turn = turn_id_from_uuid(required(row, "origin_turn_id")?);

    let queued_turn = turn_id_from_uuid(required(row, "queued_turn_id")?);
    let queued_accepted = accepted_input_id_from_uuid(required(row, "queued_accepted_input_id")?);
    if queued_accepted != accepted_input {
        return Err(SubmitInputCorruption::Inconsistent("queued accepted input").into());
    }
    let queued_session = session_id_from_uuid(required(row, "queued_session_id")?);
    let queued_position = decode_position(row, "queued_position")?;
    require_spelling(row, "priority_kind", "ordinary")?;
    require_spelling(row, "model_parameters", "provider_defaults")?;
    require_spelling(row, "known_provider_failure_retry", "disabled")?;
    require_spelling(row, "model_fallback", "disabled")?;
    let defaults_version = decode_defaults_version(row, "queued_defaults_version")?;

    let defaults_session = session_id_from_uuid(required(row, "defaults_session_id")?);
    let joined_defaults_version = decode_defaults_version(row, "defaults_version")?;
    if joined_defaults_version != defaults_version {
        return Err(SubmitInputCorruption::Inconsistent("selected defaults version").into());
    }
    let defaults = decode_defaults(
        required(row, "defaults_model_kind")?,
        row.try_get("defaults_direct_id")?,
        row.try_get("defaults_alias_id")?,
        "selected defaults",
    )?;
    let stored_requested_model = decode_model_selection(
        required(row, "requested_model_kind")?,
        row.try_get("requested_direct_model_selection_id")?,
        row.try_get("requested_model_alias_id")?,
        "requested model",
    )?;
    let stored_frozen_model = decode_frozen_model(
        required(row, "frozen_model_kind")?,
        row.try_get("frozen_direct_model_selection_id")?,
        row.try_get("frozen_model_alias_id")?,
        row.try_get("frozen_alias_selected_direct_id")?,
    )?;

    Ok(SubmitInputReconstitutionInput::applied_turn_origin(
        command,
        stored_actor,
        result_session,
        result_accepted_input,
        result_turn,
        predecessor_origin,
        accepting_command,
        accepted_input,
        accepted_session,
        accepted_content,
        accepted_delivery,
        accepted_position,
        AcceptedInputDisposition::OriginOf(accepted_origin_turn),
        queued_session,
        queued_turn,
        AcceptedInputQueueOrder::ordinary(queued_position),
        defaults_session,
        defaults_version,
        defaults,
        stored_requested_model,
        stored_frozen_model,
    ))
}

fn decode_applied_pending_steering(
    row: &PgRow,
    command: SubmitInput,
    stored_actor: Actor,
    result_session: SessionId,
    result_accepted_input: AcceptedInputId,
    result_source_turn: TurnId,
    source_turn_origin: SubmitInputTurnOriginReconstitutionInput,
) -> Result<SubmitInputReconstitutionInput, SubmitInputRepositoryError> {
    let accepting_command_uuid: Uuid = required(row, "accepting_command_id")?;
    let accepting_command = durable_command_id_from_uuid(accepting_command_uuid)
        .map_err(|_| SubmitInputCorruption::Inconsistent("accepting command identity"))?;
    let accepted_input = accepted_input_id_from_uuid(required(row, "accepted_input_id")?);
    let accepted_session = session_id_from_uuid(required(row, "accepted_session_id")?);
    let accepted_content = decode_content(
        required(row, "accepted_content_kind")?,
        required(row, "accepted_content_text")?,
        "accepted content",
    )?;
    let accepted_delivery = decode_delivery(
        required(row, "accepted_delivery_kind")?,
        row.try_get("accepted_expected_active_turn_id")?,
        row.try_get("accepted_expected_defaults_version")?,
        row.try_get("accepted_model_override_kind")?,
        row.try_get("accepted_replacement_model_kind")?,
        row.try_get("accepted_replacement_direct_id")?,
        row.try_get("accepted_replacement_alias_id")?,
        "accepted delivery",
    )?;
    let accepted_position = decode_position(row, "accepted_position")?;

    Ok(SubmitInputReconstitutionInput::applied_pending_steering(
        command,
        stored_actor,
        result_session,
        result_accepted_input,
        result_source_turn,
        source_turn_origin,
        accepting_command,
        accepted_input,
        accepted_session,
        accepted_content,
        accepted_delivery,
        accepted_position,
    ))
}

#[allow(clippy::too_many_arguments)]
fn decode_rejected(
    row: &PgRow,
    command: SubmitInput,
    stored_actor: Actor,
    result_session: SessionId,
    active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    rejection_kind: &str,
    actual_turn: Option<Uuid>,
    expected_turn: Option<Uuid>,
    expected_defaults: Option<Decimal>,
    current_defaults: Option<Decimal>,
    unknown_alias: Option<Uuid>,
    selected_defaults: Option<Decimal>,
    last_position: Option<Decimal>,
) -> Result<SubmitInputReconstitutionInput, SubmitInputRepositoryError> {
    match rejection_kind {
        "session_not_found" => {
            require_all_absent(
                actual_turn,
                expected_turn,
                expected_defaults,
                current_defaults,
                unknown_alias,
                selected_defaults,
                last_position,
                "session-not-found result fields",
            )?;
            Ok(SubmitInputReconstitutionInput::rejected_session_not_found(
                command,
                stored_actor,
                result_session,
            ))
        }
        "no_active_turn" => {
            if actual_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("no-active-turn result fields").into(),
                );
            }
            Ok(SubmitInputReconstitutionInput::rejected_no_active_turn(
                command,
                stored_actor,
                result_session,
                turn_id_from_uuid(expected_turn.ok_or(SubmitInputCorruption::Missing(
                    "result_expected_active_turn_id",
                ))?),
            ))
        }
        "active_turn_present" => {
            if expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "active-turn-present result fields",
                )
                .into());
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_active_turn_present(
                    command,
                    stored_actor,
                    result_session,
                    turn_id_from_uuid(actual_turn.ok_or(SubmitInputCorruption::Missing(
                        "result_actual_active_turn_id",
                    ))?),
                    active_turn_origin
                        .ok_or(SubmitInputCorruption::Missing("active turn origin"))?,
                ),
            )
        }
        "active_turn_mismatch" => {
            if expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "active-turn-mismatch result fields",
                )
                .into());
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                    command,
                    stored_actor,
                    result_session,
                    turn_id_from_uuid(expected_turn.ok_or(SubmitInputCorruption::Missing(
                        "result_expected_active_turn_id",
                    ))?),
                    turn_id_from_uuid(actual_turn.ok_or(SubmitInputCorruption::Missing(
                        "result_actual_active_turn_id",
                    ))?),
                    active_turn_origin
                        .ok_or(SubmitInputCorruption::Missing("actual turn origin"))?,
                ),
            )
        }
        "session_defaults_version_mismatch" => {
            if actual_turn.is_some()
                || expected_turn.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("defaults-mismatch result fields").into(),
                );
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                    command,
                    stored_actor,
                    result_session,
                    decode_optional_defaults_version(
                        expected_defaults,
                        "result_expected_defaults_version",
                    )?
                    .ok_or(SubmitInputCorruption::Missing(
                        "result_expected_defaults_version",
                    ))?,
                    decode_optional_defaults_version(
                        current_defaults,
                        "result_current_defaults_version",
                    )?
                    .ok_or(SubmitInputCorruption::Missing(
                        "result_current_defaults_version",
                    ))?,
                    active_turn_origin,
                ),
            )
        }
        "unknown_model_alias" => {
            if actual_turn.is_some()
                || expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || last_position.is_some()
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("unknown-alias result fields").into(),
                );
            }
            let selected = decode_optional_defaults_version(
                selected_defaults,
                "result_selected_defaults_version",
            )?
            .ok_or(SubmitInputCorruption::Missing(
                "result_selected_defaults_version",
            ))?;
            let defaults_session = session_id_from_uuid(required(row, "defaults_session_id")?);
            let defaults_version = decode_defaults_version(row, "defaults_version")?;
            let defaults = decode_defaults(
                required(row, "defaults_model_kind")?,
                row.try_get("defaults_direct_id")?,
                row.try_get("defaults_alias_id")?,
                "selected defaults",
            )?;
            if selected != defaults_version {
                return Err(
                    SubmitInputCorruption::Inconsistent("unknown-alias defaults version").into(),
                );
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                    command,
                    stored_actor,
                    result_session,
                    ModelAlias::from_uuid(
                        unknown_alias
                            .ok_or(SubmitInputCorruption::Missing("result_unknown_alias_id"))?,
                    ),
                    defaults_session,
                    defaults_version,
                    defaults,
                    active_turn_origin,
                ),
            )
        }
        "acceptance_position_exhausted" => {
            if actual_turn.is_some()
                || expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "position-exhausted result fields",
                )
                .into());
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                    command,
                    stored_actor,
                    result_session,
                    decode_optional_position(last_position, "result_last_position")?
                        .ok_or(SubmitInputCorruption::Missing("result_last_position"))?,
                    active_turn_origin,
                ),
            )
        }
        value => Err(SubmitInputCorruption::Unsupported {
            field: "rejection_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, SubmitInputRepositoryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| SubmitInputCorruption::Missing(field).into())
}

fn require_spelling(
    row: &PgRow,
    field: &'static str,
    expected: &str,
) -> Result<(), SubmitInputRepositoryError> {
    let actual: String = required(row, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(SubmitInputCorruption::Unsupported {
            field,
            value: actual,
        }
        .into())
    }
}

fn require_version(
    row: &PgRow,
    field: &'static str,
    expected: i16,
) -> Result<(), SubmitInputRepositoryError> {
    let actual: i16 = required(row, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(SubmitInputCorruption::Unsupported {
            field,
            value: actual.to_string(),
        }
        .into())
    }
}

#[allow(clippy::too_many_arguments)]
fn require_all_absent(
    actual_turn: Option<Uuid>,
    expected_turn: Option<Uuid>,
    expected_defaults: Option<Decimal>,
    current_defaults: Option<Decimal>,
    unknown_alias: Option<Uuid>,
    selected_defaults: Option<Decimal>,
    last_position: Option<Decimal>,
    relationship: &'static str,
) -> Result<(), SubmitInputRepositoryError> {
    if actual_turn.is_none()
        && expected_turn.is_none()
        && expected_defaults.is_none()
        && current_defaults.is_none()
        && unknown_alias.is_none()
        && selected_defaults.is_none()
        && last_position.is_none()
    {
        Ok(())
    } else {
        Err(SubmitInputCorruption::Inconsistent(relationship).into())
    }
}

fn decode_actor(
    kind: String,
    turn: Option<Uuid>,
    tool_request: Option<Uuid>,
) -> Result<Actor, SubmitInputRepositoryError> {
    match (kind.as_str(), turn, tool_request) {
        ("owner", None, None) => Ok(Actor::Owner),
        ("model", Some(turn), None) => Ok(Actor::Model {
            turn: TurnId::from_uuid(turn),
        }),
        ("recovery", None, None) => Ok(Actor::Recovery),
        ("tool", None, Some(request)) => Ok(Actor::Tool {
            request: ToolRequestId::from_uuid(request),
        }),
        ("owner" | "model" | "recovery" | "tool", _, _) => {
            Err(SubmitInputCorruption::Inconsistent("actor fields").into())
        }
        _ => Err(SubmitInputCorruption::Unsupported {
            field: "actor_kind",
            value: kind,
        }
        .into()),
    }
}

fn decode_content(
    kind: String,
    text: String,
    field: &'static str,
) -> Result<UserContent, SubmitInputRepositoryError> {
    if kind != "text" {
        return Err(SubmitInputCorruption::Unsupported { field, value: kind }.into());
    }
    UserContent::try_text(text).map_err(|error| {
        SubmitInputCorruption::InvalidContent {
            field,
            failure: error.failure(),
        }
        .into()
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_delivery(
    kind: String,
    expected_active_turn: Option<Uuid>,
    expected_defaults_version: Option<Decimal>,
    model_override_kind: Option<String>,
    replacement_model_kind: Option<String>,
    replacement_direct: Option<Uuid>,
    replacement_alias: Option<Uuid>,
    field: &'static str,
) -> Result<DeliveryRequest, SubmitInputRepositoryError> {
    match kind.as_str() {
        "start_when_no_active_turn" => {
            if expected_active_turn.is_some() {
                return Err(SubmitInputCorruption::Inconsistent(field).into());
            }
            Ok(DeliveryRequest::StartWhenNoActiveTurn {
                configuration: decode_configuration(
                    expected_defaults_version,
                    model_override_kind,
                    replacement_model_kind,
                    replacement_direct,
                    replacement_alias,
                    field,
                )?,
            })
        }
        "interrupt" | "after_current_turn" => {
            let turn = TurnId::from_uuid(
                expected_active_turn
                    .ok_or(SubmitInputCorruption::Missing("expected_active_turn_id"))?,
            );
            let configuration = decode_configuration(
                expected_defaults_version,
                model_override_kind,
                replacement_model_kind,
                replacement_direct,
                replacement_alias,
                field,
            )?;
            if kind == "interrupt" {
                Ok(DeliveryRequest::Interrupt {
                    expected_active_turn: turn,
                    configuration,
                })
            } else {
                Ok(DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: turn,
                    configuration,
                })
            }
        }
        "next_safe_point" => {
            if expected_defaults_version.is_some()
                || model_override_kind.is_some()
                || replacement_model_kind.is_some()
                || replacement_direct.is_some()
                || replacement_alias.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(field).into());
            }
            Ok(DeliveryRequest::NextSafePoint {
                expected_active_turn: TurnId::from_uuid(
                    expected_active_turn
                        .ok_or(SubmitInputCorruption::Missing("expected_active_turn_id"))?,
                ),
            })
        }
        _ => Err(SubmitInputCorruption::Unsupported { field, value: kind }.into()),
    }
}

fn decode_configuration(
    expected_defaults_version: Option<Decimal>,
    model_override_kind: Option<String>,
    replacement_model_kind: Option<String>,
    replacement_direct: Option<Uuid>,
    replacement_alias: Option<Uuid>,
    field: &'static str,
) -> Result<PerInputConfigurationChoices, SubmitInputRepositoryError> {
    let expected =
        decode_optional_defaults_version(expected_defaults_version, "expected_defaults_version")?
            .ok_or(SubmitInputCorruption::Missing("expected_defaults_version"))?;
    let model = match model_override_kind.as_deref() {
        Some("use_session_default") => {
            if replacement_model_kind.is_some()
                || replacement_direct.is_some()
                || replacement_alias.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(field).into());
            }
            ModelSelectionOverride::UseSessionDefault
        }
        Some("replace_with") => ModelSelectionOverride::ReplaceWith(decode_model_selection(
            replacement_model_kind
                .ok_or(SubmitInputCorruption::Missing("replacement_model_kind"))?,
            replacement_direct,
            replacement_alias,
            "replacement model",
        )?),
        Some(value) => {
            return Err(SubmitInputCorruption::Unsupported {
                field: "model_override_kind",
                value: value.to_owned(),
            }
            .into());
        }
        None => return Err(SubmitInputCorruption::Missing("model_override_kind").into()),
    };
    Ok(PerInputConfigurationChoices::new(expected, model))
}

fn decode_defaults_version(
    row: &PgRow,
    field: &'static str,
) -> Result<SessionConfigurationDefaultsVersion, SubmitInputRepositoryError> {
    let value: Decimal = required(row, field)?;
    defaults_version_from_numeric(value)
        .map_err(|reason| SubmitInputCorruption::InvalidOrdinal { field, reason }.into())
}

fn decode_optional_defaults_version(
    value: Option<Decimal>,
    field: &'static str,
) -> Result<Option<SessionConfigurationDefaultsVersion>, SubmitInputRepositoryError> {
    value
        .map(|value| {
            defaults_version_from_numeric(value)
                .map_err(|reason| SubmitInputCorruption::InvalidOrdinal { field, reason }.into())
        })
        .transpose()
}

fn decode_position(
    row: &PgRow,
    field: &'static str,
) -> Result<SessionInputPosition, SubmitInputRepositoryError> {
    let value: Decimal = required(row, field)?;
    input_position_from_numeric(value)
        .map_err(|reason| SubmitInputCorruption::InvalidOrdinal { field, reason }.into())
}

fn decode_optional_position(
    value: Option<Decimal>,
    field: &'static str,
) -> Result<Option<SessionInputPosition>, SubmitInputRepositoryError> {
    value
        .map(|value| {
            input_position_from_numeric(value)
                .map_err(|reason| SubmitInputCorruption::InvalidOrdinal { field, reason }.into())
        })
        .transpose()
}

fn decode_defaults(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    field: &'static str,
) -> Result<SessionConfigurationDefaults, SubmitInputRepositoryError> {
    Ok(SessionConfigurationDefaults::new(decode_model_selection(
        kind, direct, alias, field,
    )?))
}

fn decode_model_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    field: &'static str,
) -> Result<ModelSelectionRequest, SubmitInputRepositoryError> {
    match (kind.as_str(), direct, alias) {
        ("direct", Some(selection), None) => Ok(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(selection),
        )),
        ("alias", None, Some(alias)) => {
            Ok(ModelSelectionRequest::Alias(ModelAlias::from_uuid(alias)))
        }
        ("direct" | "alias", _, _) => Err(SubmitInputCorruption::Inconsistent(field).into()),
        _ => Err(SubmitInputCorruption::Unsupported { field, value: kind }.into()),
    }
}

fn decode_frozen_model(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    alias_selected: Option<Uuid>,
) -> Result<FrozenModelSelection, SubmitInputRepositoryError> {
    match (kind.as_str(), direct, alias, alias_selected) {
        ("direct", Some(selection), None, None) => Ok(FrozenModelSelection::Direct(
            DirectModelSelection::from_uuid(selection),
        )),
        ("frozen_alias", None, Some(alias), Some(selected)) => {
            Ok(FrozenModelSelection::FrozenAlias {
                alias: ModelAlias::from_uuid(alias),
                definition: FrozenAliasDefinition::selecting(DirectModelSelection::from_uuid(
                    selected,
                )),
            })
        }
        ("direct" | "frozen_alias", _, _, _) => {
            Err(SubmitInputCorruption::Inconsistent("frozen model").into())
        }
        _ => Err(SubmitInputCorruption::Unsupported {
            field: "frozen_model_kind",
            value: kind,
        }
        .into()),
    }
}

async fn inspect_registry(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<CommandKind>, SubmitInputRepositoryError> {
    command_registry::inspect(connection, command_id)
        .await
        .map_err(map_registry_error)
}

fn map_registry_error(error: RegistryInspectionError) -> SubmitInputRepositoryError {
    match error {
        RegistryInspectionError::Database(error) => error.into(),
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedKind(value)) => {
            SubmitInputCorruption::Unsupported {
                field: "registry_kind",
                value,
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedVersion(value)) => {
            SubmitInputCorruption::Unsupported {
                field: "registry_version",
                value: value.to_string(),
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::MissingTypedRecord(_)) => {
            SubmitInputCorruption::Missing("typed_command_id").into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::ConflictingTypedRecords) => {
            SubmitInputCorruption::Inconsistent("typed command family").into()
        }
    }
}
