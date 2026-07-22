//! PostgreSQL transactions surrounding the first text-only model call.
//!
//! ADR-0045's three transaction roles stay explicit here: a durable
//! `Prepared` checkpoint, a separate send-authorization commit, and a fresh
//! post-effect observation commit. No method holds a database transaction
//! across provider work.

use std::{collections::BTreeSet, error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
use signalbox_domain::{
    AmbiguousModelCallTurn, AuthorizedModelCall, CompletedModelCallTurn, DirectModelSelection,
    FailedModelCallTurn, FailedModelCallTurnIdentities, FrozenAliasDefinition,
    FrozenModelSelection, ModelAlias, ModelCallDisposition, ModelCallExecution,
    ModelCallExecutionReconstitutionFailure, ModelCallExecutionReconstitutionInput, ModelCallId,
    ModelCallOriginContent, ModelCallPreparationFailure, ModelCallReconstitutionInput,
    ModelCallReconstitutionState, ModelCallTerminalIdentities, ModelCallTerminalObservation,
    ModelCallTerminalOutcome, ModelTargetCatalog, PreparedModelCallRequest, ProviderModelIdentity,
    RefusedModelCallTurn, ResolvedProviderTarget, SemanticTranscriptEntry,
    SemanticTranscriptEntryPayload, SessionId, TurnId,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    mapping::{session_id_to_uuid, turn_id_to_uuid},
    outbox::{self, ModelCallOutboxState, OutboxEvent},
    session::{SessionCorruption, SessionRepositoryError, load_session_from_connection},
    submit_input::{
        SubmitInputCorruption, SubmitInputRepositoryError, load_scheduling_projection,
        require_recorded_batch,
    },
};

/// Which fresh execution identity collided with an existing durable record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallIdentityCollision {
    /// The proposed model-call identity already exists.
    ModelCall,
    /// A proposed semantic-entry identity already exists.
    SemanticEntry,
    /// The proposed terminal-frontier identity already exists.
    TerminalFrontier,
}

impl fmt::Display for ModelCallIdentityCollision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let identity = match self {
            Self::ModelCall => "model-call",
            Self::SemanticEntry => "semantic-entry",
            Self::TerminalFrontier => "context-frontier",
        };
        write!(formatter, "{identity} identity already exists")
    }
}

impl Error for ModelCallIdentityCollision {}

/// A durable shape that cannot reconstruct the execution aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallCorruption {
    /// One required durable record or field is absent.
    Missing(&'static str),
    /// Stored records disagree about an exact relationship.
    Inconsistent(&'static str),
    /// A closed durable discriminator is unsupported.
    Unsupported {
        /// The field whose spelling is unsupported.
        field: &'static str,
        /// The exact durable spelling.
        value: String,
    },
    /// The current session projection is invalid.
    CurrentSession(SessionCorruption),
    /// Complete scheduling records are invalid.
    Scheduling(SubmitInputCorruption),
    /// Complete live facts fail domain reconstitution.
    Execution(ModelCallExecutionReconstitutionFailure),
}

impl fmt::Display for ModelCallCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(record) => write!(formatter, "missing model-call execution {record}"),
            Self::Inconsistent(relationship) => {
                write!(
                    formatter,
                    "inconsistent model-call execution {relationship}"
                )
            }
            Self::Unsupported { field, value } => {
                write!(
                    formatter,
                    "unsupported model-call execution {field}: {value}"
                )
            }
            Self::CurrentSession(error) => {
                write!(formatter, "model-call current Session is invalid: {error}")
            }
            Self::Scheduling(error) => {
                write!(
                    formatter,
                    "model-call scheduling projection is invalid: {error}"
                )
            }
            Self::Execution(failure) => {
                write!(
                    formatter,
                    "model-call execution reconstitution failed: {failure:?}"
                )
            }
        }
    }
}

impl Error for ModelCallCorruption {}

/// Database, integrity, identity, or caller failure at the execution boundary.
#[derive(Debug)]
pub enum ModelCallRepositoryError {
    /// PostgreSQL could not complete the operation.
    Database {
        /// The underlying SQLx failure.
        source: sqlx::Error,
        /// Whether failure occurred while awaiting commit.
        commit_ambiguous: bool,
    },
    /// Committed rows cannot form the accepted aggregate.
    Corruption(ModelCallCorruption),
    /// A fresh identity collided durably.
    IdentityCollision(ModelCallIdentityCollision),
    /// The application invoked an execution transition without a live turn.
    NoLiveExecution,
    /// A checked transition rejected an application-supplied operation.
    InvalidTransition(&'static str),
}

impl fmt::Display for ModelCallRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { source, .. } => {
                write!(formatter, "model-call database failure: {source}")
            }
            Self::Corruption(error) => error.fmt(formatter),
            Self::IdentityCollision(error) => error.fmt(formatter),
            Self::NoLiveExecution => formatter.write_str("no live model-call execution exists"),
            Self::InvalidTransition(operation) => {
                write!(formatter, "model-call transition rejected: {operation}")
            }
        }
    }
}

impl Error for ModelCallRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Corruption(error) => Some(error),
            Self::IdentityCollision(error) => Some(error),
            Self::NoLiveExecution | Self::InvalidTransition(_) => None,
        }
    }
}

impl ClassifyOperatorFailure for ModelCallRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database {
                commit_ambiguous, ..
            } => OperatorFailureClass::Infrastructure {
                commit_ambiguous: *commit_ambiguous,
            },
            Self::Corruption(_) => OperatorFailureClass::FailClosedCorruption,
            Self::IdentityCollision(_) => OperatorFailureClass::IdentityCollision,
            Self::NoLiveExecution | Self::InvalidTransition(_) => {
                OperatorFailureClass::CallerOrHubBug
            }
        }
    }
}

impl From<ModelCallCorruption> for ModelCallRepositoryError {
    fn from(error: ModelCallCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl From<sqlx::Error> for ModelCallRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::from_database(error, false)
    }
}

impl ModelCallRepositoryError {
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
}

/// Result of the load-and-prepare transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareInitialModelCallOutcome {
    /// A new exact Prepared call was committed; this invocation must stop.
    Checkpointed(ModelCallId),
    /// A previously committed Prepared request is safe for capability setup.
    Ready(Box<PreparedModelCallRequest>),
    /// Immutable target resolution failed and the turn closed atomically.
    TargetUnavailable(Box<FailedModelCallTurn>),
}

/// PostgreSQL adapter for the initial model-call execution transactions.
#[derive(Clone, Debug)]
pub struct PostgresModelCallRepository {
    pool: PgPool,
    targets: ModelTargetCatalog,
}

impl PostgresModelCallRepository {
    /// Uses the shared pool and immutable deployment target catalog.
    pub fn new(pool: PgPool, targets: ModelTargetCatalog) -> Self {
        Self { pool, targets }
    }

    /// Commits Prepared before returning any provider request material.
    pub async fn prepare_initial_call(
        &self,
        session: SessionId,
        call: ModelCallId,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> Result<PrepareInitialModelCallOutcome, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_session(&mut transaction, session).await?;
            let execution =
                require_live_execution(&mut transaction, session, &self.targets).await?;
            if execution.current_call().is_some() {
                let request = execution.resume_prepared_call().map_err(|_| {
                    ModelCallRepositoryError::InvalidTransition("existing call is not Prepared")
                })?;
                return Ok((
                    false,
                    PrepareInitialModelCallOutcome::Ready(Box::new(request)),
                ));
            }

            let prepared = match execution.prepare_initial_call(call) {
                Ok(prepared) => prepared,
                Err(error) if error.failure() == ModelCallPreparationFailure::TargetUnavailable => {
                    let resolution = error.target_resolution_error().ok_or(
                        ModelCallRepositoryError::InvalidTransition(
                            "target-unavailable result omitted its resolution proof",
                        ),
                    )?;
                    let failed = error
                        .execution()
                        .clone()
                        .fail_target_resolution(resolution, failure_identities)
                        .map_err(|_| {
                            ModelCallRepositoryError::InvalidTransition(
                                "target-resolution failure could not close fresh execution state",
                            )
                        })?;
                    persist_failed(&mut transaction, &failed).await?;
                    return Ok((
                        true,
                        PrepareInitialModelCallOutcome::TargetUnavailable(Box::new(failed)),
                    ));
                }
                Err(_) => {
                    return Err(ModelCallRepositoryError::InvalidTransition(
                        "initial call cannot be prepared",
                    ));
                }
            };
            insert_prepared_call(&mut transaction, &prepared).await?;
            let reloaded = require_exact_call(
                require_live_execution(&mut transaction, session, &self.targets).await?,
                call,
            )?;
            reloaded.resume_prepared_call().map_err(|_| {
                ModelCallCorruption::Inconsistent("committed Prepared call cannot resume")
            })?;
            Ok((true, PrepareInitialModelCallOutcome::Checkpointed(call)))
        }
        .await;

        finish_optional_commit(transaction, result).await
    }

    /// Atomically authorizes the exact Prepared call and attempt for send.
    pub async fn authorize_send(
        &self,
        session: SessionId,
        call: ModelCallId,
    ) -> Result<AuthorizedModelCall, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_session(&mut transaction, session).await?;
            let execution = require_exact_call(
                require_live_execution(&mut transaction, session, &self.targets).await?,
                call,
            )?;
            let authorized = execution.authorize_send().map_err(|_| {
                ModelCallRepositoryError::InvalidTransition("send authorization requires Prepared")
            })?;
            persist_authorization(&mut transaction, &authorized).await?;
            Ok(authorized)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Freshly reloads issued authority and commits one terminal observation.
    pub async fn apply_terminal_observation(
        &self,
        session: SessionId,
        call: ModelCallId,
        observation: ModelCallTerminalObservation,
        identities: ModelCallTerminalIdentities,
    ) -> Result<ModelCallTerminalOutcome, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_session(&mut transaction, session).await?;
            let execution = require_exact_call(
                require_live_execution(&mut transaction, session, &self.targets).await?,
                call,
            )?;
            let outcome = execution
                .apply_terminal_observation(observation, identities)
                .map_err(|_| {
                    ModelCallRepositoryError::InvalidTransition(
                        "terminal observation does not match fresh issued state",
                    )
                })?;
            persist_terminal_outcome(&mut transaction, &outcome).await?;
            Ok(outcome)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Atomically closes a trustworthy capability failure before send.
    pub async fn fail_prepared_call(
        &self,
        session: SessionId,
        call: ModelCallId,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_session(&mut transaction, session).await?;
            let execution = require_exact_call(
                require_live_execution(&mut transaction, session, &self.targets).await?,
                call,
            )?;
            let failed = execution.fail_prepared_call(identities).map_err(|_| {
                ModelCallRepositoryError::InvalidTransition(
                    "capability failure requires a Prepared call",
                )
            })?;
            persist_failed(&mut transaction, &failed).await?;
            Ok(failed)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Applies the accepted prior-process recovery rule to one live call.
    pub async fn recover_after_restart(
        &self,
        session: SessionId,
        call: ModelCallId,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<ModelCallTerminalOutcome, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_session(&mut transaction, session).await?;
            let execution = require_exact_call(
                require_live_execution(&mut transaction, session, &self.targets).await?,
                call,
            )?;
            let outcome = execution.recover_after_restart(identities).map_err(|_| {
                ModelCallRepositoryError::InvalidTransition(
                    "startup recovery requires a live Prepared or issued call",
                )
            })?;
            persist_terminal_outcome(&mut transaction, &outcome).await?;
            Ok(outcome)
        }
        .await;
        finish_commit(transaction, result).await
    }
}

async fn lock_session(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<(), ModelCallRepositoryError> {
    let (session_exists, scheduler): (bool, Option<Uuid>) =
        sqlx::query_as(crate::lock_inventory::MODEL_CALL_EXECUTION)
            .bind(session_id_to_uuid(session))
            .fetch_one(connection)
            .await?;
    match (session_exists, scheduler) {
        (true, Some(_)) => Ok(()),
        (true, None) => Err(ModelCallCorruption::Missing("session scheduler row").into()),
        (false, None) => Err(ModelCallRepositoryError::NoLiveExecution),
        (false, Some(_)) => Err(ModelCallCorruption::Inconsistent("orphan scheduler row").into()),
    }
}

async fn require_live_execution(
    connection: &mut PgConnection,
    requested_session: SessionId,
    targets: &ModelTargetCatalog,
) -> Result<ModelCallExecution, ModelCallRepositoryError> {
    let session = match load_session_from_connection(connection, requested_session).await {
        Ok(Some(session)) => session,
        Ok(None) => return Err(ModelCallRepositoryError::NoLiveExecution),
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(ModelCallCorruption::CurrentSession(error).into());
        }
    };
    let scheduling = load_scheduling_projection(connection, session)
        .await
        .map_err(map_scheduling_error)?;
    let active = scheduling
        .active_turn()
        .ok_or(ModelCallRepositoryError::NoLiveExecution)?;
    let active_turn = active
        .active_turn_execution()
        .ok_or(ModelCallCorruption::Inconsistent(
            "active execution witness",
        ))?;
    if !matches!(
        active_turn.phase(),
        signalbox_domain::ActiveTurnPhase::Running { .. }
    ) {
        return Err(ModelCallRepositoryError::NoLiveExecution);
    }
    let starting_snapshot = scheduling
        .resolved_snapshot(active_turn.start().frontier().snapshot())
        .cloned()
        .ok_or(ModelCallCorruption::Missing("starting snapshot"))?;
    let frontier_entries = starting_snapshot
        .ordered_entries()
        .map(|reference| {
            scheduling
                .semantic_entry(reference)
                .cloned()
                .ok_or(ModelCallCorruption::Missing("frontier semantic entry"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let origin_contents = load_origin_contents(connection, &frontier_entries).await?;
    let calls = load_live_turn_calls(connection, requested_session, active_turn.turn()).await?;

    ModelCallExecutionReconstitutionInput::new(
        active_turn,
        targets.clone(),
        starting_snapshot,
        frontier_entries,
        origin_contents,
        calls,
    )
    .reconstitute()
    .map_err(|error| {
        let (_, failure) = error.into_parts();
        ModelCallCorruption::Execution(failure).into()
    })
}

async fn load_origin_contents(
    connection: &mut PgConnection,
    entries: &[SemanticTranscriptEntry],
) -> Result<Vec<ModelCallOriginContent>, ModelCallRepositoryError> {
    let accepted_inputs = entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input } => {
                Some(accepted_input.into_uuid())
            }
            SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::AssistantText { .. }
            | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. } => None,
        })
        .collect::<Vec<_>>();
    if accepted_inputs.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT accepted_input_id, accepting_command_id
           FROM accepted_input
          WHERE accepted_input_id = ANY($1)
          ORDER BY accepted_input_id",
    )
    .bind(&accepted_inputs)
    .fetch_all(&mut *connection)
    .await?;
    if rows.len() != accepted_inputs.len() {
        return Err(ModelCallCorruption::Missing("origin accepted input receipt").into());
    }
    let mut loaded = BTreeSet::new();
    let mut commands = Vec::with_capacity(rows.len());
    for row in rows {
        let accepted: Uuid = required(&row, "accepted_input_id")?;
        if !accepted_inputs.contains(&accepted) || !loaded.insert(accepted) {
            return Err(ModelCallCorruption::Inconsistent("origin receipt inventory").into());
        }
        let command: Uuid = required(&row, "accepting_command_id")?;
        commands.push(
            crate::mapping::durable_command_id_from_uuid(command)
                .map_err(|_| ModelCallCorruption::Inconsistent("origin command identity"))?,
        );
    }
    let recorded = require_recorded_batch(connection, &commands)
        .await
        .map_err(map_scheduling_error)?;
    recorded
        .values()
        .map(|receipt| {
            ModelCallOriginContent::from_recorded_submit(receipt)
                .ok_or_else(|| ModelCallCorruption::Inconsistent("origin receipt result").into())
        })
        .collect()
}

async fn load_live_turn_calls(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Vec<ModelCallReconstitutionInput>, ModelCallRepositoryError> {
    let rows = sqlx::query(
        "SELECT model_call_id, turn_id, turn_attempt_id,
                selection_kind, direct_model_selection_id,
                frozen_model_alias_id, frozen_alias_selected_direct_id,
                resolved_provider_model_identity_id, context_frontier_id,
                state_kind, terminal_disposition_kind
           FROM model_call
          WHERE session_id = $1
            AND turn_id = $2
          ORDER BY model_call_id",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_all(&mut *connection)
    .await?;
    rows.into_iter().map(decode_model_call).collect()
}

fn decode_model_call(row: PgRow) -> Result<ModelCallReconstitutionInput, ModelCallRepositoryError> {
    let state_kind: String = required(&row, "state_kind")?;
    let terminal: Option<String> = row.try_get("terminal_disposition_kind")?;
    let state = match (state_kind.as_str(), terminal.as_deref()) {
        ("prepared", None) => ModelCallReconstitutionState::Prepared,
        ("in_flight", None) => ModelCallReconstitutionState::InFlight,
        ("cancellation_requested", None) => ModelCallReconstitutionState::CancellationRequested,
        ("terminal", Some(value)) => {
            ModelCallReconstitutionState::Terminal(decode_disposition(value)?)
        }
        ("prepared" | "in_flight" | "cancellation_requested" | "terminal", _) => {
            return Err(ModelCallCorruption::Inconsistent("model-call state payload").into());
        }
        (value, _) => {
            return Err(ModelCallCorruption::Unsupported {
                field: "model_call.state_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    Ok(ModelCallReconstitutionInput::new(
        ModelCallId::from_uuid(required(&row, "model_call_id")?),
        TurnId::from_uuid(required(&row, "turn_id")?),
        signalbox_domain::TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?),
        decode_selection(
            required(&row, "selection_kind")?,
            row.try_get("direct_model_selection_id")?,
            row.try_get("frozen_model_alias_id")?,
            row.try_get("frozen_alias_selected_direct_id")?,
        )?,
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(required(
            &row,
            "resolved_provider_model_identity_id",
        )?)),
        signalbox_domain::ContextFrontierId::from_uuid(required(&row, "context_frontier_id")?),
        state,
    ))
}

fn decode_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    alias_selected: Option<Uuid>,
) -> Result<FrozenModelSelection, ModelCallRepositoryError> {
    match (kind.as_str(), direct, alias, alias_selected) {
        ("direct", Some(direct), None, None) => Ok(FrozenModelSelection::Direct(
            DirectModelSelection::from_uuid(direct),
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
            Err(ModelCallCorruption::Inconsistent("frozen selection payload").into())
        }
        (value, _, _, _) => Err(ModelCallCorruption::Unsupported {
            field: "model_call.selection_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_disposition(value: &str) -> Result<ModelCallDisposition, ModelCallRepositoryError> {
    match value {
        "completed" => Ok(ModelCallDisposition::Completed),
        "known_failed" => Ok(ModelCallDisposition::KnownFailed),
        "refused" => Ok(ModelCallDisposition::Refused),
        "cancelled" => Ok(ModelCallDisposition::Cancelled),
        "ambiguous" => Ok(ModelCallDisposition::Ambiguous),
        value => Err(ModelCallCorruption::Unsupported {
            field: "model_call.terminal_disposition_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn require_exact_call(
    execution: ModelCallExecution,
    call: ModelCallId,
) -> Result<ModelCallExecution, ModelCallRepositoryError> {
    if matches!(execution.current_call(), Some(current) if current.id() == call) {
        Ok(execution)
    } else {
        Err(ModelCallRepositoryError::InvalidTransition(
            "fresh execution does not contain the expected call",
        ))
    }
}

async fn insert_prepared_call(
    connection: &mut PgConnection,
    prepared: &signalbox_domain::PreparedInitialModelCall,
) -> Result<(), ModelCallRepositoryError> {
    let call = prepared.call();
    let (kind, direct, alias, alias_selected) = encode_selection(call.selection());
    sqlx::query(
        "INSERT INTO model_call
            (model_call_id, turn_id, session_id, turn_attempt_id,
             selection_kind, direct_model_selection_id, frozen_model_alias_id,
             frozen_alias_selected_direct_id, resolved_provider_model_identity_id,
             context_frontier_id, state_kind, terminal_disposition_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'prepared', NULL)",
    )
    .bind(call.id().into_uuid())
    .bind(turn_id_to_uuid(prepared.turn()))
    .bind(session_id_to_uuid(prepared.session()))
    .bind(prepared.attempt().into_uuid())
    .bind(kind)
    .bind(direct)
    .bind(alias)
    .bind(alias_selected)
    .bind(call.target().identity().into_uuid())
    .bind(call.frontier().snapshot().into_uuid())
    .execute(&mut *connection)
    .await?;
    outbox::append(
        connection,
        OutboxEvent::ModelCallTransition {
            session: prepared.session(),
            turn: prepared.turn(),
            call: call.id(),
            state: ModelCallOutboxState::Prepared,
        },
    )
    .await?;
    Ok(())
}

async fn persist_authorization(
    connection: &mut PgConnection,
    authorized: &AuthorizedModelCall,
) -> Result<(), ModelCallRepositoryError> {
    let attempt_rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'running'
          WHERE turn_attempt_id = $1
            AND turn_id = $2
            AND session_id = $3
            AND state_kind = 'prepared'
            AND end_variant IS NULL
            AND end_disposition IS NULL",
    )
    .bind(authorized.attempt().id().into_uuid())
    .bind(turn_id_to_uuid(authorized.turn()))
    .bind(session_id_to_uuid(authorized.session()))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    let call_rows = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'in_flight'
          WHERE model_call_id = $1
            AND turn_id = $2
            AND session_id = $3
            AND turn_attempt_id = $4
            AND state_kind = 'prepared'
            AND terminal_disposition_kind IS NULL",
    )
    .bind(authorized.call().id().into_uuid())
    .bind(turn_id_to_uuid(authorized.turn()))
    .bind(session_id_to_uuid(authorized.session()))
    .bind(authorized.attempt().id().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(attempt_rows, "send-authorization attempt")?;
    require_single(call_rows, "send-authorization call")?;
    outbox::append(
        connection,
        OutboxEvent::ModelCallTransition {
            session: authorized.session(),
            turn: authorized.turn(),
            call: authorized.call().id(),
            state: ModelCallOutboxState::InFlight,
        },
    )
    .await?;
    Ok(())
}

async fn persist_terminal_outcome(
    connection: &mut PgConnection,
    outcome: &ModelCallTerminalOutcome,
) -> Result<(), ModelCallRepositoryError> {
    match outcome {
        ModelCallTerminalOutcome::Completed(completed) => {
            persist_completed(connection, completed).await
        }
        ModelCallTerminalOutcome::Failed(failed) => persist_failed(connection, failed).await,
        ModelCallTerminalOutcome::Refused(refused) => persist_refused(connection, refused).await,
        ModelCallTerminalOutcome::AwaitingRecovery(ambiguous) => {
            persist_ambiguous(connection, ambiguous).await
        }
    }
}

async fn persist_completed(
    connection: &mut PgConnection,
    completed: &CompletedModelCallTurn,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call(
        connection,
        completed.session(),
        completed.turn(),
        completed.call(),
    )
    .await?;
    persist_ended_attempt(
        connection,
        completed.session(),
        completed.turn(),
        completed.attempt(),
    )
    .await?;
    for entry in completed.assistant_entries() {
        let SemanticTranscriptEntryPayload::AssistantText {
            producing_call,
            value,
        } = entry.payload()
        else {
            return Err(ModelCallCorruption::Inconsistent("completed assistant payload").into());
        };
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 assistant_text_value, producing_model_call_id)
             VALUES ($1, $2, 'assistant_text', $3, $4)",
        )
        .bind(session_id_to_uuid(entry.source_session()))
        .bind(entry.identity().into_uuid())
        .bind(value.as_str())
        .bind(producing_call.into_uuid())
        .execute(&mut *connection)
        .await?;
    }
    let completion = completed.completion_entry();
    if !matches!(
        completion.payload(),
        SemanticTranscriptEntryPayload::TurnCompleted { turn } if *turn == completed.turn()
    ) {
        return Err(ModelCallCorruption::Inconsistent("completion entry payload").into());
    }
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind, completed_turn_id)
         VALUES ($1, $2, 'turn_completed', $3)",
    )
    .bind(session_id_to_uuid(completion.source_session()))
    .bind(completion.identity().into_uuid())
    .bind(turn_id_to_uuid(completed.turn()))
    .execute(&mut *connection)
    .await?;
    insert_snapshot(connection, completed.terminal_snapshot()).await?;
    terminalize_lifecycle(
        connection,
        completed.session(),
        completed.turn(),
        "completed",
        completed.terminal_snapshot().frontier().snapshot(),
        Some(completed.attempt().id()),
        Some(completed.call().id()),
    )
    .await?;
    append_terminal_call_event(
        connection,
        completed.session(),
        completed.turn(),
        completed.call(),
    )
    .await?;
    outbox::append(
        connection,
        OutboxEvent::TurnCompleted {
            session: completed.session(),
            turn: completed.turn(),
            call: completed.call().id(),
            completion_entry: completion.identity(),
            terminal_frontier: completed.terminal_snapshot().frontier().snapshot(),
        },
    )
    .await?;
    Ok(())
}

async fn persist_failed(
    connection: &mut PgConnection,
    failed: &FailedModelCallTurn,
) -> Result<(), ModelCallRepositoryError> {
    if let Some(call) = failed.call() {
        persist_ended_call(connection, failed.session(), failed.turn(), call).await?;
    }
    persist_ended_attempt(
        connection,
        failed.session(),
        failed.turn(),
        failed.attempt(),
    )
    .await?;
    let entry = failed.failure_entry();
    if !matches!(
        entry.payload(),
        SemanticTranscriptEntryPayload::TurnFailed { turn } if *turn == failed.turn()
    ) {
        return Err(ModelCallCorruption::Inconsistent("failure entry payload").into());
    }
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', $3)",
    )
    .bind(session_id_to_uuid(entry.source_session()))
    .bind(entry.identity().into_uuid())
    .bind(turn_id_to_uuid(failed.turn()))
    .execute(&mut *connection)
    .await?;
    insert_snapshot(connection, failed.terminal_snapshot()).await?;
    terminalize_lifecycle(
        connection,
        failed.session(),
        failed.turn(),
        "failed",
        failed.terminal_snapshot().frontier().snapshot(),
        None,
        None,
    )
    .await?;
    if let Some(call) = failed.call() {
        append_terminal_call_event(connection, failed.session(), failed.turn(), call).await?;
    }
    outbox::append(
        connection,
        OutboxEvent::TurnFailed {
            session: failed.session(),
            turn: failed.turn(),
            failure_entry: entry.identity(),
            terminal_frontier: failed.terminal_snapshot().frontier().snapshot(),
        },
    )
    .await?;
    Ok(())
}

async fn persist_refused(
    connection: &mut PgConnection,
    refused: &RefusedModelCallTurn,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call(
        connection,
        refused.session(),
        refused.turn(),
        refused.call(),
    )
    .await?;
    persist_ended_attempt(
        connection,
        refused.session(),
        refused.turn(),
        refused.attempt(),
    )
    .await?;
    insert_snapshot(connection, refused.terminal_snapshot()).await?;
    terminalize_lifecycle(
        connection,
        refused.session(),
        refused.turn(),
        "refused",
        refused.terminal_snapshot().frontier().snapshot(),
        Some(refused.attempt().id()),
        Some(refused.call().id()),
    )
    .await?;
    append_terminal_call_event(
        connection,
        refused.session(),
        refused.turn(),
        refused.call(),
    )
    .await?;
    outbox::append(
        connection,
        OutboxEvent::TurnRefused {
            session: refused.session(),
            turn: refused.turn(),
            call: refused.call().id(),
            terminal_frontier: refused.terminal_snapshot().frontier().snapshot(),
        },
    )
    .await?;
    Ok(())
}

async fn persist_ambiguous(
    connection: &mut PgConnection,
    ambiguous: &AmbiguousModelCallTurn,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call(
        connection,
        ambiguous.session(),
        ambiguous.turn(),
        ambiguous.call(),
    )
    .await?;
    persist_ended_attempt(
        connection,
        ambiguous.session(),
        ambiguous.turn(),
        ambiguous.attempt(),
    )
    .await?;
    let rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_model_call_recovery',
                recovery_model_call_id = $1
          WHERE turn_id = $2
            AND session_id = $3
            AND state_kind = 'active'
            AND active_phase_kind = 'running'
            AND current_attempt_id = $4",
    )
    .bind(ambiguous.call().id().into_uuid())
    .bind(turn_id_to_uuid(ambiguous.turn()))
    .bind(session_id_to_uuid(ambiguous.session()))
    .bind(ambiguous.attempt().id().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "ambiguous recovery lifecycle")?;
    append_terminal_call_event(
        connection,
        ambiguous.session(),
        ambiguous.turn(),
        ambiguous.call(),
    )
    .await?;
    Ok(())
}

async fn persist_ended_call(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: &signalbox_domain::EndedModelCall,
) -> Result<(), ModelCallRepositoryError> {
    let rows = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'terminal',
                terminal_disposition_kind = $1
          WHERE model_call_id = $2
            AND turn_id = $3
            AND session_id = $4
            AND turn_attempt_id = $5
            AND state_kind <> 'terminal'
            AND terminal_disposition_kind IS NULL",
    )
    .bind(encode_disposition(call.disposition()))
    .bind(call.id().into_uuid())
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .bind(call.attempt().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "terminal model call")
}

async fn persist_ended_attempt(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    attempt: &signalbox_domain::EndedTurnAttempt,
) -> Result<(), ModelCallRepositoryError> {
    let (variant, disposition) = encode_attempt_end(attempt.end())?;
    let rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = $1,
                end_disposition = $2
          WHERE turn_attempt_id = $3
            AND turn_id = $4
            AND session_id = $5
            AND state_kind IN ('prepared', 'running')
            AND end_variant IS NULL
            AND end_disposition IS NULL",
    )
    .bind(variant)
    .bind(disposition)
    .bind(attempt.id().into_uuid())
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "terminal model-call attempt")
}

fn encode_attempt_end(
    end: &signalbox_domain::AttemptEnd,
) -> Result<(&'static str, &'static str), ModelCallRepositoryError> {
    let signalbox_domain::AttemptEnd::WithoutStop { disposition } = end else {
        return Err(ModelCallRepositoryError::InvalidTransition(
            "initial model execution produced a stop-caused attempt end",
        ));
    };
    let disposition = match disposition {
        signalbox_domain::UnstoppedAttemptDisposition::TurnCompleted => "turn_completed",
        signalbox_domain::UnstoppedAttemptDisposition::TurnRefused => "turn_refused",
        signalbox_domain::UnstoppedAttemptDisposition::YieldedToDurableWait => {
            "yielded_to_durable_wait"
        }
        signalbox_domain::UnstoppedAttemptDisposition::KnownFailure => "known_failure",
        signalbox_domain::UnstoppedAttemptDisposition::Lost => "lost",
        signalbox_domain::UnstoppedAttemptDisposition::Ambiguous => "ambiguous",
    };
    Ok(("without_stop", disposition))
}

async fn insert_snapshot(
    connection: &mut PgConnection,
    snapshot: &signalbox_domain::ResolvedContextFrontierSnapshot,
) -> Result<(), ModelCallRepositoryError> {
    let member_count = u64::try_from(snapshot.entry_count())
        .map_err(|_| ModelCallCorruption::Inconsistent("frontier member count"))?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, $3)",
    )
    .bind(session_id_to_uuid(snapshot.frontier().owning_session()))
    .bind(snapshot.frontier().snapshot().into_uuid())
    .bind(Decimal::from(member_count))
    .execute(&mut *connection)
    .await?;
    for (index, entry) in snapshot.ordered_entries().enumerate() {
        let position = u64::try_from(index + 1)
            .map_err(|_| ModelCallCorruption::Inconsistent("frontier member position"))?;
        sqlx::query(
            "INSERT INTO context_frontier_member
                (owning_session_id, context_frontier_id, member_position,
                 source_session_id, semantic_entry_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(session_id_to_uuid(snapshot.frontier().owning_session()))
        .bind(snapshot.frontier().snapshot().into_uuid())
        .bind(Decimal::from(position))
        .bind(session_id_to_uuid(entry.source_session()))
        .bind(entry.entry().into_uuid())
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

async fn terminalize_lifecycle(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    disposition: &'static str,
    terminal_frontier: signalbox_domain::ContextFrontierId,
    terminal_attempt: Option<signalbox_domain::TurnAttemptId>,
    terminal_call: Option<ModelCallId>,
) -> Result<(), ModelCallRepositoryError> {
    let rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = $1,
                active_phase_kind = NULL,
                current_attempt_id = NULL,
                recovery_model_call_id = NULL,
                terminal_attempt_id = $2,
                terminal_model_call_id = $3,
                terminal_disposition_kind = $4
          WHERE turn_id = $5
            AND session_id = $6
            AND state_kind = 'active'
            AND active_phase_kind = 'running'",
    )
    .bind(terminal_frontier.into_uuid())
    .bind(terminal_attempt.map(signalbox_domain::TurnAttemptId::into_uuid))
    .bind(terminal_call.map(ModelCallId::into_uuid))
    .bind(disposition)
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "terminal model-call lifecycle")
}

async fn append_terminal_call_event(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: &signalbox_domain::EndedModelCall,
) -> Result<(), ModelCallRepositoryError> {
    outbox::append(
        connection,
        OutboxEvent::ModelCallTransition {
            session,
            turn,
            call: call.id(),
            state: ModelCallOutboxState::Terminal(call.disposition()),
        },
    )
    .await?;
    Ok(())
}

fn encode_selection(
    selection: FrozenModelSelection,
) -> (&'static str, Option<Uuid>, Option<Uuid>, Option<Uuid>) {
    match selection {
        FrozenModelSelection::Direct(direct) => ("direct", Some(direct.into_uuid()), None, None),
        FrozenModelSelection::FrozenAlias { alias, definition } => (
            "frozen_alias",
            None,
            Some(alias.into_uuid()),
            Some(definition.selected().into_uuid()),
        ),
    }
}

fn encode_disposition(disposition: ModelCallDisposition) -> &'static str {
    match disposition {
        ModelCallDisposition::Completed => "completed",
        ModelCallDisposition::KnownFailed => "known_failed",
        ModelCallDisposition::Refused => "refused",
        ModelCallDisposition::Cancelled => "cancelled",
        ModelCallDisposition::Ambiguous => "ambiguous",
    }
}

fn require_single(rows: u64, relationship: &'static str) -> Result<(), ModelCallRepositoryError> {
    if rows == 1 {
        Ok(())
    } else {
        Err(ModelCallCorruption::Inconsistent(relationship).into())
    }
}

async fn finish_commit<T>(
    transaction: sqlx::Transaction<'_, sqlx::Postgres>,
    result: Result<T, ModelCallRepositoryError>,
) -> Result<T, ModelCallRepositoryError> {
    match result {
        Ok(value) => {
            transaction.commit().await.map_err(|error| {
                let commit_ambiguous = commit_failure_is_ambiguous(&error);
                ModelCallRepositoryError::from_database(error, commit_ambiguous)
            })?;
            Ok(value)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

async fn finish_optional_commit<T>(
    transaction: sqlx::Transaction<'_, sqlx::Postgres>,
    result: Result<(bool, T), ModelCallRepositoryError>,
) -> Result<T, ModelCallRepositoryError> {
    match result {
        Ok((true, value)) => {
            transaction.commit().await.map_err(|error| {
                let commit_ambiguous = commit_failure_is_ambiguous(&error);
                ModelCallRepositoryError::from_database(error, commit_ambiguous)
            })?;
            Ok(value)
        }
        Ok((false, value)) => {
            transaction.rollback().await?;
            Ok(value)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

fn map_scheduling_error(error: SubmitInputRepositoryError) -> ModelCallRepositoryError {
    match error {
        SubmitInputRepositoryError::Database(error) => error.into(),
        SubmitInputRepositoryError::Corruption(error) => {
            ModelCallCorruption::Scheduling(error).into()
        }
        SubmitInputRepositoryError::DifferentCommandKind { .. } => {
            ModelCallCorruption::Inconsistent("origin command kind").into()
        }
        SubmitInputRepositoryError::AcceptedInputIdentityCollision { .. } => {
            ModelCallCorruption::Inconsistent("origin accepted-input identity").into()
        }
        SubmitInputRepositoryError::InterruptApplicationUnavailable { .. } => {
            ModelCallCorruption::Inconsistent("origin command application").into()
        }
    }
}

fn identity_collision(error: &sqlx::Error) -> Option<ModelCallIdentityCollision> {
    match error
        .as_database_error()
        .and_then(|database| database.constraint())
    {
        Some("model_call_pkey") => Some(ModelCallIdentityCollision::ModelCall),
        Some("semantic_transcript_entry_pk" | "semantic_transcript_entry_id_global") => {
            Some(ModelCallIdentityCollision::SemanticEntry)
        }
        Some("context_frontier_pk" | "context_frontier_id_global") => {
            Some(ModelCallIdentityCollision::TerminalFrontier)
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

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, ModelCallRepositoryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| ModelCallCorruption::Missing(field).into())
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, error::Error, fmt, io};

    use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
    use sqlx::error::{DatabaseError, ErrorKind};

    use super::{ModelCallRepositoryError, commit_failure_is_ambiguous};

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
    fn lost_commit_response_is_commit_ambiguous() {
        let error = sqlx::Error::Io(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "commit response was lost",
        ));
        let commit_ambiguous = commit_failure_is_ambiguous(&error);

        assert!(commit_ambiguous);
        assert_eq!(
            ModelCallRepositoryError::from_database(error, commit_ambiguous)
                .operator_failure_class(),
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
        assert_eq!(
            ModelCallRepositoryError::from_database(error, commit_ambiguous)
                .operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false
            }
        );
    }

    #[test]
    fn server_reported_unknown_commit_outcomes_are_ambiguous() {
        for code in ["08007", "40003"] {
            let error = sqlx::Error::Database(Box::new(ServerCommitFailure { code }));
            assert!(commit_failure_is_ambiguous(&error));
        }
    }
}
