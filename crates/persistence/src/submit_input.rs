//! Atomic PostgreSQL persistence and replay for durable input acceptance.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{SubmitInputOutcome, SubmitInputTransaction};
use signalbox_domain::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputQueueOrder, Actor, DeliveryRequest,
    DirectModelSelection, DurableCommandId, FrozenAliasDefinition, FrozenModelSelection,
    ModelAlias, ModelSelectionOverride, ModelSelectionRequest, NonEmptyUnicodeTextFailure,
    PerInputConfigurationChoices, PreparedSubmitInput, ReconstitutedSubmitInput,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionId,
    SessionInputPosition, SubmitInput, SubmitInputPreparationFailure,
    SubmitInputReconstitutionFailure, SubmitInputReconstitutionInput, SubmitInputRejectedResult,
    SubmitInputResult, ToolRequestId, TurnId, UserContent,
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
    /// Durable records cannot reconstruct the requested domain value.
    Corruption(SubmitInputCorruption),
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
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for SubmitInputRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::DifferentCommandKind { .. } => None,
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
    // current-defaults pointer row, while a concurrent defaults replacement
    // holds the pointer row (its compare-and-set) when its
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

    let previous_position = if matches!(
        command.delivery(),
        DeliveryRequest::StartWhenNoActiveTurn { .. }
    ) {
        sqlx::query_scalar::<_, Decimal>(
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
        .transpose()?
    } else {
        None
    };

    command
        .prepare_when_no_active_turn(&session, accepted_input, turn, previous_position, |_| None)
        .map_err(|error| {
            let relationship = match error.failure() {
                SubmitInputPreparationFailure::SessionMismatch { .. } => {
                    "current session ownership"
                }
                SubmitInputPreparationFailure::TurnCandidateMismatch => "delivery turn candidate",
            };
            SubmitInputCorruption::Inconsistent(relationship).into()
        })
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
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
             $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25,
             $26, $27)",
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
    .bind(result.expected_active_turn)
    .bind(result.expected_defaults_version)
    .bind(result.current_defaults_version)
    .bind(result.unknown_alias)
    .bind(result.selected_defaults_version)
    .bind(result.last_position)
    .execute(&mut *connection)
    .await?;

    if let SubmitInputResult::Applied(applied) = prepared.result() {
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
    expected_active_turn: Option<Uuid>,
    expected_defaults_version: Option<Decimal>,
    current_defaults_version: Option<Decimal>,
    unknown_alias: Option<Uuid>,
    selected_defaults_version: Option<Decimal>,
    last_position: Option<Decimal>,
}

fn encode_result(result: &SubmitInputResult, delivery: DeliveryRequest) -> EncodedResult {
    match result {
        SubmitInputResult::Applied(result) => EncodedResult {
            kind: APPLIED,
            rejection_kind: None,
            session: result.session(),
            accepted_input: Some(accepted_input_id_to_uuid(result.accepted_input())),
            turn: Some(turn_id_to_uuid(result.turn())),
            expected_active_turn: None,
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

async fn load_from_connection(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<ReconstitutedSubmitInput>, SubmitInputRepositoryError> {
    let row = sqlx::query(
        "SELECT
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
         WHERE registry.command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;

    row.map(|row| decode_complete(row, command_id)).transpose()
}

fn decode_complete(
    row: PgRow,
    command_id: DurableCommandId,
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
            if accepted_effect_count != 1 || queued_effect_count != 1 {
                return Err(
                    SubmitInputCorruption::Inconsistent("applied effect cardinality").into(),
                );
            }
            decode_applied(
                &row,
                command,
                actor,
                result_session,
                accepted_input_id_from_uuid(
                    result_accepted
                        .ok_or(SubmitInputCorruption::Missing("result_accepted_input_id"))?,
                ),
                turn_id_from_uuid(
                    result_turn.ok_or(SubmitInputCorruption::Missing("result_turn_id"))?,
                ),
            )?
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
                kind,
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

fn decode_applied(
    row: &PgRow,
    command: SubmitInput,
    stored_actor: Actor,
    result_session: SessionId,
    result_accepted_input: AcceptedInputId,
    result_turn: TurnId,
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

    Ok(SubmitInputReconstitutionInput::applied(
        command,
        stored_actor,
        result_session,
        result_accepted_input,
        result_turn,
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

#[allow(clippy::too_many_arguments)]
fn decode_rejected(
    row: &PgRow,
    command: SubmitInput,
    stored_actor: Actor,
    result_session: SessionId,
    rejection_kind: &str,
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
            if expected_defaults.is_some()
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
        "session_defaults_version_mismatch" => {
            if expected_turn.is_some()
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
                ),
            )
        }
        "unknown_model_alias" => {
            if expected_turn.is_some()
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
                ),
            )
        }
        "acceptance_position_exhausted" => {
            if expected_turn.is_some()
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

fn require_all_absent(
    expected_turn: Option<Uuid>,
    expected_defaults: Option<Decimal>,
    current_defaults: Option<Decimal>,
    unknown_alias: Option<Uuid>,
    selected_defaults: Option<Decimal>,
    last_position: Option<Decimal>,
    relationship: &'static str,
) -> Result<(), SubmitInputRepositoryError> {
    if expected_turn.is_none()
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
