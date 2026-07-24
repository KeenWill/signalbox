//! PostgreSQL transactions for durable tool approval and execution.
//!
//! Every mutating method reloads the complete batch under the session's
//! scheduler lock before asking the domain aggregate for authority. Executor
//! work remains outside database transactions.

use std::{collections::BTreeMap, error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{
    ClassifyOperatorFailure, DecideToolRequestTransaction, ModelCallCredentialReference,
    OperatorFailureClass, PrepareToolContinuationOutcome, RetainedToolAttemptObservationStatus,
    ToolAttemptAuthorizationStatus, ToolContinuationIdentities, ToolCrashClosureIdentities,
    ToolExecutionTransaction,
};
use signalbox_domain::{
    ActiveTurnPhase, AuthorizedToolAttempt, CorrelatedToolAttemptObservation, CurrentToolAttempt,
    CurrentToolAttemptState, DecideToolRequest, DecideToolRequestRejectedResult,
    DecideToolRequestResult, EndedToolAttempt, NormalizedToolArguments, PreparedDecideToolRequest,
    PreparedToolBatchDecision, PreparedToolResultProjection, ReconstitutedToolAttempt,
    ResolvedContextFrontierReconstitutionInput, ResolvedContextFrontierSnapshot,
    SemanticTranscriptEntryPayload, SessionId, ToolApprovalDecision,
    ToolApprovalResolutionReconstitutionInput, ToolArgumentsKind, ToolAttemptEnd, ToolAttemptId,
    ToolAttemptObservation, ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState,
    ToolBatch, ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionFailure,
    ToolBatchReconstitutionInput, ToolDecisionSource, ToolDenialReason, ToolDispatchGeneration,
    ToolEffectClass, ToolExecutionError, ToolExecutionErrorDetail, ToolExecutionErrorKind,
    ToolName, ToolRequestId, ToolRequestOrdinal, ToolRequestReconstitutionInput, ToolResultContent,
    ToolResultText, TurnId,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    command_registry::{
        self, CommandKind, DECIDE_TOOL_REQUEST_KIND, RegistryCorruption, RegistryInspectionError,
    },
    mapping::{
        durable_command_id_to_uuid, session_id_from_uuid, session_id_to_uuid,
        tool_attempt_id_from_uuid, tool_attempt_id_to_uuid, tool_request_id_from_uuid,
        tool_request_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid,
    },
    model_execution::{insert_prepared_call, insert_snapshot},
};

const STORAGE_VERSION: i16 = 1;

/// Stored tool-loop facts failed checked domain reconstruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolLoopCorruption {
    /// A required row or value is absent.
    Missing(&'static str),
    /// Stored facts disagree about an exact relationship.
    Inconsistent(&'static str),
    /// A closed discriminator has an unknown spelling.
    Unsupported {
        /// Storage field.
        field: &'static str,
        /// Unsupported value.
        value: String,
    },
    /// Complete batch facts failed aggregate reconstruction.
    Batch(ToolBatchReconstitutionFailure),
}

impl fmt::Display for ToolLoopCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(value) => write!(formatter, "missing tool-loop {value}"),
            Self::Inconsistent(value) => write!(formatter, "inconsistent tool-loop {value}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported tool-loop {field}: {value}")
            }
            Self::Batch(failure) => {
                write!(formatter, "tool batch reconstitution failed: {failure:?}")
            }
        }
    }
}

impl Error for ToolLoopCorruption {}

/// Database, replay, corruption, or rejected transition at the tool boundary.
#[derive(Debug)]
pub enum ToolLoopRepositoryError {
    /// PostgreSQL failure.
    Database {
        /// Original driver error.
        source: sqlx::Error,
        /// Whether a failed commit acknowledgement leaves outcome unknown.
        commit_ambiguous: bool,
    },
    /// A fresh application-owned identity collided with durable state.
    IdentityCollision,
    /// Durable facts failed closed reconstruction.
    Corruption(ToolLoopCorruption),
    /// The command identity belongs to another durable command kind.
    DifferentCommandKind,
    /// Caller supplied a transition the current batch does not authorize.
    InvalidTransition(&'static str),
}

impl fmt::Display for ToolLoopRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { source, .. } => {
                write!(formatter, "tool-loop database failure: {source}")
            }
            Self::IdentityCollision => {
                formatter.write_str("tool-loop identity candidate already exists")
            }
            Self::Corruption(error) => error.fmt(formatter),
            Self::DifferentCommandKind => {
                formatter.write_str("command identity already belongs to another kind")
            }
            Self::InvalidTransition(value) => {
                write!(formatter, "tool-loop transition rejected: {value}")
            }
        }
    }
}

impl Error for ToolLoopRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Corruption(error) => Some(error),
            Self::IdentityCollision | Self::DifferentCommandKind | Self::InvalidTransition(_) => {
                None
            }
        }
    }
}

impl From<sqlx::Error> for ToolLoopRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        if error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref()
            == Some("23505")
        {
            Self::IdentityCollision
        } else {
            Self::Database {
                source: error,
                commit_ambiguous: false,
            }
        }
    }
}

impl From<ToolLoopCorruption> for ToolLoopRepositoryError {
    fn from(error: ToolLoopCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl ClassifyOperatorFailure for ToolLoopRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database {
                commit_ambiguous, ..
            } => OperatorFailureClass::Infrastructure {
                commit_ambiguous: *commit_ambiguous,
            },
            Self::IdentityCollision => OperatorFailureClass::IdentityCollision,
            Self::Corruption(_) => OperatorFailureClass::FailClosedCorruption,
            Self::DifferentCommandKind | Self::InvalidTransition(_) => {
                OperatorFailureClass::CallerOrHubBug
            }
        }
    }
}

/// PostgreSQL adapter for serialized tool-loop transactions.
#[derive(Clone, Debug)]
pub struct PostgresToolLoopRepository {
    pool: PgPool,
    continuation_targets: Option<signalbox_domain::ModelTargetCatalog>,
    continuation_credential: Option<ModelCallCredentialReference>,
}

impl PostgresToolLoopRepository {
    /// Uses the shared production pool.
    pub const fn new(pool: PgPool) -> Self {
        Self {
            pool,
            continuation_targets: None,
            continuation_credential: None,
        }
    }

    /// Uses the shared pool plus immutable model-call configuration required
    /// by atomic tool-result continuation.
    pub fn with_model_calls(
        pool: PgPool,
        targets: signalbox_domain::ModelTargetCatalog,
        credential_reference: ModelCallCredentialReference,
    ) -> Self {
        Self {
            pool,
            continuation_targets: Some(targets),
            continuation_credential: Some(credential_reference),
        }
    }

    /// Reloads the active logical batch without granting mutation authority.
    pub async fn load_active_batch(
        &self,
        session: SessionId,
        turn: TurnId,
    ) -> Result<Option<ToolBatch>, ToolLoopRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_tool_session(&mut transaction, session).await?;
        let result = load_active_batch_from_connection(&mut transaction, session, turn).await;
        transaction.rollback().await?;
        result
    }

    /// Finds the exact active tool turn whose durable phase can make progress.
    ///
    /// This is a reconciliation hint only. Every later tool transaction
    /// rechecks the complete batch under the session scheduler lock.
    pub async fn find_resumable_turn(
        &self,
        session: SessionId,
    ) -> Result<Option<TurnId>, ToolLoopRepositoryError> {
        let turn = sqlx::query_scalar::<_, Uuid>(
            "SELECT turn_id
               FROM turn_lifecycle
              WHERE session_id = $1
                AND state_kind = 'active'
                AND active_phase_kind = 'running'
                AND active_tool_round_call_id IS NOT NULL",
        )
        .bind(session_id_to_uuid(session))
        .fetch_optional(&self.pool)
        .await?;
        Ok(turn.map(turn_id_from_uuid))
    }

    /// Atomically records one replay-idempotent owner decision and successor
    /// phase. A fresh continuation attempt is supplied only for the final
    /// undecided request.
    pub async fn decide<NextAttempt>(
        &self,
        command: DecideToolRequest,
        mut next_attempt: NextAttempt,
    ) -> Result<PreparedDecideToolRequest, ToolLoopRepositoryError>
    where
        NextAttempt: FnMut() -> signalbox_domain::TurnAttemptId,
    {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            if let Some(kind) = inspect_registry(&mut transaction, command.command_id()).await? {
                if kind != CommandKind::DecideToolRequest {
                    return Err(ToolLoopRepositoryError::DifferentCommandKind);
                }
                let receipt = load_decision_receipt(&mut transaction, command.command_id())
                    .await?
                    .ok_or(ToolLoopCorruption::Missing("decision command receipt"))?;
                if receipt.command() != &command {
                    return Err(ToolLoopRepositoryError::InvalidTransition(
                        "command replay payload differs from the durable command",
                    ));
                }
                return Ok(receipt);
            }
            let claimed = sqlx::query(
                "INSERT INTO durable_command
                    (command_id, command_kind, storage_version, claimed_at)
                 VALUES ($1, $2, $3, transaction_timestamp())
                 ON CONFLICT DO NOTHING",
            )
            .bind(durable_command_id_to_uuid(command.command_id()))
            .bind(DECIDE_TOOL_REQUEST_KIND)
            .bind(STORAGE_VERSION)
            .execute(&mut *transaction)
            .await?
            .rows_affected()
                == 1;
            if !claimed {
                let kind = inspect_registry(&mut transaction, command.command_id())
                    .await?
                    .ok_or(ToolLoopCorruption::Missing("winner command claim"))?;
                if kind != CommandKind::DecideToolRequest {
                    return Err(ToolLoopRepositoryError::DifferentCommandKind);
                }
                let receipt = load_decision_receipt(&mut transaction, command.command_id())
                    .await?
                    .ok_or(ToolLoopCorruption::Missing("winner decision receipt"))?;
                if receipt.command() != &command {
                    return Err(ToolLoopRepositoryError::InvalidTransition(
                        "command replay payload differs from the durable command",
                    ));
                }
                return Ok(receipt);
            }

            let ownership = sqlx::query_as::<_, (Uuid, Uuid)>(
                "SELECT session_id, turn_id
                   FROM tool_request
                  WHERE request_id = $1",
            )
            .bind(tool_request_id_to_uuid(command.request()))
            .fetch_optional(&mut *transaction)
            .await?;
            let prepared = match ownership {
                None => command.prepare_request_not_found(),
                Some((stored_session, stored_turn)) => {
                    let session = session_id_from_uuid(stored_session);
                    let turn = turn_id_from_uuid(stored_turn);
                    lock_tool_session(&mut transaction, session).await?;
                    if decision_exists(&mut transaction, command.request()).await? {
                        let prepared = command.prepare_already_resolved();
                        persist_decision_command(&mut transaction, &prepared).await?;
                        return Ok(prepared);
                    }
                    if request_closed_by_turn_end(&mut transaction, command.request()).await? {
                        let prepared = command.prepare_already_resolved();
                        persist_decision_command(&mut transaction, &prepared).await?;
                        return Ok(prepared);
                    }
                    let batch = load_active_batch_from_connection(&mut transaction, session, turn)
                        .await?
                        .ok_or(ToolLoopCorruption::Missing("active tool batch"))?;
                    let continuation_attempt = batch
                        .awaiting_approval()
                        .filter(|waiting| waiting.request() == command.request())
                        .filter(|_| {
                            batch
                                .requests()
                                .iter()
                                .filter(|request| batch.approval(request.id()).is_none())
                                .count()
                                == 1
                        })
                        .map(|_| next_attempt());
                    let decision = batch
                        .prepare_owner_decision(command, continuation_attempt)
                        .map_err(|_| {
                            ToolLoopRepositoryError::InvalidTransition(
                                "owner decision does not match active batch",
                            )
                        })?;
                    persist_batch_decision(&mut transaction, &decision).await?;
                    return Ok(decision.prepared_command().clone());
                }
            };
            persist_decision_command(&mut transaction, &prepared).await?;
            Ok(prepared)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Atomically prepares the next proposal-order approved attempt.
    pub async fn prepare_next_attempt(
        &self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        effect_class: ToolEffectClass,
    ) -> Result<CurrentToolAttempt, ToolLoopRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_tool_session(&mut transaction, session).await?;
            let batch = load_active_batch_from_connection(&mut transaction, session, turn)
                .await?
                .ok_or(ToolLoopCorruption::Missing("active tool batch"))?;
            let prepared = batch
                .prepare_next_attempt(attempt, effect_class)
                .map_err(|_| {
                    ToolLoopRepositoryError::InvalidTransition(
                        "batch has no next serialized attempt",
                    )
                })?
                .into_attempt();
            insert_prepared_attempt(&mut transaction, &prepared).await?;
            Ok(prepared)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Atomically authorizes one exact prepared attempt, returning the fence
    /// that must accompany executor evidence.
    pub async fn authorize_attempt(
        &self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
    ) -> Result<AuthorizedToolAttempt, ToolLoopRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_tool_session(&mut transaction, session).await?;
            let current = load_current_attempt(&mut transaction, attempt)
                .await?
                .ok_or(ToolLoopCorruption::Missing("prepared tool attempt"))?;
            if current.session() != session || current.turn() != turn {
                return Err(ToolLoopCorruption::Inconsistent("attempt ownership").into());
            }
            let authorized = current.authorize().map_err(|_| {
                ToolLoopRepositoryError::InvalidTransition("tool attempt is not prepared")
            })?;
            mark_issuing_turn_attempt_running(&mut transaction, authorized.attempt()).await?;
            let rows = sqlx::query(
                "UPDATE tool_attempt
                    SET state_kind = 'in_flight'
                  WHERE attempt_id = $1
                    AND request_id = $2
                    AND session_id = $3
                    AND turn_id = $4
                    AND issuing_turn_attempt_id = $5
                    AND dispatch_generation = $6
                    AND state_kind = 'prepared'",
            )
            .bind(tool_attempt_id_to_uuid(authorized.attempt().attempt()))
            .bind(tool_request_id_to_uuid(authorized.attempt().request()))
            .bind(session_id_to_uuid(session))
            .bind(turn_id_to_uuid(turn))
            .bind(authorized.attempt().issuing_attempt().into_uuid())
            .bind(Decimal::from(authorized.attempt().generation().as_u64()))
            .execute(&mut *transaction)
            .await?
            .rows_affected();
            require_single(rows, "tool attempt authorization")?;
            Ok(authorized)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Rereads exact dispatch authority after an ambiguous authorization
    /// commit acknowledgement.
    pub async fn reread_ambiguous_authorization(
        &self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
    ) -> Result<ToolAttemptAuthorizationStatus, ToolLoopRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_tool_session(&mut transaction, session).await?;
        let current = load_current_attempt(&mut transaction, attempt)
            .await?
            .ok_or(ToolLoopCorruption::Missing("authorized tool attempt"))?;
        if current.session() != session || current.turn() != turn {
            return Err(ToolLoopCorruption::Inconsistent("attempt ownership").into());
        }
        let status = match current.state() {
            CurrentToolAttemptState::Prepared => ToolAttemptAuthorizationStatus::Prepared(current),
            CurrentToolAttemptState::InFlight => ToolAttemptAuthorizationStatus::InFlight(
                current.resume_in_flight().map_err(|_| {
                    ToolLoopRepositoryError::InvalidTransition(
                        "in-flight authorization could not restore its fence",
                    )
                })?,
            ),
        };
        transaction.rollback().await?;
        Ok(status)
    }

    /// Atomically applies exact executor evidence through the returned fence.
    pub async fn commit_observation(
        &self,
        observation: CorrelatedToolAttemptObservation,
    ) -> Result<EndedToolAttempt, ToolLoopRepositoryError> {
        let correlation = *observation.correlation();
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_tool_session(&mut transaction, correlation.session()).await?;
            let current = load_current_attempt(&mut transaction, correlation.attempt())
                .await?
                .ok_or(ToolLoopCorruption::Missing("in-flight tool attempt"))?;
            let ended = current
                .apply_terminal_observation(observation)
                .map_err(|_| {
                    ToolLoopRepositoryError::InvalidTransition(
                        "executor evidence does not match current fence",
                    )
                })?;
            persist_ended_attempt(&mut transaction, &ended).await?;
            if ended.end() == &ToolAttemptEnd::Ambiguous {
                persist_tool_recovery_wait(&mut transaction, &ended, false).await?;
            }
            Ok(ended)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Rereads whether one unchanged executor observation committed.
    pub async fn reread_observation(
        &self,
        observation: &CorrelatedToolAttemptObservation,
    ) -> Result<RetainedToolAttemptObservationStatus, ToolLoopRepositoryError> {
        let correlation = observation.correlation();
        let mut transaction = self.pool.begin().await?;
        lock_tool_session(&mut transaction, correlation.session()).await?;
        let mut attempts = load_attempts_by_id(&mut transaction, &[correlation.attempt()]).await?;
        let attempt = attempts
            .remove(&correlation.attempt())
            .ok_or(ToolLoopCorruption::Missing("retained tool attempt"))?;
        let status = match attempt {
            ReconstitutedToolAttempt::Current(current)
                if current.state() == CurrentToolAttemptState::InFlight
                    && current.session() == correlation.session()
                    && current.turn() == correlation.turn()
                    && current.issuing_attempt() == correlation.issuing_attempt()
                    && current.request() == correlation.request()
                    && current.generation() == correlation.generation() =>
            {
                RetainedToolAttemptObservationStatus::Pending
            }
            ReconstitutedToolAttempt::Ended(ended)
                if ended.session() == correlation.session()
                    && ended.turn() == correlation.turn()
                    && ended.issuing_attempt() == correlation.issuing_attempt()
                    && ended.request() == correlation.request()
                    && ended.generation() == correlation.generation()
                    && attempt_end_matches_observation(ended.end(), observation.observation()) =>
            {
                RetainedToolAttemptObservationStatus::AlreadyCommitted
            }
            ReconstitutedToolAttempt::Current(_) | ReconstitutedToolAttempt::Ended(_) => {
                return Err(ToolLoopCorruption::Inconsistent("retained tool observation").into());
            }
        };
        transaction.rollback().await?;
        Ok(status)
    }

    /// Atomically records a lookup/schema error before any executor effect.
    pub async fn commit_preflight_error(
        &self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        error: ToolExecutionError,
    ) -> Result<EndedToolAttempt, ToolLoopRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_tool_session(&mut transaction, session).await?;
            let current = load_current_attempt(&mut transaction, attempt)
                .await?
                .ok_or(ToolLoopCorruption::Missing("prepared tool attempt"))?;
            if current.session() != session || current.turn() != turn {
                return Err(ToolLoopCorruption::Inconsistent("attempt ownership").into());
            }
            let ended = current.end_preflight_error(error).map_err(|_| {
                ToolLoopRepositoryError::InvalidTransition("invalid tool preflight result")
            })?;
            persist_ended_attempt(&mut transaction, &ended).await?;
            Ok(ended)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Classifies one process-lost attempt and, for known loss, atomically
    /// closes the current turn with proof-bearing failure identities.
    pub async fn classify_crash_loss_and_close<NextTurn>(
        &self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        identities: ToolCrashClosureIdentities,
        next_turn: NextTurn,
    ) -> Result<signalbox_domain::ToolAttemptCrashOutcome, ToolLoopRepositoryError>
    where
        NextTurn: FnMut(signalbox_domain::AcceptedInputId) -> TurnId,
    {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_tool_session(&mut transaction, session).await?;
            load_active_batch_from_connection(&mut transaction, session, turn)
                .await?
                .ok_or(ToolLoopCorruption::Missing("active tool batch"))?;
            let current = load_current_attempt(&mut transaction, attempt)
                .await?
                .ok_or(ToolLoopCorruption::Missing("live tool attempt"))?;
            if current.session() != session || current.turn() != turn {
                return Err(ToolLoopCorruption::Inconsistent("attempt ownership").into());
            }
            let outcome = current.classify_crash_loss();
            let ended = match &outcome {
                signalbox_domain::ToolAttemptCrashOutcome::KnownFailed(ended)
                | signalbox_domain::ToolAttemptCrashOutcome::Ambiguous(ended) => ended,
            };
            persist_ended_attempt(&mut transaction, ended).await?;
            match &outcome {
                signalbox_domain::ToolAttemptCrashOutcome::Ambiguous(_) => {
                    persist_tool_recovery_wait(&mut transaction, ended, true).await?;
                }
                signalbox_domain::ToolAttemptCrashOutcome::KnownFailed(_) => {
                    let closed_batch =
                        load_active_batch_from_connection(&mut transaction, session, turn)
                            .await?
                            .ok_or(ToolLoopCorruption::Missing("crash-closed tool batch"))?;
                    let projection = closed_batch
                        .prepare_cancellation_projection(
                            identities.result_entries().to_vec(),
                            identities.result_frontier(),
                        )
                        .map_err(|_| {
                            ToolLoopRepositoryError::InvalidTransition(
                                "known tool crash could not close its request batch",
                            )
                        })?;
                    persist_result_entries(&mut transaction, &projection).await?;
                    insert_snapshot(&mut transaction, projection.snapshot())
                        .await
                        .map_err(|_| ToolLoopCorruption::Inconsistent("crash closure frontier"))?;
                    crate::model_execution::fail_tool_crash_in_transaction(
                        &mut transaction,
                        session,
                        turn,
                        projection.snapshot(),
                        identities.failure().clone(),
                        next_turn,
                    )
                    .await
                    .map_err(map_model_call_error)?;
                }
            }
            Ok(outcome)
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Atomically commits result projection, steering consumption, and the
    /// next prepared model call for the same logical turn.
    pub async fn commit_result_and_prepare_continuation(
        &self,
        producing_call: signalbox_domain::ModelCallId,
        projection: &PreparedToolResultProjection,
        prepared: &signalbox_domain::PreparedInitialModelCall,
        credential_reference: &ModelCallCredentialReference,
    ) -> Result<(), ToolLoopRepositoryError> {
        let session = prepared.session();
        let turn = prepared.turn();
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_tool_session(&mut transaction, session).await?;
            let batch = load_active_batch_from_connection(&mut transaction, session, turn)
                .await?
                .ok_or(ToolLoopCorruption::Missing("active tool batch"))?;
            if batch.producing_call() != producing_call
                || batch.yielded_snapshot().frontier().owning_session() != session
                || !matches!(
                    batch.phase(),
                    signalbox_domain::ToolBatchPhase::Executing { turn_attempt }
                        if turn_attempt == prepared.attempt()
                )
            {
                return Err(ToolLoopCorruption::Inconsistent("continuation batch").into());
            }
            let projection_frontier = projection.snapshot().frontier();
            let call_frontier = prepared.call().frontier();
            let frontier_matches = match prepared.steering_snapshot() {
                Some(steering_snapshot) => {
                    projection
                        .snapshot()
                        .is_semantic_prefix_of(steering_snapshot)
                        && call_frontier == steering_snapshot.frontier()
                }
                None => call_frontier == projection_frontier,
            };
            if projection_frontier.owning_session() != session || !frontier_matches {
                return Err(ToolLoopCorruption::Inconsistent("continuation call frontier").into());
            }

            persist_result_entries(&mut transaction, projection).await?;
            insert_snapshot(&mut transaction, projection.snapshot())
                .await
                .map_err(|_| ToolLoopCorruption::Inconsistent("result frontier"))?;
            insert_prepared_call(&mut transaction, prepared, credential_reference)
                .await
                .map_err(map_model_call_error)?;
            let rows = sqlx::query(
                "UPDATE turn_lifecycle
                    SET active_tool_round_call_id = NULL,
                        approval_tool_request_id = NULL,
                        recovery_tool_attempt_id = NULL
                  WHERE turn_id = $1
                    AND session_id = $2
                    AND current_attempt_id = $3
                    AND state_kind = 'active'
                    AND active_phase_kind = 'running'
                    AND active_tool_round_call_id = $4",
            )
            .bind(turn_id_to_uuid(turn))
            .bind(session_id_to_uuid(session))
            .bind(prepared.attempt().into_uuid())
            .bind(producing_call.into_uuid())
            .execute(&mut *transaction)
            .await?
            .rows_affected();
            require_single(rows, "tool result continuation call")?;
            Ok(())
        }
        .await;
        finish_commit(transaction, result).await
    }

    /// Atomically derives and commits result projection, consumes all pending
    /// steering, and prepares the next same-turn model call.
    pub async fn prepare_continuation<NextSteering>(
        &self,
        session: SessionId,
        turn: TurnId,
        producing_call: signalbox_domain::ModelCallId,
        identities: ToolContinuationIdentities,
        next_steering: NextSteering,
    ) -> Result<PrepareToolContinuationOutcome, ToolLoopRepositoryError>
    where
        NextSteering: FnMut(
            signalbox_domain::AcceptedInputId,
        ) -> (signalbox_domain::SemanticTranscriptEntryId, TurnId),
    {
        let targets = self.continuation_targets.as_ref().ok_or(
            ToolLoopRepositoryError::InvalidTransition(
                "tool continuation model targets are not configured",
            ),
        )?;
        let credential_reference = self.continuation_credential.as_ref().ok_or(
            ToolLoopRepositoryError::InvalidTransition(
                "tool continuation credential reference is not configured",
            ),
        )?;
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_tool_session(&mut transaction, session).await?;
            let Some(batch) =
                load_active_batch_from_connection(&mut transaction, session, turn).await?
            else {
                return Ok(PrepareToolContinuationOutcome::NoWork);
            };
            let turn_attempt = match batch.phase() {
                signalbox_domain::ToolBatchPhase::Executing { turn_attempt }
                    if batch.producing_call() == producing_call =>
                {
                    turn_attempt
                }
                _ => return Ok(PrepareToolContinuationOutcome::NoWork),
            };
            let projection = batch
                .prepare_result_projection(
                    identities.result_entries().to_vec(),
                    identities.result_frontier(),
                )
                .map_err(|_| {
                    ToolLoopRepositoryError::InvalidTransition(
                        "tool batch is not ready for continuation",
                    )
                })?;
            persist_result_entries(&mut transaction, &projection).await?;
            insert_snapshot(&mut transaction, projection.snapshot())
                .await
                .map_err(|_| ToolLoopCorruption::Inconsistent("result frontier"))?;
            let outcome = crate::model_execution::prepare_tool_continuation_call(
                &mut transaction,
                session,
                turn,
                targets,
                credential_reference,
                projection.snapshot(),
                identities.call(),
                identities.target_failure().clone(),
                identities.steering_frontier(),
                next_steering,
            )
            .await
            .map_err(map_model_call_error)?;
            if matches!(outcome, PrepareToolContinuationOutcome::Checkpointed(_)) {
                let rows = sqlx::query(
                    "UPDATE turn_lifecycle
                        SET active_tool_round_call_id = NULL,
                            approval_tool_request_id = NULL,
                            recovery_tool_attempt_id = NULL
                      WHERE turn_id = $1
                        AND session_id = $2
                        AND current_attempt_id = $3
                        AND state_kind = 'active'
                        AND active_phase_kind = 'running'
                        AND active_tool_round_call_id = $4",
                )
                .bind(turn_id_to_uuid(turn))
                .bind(session_id_to_uuid(session))
                .bind(turn_attempt.into_uuid())
                .bind(producing_call.into_uuid())
                .execute(&mut *transaction)
                .await?
                .rows_affected();
                require_single(rows, "tool continuation call boundary")?;
            }
            Ok(outcome)
        }
        .await;
        finish_commit(transaction, result).await
    }
}

impl DecideToolRequestTransaction for PostgresToolLoopRepository {
    type Error = ToolLoopRepositoryError;

    async fn decide<NextAttempt>(
        &mut self,
        command: DecideToolRequest,
        next_attempt: NextAttempt,
    ) -> Result<PreparedDecideToolRequest, Self::Error>
    where
        NextAttempt: FnMut() -> signalbox_domain::TurnAttemptId + Send,
    {
        PostgresToolLoopRepository::decide(self, command, next_attempt).await
    }
}

impl ToolExecutionTransaction for PostgresToolLoopRepository {
    type Error = ToolLoopRepositoryError;

    async fn load_active_batch(
        &mut self,
        session: SessionId,
        turn: TurnId,
    ) -> Result<Option<ToolBatch>, Self::Error> {
        PostgresToolLoopRepository::load_active_batch(self, session, turn).await
    }

    async fn prepare_next_attempt(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        effect_class: ToolEffectClass,
    ) -> Result<CurrentToolAttempt, Self::Error> {
        PostgresToolLoopRepository::prepare_next_attempt(self, session, turn, attempt, effect_class)
            .await
    }

    async fn authorize_attempt(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
    ) -> Result<AuthorizedToolAttempt, Self::Error> {
        PostgresToolLoopRepository::authorize_attempt(self, session, turn, attempt).await
    }

    async fn reread_ambiguous_authorization(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
    ) -> Result<ToolAttemptAuthorizationStatus, Self::Error> {
        PostgresToolLoopRepository::reread_ambiguous_authorization(self, session, turn, attempt)
            .await
    }

    async fn commit_preflight_error(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        error: ToolExecutionError,
    ) -> Result<EndedToolAttempt, Self::Error> {
        PostgresToolLoopRepository::commit_preflight_error(self, session, turn, attempt, error)
            .await
    }

    async fn commit_observation(
        &mut self,
        observation: CorrelatedToolAttemptObservation,
    ) -> Result<EndedToolAttempt, Self::Error> {
        PostgresToolLoopRepository::commit_observation(self, observation).await
    }

    async fn reread_observation(
        &mut self,
        observation: &CorrelatedToolAttemptObservation,
    ) -> Result<RetainedToolAttemptObservationStatus, Self::Error> {
        PostgresToolLoopRepository::reread_observation(self, observation).await
    }

    async fn classify_crash_loss<NextTurn>(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        identities: ToolCrashClosureIdentities,
        next_turn: NextTurn,
    ) -> Result<signalbox_domain::ToolAttemptCrashOutcome, Self::Error>
    where
        NextTurn: FnMut(signalbox_domain::AcceptedInputId) -> TurnId + Send,
    {
        PostgresToolLoopRepository::classify_crash_loss_and_close(
            self, session, turn, attempt, identities, next_turn,
        )
        .await
    }

    async fn prepare_continuation<NextSteering>(
        &mut self,
        session: SessionId,
        turn: TurnId,
        producing_call: signalbox_domain::ModelCallId,
        identities: ToolContinuationIdentities,
        next_steering: NextSteering,
    ) -> Result<PrepareToolContinuationOutcome, Self::Error>
    where
        NextSteering: FnMut(
                signalbox_domain::AcceptedInputId,
            ) -> (signalbox_domain::SemanticTranscriptEntryId, TurnId)
            + Send,
    {
        PostgresToolLoopRepository::prepare_continuation(
            self,
            session,
            turn,
            producing_call,
            identities,
            next_steering,
        )
        .await
    }
}

fn map_model_call_error(
    error: crate::model_execution::ModelCallRepositoryError,
) -> ToolLoopRepositoryError {
    match error {
        crate::model_execution::ModelCallRepositoryError::Database {
            source,
            commit_ambiguous,
        } => ToolLoopRepositoryError::Database {
            source,
            commit_ambiguous,
        },
        crate::model_execution::ModelCallRepositoryError::IdentityCollision(_) => {
            ToolLoopRepositoryError::IdentityCollision
        }
        crate::model_execution::ModelCallRepositoryError::Corruption(_)
        | crate::model_execution::ModelCallRepositoryError::NoLiveExecution
        | crate::model_execution::ModelCallRepositoryError::InvalidTransition(_) => {
            ToolLoopCorruption::Inconsistent("continuation model call").into()
        }
    }
}

pub(crate) async fn load_active_batch_from_connection(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Option<ToolBatch>, ToolLoopRepositoryError> {
    let lifecycle = sqlx::query(
        "SELECT active_phase_kind, current_attempt_id,
                active_tool_round_call_id, approval_tool_request_id,
                recovery_tool_attempt_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'active'
            AND active_tool_round_call_id IS NOT NULL",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(lifecycle) = lifecycle else {
        return Ok(None);
    };
    let producing_call = signalbox_domain::ModelCallId::from_uuid(required(
        &lifecycle,
        "active_tool_round_call_id",
    )?);
    let round = sqlx::query(
        "SELECT boundary_kind, boundary_frontier_id
           FROM tool_round
          WHERE producing_model_call_id = $1
            AND session_id = $2
            AND turn_id = $3",
    )
    .bind(producing_call.into_uuid())
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ToolLoopCorruption::Missing("tool round"))?;
    let boundary_kind: String = required(&round, "boundary_kind")?;
    if boundary_kind != "continuing" {
        return Err(ToolLoopCorruption::Inconsistent("active round boundary").into());
    }
    let frontier =
        signalbox_domain::ContextFrontierId::from_uuid(required(&round, "boundary_frontier_id")?);
    let yielded_snapshot = load_snapshot(connection, session, frontier).await?;
    let requests = load_requests(connection, producing_call, session, turn).await?;
    let approvals = load_approvals(connection, producing_call).await?;
    let attempts = load_attempts(connection, producing_call).await?;
    let phase_kind: String = required(&lifecycle, "active_phase_kind")?;
    let phase = match phase_kind.as_str() {
        "awaiting_tool_approval" => ToolBatchPhaseReconstitutionInput::AwaitingApproval {
            request: tool_request_id_from_uuid(required(&lifecycle, "approval_tool_request_id")?),
        },
        "running" => ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: signalbox_domain::TurnAttemptId::from_uuid(required(
                &lifecycle,
                "current_attempt_id",
            )?),
        },
        "awaiting_tool_recovery" => ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
            attempt: tool_attempt_id_from_uuid(required(&lifecycle, "recovery_tool_attempt_id")?),
        },
        value => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "active_phase_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    ToolBatchReconstitutionInput::new(
        session,
        turn,
        producing_call,
        yielded_snapshot,
        requests,
        approvals,
        attempts,
        phase,
    )
    .reconstitute()
    .map(Some)
    .map_err(|error| ToolLoopCorruption::Batch(error.failure()).into())
}

pub(crate) async fn load_recovery_batch_by_attempt(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    recovery_attempt: ToolAttemptId,
) -> Result<ToolBatch, ToolLoopRepositoryError> {
    let producing_call = sqlx::query_scalar::<_, Uuid>(
        "SELECT request.producing_model_call_id
           FROM tool_attempt AS attempt
           JOIN tool_request AS request
             ON request.request_id = attempt.request_id
          WHERE attempt.attempt_id = $1
            AND attempt.session_id = $2
            AND attempt.turn_id = $3",
    )
    .bind(tool_attempt_id_to_uuid(recovery_attempt))
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .map(signalbox_domain::ModelCallId::from_uuid)
    .ok_or(ToolLoopCorruption::Missing("tool recovery round"))?;
    let round = sqlx::query(
        "SELECT boundary_kind, boundary_frontier_id
           FROM tool_round
          WHERE producing_model_call_id = $1
            AND session_id = $2
            AND turn_id = $3",
    )
    .bind(producing_call.into_uuid())
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ToolLoopCorruption::Missing("tool recovery round"))?;
    if required::<String>(&round, "boundary_kind")? != "continuing" {
        return Err(ToolLoopCorruption::Inconsistent("tool recovery round boundary").into());
    }
    let frontier =
        signalbox_domain::ContextFrontierId::from_uuid(required(&round, "boundary_frontier_id")?);
    ToolBatchReconstitutionInput::new(
        session,
        turn,
        producing_call,
        load_snapshot(connection, session, frontier).await?,
        load_requests(connection, producing_call, session, turn).await?,
        load_approvals(connection, producing_call).await?,
        load_attempts(connection, producing_call).await?,
        ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
            attempt: recovery_attempt,
        },
    )
    .reconstitute()
    .map_err(|error| ToolLoopCorruption::Batch(error.failure()).into())
}

async fn load_snapshot(
    connection: &mut PgConnection,
    session: SessionId,
    frontier: signalbox_domain::ContextFrontierId,
) -> Result<ResolvedContextFrontierSnapshot, ToolLoopRepositoryError> {
    let declared: Decimal = sqlx::query_scalar(
        "SELECT member_count
           FROM context_frontier
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ToolLoopCorruption::Missing("tool round frontier"))?;
    let rows = sqlx::query_as::<_, (Decimal, Uuid, Uuid)>(
        "SELECT member_position, source_session_id, semantic_entry_id
           FROM context_frontier_member
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
          ORDER BY member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    if declared
        != Decimal::from(
            u64::try_from(rows.len())
                .map_err(|_| ToolLoopCorruption::Inconsistent("frontier count"))?,
        )
    {
        return Err(ToolLoopCorruption::Inconsistent("frontier count").into());
    }
    let mut entries = Vec::with_capacity(rows.len());
    for (index, (position, source_session, entry)) in rows.into_iter().enumerate() {
        let expected = u64::try_from(index + 1)
            .map_err(|_| ToolLoopCorruption::Inconsistent("frontier position"))?;
        if position != Decimal::from(expected) {
            return Err(ToolLoopCorruption::Inconsistent("frontier position").into());
        }
        entries.push(signalbox_domain::SemanticTranscriptEntryRef::from_source(
            session_id_from_uuid(source_session),
            signalbox_domain::SemanticTranscriptEntryId::from_uuid(entry),
        ));
    }
    ResolvedContextFrontierReconstitutionInput::new(session, frontier, entries)
        .reconstitute()
        .ok_or_else(|| ToolLoopCorruption::Inconsistent("frontier snapshot").into())
}

async fn load_requests(
    connection: &mut PgConnection,
    producing_call: signalbox_domain::ModelCallId,
    session: SessionId,
    turn: TurnId,
) -> Result<Vec<signalbox_domain::ToolRequest>, ToolLoopRepositoryError> {
    let rows = sqlx::query(
        "SELECT request_id, request_ordinal, tool_name,
                arguments_kind, arguments_text
           FROM tool_request
          WHERE producing_model_call_id = $1
          ORDER BY request_ordinal",
    )
    .bind(producing_call.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    rows.into_iter()
        .map(|row| decode_request(row, producing_call, session, turn))
        .collect()
}

pub(crate) fn decode_request(
    row: PgRow,
    producing_call: signalbox_domain::ModelCallId,
    session: SessionId,
    turn: TurnId,
) -> Result<signalbox_domain::ToolRequest, ToolLoopRepositoryError> {
    let ordinal: Decimal = required(&row, "request_ordinal")?;
    let ordinal =
        u32::try_from(ordinal).map_err(|_| ToolLoopCorruption::Inconsistent("request ordinal"))?;
    let name = ToolName::try_new(required(&row, "tool_name")?)
        .map_err(|_| ToolLoopCorruption::Inconsistent("tool name"))?;
    let arguments_kind = match required::<String>(&row, "arguments_kind")?.as_str() {
        "json" => ToolArgumentsKind::Json,
        "undecodable" => ToolArgumentsKind::Undecodable,
        value => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "arguments_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    let arguments =
        NormalizedToolArguments::try_from_stored(arguments_kind, required(&row, "arguments_text")?)
            .map_err(|_| ToolLoopCorruption::Inconsistent("normalized arguments"))?;
    Ok(ToolRequestReconstitutionInput::new(
        tool_request_id_from_uuid(required(&row, "request_id")?),
        session,
        turn,
        producing_call,
        ToolRequestOrdinal::from_u32(ordinal),
        name,
        arguments,
    )
    .into_request())
}

async fn load_approvals(
    connection: &mut PgConnection,
    producing_call: signalbox_domain::ModelCallId,
) -> Result<Vec<signalbox_domain::ToolApprovalResolution>, ToolLoopRepositoryError> {
    let rows = sqlx::query(
        "SELECT approval.request_id, approval.decision_kind,
                approval.decision_source, approval.denial_reason
           FROM tool_approval_decision AS approval
           JOIN tool_request AS request
             ON request.request_id = approval.request_id
          WHERE request.producing_model_call_id = $1
          ORDER BY request.request_ordinal",
    )
    .bind(producing_call.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    rows.into_iter().map(decode_approval).collect()
}

pub(crate) fn decode_approval(
    row: PgRow,
) -> Result<signalbox_domain::ToolApprovalResolution, ToolLoopRepositoryError> {
    let request = tool_request_id_from_uuid(required(&row, "request_id")?);
    let reason: Option<String> = row.try_get("denial_reason")?;
    let decision = match required::<String>(&row, "decision_kind")?.as_str() {
        "approve" if reason.is_none() => ToolApprovalDecision::Approve,
        "deny" => ToolApprovalDecision::Deny {
            reason: reason
                .map(|value| {
                    ToolDenialReason::try_new(value)
                        .map_err(|_| ToolLoopCorruption::Inconsistent("denial reason"))
                })
                .transpose()?,
        },
        "approve" => {
            return Err(ToolLoopCorruption::Inconsistent("approval payload").into());
        }
        value => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "decision_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    let source = match required::<String>(&row, "decision_source")?.as_str() {
        "owner_command" => ToolDecisionSource::OwnerCommand,
        "policy_auto" => ToolDecisionSource::PolicyAuto,
        "session_blanket" => ToolDecisionSource::SessionBlanket,
        value => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "decision_source",
                value: value.to_owned(),
            }
            .into());
        }
    };
    ToolApprovalResolutionReconstitutionInput::new(request, decision, source)
        .reconstitute()
        .map_err(|_| ToolLoopCorruption::Inconsistent("approval resolution").into())
}

async fn load_attempts(
    connection: &mut PgConnection,
    producing_call: signalbox_domain::ModelCallId,
) -> Result<Vec<ReconstitutedToolAttempt>, ToolLoopRepositoryError> {
    let rows = sqlx::query(
        "SELECT attempt.*
           FROM tool_attempt AS attempt
           JOIN tool_request AS request
             ON request.request_id = attempt.request_id
          WHERE request.producing_model_call_id = $1
          ORDER BY request.request_ordinal",
    )
    .bind(producing_call.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    rows.into_iter().map(decode_attempt).collect()
}

pub(crate) fn decode_attempt(
    row: PgRow,
) -> Result<ReconstitutedToolAttempt, ToolLoopRepositoryError> {
    let effect_class = match required::<String>(&row, "effect_class")?.as_str() {
        "effect_free" => ToolEffectClass::EffectFree,
        "external_effect" => ToolEffectClass::ExternalEffect,
        value => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "effect_class",
                value: value.to_owned(),
            }
            .into());
        }
    };
    let generation: Decimal = required(&row, "dispatch_generation")?;
    let generation = u64::try_from(generation)
        .ok()
        .and_then(ToolDispatchGeneration::try_from_u64)
        .ok_or(ToolLoopCorruption::Inconsistent("dispatch generation"))?;
    let terminal: Option<String> = row.try_get("terminal_disposition_kind")?;
    let state = match (
        required::<String>(&row, "state_kind")?.as_str(),
        terminal.as_deref(),
    ) {
        ("prepared", None) => ToolAttemptReconstitutionState::Prepared,
        ("in_flight", None) => ToolAttemptReconstitutionState::InFlight,
        ("terminal", Some(_)) => ToolAttemptReconstitutionState::Ended(decode_attempt_end(&row)?),
        ("prepared" | "in_flight" | "terminal", _) => {
            return Err(ToolLoopCorruption::Inconsistent("attempt state payload").into());
        }
        (value, _) => {
            return Err(ToolLoopCorruption::Unsupported {
                field: "tool_attempt.state_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    Ok(ToolAttemptReconstitutionInput::new(
        tool_attempt_id_from_uuid(required(&row, "attempt_id")?),
        tool_request_id_from_uuid(required(&row, "request_id")?),
        session_id_from_uuid(required(&row, "session_id")?),
        turn_id_from_uuid(required(&row, "turn_id")?),
        signalbox_domain::TurnAttemptId::from_uuid(required(&row, "issuing_turn_attempt_id")?),
        effect_class,
        generation,
        state,
    )
    .reconstitute())
}

pub(crate) async fn load_approvals_by_request(
    connection: &mut PgConnection,
    requests: &[ToolRequestId],
) -> Result<
    BTreeMap<ToolRequestId, signalbox_domain::ToolApprovalResolution>,
    ToolLoopRepositoryError,
> {
    if requests.is_empty() {
        return Ok(BTreeMap::new());
    }
    let request_uuids = requests
        .iter()
        .map(|request| tool_request_id_to_uuid(*request))
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT request_id, decision_kind, decision_source, denial_reason
           FROM tool_approval_decision
          WHERE request_id = ANY($1)",
    )
    .bind(&request_uuids)
    .fetch_all(&mut *connection)
    .await?;
    let mut approvals = BTreeMap::new();
    for row in rows {
        let request = tool_request_id_from_uuid(required(&row, "request_id")?);
        let approval = decode_approval(row)?;
        if approvals.insert(request, approval).is_some() {
            return Err(ToolLoopCorruption::Inconsistent("duplicate tool approval").into());
        }
    }
    Ok(approvals)
}

pub(crate) async fn load_attempts_by_id(
    connection: &mut PgConnection,
    attempts: &[ToolAttemptId],
) -> Result<BTreeMap<ToolAttemptId, ReconstitutedToolAttempt>, ToolLoopRepositoryError> {
    if attempts.is_empty() {
        return Ok(BTreeMap::new());
    }
    let attempt_uuids = attempts
        .iter()
        .map(|attempt| tool_attempt_id_to_uuid(*attempt))
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT *
           FROM tool_attempt
          WHERE attempt_id = ANY($1)",
    )
    .bind(&attempt_uuids)
    .fetch_all(&mut *connection)
    .await?;
    let mut loaded = BTreeMap::new();
    for row in rows {
        let attempt = tool_attempt_id_from_uuid(required(&row, "attempt_id")?);
        let reconstituted = decode_attempt(row)?;
        if loaded.insert(attempt, reconstituted).is_some() {
            return Err(ToolLoopCorruption::Inconsistent("duplicate tool attempt").into());
        }
    }
    Ok(loaded)
}

fn decode_attempt_end(row: &PgRow) -> Result<ToolAttemptEnd, ToolLoopRepositoryError> {
    match required::<String>(row, "terminal_disposition_kind")?.as_str() {
        "completed" => match required::<String>(row, "result_content_kind")?.as_str() {
            "text" => Ok(ToolAttemptEnd::Completed {
                result: ToolResultContent::Text(
                    ToolResultText::try_new(required(row, "result_text")?)
                        .map_err(|_| ToolLoopCorruption::Inconsistent("tool result text"))?,
                ),
            }),
            value => Err(ToolLoopCorruption::Unsupported {
                field: "result_content_kind",
                value: value.to_owned(),
            }
            .into()),
        },
        "known_failed" => {
            let kind = decode_error_kind(&required::<String>(row, "error_kind")?)?;
            let detail = row
                .try_get::<Option<String>, _>("error_detail")?
                .map(|value| {
                    ToolExecutionErrorDetail::try_new(value)
                        .map_err(|_| ToolLoopCorruption::Inconsistent("tool error detail"))
                })
                .transpose()?;
            Ok(ToolAttemptEnd::KnownFailed {
                error: ToolExecutionError::new(kind, detail),
            })
        }
        "ambiguous" => Ok(ToolAttemptEnd::Ambiguous),
        value => Err(ToolLoopCorruption::Unsupported {
            field: "terminal_disposition_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn attempt_end_matches_observation(
    end: &ToolAttemptEnd,
    observation: &ToolAttemptObservation,
) -> bool {
    matches!(
        (end, observation),
        (
            ToolAttemptEnd::Completed { result: stored },
            ToolAttemptObservation::Completed { result: observed },
        ) if stored == observed
    ) || matches!(
        (end, observation),
        (
            ToolAttemptEnd::KnownFailed { error: stored },
            ToolAttemptObservation::KnownFailed { error: observed },
        ) if stored == observed
    ) || matches!(
        (end, observation),
        (ToolAttemptEnd::Ambiguous, ToolAttemptObservation::Ambiguous)
    )
}

fn decode_error_kind(value: &str) -> Result<ToolExecutionErrorKind, ToolLoopRepositoryError> {
    match value {
        "unknown_tool" => Ok(ToolExecutionErrorKind::UnknownTool),
        "invalid_arguments" => Ok(ToolExecutionErrorKind::InvalidArguments),
        "execution_failed" => Ok(ToolExecutionErrorKind::ExecutionFailed),
        "result_too_large" => Ok(ToolExecutionErrorKind::ResultTooLarge),
        "crash_lost" => Ok(ToolExecutionErrorKind::CrashLost),
        value => Err(ToolLoopCorruption::Unsupported {
            field: "error_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

async fn load_current_attempt(
    connection: &mut PgConnection,
    attempt: ToolAttemptId,
) -> Result<Option<CurrentToolAttempt>, ToolLoopRepositoryError> {
    let row = sqlx::query(
        "SELECT *
           FROM tool_attempt
          WHERE attempt_id = $1
            AND state_kind IN ('prepared', 'in_flight')",
    )
    .bind(tool_attempt_id_to_uuid(attempt))
    .fetch_optional(&mut *connection)
    .await?;
    row.map(decode_attempt)
        .transpose()?
        .map(|attempt| match attempt {
            ReconstitutedToolAttempt::Current(current) => Ok(current),
            ReconstitutedToolAttempt::Ended(_) => {
                Err(ToolLoopCorruption::Inconsistent("live attempt decode").into())
            }
        })
        .transpose()
}

async fn insert_prepared_attempt(
    connection: &mut PgConnection,
    attempt: &CurrentToolAttempt,
) -> Result<(), ToolLoopRepositoryError> {
    if attempt.state() != CurrentToolAttemptState::Prepared {
        return Err(ToolLoopRepositoryError::InvalidTransition(
            "only a prepared attempt can be inserted",
        ));
    }
    sqlx::query(
        "INSERT INTO tool_attempt
            (attempt_id, request_id, session_id, turn_id,
             issuing_turn_attempt_id, effect_class, dispatch_generation,
             state_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'prepared')",
    )
    .bind(tool_attempt_id_to_uuid(attempt.attempt()))
    .bind(tool_request_id_to_uuid(attempt.request()))
    .bind(session_id_to_uuid(attempt.session()))
    .bind(turn_id_to_uuid(attempt.turn()))
    .bind(attempt.issuing_attempt().into_uuid())
    .bind(match attempt.effect_class() {
        ToolEffectClass::EffectFree => "effect_free",
        ToolEffectClass::ExternalEffect => "external_effect",
    })
    .bind(Decimal::from(attempt.generation().as_u64()))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(crate) async fn persist_ended_attempt(
    connection: &mut PgConnection,
    attempt: &EndedToolAttempt,
) -> Result<(), ToolLoopRepositoryError> {
    let (disposition, result_kind, result_text, error_kind, error_detail) =
        encode_attempt_end(attempt.end());
    let rows = sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = $1,
                result_content_kind = $2,
                result_text = $3,
                error_kind = $4,
                error_detail = $5
          WHERE attempt_id = $6
            AND request_id = $7
            AND session_id = $8
            AND turn_id = $9
            AND issuing_turn_attempt_id = $10
            AND dispatch_generation = $11
            AND state_kind IN ('prepared', 'in_flight')
            AND terminal_disposition_kind IS NULL",
    )
    .bind(disposition)
    .bind(result_kind)
    .bind(result_text)
    .bind(error_kind)
    .bind(error_detail)
    .bind(tool_attempt_id_to_uuid(attempt.attempt()))
    .bind(tool_request_id_to_uuid(attempt.request()))
    .bind(session_id_to_uuid(attempt.session()))
    .bind(turn_id_to_uuid(attempt.turn()))
    .bind(attempt.issuing_attempt().into_uuid())
    .bind(Decimal::from(attempt.generation().as_u64()))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "terminal tool attempt")
}

async fn mark_issuing_turn_attempt_running(
    connection: &mut PgConnection,
    attempt: &CurrentToolAttempt,
) -> Result<(), ToolLoopRepositoryError> {
    let rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'running'
          WHERE turn_attempt_id = $1
            AND turn_id = $2
            AND session_id = $3
            AND state_kind IN ('prepared', 'running')
            AND end_variant IS NULL
            AND end_disposition IS NULL",
    )
    .bind(attempt.issuing_attempt().into_uuid())
    .bind(turn_id_to_uuid(attempt.turn()))
    .bind(session_id_to_uuid(attempt.session()))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "tool issuing attempt authorization")
}

type EncodedToolAttemptEnd<'a> = (
    &'static str,
    Option<&'static str>,
    Option<&'a str>,
    Option<&'static str>,
    Option<&'a str>,
);

fn encode_attempt_end(end: &ToolAttemptEnd) -> EncodedToolAttemptEnd<'_> {
    match end {
        ToolAttemptEnd::Completed {
            result: ToolResultContent::Text(text),
        } => ("completed", Some("text"), Some(text.as_str()), None, None),
        ToolAttemptEnd::KnownFailed { error } => (
            "known_failed",
            None,
            None,
            Some(encode_error_kind(error.kind())),
            error.detail().map(ToolExecutionErrorDetail::as_str),
        ),
        ToolAttemptEnd::Ambiguous => ("ambiguous", None, None, None, None),
    }
}

const fn encode_error_kind(value: ToolExecutionErrorKind) -> &'static str {
    match value {
        ToolExecutionErrorKind::UnknownTool => "unknown_tool",
        ToolExecutionErrorKind::InvalidArguments => "invalid_arguments",
        ToolExecutionErrorKind::ExecutionFailed => "execution_failed",
        ToolExecutionErrorKind::ResultTooLarge => "result_too_large",
        ToolExecutionErrorKind::CrashLost => "crash_lost",
    }
}

pub(crate) async fn persist_tool_recovery_wait(
    connection: &mut PgConnection,
    attempt: &EndedToolAttempt,
    crash_lost: bool,
) -> Result<(), ToolLoopRepositoryError> {
    let turn_disposition = if crash_lost { "lost" } else { "ambiguous" };
    let attempt_rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = $1
          WHERE turn_attempt_id = $2
            AND turn_id = $3
            AND session_id = $4
            AND state_kind IN ('prepared', 'running')
            AND end_variant IS NULL
            AND end_disposition IS NULL",
    )
    .bind(turn_disposition)
    .bind(attempt.issuing_attempt().into_uuid())
    .bind(turn_id_to_uuid(attempt.turn()))
    .bind(session_id_to_uuid(attempt.session()))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(attempt_rows, "ambiguous tool issuing attempt")?;
    let lifecycle_rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_tool_recovery',
                recovery_tool_attempt_id = $1,
                approval_tool_request_id = NULL
          WHERE turn_id = $2
            AND session_id = $3
            AND state_kind = 'active'
            AND active_phase_kind = 'running'
            AND current_attempt_id = $4
            AND active_tool_round_call_id IS NOT NULL",
    )
    .bind(tool_attempt_id_to_uuid(attempt.attempt()))
    .bind(turn_id_to_uuid(attempt.turn()))
    .bind(session_id_to_uuid(attempt.session()))
    .bind(attempt.issuing_attempt().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(lifecycle_rows, "tool recovery lifecycle")
}

async fn persist_batch_decision(
    connection: &mut PgConnection,
    decision: &PreparedToolBatchDecision,
) -> Result<(), ToolLoopRepositoryError> {
    persist_decision_command(connection, decision.prepared_command()).await?;
    let DecideToolRequestResult::Applied(applied) = decision.prepared_command().result() else {
        return Ok(());
    };
    let (decision_kind, denial_reason) = encode_approval(applied.resolution().decision());
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, denial_reason,
             owner_command_id)
         VALUES ($1, $2, 'owner_command', $3, $4)",
    )
    .bind(tool_request_id_to_uuid(applied.resolution().request()))
    .bind(decision_kind)
    .bind(denial_reason)
    .bind(durable_command_id_to_uuid(
        decision.prepared_command().command().command_id(),
    ))
    .execute(&mut *connection)
    .await?;
    match decision.active_phase() {
        ActiveTurnPhase::AwaitingApproval { request } => {
            let rows = sqlx::query(
                "UPDATE turn_lifecycle
                    SET approval_tool_request_id = $1
                  WHERE turn_id = $2
                    AND session_id = $3
                    AND state_kind = 'active'
                    AND active_phase_kind = 'awaiting_tool_approval'
                    AND approval_tool_request_id = $4
                    AND active_tool_round_call_id = $5",
            )
            .bind(tool_request_id_to_uuid(*request))
            .bind(turn_id_to_uuid(decision.batch().turn()))
            .bind(session_id_to_uuid(decision.batch().session()))
            .bind(tool_request_id_to_uuid(
                decision.prepared_command().command().request(),
            ))
            .bind(decision.batch().producing_call().into_uuid())
            .execute(&mut *connection)
            .await?
            .rows_affected();
            require_single(rows, "next tool approval wait")?;
        }
        ActiveTurnPhase::Running { current_attempt } => {
            if current_attempt.state() != &signalbox_domain::CurrentTurnAttemptState::Prepared {
                return Err(ToolLoopRepositoryError::InvalidTransition(
                    "decision continuation attempt is not prepared",
                ));
            }
            let predecessor: Uuid = sqlx::query_scalar(
                "SELECT turn_attempt_id
                   FROM model_call
                  WHERE model_call_id = $1
                    AND turn_id = $2
                    AND session_id = $3",
            )
            .bind(decision.batch().producing_call().into_uuid())
            .bind(turn_id_to_uuid(decision.batch().turn()))
            .bind(session_id_to_uuid(decision.batch().session()))
            .fetch_one(&mut *connection)
            .await?;
            sqlx::query(
                "INSERT INTO turn_attempt
                    (turn_attempt_id, turn_id, session_id,
                     continued_from_attempt_id, state_kind)
                 VALUES ($1, $2, $3, $4, 'prepared')",
            )
            .bind(current_attempt.id().into_uuid())
            .bind(turn_id_to_uuid(decision.batch().turn()))
            .bind(session_id_to_uuid(decision.batch().session()))
            .bind(predecessor)
            .execute(&mut *connection)
            .await?;
            let rows = sqlx::query(
                "UPDATE turn_lifecycle
                    SET active_phase_kind = 'running',
                        current_attempt_id = $1,
                        approval_tool_request_id = NULL
                  WHERE turn_id = $2
                    AND session_id = $3
                    AND state_kind = 'active'
                    AND active_phase_kind = 'awaiting_tool_approval'
                    AND current_attempt_id IS NULL
                    AND approval_tool_request_id = $4
                    AND active_tool_round_call_id = $5",
            )
            .bind(current_attempt.id().into_uuid())
            .bind(turn_id_to_uuid(decision.batch().turn()))
            .bind(session_id_to_uuid(decision.batch().session()))
            .bind(tool_request_id_to_uuid(
                decision.prepared_command().command().request(),
            ))
            .bind(decision.batch().producing_call().into_uuid())
            .execute(&mut *connection)
            .await?
            .rows_affected();
            require_single(rows, "approved tool execution phase")?;
        }
        ActiveTurnPhase::AwaitingRecoveryDecision { .. } => {
            return Err(ToolLoopRepositoryError::InvalidTransition(
                "approval command cannot enter recovery",
            ));
        }
    }
    Ok(())
}

async fn persist_decision_command(
    connection: &mut PgConnection,
    prepared: &PreparedDecideToolRequest,
) -> Result<(), ToolLoopRepositoryError> {
    let command = prepared.command();
    let (decision_kind, denial_reason) = encode_approval(command.decision());
    let (result_kind, rejection_kind, earliest) = match prepared.result() {
        DecideToolRequestResult::Applied(_) => ("applied", None, None),
        DecideToolRequestResult::Rejected(DecideToolRequestRejectedResult::RequestNotFound {
            ..
        }) => ("rejected", Some("request_not_found"), None),
        DecideToolRequestResult::Rejected(DecideToolRequestRejectedResult::AlreadyResolved {
            ..
        }) => ("rejected", Some("already_resolved"), None),
        DecideToolRequestResult::Rejected(
            DecideToolRequestRejectedResult::NotEarliestUndecided { earliest, .. },
        ) => (
            "rejected",
            Some("not_earliest_undecided"),
            Some(tool_request_id_to_uuid(*earliest)),
        ),
    };
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, $2, $3, transaction_timestamp())
         ON CONFLICT DO NOTHING",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(DECIDE_TOOL_REQUEST_KIND)
    .bind(STORAGE_VERSION)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO decide_tool_request_command
            (command_id, command_kind, storage_version, request_id,
             decision_kind, denial_reason, result_kind, rejection_kind,
             result_earliest_undecided_request_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(DECIDE_TOOL_REQUEST_KIND)
    .bind(STORAGE_VERSION)
    .bind(tool_request_id_to_uuid(command.request()))
    .bind(decision_kind)
    .bind(denial_reason)
    .bind(result_kind)
    .bind(rejection_kind)
    .bind(earliest)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn load_decision_receipt(
    connection: &mut PgConnection,
    command_id: signalbox_domain::DurableCommandId,
) -> Result<Option<PreparedDecideToolRequest>, ToolLoopRepositoryError> {
    let row = sqlx::query(
        "SELECT request_id, decision_kind, denial_reason,
                result_kind, rejection_kind,
                result_earliest_undecided_request_id
           FROM decide_tool_request_command
          WHERE command_id = $1
            AND command_kind = 'decide_tool_request'
            AND storage_version = 1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let request = tool_request_id_from_uuid(required(&row, "request_id")?);
    let decision = decode_command_decision(&row)?;
    let command = DecideToolRequest::new(command_id, request, decision);
    let result_kind: String = required(&row, "result_kind")?;
    let rejection: Option<String> = row.try_get("rejection_kind")?;
    let prepared = match (result_kind.as_str(), rejection.as_deref()) {
        ("applied", None) => {
            let request_record = load_request_by_id(connection, request)
                .await?
                .ok_or(ToolLoopCorruption::Missing("applied decision request"))?;
            command
                .prepare_applied(&request_record)
                .map_err(|_| ToolLoopCorruption::Inconsistent("applied decision receipt"))?
        }
        ("rejected", Some("request_not_found")) => command.prepare_request_not_found(),
        ("rejected", Some("already_resolved")) => command.prepare_already_resolved(),
        ("rejected", Some("not_earliest_undecided")) => command.prepare_not_earliest(
            tool_request_id_from_uuid(required(&row, "result_earliest_undecided_request_id")?),
        ),
        _ => return Err(ToolLoopCorruption::Inconsistent("decision receipt result").into()),
    };
    Ok(Some(prepared))
}

async fn decision_exists(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<bool, ToolLoopRepositoryError> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM tool_approval_decision
              WHERE request_id = $1
         )",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_one(&mut *connection)
    .await
    .map_err(Into::into)
}

async fn request_closed_by_turn_end(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<bool, ToolLoopRepositoryError> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM tool_request AS request
               JOIN turn_lifecycle AS turn
                 ON turn.turn_id = request.turn_id
                AND turn.session_id = request.session_id
              WHERE request.request_id = $1
                AND turn.state_kind = 'terminal'
         )",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_one(connection)
    .await
    .map_err(Into::into)
}

pub(crate) async fn load_request_by_id(
    connection: &mut PgConnection,
    request: ToolRequestId,
) -> Result<Option<signalbox_domain::ToolRequest>, ToolLoopRepositoryError> {
    let row = sqlx::query(
        "SELECT request_id, request_ordinal, tool_name,
                arguments_kind, arguments_text,
                producing_model_call_id, session_id, turn_id
           FROM tool_request
          WHERE request_id = $1",
    )
    .bind(tool_request_id_to_uuid(request))
    .fetch_optional(&mut *connection)
    .await?;
    row.map(|row| {
        let call =
            signalbox_domain::ModelCallId::from_uuid(required(&row, "producing_model_call_id")?);
        let session = session_id_from_uuid(required(&row, "session_id")?);
        let turn = turn_id_from_uuid(required(&row, "turn_id")?);
        decode_request(row, call, session, turn)
    })
    .transpose()
}

pub(crate) async fn load_requests_by_id(
    connection: &mut PgConnection,
    requests: &[ToolRequestId],
) -> Result<BTreeMap<ToolRequestId, signalbox_domain::ToolRequest>, ToolLoopRepositoryError> {
    if requests.is_empty() {
        return Ok(BTreeMap::new());
    }
    let request_uuids = requests
        .iter()
        .map(|request| tool_request_id_to_uuid(*request))
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT request_id, request_ordinal, tool_name,
                arguments_kind, arguments_text,
                producing_model_call_id, session_id, turn_id
           FROM tool_request
          WHERE request_id = ANY($1)",
    )
    .bind(&request_uuids)
    .fetch_all(&mut *connection)
    .await?;
    let mut loaded = BTreeMap::new();
    for row in rows {
        let request = tool_request_id_from_uuid(required(&row, "request_id")?);
        let call =
            signalbox_domain::ModelCallId::from_uuid(required(&row, "producing_model_call_id")?);
        let session = session_id_from_uuid(required(&row, "session_id")?);
        let turn = turn_id_from_uuid(required(&row, "turn_id")?);
        let record = decode_request(row, call, session, turn)?;
        if loaded.insert(request, record).is_some() {
            return Err(ToolLoopCorruption::Inconsistent("duplicate tool request").into());
        }
    }
    Ok(loaded)
}

fn decode_command_decision(row: &PgRow) -> Result<ToolApprovalDecision, ToolLoopRepositoryError> {
    let reason: Option<String> = row.try_get("denial_reason")?;
    match required::<String>(row, "decision_kind")?.as_str() {
        "approve" if reason.is_none() => Ok(ToolApprovalDecision::Approve),
        "deny" => Ok(ToolApprovalDecision::Deny {
            reason: reason
                .map(|value| {
                    ToolDenialReason::try_new(value)
                        .map_err(|_| ToolLoopCorruption::Inconsistent("command denial reason"))
                })
                .transpose()?,
        }),
        "approve" => Err(ToolLoopCorruption::Inconsistent("command decision payload").into()),
        value => Err(ToolLoopCorruption::Unsupported {
            field: "decision_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn encode_approval(decision: &ToolApprovalDecision) -> (&'static str, Option<&str>) {
    match decision {
        ToolApprovalDecision::Approve => ("approve", None),
        ToolApprovalDecision::Deny { reason } => {
            ("deny", reason.as_ref().map(ToolDenialReason::as_str))
        }
    }
}

pub(crate) async fn persist_result_entries(
    connection: &mut PgConnection,
    projection: &PreparedToolResultProjection,
) -> Result<(), ToolLoopRepositoryError> {
    persist_result_entry_slice(connection, projection.entries()).await
}

pub(crate) async fn persist_result_entry_slice(
    connection: &mut PgConnection,
    entries: &[signalbox_domain::SemanticTranscriptEntry],
) -> Result<(), ToolLoopRepositoryError> {
    for entry in entries {
        let (kind, request, attempt) = match entry.payload() {
            SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } => (
                "tool_execution_result",
                None,
                Some(tool_attempt_id_to_uuid(*attempt)),
            ),
            SemanticTranscriptEntryPayload::ToolDenied { request } => {
                ("tool_denied", Some(tool_request_id_to_uuid(*request)), None)
            }
            SemanticTranscriptEntryPayload::ToolClosed { request } => (
                "tool_closed_by_turn_end",
                Some(tool_request_id_to_uuid(*request)),
                None,
            ),
            _ => {
                return Err(ToolLoopCorruption::Inconsistent("tool result payload").into());
            }
        };
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 tool_result_request_id, tool_result_attempt_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(session_id_to_uuid(entry.source_session()))
        .bind(entry.identity().into_uuid())
        .bind(kind)
        .bind(request)
        .bind(attempt)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

async fn inspect_registry(
    connection: &mut PgConnection,
    command_id: signalbox_domain::DurableCommandId,
) -> Result<Option<CommandKind>, ToolLoopRepositoryError> {
    command_registry::inspect(connection, command_id)
        .await
        .map_err(|error| match error {
            RegistryInspectionError::Database(error) => error.into(),
            RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedKind(value)) => {
                ToolLoopCorruption::Unsupported {
                    field: "durable_command.command_kind",
                    value,
                }
                .into()
            }
            RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedVersion(_)) => {
                ToolLoopCorruption::Inconsistent("durable command storage version").into()
            }
            RegistryInspectionError::Corruption(
                RegistryCorruption::MissingTypedRecord(_)
                | RegistryCorruption::ConflictingTypedRecords,
            ) => ToolLoopCorruption::Inconsistent("durable command typed record").into(),
        })
}

async fn lock_tool_session(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<(), ToolLoopRepositoryError> {
    let (session_exists, scheduler): (bool, Option<Uuid>) =
        sqlx::query_as(crate::lock_inventory::START_ELIGIBLE_TURN)
            .bind(session_id_to_uuid(session))
            .fetch_one(connection)
            .await?;
    match (session_exists, scheduler) {
        (true, Some(_)) => Ok(()),
        (true, None) => Err(ToolLoopCorruption::Missing("session scheduler row").into()),
        (false, None) => Err(ToolLoopCorruption::Missing("session").into()),
        (false, Some(_)) => Err(ToolLoopCorruption::Inconsistent("orphan scheduler row").into()),
    }
}

fn required<T>(row: &PgRow, column: &'static str) -> Result<T, ToolLoopRepositoryError>
where
    for<'value> T: sqlx::Decode<'value, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(column)?
        .ok_or_else(|| ToolLoopCorruption::Missing(column).into())
}

fn require_single(rows: u64, relationship: &'static str) -> Result<(), ToolLoopRepositoryError> {
    if rows == 1 {
        Ok(())
    } else {
        Err(ToolLoopCorruption::Inconsistent(relationship).into())
    }
}

async fn finish_commit<T>(
    transaction: sqlx::Transaction<'_, sqlx::Postgres>,
    result: Result<T, ToolLoopRepositoryError>,
) -> Result<T, ToolLoopRepositoryError> {
    match result {
        Ok(value) => {
            transaction.commit().await.map_err(|source| {
                let commit_ambiguous = commit_failure_is_ambiguous(&source);
                ToolLoopRepositoryError::Database {
                    source,
                    commit_ambiguous,
                }
            })?;
            Ok(value)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
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
