//! PostgreSQL transactions surrounding the first text-only model call.
//!
//! The three transaction roles in docs/spec/model-call-execution.md stay
//! explicit here: a durable `Prepared` checkpoint, a separate
//! send-authorization commit, and a fresh post-effect observation commit. No
//! method holds a database transaction across provider work.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use rust_decimal::Decimal;
use signalbox_application::{
    AuthorizeModelCallOutcome, AuthorizeModelCallTransaction, ClassifyOperatorFailure,
    CommitModelCallObservationTransaction, FailPreparedModelCallTransaction,
    ModelCallAuthorizationReread, ModelCallCredentialReference, OperatorFailureClass,
    PrepareModelCallOutcome, PrepareModelCallTransaction, RetainedCapabilityFailureStatus,
    RetainedModelCallObservationStatus,
};
use signalbox_domain::{
    AcceptedInputDisposition, AcceptedInputId, AmbiguousModelCallTurn, AssistantText,
    AuthorizedModelCall, CancelledModelCallTurn, CompletedModelCallTurn,
    CorrelatedModelCallTerminalObservation, DirectModelSelection, FailedModelCallTurn,
    FailedModelCallTurnIdentities, FrozenAliasDefinition, FrozenModelSelection, ModelAlias,
    ModelCallDisposition, ModelCallExecution, ModelCallExecutionReconstitutionFailure,
    ModelCallExecutionReconstitutionInput, ModelCallId, ModelCallOriginContent,
    ModelCallPreparationFailure, ModelCallReconstitutionInput, ModelCallReconstitutionState,
    ModelCallTerminalIdentities, ModelCallTerminalObservation, ModelCallTerminalOutcome,
    ModelTargetCatalog, ModelTargetDefinition, PendingSteeringReclassificationIdentity,
    PinnedProviderTargetReconstitutionInput, PreparedModelCallRequest, ProviderModelIdentity,
    ReclassifiedPendingSteeringTurn, ReconciliationRequiredModelCallTurn, RefusedModelCallTurn,
    ResolvedContextFrontierReconstitutionInput, ResolvedProviderTarget, SemanticTranscriptEntry,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryRef, SessionId,
    StopRequestedModelCallTurn, TurnId,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    mapping::{
        durable_command_id_from_uuid, durable_command_id_to_uuid, session_id_from_uuid,
        session_id_to_uuid, turn_id_to_uuid,
    },
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
    /// A proposed reclassified successor-turn identity already exists.
    ReclassifiedTurn,
}

impl fmt::Display for ModelCallIdentityCollision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let identity = match self {
            Self::ModelCall => "model-call",
            Self::SemanticEntry => "semantic-entry",
            Self::TerminalFrontier => "context-frontier",
            Self::ReclassifiedTurn => "reclassified successor-turn",
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

/// Compatibility spelling for the application-owned prepare result.
pub use signalbox_application::PrepareModelCallOutcome as PrepareInitialModelCallOutcome;

/// PostgreSQL adapter for the initial model-call execution transactions.
#[derive(Clone, Debug)]
pub struct PostgresModelCallRepository {
    pool: PgPool,
    targets: ModelTargetCatalog,
    credential_reference: ModelCallCredentialReference,
}

impl PostgresModelCallRepository {
    /// Uses the shared pool, immutable target catalog, and current non-secret
    /// credential reference for calls first pinned by this repository.
    pub fn new(
        pool: PgPool,
        targets: ModelTargetCatalog,
        credential_reference: ModelCallCredentialReference,
    ) -> Self {
        Self {
            pool,
            targets,
            credential_reference,
        }
    }

    /// Commits Prepared while consuming the complete locked steering inventory.
    pub async fn prepare_initial_call<NextSteeringIdentities>(
        &self,
        session: SessionId,
        call: ModelCallId,
        failure_identities: FailedModelCallTurnIdentities,
        steering_frontier: signalbox_domain::ContextFrontierId,
        mut next_steering_identities: NextSteeringIdentities,
    ) -> Result<PrepareInitialModelCallOutcome, ModelCallRepositoryError>
    where
        NextSteeringIdentities:
            FnMut(AcceptedInputId) -> (signalbox_domain::SemanticTranscriptEntryId, TurnId),
    {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_session(&mut transaction, session).await?;
            let execution =
                require_live_execution(&mut transaction, session, &self.targets).await?;
            if let Some(current_call) = execution.current_call() {
                return match current_call.state() {
                    signalbox_domain::CurrentModelCallState::Prepared => {
                        let current_call_id = current_call.id();
                        let request = execution.resume_prepared_call().map_err(|_| {
                            ModelCallRepositoryError::InvalidTransition(
                                "Prepared call could not resume",
                            )
                        })?;
                        let credential_reference = load_call_credential_reference(
                            &mut transaction,
                            session,
                            current_call_id,
                        )
                        .await?;
                        Ok((
                            false,
                            PrepareInitialModelCallOutcome::Ready {
                                request: Box::new(request),
                                credential_reference,
                            },
                        ))
                    }
                    signalbox_domain::CurrentModelCallState::InFlight
                    | signalbox_domain::CurrentModelCallState::CancellationRequested => {
                        Ok((false, PrepareInitialModelCallOutcome::NoWork))
                    }
                };
            }

            let mut reserved_entries = execution
                .frontier_entries()
                .map(signalbox_domain::SemanticTranscriptEntry::identity)
                .collect::<std::collections::BTreeSet<_>>();
            let mut steering_identities =
                Vec::with_capacity(execution.active_turn().pending_steering().len());
            for pending in execution.active_turn().pending_steering() {
                let accepted_input = pending.accepted_input();
                let (entry, turn) = next_steering_identities(accepted_input);
                if !reserved_entries.insert(entry) {
                    return Err(ModelCallRepositoryError::IdentityCollision(
                        ModelCallIdentityCollision::SemanticEntry,
                    ));
                }
                steering_identities.push((
                    entry,
                    PendingSteeringReclassificationIdentity::new(accepted_input, turn),
                ));
            }
            let steering_entries = steering_identities
                .iter()
                .map(|(entry, _)| *entry)
                .collect::<Vec<_>>();
            if !steering_entries.is_empty()
                && steering_frontier == execution.start().frontier().snapshot()
            {
                return Err(ModelCallRepositoryError::IdentityCollision(
                    ModelCallIdentityCollision::TerminalFrontier,
                ));
            }
            let steering_snapshot = (!steering_entries.is_empty()).then_some(steering_frontier);
            let prepared = match execution.prepare_initial_call_consuming_steering(
                call,
                steering_entries,
                steering_snapshot,
            ) {
                Ok(prepared) => prepared,
                Err(error) if error.failure() == ModelCallPreparationFailure::TargetUnavailable => {
                    let resolution = error.target_resolution_error().ok_or(
                        ModelCallRepositoryError::InvalidTransition(
                            "target-unavailable result omitted its resolution proof",
                        ),
                    )?;
                    let source_turn = error.execution().turn();
                    let reclassifications = steering_identities
                        .into_iter()
                        .map(|(_, reclassification)| reclassification)
                        .collect::<Vec<_>>();
                    let mut proposed_turns = BTreeSet::new();
                    for reclassification in &reclassifications {
                        record_reclassified_turn_candidate(
                            source_turn,
                            reclassification.turn(),
                            &mut proposed_turns,
                        )?;
                    }
                    let failed = error
                        .execution()
                        .clone()
                        .fail_target_resolution(
                            resolution,
                            failure_identities
                                .with_pending_steering_reclassifications(reclassifications),
                        )
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
            insert_prepared_call(&mut transaction, &prepared, &self.credential_reference).await?;
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
    ) -> Result<AuthorizeModelCallOutcome, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            if let Err(error) = lock_session(&mut transaction, session).await {
                return match error {
                    ModelCallRepositoryError::NoLiveExecution => {
                        Ok((false, AuthorizeModelCallOutcome::NoSend))
                    }
                    error => Err(error),
                };
            }
            let execution =
                match require_live_execution(&mut transaction, session, &self.targets).await {
                    Ok(execution) => execution,
                    Err(ModelCallRepositoryError::NoLiveExecution) => {
                        return Ok((false, AuthorizeModelCallOutcome::NoSend));
                    }
                    Err(error) => return Err(error),
                };
            if !matches!(
                execution.current_call(),
                Some(current)
                    if current.id() == call
                        && current.state()
                            == signalbox_domain::CurrentModelCallState::Prepared
            ) {
                return Ok((false, AuthorizeModelCallOutcome::NoSend));
            }
            let authorized = execution.authorize_send().map_err(|_| {
                ModelCallCorruption::Inconsistent("checked Prepared call could not authorize send")
            })?;
            persist_authorization(&mut transaction, &authorized).await?;
            Ok((
                true,
                AuthorizeModelCallOutcome::Authorized(Box::new(authorized)),
            ))
        }
        .await;
        finish_optional_commit(transaction, result).await
    }

    /// Freshly reloads issued authority and commits one terminal observation.
    pub async fn apply_terminal_observation<NextTurn>(
        &self,
        session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
        identities: ModelCallTerminalIdentities,
        mut next_reclassified_turn: NextTurn,
    ) -> Result<ModelCallTerminalOutcome, ModelCallRepositoryError>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_session(&mut transaction, session).await?;
            let execution = require_exact_call(
                require_live_execution(&mut transaction, session, &self.targets).await?,
                observation.call(),
            )?;
            let identities = attach_pending_reclassification_candidates(
                identities,
                &execution,
                &mut next_reclassified_turn,
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
    pub async fn fail_prepared_call<NextTurn>(
        &self,
        session: SessionId,
        call: ModelCallId,
        identities: FailedModelCallTurnIdentities,
        mut next_reclassified_turn: NextTurn,
    ) -> Result<FailedModelCallTurn, ModelCallRepositoryError>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_session(&mut transaction, session).await?;
            let execution = require_exact_call(
                require_live_execution(&mut transaction, session, &self.targets).await?,
                call,
            )?;
            let reclassifications =
                pending_reclassification_candidates(&execution, &mut next_reclassified_turn)?;
            let failed = execution
                .fail_prepared_call(
                    identities.with_pending_steering_reclassifications(reclassifications),
                )
                .map_err(|_| {
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

    /// Rereads whether an unchanged pre-send capability failure committed.
    pub async fn reread_capability_failure(
        &self,
        session: SessionId,
        call: ModelCallId,
    ) -> Result<RetainedCapabilityFailureStatus, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_session(&mut transaction, session).await?;
            let stored = sqlx::query_as::<_, (Uuid, Uuid, Uuid, String, Option<String>)>(
                "SELECT turn_id, turn_attempt_id, context_frontier_id, state_kind,
                        terminal_disposition_kind
                   FROM model_call
                  WHERE session_id = $1
                    AND model_call_id = $2",
            )
            .bind(session_id_to_uuid(session))
            .bind(call.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(ModelCallCorruption::Missing(
                "retained capability-failure model call",
            ))?;
            let (turn, attempt, source_frontier, state, disposition) = stored;
            match (state.as_str(), disposition.as_deref()) {
                ("prepared", None) => {
                    let execution = require_exact_call(
                        require_live_execution(&mut transaction, session, &self.targets).await?,
                        call,
                    )?;
                    execution.resume_prepared_call().map_err(|_| {
                        ModelCallRepositoryError::InvalidTransition(
                            "retained capability failure could not resume Prepared",
                        )
                    })?;
                    Ok(RetainedCapabilityFailureStatus::Pending)
                }
                ("terminal", Some("known_failed")) => {
                    let transition_history_matches = sqlx::query_scalar::<_, bool>(
                        "SELECT
                            EXISTS (
                                SELECT 1
                                  FROM model_call_transition_outbox_event
                                 WHERE session_id = $1
                                   AND model_call_id = $3
                                   AND turn_id = $2
                                   AND call_state_kind = 'prepared'
                                   AND terminal_disposition_kind IS NULL
                            )
                            AND NOT EXISTS (
                                SELECT 1
                                  FROM model_call_transition_outbox_event
                                 WHERE session_id = $1
                                   AND model_call_id = $3
                                   AND turn_id = $2
                                   AND call_state_kind = 'in_flight'
                            )
                            AND EXISTS (
                                SELECT 1
                                  FROM model_call_transition_outbox_event
                                 WHERE session_id = $1
                                   AND model_call_id = $3
                                   AND turn_id = $2
                                   AND call_state_kind = 'terminal'
                                   AND terminal_disposition_kind = 'known_failed'
                            )",
                    )
                    .bind(session_id_to_uuid(session))
                    .bind(turn)
                    .bind(call.into_uuid())
                    .fetch_one(&mut *transaction)
                    .await?;
                    let closure_matches = failed_turn_closure_matches(
                        &mut transaction,
                        session,
                        turn,
                        attempt,
                        call.into_uuid(),
                        source_frontier,
                    )
                    .await?;
                    if transition_history_matches && closure_matches {
                        Ok(RetainedCapabilityFailureStatus::AlreadyCommitted)
                    } else {
                        Err(ModelCallRepositoryError::InvalidTransition(
                            "retained capability failure durable closure is incomplete",
                        ))
                    }
                }
                _ => Err(ModelCallRepositoryError::InvalidTransition(
                    "retained capability failure durable state changed",
                )),
            }
        }
        .await;
        transaction.rollback().await?;
        result
    }

    /// Rereads exact durable authority after an ambiguous authorization commit.
    pub async fn reread_ambiguous_authorization(
        &self,
        session: SessionId,
        prepared: &signalbox_domain::PreparedModelCallRequest,
    ) -> Result<ModelCallAuthorizationReread, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_session(&mut transaction, session).await?;
            let execution = require_exact_call(
                require_live_execution(&mut transaction, session, &self.targets).await?,
                prepared.call().id(),
            )?;
            match execution
                .current_call()
                .map(signalbox_domain::CurrentModelCall::state)
            {
                Some(signalbox_domain::CurrentModelCallState::Prepared) => {
                    let reloaded = execution.resume_prepared_call().map_err(|_| {
                        ModelCallRepositoryError::InvalidTransition(
                            "ambiguous authorization reread could not resume Prepared",
                        )
                    })?;
                    if &reloaded != prepared {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "ambiguous authorization reread changed Prepared request",
                        ));
                    }
                    Ok(ModelCallAuthorizationReread::Prepared)
                }
                Some(signalbox_domain::CurrentModelCallState::InFlight) => {
                    let authorized = execution.resume_in_flight_call().ok_or(
                        ModelCallRepositoryError::InvalidTransition(
                            "ambiguous authorization reread could not resume InFlight",
                        ),
                    )?;
                    if !prepared_matches_authorized(prepared, &authorized) {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "ambiguous authorization reread changed issued request",
                        ));
                    }
                    Ok(ModelCallAuthorizationReread::InFlight(Box::new(authorized)))
                }
                Some(signalbox_domain::CurrentModelCallState::CancellationRequested) => {
                    let stopped = execution.resume_cancellation_requested_call().ok_or(
                        ModelCallRepositoryError::InvalidTransition(
                            "ambiguous authorization reread could not resume CancellationRequested",
                        ),
                    )?;
                    if !prepared_matches_stopped(prepared, &execution, &stopped) {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "ambiguous authorization reread changed stopped request",
                        ));
                    }
                    Ok(ModelCallAuthorizationReread::CancellationRequested(
                        Box::new(stopped),
                    ))
                }
                None => Err(ModelCallRepositoryError::InvalidTransition(
                    "ambiguous authorization reread found no resumable call",
                )),
            }
        }
        .await;
        transaction.rollback().await?;
        result
    }

    /// Rereads whether an unchanged terminal observation already committed.
    pub async fn reread_terminal_observation(
        &self,
        session: SessionId,
        observation: &CorrelatedModelCallTerminalObservation,
    ) -> Result<RetainedModelCallObservationStatus, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_session(&mut transaction, session).await?;
            let correlation = observation.correlation();
            if correlation.session() != session {
                return Err(ModelCallRepositoryError::InvalidTransition(
                    "retained observation session changed",
                ));
            }
            let stored =
                sqlx::query_as::<_, (Uuid, Uuid, Uuid, Uuid, Uuid, String, Option<String>)>(
                    "SELECT session_id, turn_id, turn_attempt_id,
                        resolved_provider_model_identity_id, context_frontier_id,
                        state_kind, terminal_disposition_kind
                   FROM model_call
                  WHERE model_call_id = $1",
                )
                .bind(observation.call().into_uuid())
                .fetch_optional(&mut *transaction)
                .await?
                .ok_or(ModelCallCorruption::Missing(
                    "retained observation model call",
                ))?;
            let (stored_session, turn, attempt, target, frontier, state, disposition) = stored;
            if stored_session != session_id_to_uuid(correlation.session())
                || turn != turn_id_to_uuid(correlation.turn())
                || attempt != correlation.attempt().into_uuid()
                || target != correlation.target().identity().into_uuid()
                || frontier != correlation.frontier().into_uuid()
            {
                return Err(ModelCallRepositoryError::InvalidTransition(
                    "retained observation correlation changed",
                ));
            }
            match (state.as_str(), disposition.as_deref()) {
                ("in_flight", None) => {
                    let execution = require_exact_call(
                        require_live_execution(&mut transaction, session, &self.targets).await?,
                        observation.call(),
                    )?;
                    let authorized = execution.resume_in_flight_call().ok_or(
                        ModelCallRepositoryError::InvalidTransition(
                            "retained observation could not resume issued call",
                        ),
                    )?;
                    if authorized.observation_correlation() != *correlation {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "retained observation issued authority changed",
                        ));
                    }
                    Ok(RetainedModelCallObservationStatus::Pending)
                }
                ("cancellation_requested", None) => {
                    let retained_stop = sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS (
                            SELECT 1
                              FROM turn_lifecycle AS lifecycle
                              JOIN turn_attempt AS attempt
                                ON attempt.turn_attempt_id =
                                    lifecycle.current_attempt_id
                               AND attempt.turn_id = lifecycle.turn_id
                               AND attempt.session_id = lifecycle.session_id
                               AND attempt.state_kind = 'stop_requested'
                               AND attempt.interrupt_command_id IS NOT NULL
                              JOIN model_call_transition_outbox_event AS event
                                ON event.session_id = lifecycle.session_id
                               AND event.turn_id = lifecycle.turn_id
                               AND event.model_call_id = $3
                               AND event.call_state_kind =
                                   'cancellation_requested'
                             WHERE lifecycle.session_id = $1
                               AND lifecycle.turn_id = $2
                               AND lifecycle.state_kind = 'active'
                               AND lifecycle.active_phase_kind = 'running'
                        )",
                    )
                    .bind(session_id_to_uuid(session))
                    .bind(turn)
                    .bind(observation.call().into_uuid())
                    .fetch_one(&mut *transaction)
                    .await?;
                    if !retained_stop {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "retained observation stop authority changed",
                        ));
                    }
                    Ok(RetainedModelCallObservationStatus::Pending)
                }
                ("terminal", Some(stored_disposition))
                    if stored_disposition
                        == encode_disposition(observation.observation().disposition()) =>
                {
                    if !terminal_observation_closure_matches(&mut transaction, session, observation)
                        .await?
                    {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "retained observation terminal closure changed",
                        ));
                    }
                    Ok(RetainedModelCallObservationStatus::AlreadyCommitted)
                }
                _ => Err(ModelCallRepositoryError::InvalidTransition(
                    "retained observation durable state changed",
                )),
            }
        }
        .await;
        transaction.rollback().await?;
        result
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
                require_live_execution_for_restart(&mut transaction, session).await?,
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

impl PrepareModelCallTransaction for PostgresModelCallRepository {
    type Error = ModelCallRepositoryError;

    async fn prepare<NextSteeringIdentities>(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        failure_identities: FailedModelCallTurnIdentities,
        steering_frontier: signalbox_domain::ContextFrontierId,
        next_steering_identities: NextSteeringIdentities,
    ) -> Result<PrepareModelCallOutcome, Self::Error>
    where
        NextSteeringIdentities:
            FnMut(AcceptedInputId) -> (signalbox_domain::SemanticTranscriptEntryId, TurnId) + Send,
    {
        match self
            .prepare_initial_call(
                session,
                call,
                failure_identities,
                steering_frontier,
                next_steering_identities,
            )
            .await
        {
            Err(ModelCallRepositoryError::NoLiveExecution) => Ok(PrepareModelCallOutcome::NoWork),
            result => result,
        }
    }
}

impl FailPreparedModelCallTransaction for PostgresModelCallRepository {
    type Error = ModelCallRepositoryError;

    async fn fail_prepared<NextTurn>(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        identities: FailedModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
    ) -> Result<FailedModelCallTurn, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        PostgresModelCallRepository::fail_prepared_call(
            self,
            session,
            call,
            identities,
            next_reclassified_turn,
        )
        .await
    }

    async fn reread_failure(
        &mut self,
        session: SessionId,
        call: ModelCallId,
    ) -> Result<RetainedCapabilityFailureStatus, Self::Error> {
        self.reread_capability_failure(session, call).await
    }
}

impl AuthorizeModelCallTransaction for PostgresModelCallRepository {
    type Error = ModelCallRepositoryError;

    async fn authorize(
        &mut self,
        session: SessionId,
        call: ModelCallId,
    ) -> Result<AuthorizeModelCallOutcome, Self::Error> {
        self.authorize_send(session, call).await
    }

    async fn reread_after_ambiguous_commit(
        &mut self,
        session: SessionId,
        prepared: &signalbox_domain::PreparedModelCallRequest,
    ) -> Result<ModelCallAuthorizationReread, Self::Error> {
        self.reread_ambiguous_authorization(session, prepared).await
    }

    fn cancellation_signal(
        &self,
        session: SessionId,
        call: ModelCallId,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let pool = self.pool.clone();
        async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(25));
            loop {
                interval.tick().await;
                let state = sqlx::query_scalar::<_, String>(
                    "SELECT state_kind
                       FROM model_call
                      WHERE session_id = $1
                        AND model_call_id = $2",
                )
                .bind(session_id_to_uuid(session))
                .bind(call.into_uuid())
                .fetch_optional(&pool)
                .await;
                if matches!(
                    state,
                    Ok(Some(ref state))
                        if state == "cancellation_requested" || state == "terminal"
                ) {
                    return;
                }
            }
        }
    }
}

impl CommitModelCallObservationTransaction for PostgresModelCallRepository {
    type Error = ModelCallRepositoryError;

    async fn commit_observation<NextTurn>(
        &mut self,
        session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
        identities: ModelCallTerminalIdentities,
        next_reclassified_turn: NextTurn,
    ) -> Result<ModelCallTerminalOutcome, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        self.apply_terminal_observation(session, observation, identities, next_reclassified_turn)
            .await
    }

    async fn reread_observation(
        &mut self,
        session: SessionId,
        observation: &CorrelatedModelCallTerminalObservation,
    ) -> Result<RetainedModelCallObservationStatus, Self::Error> {
        self.reread_terminal_observation(session, observation).await
    }
}

async fn terminal_observation_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    if !terminal_observation_transition_events_match(connection, session, observation).await? {
        return Ok(false);
    }
    match observation.observation() {
        ModelCallTerminalObservation::Completed { assistant_text } => {
            completed_terminal_closure_matches(connection, session, observation, assistant_text)
                .await
        }
        ModelCallTerminalObservation::KnownFailed => {
            failed_terminal_closure_matches(connection, session, observation).await
        }
        ModelCallTerminalObservation::Cancelled => {
            if cancelled_terminal_closure_matches(connection, session, observation).await? {
                Ok(true)
            } else {
                failed_terminal_closure_matches(connection, session, observation).await
            }
        }
        ModelCallTerminalObservation::Refused => {
            refused_terminal_closure_matches(connection, session, observation).await
        }
        ModelCallTerminalObservation::Ambiguous => {
            ambiguous_terminal_closure_matches(connection, session, observation).await
        }
    }
}

async fn terminal_observation_transition_events_match(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT
            EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND model_call_id = $2
                   AND turn_id = $3
                   AND call_state_kind = 'in_flight'
                   AND terminal_disposition_kind IS NULL
            )
            AND EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND model_call_id = $2
                   AND turn_id = $3
                   AND call_state_kind = 'terminal'
                   AND terminal_disposition_kind = $4
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(observation.call().into_uuid())
    .bind(turn_id_to_uuid(observation.correlation().turn()))
    .bind(encode_disposition(observation.observation().disposition()))
    .fetch_one(&mut *connection)
    .await?)
}

async fn completed_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
    assistant_text: &[AssistantText],
) -> Result<bool, ModelCallRepositoryError> {
    let terminal_frontier = sqlx::query_scalar::<_, Uuid>(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'completed'
            AND terminal_model_call_id = $3",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(observation.correlation().turn()))
    .bind(observation.call().into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(terminal_frontier) = terminal_frontier else {
        return Ok(false);
    };
    let source_frontier = load_frontier_members(
        connection,
        session,
        observation.correlation().frontier().into_uuid(),
    )
    .await?;
    let terminal_members = load_terminal_frontier(connection, session, terminal_frontier).await?;
    if !completed_terminal_frontier_matches(
        &source_frontier,
        &terminal_members,
        session_id_to_uuid(session),
        turn_id_to_uuid(observation.correlation().turn()),
        observation.call().into_uuid(),
        assistant_text,
    ) {
        return Ok(false);
    }
    let Some(completion_entry) = terminal_members.last().map(|member| member.entry) else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM turn_completed_outbox_event
             WHERE session_id = $1
               AND turn_id = $2
               AND model_call_id = $3
               AND completion_entry_id = $4
               AND terminal_frontier_id = $5
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(observation.correlation().turn()))
    .bind(observation.call().into_uuid())
    .bind(completion_entry)
    .bind(terminal_frontier)
    .fetch_one(&mut *connection)
    .await?)
}

async fn failed_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    let correlation = observation.correlation();
    failed_turn_closure_matches(
        connection,
        session,
        turn_id_to_uuid(correlation.turn()),
        correlation.attempt().into_uuid(),
        observation.call().into_uuid(),
        correlation.frontier().into_uuid(),
    )
    .await
}

async fn cancelled_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    let correlation = observation.correlation();
    let terminal_frontier = sqlx::query_scalar::<_, Uuid>(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'cancelled'
            AND terminal_attempt_id = $3
            AND terminal_model_call_id = $4
            AND EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND turn_attempt_id = $3
                   AND state_kind = 'ended'
                   AND end_variant = 'after_cancellation'
                   AND end_disposition = 'cancelled'
                   AND interrupt_command_id IS NOT NULL
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(correlation.attempt().into_uuid())
    .bind(observation.call().into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(terminal_frontier) = terminal_frontier else {
        return Ok(false);
    };
    let source_frontier =
        load_frontier_members(connection, session, correlation.frontier().into_uuid()).await?;
    let terminal_members = load_terminal_frontier(connection, session, terminal_frontier).await?;
    if terminal_members.len() != source_frontier.len() + 1
        || terminal_members
            .iter()
            .zip(&source_frontier)
            .any(|(stored, expected)| (stored.source_session, stored.entry) != *expected)
    {
        return Ok(false);
    }
    let cancellation = &terminal_members[source_frontier.len()];
    if cancellation.source_session != session_id_to_uuid(session)
        || cancellation.payload_kind != "turn_cancelled"
        || cancellation.assistant_text.is_some()
        || cancellation.producing_call.is_some()
        || cancellation.completed_turn.is_some()
        || cancellation.failed_turn.is_some()
        || cancellation.cancelled_turn != Some(turn_id_to_uuid(correlation.turn()))
    {
        return Ok(false);
    }
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT
            EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND model_call_id = $3
                   AND call_state_kind = 'cancellation_requested'
                   AND terminal_disposition_kind IS NULL
            )
            AND EXISTS (
                SELECT 1
                  FROM turn_cancelled_outbox_event
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND cancellation_entry_id = $4
                   AND terminal_frontier_id = $5
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(observation.call().into_uuid())
    .bind(cancellation.entry)
    .bind(terminal_frontier)
    .fetch_one(&mut *connection)
    .await?)
}

async fn failed_turn_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    turn: Uuid,
    attempt: Uuid,
    call: Uuid,
    source_frontier: Uuid,
) -> Result<bool, ModelCallRepositoryError> {
    let terminal_frontier = sqlx::query_scalar::<_, Uuid>(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'failed'
            AND terminal_attempt_id = $3
            AND terminal_model_call_id = $4
            AND active_phase_kind IS NULL
            AND current_attempt_id IS NULL
            AND recovery_model_call_id IS NULL
            AND EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND turn_attempt_id = $3
                   AND state_kind = 'ended'
                   AND end_disposition = 'known_failure'
                   AND (
                        end_variant = 'without_stop'
                        OR (
                            end_variant = 'after_cancellation'
                            AND interrupt_command_id IS NOT NULL
                            AND interrupt_predecessor_turn_id = $2
                        )
                   )
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn)
    .bind(attempt)
    .bind(call)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(terminal_frontier) = terminal_frontier else {
        return Ok(false);
    };
    let source_frontier = load_frontier_members(connection, session, source_frontier).await?;
    let terminal_members = load_terminal_frontier(connection, session, terminal_frontier).await?;
    if !failed_terminal_frontier_matches(
        &source_frontier,
        &terminal_members,
        session_id_to_uuid(session),
        turn,
    ) {
        return Ok(false);
    }
    let Some(failure_entry) = terminal_members.last().map(|member| member.entry) else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM turn_failed_outbox_event
             WHERE session_id = $1
               AND turn_id = $2
               AND failure_entry_id = $3
               AND terminal_frontier_id = $4
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn)
    .bind(failure_entry)
    .bind(terminal_frontier)
    .fetch_one(&mut *connection)
    .await?)
}

async fn refused_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    let correlation = observation.correlation();
    let terminal_frontier = sqlx::query_scalar::<_, Uuid>(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'refused'
            AND terminal_attempt_id = $3
            AND terminal_model_call_id = $4
            AND active_phase_kind IS NULL
            AND current_attempt_id IS NULL
            AND recovery_model_call_id IS NULL
            AND EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND turn_attempt_id = $3
                   AND state_kind = 'ended'
                   AND end_disposition = 'turn_refused'
                   AND (
                        end_variant = 'without_stop'
                        OR (
                            end_variant = 'after_cancellation'
                            AND interrupt_command_id IS NOT NULL
                            AND interrupt_predecessor_turn_id = $2
                        )
                   )
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(correlation.attempt().into_uuid())
    .bind(observation.call().into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(terminal_frontier) = terminal_frontier else {
        return Ok(false);
    };
    let source_frontier =
        load_frontier_members(connection, session, correlation.frontier().into_uuid()).await?;
    let terminal_members = load_terminal_frontier(connection, session, terminal_frontier).await?;
    if terminal_members.len() != source_frontier.len()
        || terminal_members
            .iter()
            .zip(&source_frontier)
            .any(|(stored, expected)| (stored.source_session, stored.entry) != *expected)
    {
        return Ok(false);
    }
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM turn_refused_outbox_event
             WHERE session_id = $1
               AND turn_id = $2
               AND model_call_id = $3
               AND terminal_frontier_id = $4
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(observation.call().into_uuid())
    .bind(terminal_frontier)
    .fetch_one(&mut *connection)
    .await?)
}

async fn ambiguous_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    let correlation = observation.correlation();
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT
            EXISTS (
                SELECT 1
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND state_kind = 'active'
                   AND terminal_disposition_kind IS NULL
                   AND terminal_frontier_id IS NULL
                   AND terminal_attempt_id IS NULL
                   AND terminal_model_call_id IS NULL
                   AND active_phase_kind = 'awaiting_model_call_recovery'
                   AND current_attempt_id = $3
                   AND recovery_model_call_id = $4
                   AND EXISTS (
                        SELECT 1
                          FROM turn_attempt
                         WHERE session_id = $1
                           AND turn_id = $2
                           AND turn_attempt_id = $3
                           AND state_kind = 'ended'
                           AND end_variant = 'without_stop'
                           AND end_disposition = 'ambiguous'
                   )
            )
            OR EXISTS (
                SELECT 1
                  FROM turn_lifecycle AS lifecycle
                 WHERE lifecycle.session_id = $1
                   AND lifecycle.turn_id = $2
                   AND lifecycle.state_kind = 'terminal'
                   AND lifecycle.terminal_disposition_kind =
                       'reconciliation_required'
                   AND lifecycle.terminal_attempt_id = $3
                   AND lifecycle.terminal_model_call_id = $4
                   AND lifecycle.active_phase_kind IS NULL
                   AND lifecycle.current_attempt_id IS NULL
                   AND lifecycle.recovery_model_call_id IS NULL
                   AND EXISTS (
                        SELECT 1
                          FROM turn_attempt
                         WHERE session_id = $1
                           AND turn_id = $2
                           AND turn_attempt_id = $3
                           AND state_kind = 'ended'
                           AND end_variant = 'after_cancellation'
                           AND end_disposition = 'ambiguous'
                           AND interrupt_command_id IS NOT NULL
                           AND interrupt_predecessor_turn_id = $2
                   )
                   AND EXISTS (
                        SELECT 1
                          FROM turn_reconciliation_required_outbox_event
                         WHERE session_id = $1
                           AND turn_id = $2
                           AND model_call_id = $4
                           AND terminal_frontier_id =
                               lifecycle.terminal_frontier_id
                   )
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(correlation.attempt().into_uuid())
    .bind(observation.call().into_uuid())
    .fetch_one(&mut *connection)
    .await?)
}

async fn load_frontier_members(
    connection: &mut PgConnection,
    session: SessionId,
    frontier: Uuid,
) -> Result<Vec<(Uuid, Uuid)>, ModelCallRepositoryError> {
    Ok(sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT source_session_id, semantic_entry_id
           FROM context_frontier_member
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
          ORDER BY member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier)
    .fetch_all(&mut *connection)
    .await?)
}

async fn load_terminal_frontier(
    connection: &mut PgConnection,
    session: SessionId,
    frontier: Uuid,
) -> Result<Vec<StoredTerminalFrontierMember>, ModelCallRepositoryError> {
    sqlx::query(
        "SELECT member.source_session_id, member.semantic_entry_id,
                entry.payload_kind, entry.assistant_text_value,
                entry.producing_model_call_id, entry.completed_turn_id,
                entry.failed_turn_id, entry.cancelled_turn_id
           FROM context_frontier_member AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
          WHERE member.owning_session_id = $1
            AND member.context_frontier_id = $2
          ORDER BY member.member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier)
    .fetch_all(&mut *connection)
    .await?
    .into_iter()
    .map(|row| {
        Ok(StoredTerminalFrontierMember {
            source_session: required(&row, "source_session_id")?,
            entry: required(&row, "semantic_entry_id")?,
            payload_kind: required(&row, "payload_kind")?,
            assistant_text: row.try_get("assistant_text_value")?,
            producing_call: row.try_get("producing_model_call_id")?,
            completed_turn: row.try_get("completed_turn_id")?,
            failed_turn: row.try_get("failed_turn_id")?,
            cancelled_turn: row.try_get("cancelled_turn_id")?,
        })
    })
    .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredTerminalFrontierMember {
    source_session: Uuid,
    entry: Uuid,
    payload_kind: String,
    assistant_text: Option<String>,
    producing_call: Option<Uuid>,
    completed_turn: Option<Uuid>,
    failed_turn: Option<Uuid>,
    cancelled_turn: Option<Uuid>,
}

fn completed_terminal_frontier_matches(
    source_frontier: &[(Uuid, Uuid)],
    terminal_frontier: &[StoredTerminalFrontierMember],
    session: Uuid,
    turn: Uuid,
    call: Uuid,
    assistant_text: &[AssistantText],
) -> bool {
    if terminal_frontier.len() != source_frontier.len() + assistant_text.len() + 1 {
        return false;
    }
    if terminal_frontier
        .iter()
        .zip(source_frontier)
        .any(|(stored, expected)| (stored.source_session, stored.entry) != *expected)
    {
        return false;
    }
    let assistant_start = source_frontier.len();
    if terminal_frontier[assistant_start..assistant_start + assistant_text.len()]
        .iter()
        .zip(assistant_text)
        .any(|(stored, expected)| {
            stored.source_session != session
                || stored.payload_kind != "assistant_text"
                || stored.assistant_text.as_deref() != Some(expected.as_str())
                || stored.producing_call != Some(call)
                || stored.completed_turn.is_some()
                || stored.failed_turn.is_some()
                || stored.cancelled_turn.is_some()
        })
    {
        return false;
    }
    let completion = &terminal_frontier[assistant_start + assistant_text.len()];
    completion.source_session == session
        && completion.payload_kind == "turn_completed"
        && completion.assistant_text.is_none()
        && completion.producing_call.is_none()
        && completion.completed_turn == Some(turn)
        && completion.failed_turn.is_none()
        && completion.cancelled_turn.is_none()
}

fn failed_terminal_frontier_matches(
    source_frontier: &[(Uuid, Uuid)],
    terminal_frontier: &[StoredTerminalFrontierMember],
    session: Uuid,
    turn: Uuid,
) -> bool {
    if terminal_frontier.len() != source_frontier.len() + 1
        || terminal_frontier
            .iter()
            .zip(source_frontier)
            .any(|(stored, expected)| (stored.source_session, stored.entry) != *expected)
    {
        return false;
    }
    let failure = &terminal_frontier[source_frontier.len()];
    failure.source_session == session
        && failure.payload_kind == "turn_failed"
        && failure.assistant_text.is_none()
        && failure.producing_call.is_none()
        && failure.completed_turn.is_none()
        && failure.failed_turn == Some(turn)
        && failure.cancelled_turn.is_none()
}

fn pending_reclassification_candidates(
    execution: &ModelCallExecution,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<Vec<PendingSteeringReclassificationIdentity>, ModelCallRepositoryError> {
    pending_reclassification_candidates_for_active(execution.active_turn(), next_turn)
}

fn pending_reclassification_candidates_for_active(
    active_turn: &signalbox_domain::ActivatedAcceptedInputTurn,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<Vec<PendingSteeringReclassificationIdentity>, ModelCallRepositoryError> {
    let mut proposed_turns = BTreeSet::new();
    let mut reclassifications = Vec::new();
    for pending in active_turn.pending_steering() {
        let accepted_input = pending.accepted_input();
        let proposed_turn = next_turn(accepted_input);
        record_reclassified_turn_candidate(active_turn.turn(), proposed_turn, &mut proposed_turns)?;
        reclassifications.push(PendingSteeringReclassificationIdentity::new(
            accepted_input,
            proposed_turn,
        ));
    }
    Ok(reclassifications)
}

pub(crate) fn attach_interrupt_reclassification_candidates(
    identities: signalbox_domain::CancelledModelCallTurnIdentities,
    execution: &ModelCallExecution,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<signalbox_domain::CancelledModelCallTurnIdentities, ModelCallRepositoryError> {
    Ok(
        identities.with_pending_steering_reclassifications(pending_reclassification_candidates(
            execution, next_turn,
        )?),
    )
}

pub(crate) fn attach_recovery_interrupt_reclassification_candidates(
    identities: signalbox_domain::CancelledModelCallTurnIdentities,
    active_turn: &signalbox_domain::ActivatedAcceptedInputTurn,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<signalbox_domain::CancelledModelCallTurnIdentities, ModelCallRepositoryError> {
    Ok(identities.with_pending_steering_reclassifications(
        pending_reclassification_candidates_for_active(active_turn, next_turn)?,
    ))
}

fn record_reclassified_turn_candidate(
    source_turn: TurnId,
    proposed_turn: TurnId,
    proposed_turns: &mut BTreeSet<TurnId>,
) -> Result<(), ModelCallRepositoryError> {
    if proposed_turn == source_turn || !proposed_turns.insert(proposed_turn) {
        return Err(ModelCallRepositoryError::IdentityCollision(
            ModelCallIdentityCollision::ReclassifiedTurn,
        ));
    }
    Ok(())
}

fn attach_pending_reclassification_candidates(
    identities: ModelCallTerminalIdentities,
    execution: &ModelCallExecution,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<ModelCallTerminalIdentities, ModelCallRepositoryError> {
    let reclassifications = pending_reclassification_candidates(execution, next_turn)?;
    Ok(match identities {
        ModelCallTerminalIdentities::Completed(identities) => {
            ModelCallTerminalIdentities::Completed(
                identities.with_pending_steering_reclassifications(reclassifications),
            )
        }
        ModelCallTerminalIdentities::Failed(identities) => ModelCallTerminalIdentities::Failed(
            identities.with_pending_steering_reclassifications(reclassifications),
        ),
        ModelCallTerminalIdentities::PhysicalCancellation(identities) => {
            ModelCallTerminalIdentities::PhysicalCancellation(
                identities.with_pending_steering_reclassifications(reclassifications),
            )
        }
        ModelCallTerminalIdentities::Refused(identities) => ModelCallTerminalIdentities::Refused(
            identities.with_pending_steering_reclassifications(reclassifications),
        ),
        ModelCallTerminalIdentities::Ambiguous(identities) => {
            ModelCallTerminalIdentities::Ambiguous(
                identities.with_pending_steering_reclassifications(reclassifications),
            )
        }
    })
}

fn prepared_matches_authorized(
    prepared: &PreparedModelCallRequest,
    authorized: &AuthorizedModelCall,
) -> bool {
    prepared.session() == authorized.session()
        && prepared.turn() == authorized.turn()
        && prepared.attempt() == authorized.attempt().id()
        && prepared.call().id() == authorized.call().id()
        && prepared.call().selection() == authorized.call().selection()
        && prepared.call().target() == authorized.call().target()
        && prepared.call().frontier() == authorized.call().frontier()
        && prepared
            .frontier_entries()
            .eq(authorized.frontier_entries())
        && prepared
            .frontier_entries()
            .all(|entry| match entry.payload() {
                SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
                | SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input, ..
                } => {
                    prepared.origin_content(*accepted_input)
                        == authorized.origin_content(*accepted_input)
                }
                _ => true,
            })
}

fn prepared_matches_stopped(
    prepared: &PreparedModelCallRequest,
    execution: &ModelCallExecution,
    stopped: &StopRequestedModelCallTurn,
) -> bool {
    prepared.session() == stopped.session()
        && prepared.turn() == stopped.turn()
        && prepared.attempt() == stopped.attempt().id()
        && prepared.call().id() == stopped.call().id()
        && prepared.call().selection() == stopped.call().selection()
        && prepared.call().target() == stopped.call().target()
        && prepared.call().frontier() == stopped.call().frontier()
        && prepared.frontier_entries().eq(execution.frontier_entries())
        && prepared
            .frontier_entries()
            .all(|entry| match entry.payload() {
                SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
                | SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input, ..
                } => {
                    prepared.origin_content(*accepted_input)
                        == execution.origin_content(*accepted_input)
                }
                _ => true,
             })
}

async fn lock_session(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<(), ModelCallRepositoryError> {
    let (session_exists, scheduler): (bool, Option<Uuid>) =
        sqlx::query_as(crate::lock_inventory::START_ELIGIBLE_TURN)
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
    require_live_execution_with_targets(connection, requested_session, Some(targets)).await
}

pub(crate) async fn require_live_execution_for_restart(
    connection: &mut PgConnection,
    requested_session: SessionId,
) -> Result<ModelCallExecution, ModelCallRepositoryError> {
    require_live_execution_with_targets(connection, requested_session, None).await
}

async fn require_live_execution_with_targets(
    connection: &mut PgConnection,
    requested_session: SessionId,
    configured_targets: Option<&ModelTargetCatalog>,
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
    let active_turn = scheduling
        .active_turn_execution()
        .ok_or(ModelCallRepositoryError::NoLiveExecution)?;
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
    let (pinned_target, calls) =
        load_live_turn_calls(connection, requested_session, active_turn.turn()).await?;
    let call_snapshot = match calls
        .first()
        .filter(|call| call.frontier() != starting_snapshot.frontier().snapshot())
    {
        Some(call) => {
            Some(load_call_snapshot(connection, requested_session, call.frontier()).await?)
        }
        None => None,
    };
    let frontier_references = call_snapshot.as_ref().map_or_else(
        || starting_snapshot.ordered_entries().collect::<Vec<_>>(),
        |snapshot| snapshot.ordered_entries().to_vec(),
    );
    let frontier_entries = frontier_references
        .iter()
        .map(|reference| {
            scheduling
                .semantic_entry(*reference)
                .cloned()
                .ok_or(ModelCallCorruption::Missing("frontier semantic entry"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let pending_steering = active_turn
        .pending_steering()
        .iter()
        .map(signalbox_domain::PendingSteeringInput::accepted_input)
        .collect::<Vec<_>>();
    let origin_contents =
        load_origin_contents(connection, &frontier_entries, &pending_steering).await?;
    let recovered_targets;
    let targets = if let Some(targets) = configured_targets {
        targets.clone()
    } else {
        recovered_targets = ModelTargetCatalog::try_from_definitions(calls.iter().map(|call| {
            let direct = match call.selection() {
                FrozenModelSelection::Direct(direct) => direct,
                FrozenModelSelection::FrozenAlias { definition, .. } => definition.selected(),
            };
            ModelTargetDefinition::new(direct, call.target())
        }))
        .map_err(|_| ModelCallCorruption::Inconsistent("recovery model-target catalog"))?;
        recovered_targets
    };

    let mut input = ModelCallExecutionReconstitutionInput::new(
        active_turn,
        targets,
        starting_snapshot,
        frontier_entries,
        origin_contents,
        pinned_target,
        calls,
    );
    if let Some(call_snapshot) = call_snapshot {
        input = input.with_call_snapshot(call_snapshot);
    }
    input.reconstitute().map_err(|error| {
        let (_, failure) = error.into_parts();
        ModelCallCorruption::Execution(failure).into()
    })
}

async fn load_call_snapshot(
    connection: &mut PgConnection,
    session: SessionId,
    frontier: signalbox_domain::ContextFrontierId,
) -> Result<ResolvedContextFrontierReconstitutionInput, ModelCallRepositoryError> {
    let declared_count = sqlx::query_scalar::<_, Decimal>(
        "SELECT member_count
           FROM context_frontier
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing("model-call snapshot"))?;
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
    let actual_count = u64::try_from(rows.len())
        .map_err(|_| ModelCallCorruption::Inconsistent("model-call snapshot member count"))?;
    if declared_count != Decimal::from(actual_count) {
        return Err(ModelCallCorruption::Inconsistent("model-call snapshot member count").into());
    }
    let ordered_entries = rows
        .into_iter()
        .enumerate()
        .map(|(index, (position, source_session, semantic_entry))| {
            let expected_position = u64::try_from(index + 1).map_err(|_| {
                ModelCallCorruption::Inconsistent("model-call snapshot member positions")
            })?;
            if position != Decimal::from(expected_position) {
                return Err(ModelCallCorruption::Inconsistent(
                    "model-call snapshot member positions",
                )
                .into());
            }
            Ok(SemanticTranscriptEntryRef::from_source(
                session_id_from_uuid(source_session),
                signalbox_domain::SemanticTranscriptEntryId::from_uuid(semantic_entry),
            ))
        })
        .collect::<Result<Vec<_>, ModelCallRepositoryError>>()?;
    Ok(ResolvedContextFrontierReconstitutionInput::new(
        session,
        frontier,
        ordered_entries,
    ))
}

async fn load_origin_contents(
    connection: &mut PgConnection,
    entries: &[SemanticTranscriptEntry],
    pending_steering: &[AcceptedInputId],
) -> Result<Vec<ModelCallOriginContent>, ModelCallRepositoryError> {
    let accepted_inputs = entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { accepted_input, .. } => {
                Some(*accepted_input)
            }
            SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. }
            | SemanticTranscriptEntryPayload::AssistantText { .. }
            | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. } => None,
        })
        .chain(pending_steering.iter().copied())
        .collect::<BTreeSet<_>>();
    if accepted_inputs.is_empty() {
        return Ok(Vec::new());
    }
    let accepted_input_uuids = accepted_inputs
        .iter()
        .map(|accepted_input| accepted_input.into_uuid())
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT accepted_input_id, accepting_command_id
           FROM accepted_input
          WHERE accepted_input_id = ANY($1)
          ORDER BY accepted_input_id",
    )
    .bind(&accepted_input_uuids)
    .fetch_all(&mut *connection)
    .await?;
    if rows.len() != accepted_input_uuids.len() {
        return Err(ModelCallCorruption::Missing("accepted input receipt").into());
    }
    let mut loaded = BTreeSet::new();
    let mut command_by_accepted = BTreeMap::new();
    for row in rows {
        let accepted: Uuid = required(&row, "accepted_input_id")?;
        if !accepted_input_uuids.contains(&accepted) || !loaded.insert(accepted) {
            return Err(ModelCallCorruption::Inconsistent("accepted receipt inventory").into());
        }
        let command = durable_command_id_from_uuid(required(&row, "accepting_command_id")?)
            .map_err(|_| ModelCallCorruption::Inconsistent("accepting command identity"))?;
        if command_by_accepted
            .insert(AcceptedInputId::from_uuid(accepted), command)
            .is_some()
        {
            return Err(ModelCallCorruption::Inconsistent("accepted receipt inventory").into());
        }
    }
    let commands = command_by_accepted.values().copied().collect::<Vec<_>>();
    let recorded = require_recorded_batch(connection, &commands)
        .await
        .map_err(map_scheduling_error)?;
    accepted_inputs
        .into_iter()
        .map(|accepted| {
            let command = command_by_accepted
                .get(&accepted)
                .ok_or(ModelCallCorruption::Missing("accepted command correlation"))?;
            let submit = recorded
                .get(command)
                .ok_or(ModelCallCorruption::Missing("accepted submit command"))?;
            let content = ModelCallOriginContent::from_recorded_submit(submit)
                .ok_or(ModelCallCorruption::Inconsistent("accepted input content"))?;
            if content.accepted_input() != accepted {
                return Err(ModelCallCorruption::Inconsistent("accepted content identity").into());
            }
            Ok(content)
        })
        .collect()
}

async fn load_live_turn_calls(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<
    (
        Option<PinnedProviderTargetReconstitutionInput>,
        Vec<ModelCallReconstitutionInput>,
    ),
    ModelCallRepositoryError,
> {
    let lifecycle = sqlx::query(
        "SELECT pinned_provider_model_identity_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing("live turn lifecycle"))?;
    let pinned_identity: Option<Uuid> = lifecycle.try_get("pinned_provider_model_identity_id")?;
    let pinned_target = pinned_identity.map(|identity| {
        PinnedProviderTargetReconstitutionInput::new(
            turn,
            ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(identity)),
        )
    });
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
    Ok((
        pinned_target,
        rows.into_iter()
            .map(decode_model_call)
            .collect::<Result<_, _>>()?,
    ))
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
    credential_reference: &ModelCallCredentialReference,
) -> Result<(), ModelCallRepositoryError> {
    let call = prepared.call();
    let (kind, direct, alias, alias_selected) = encode_selection(call.selection());
    for steering in prepared.consumed_steering() {
        let SemanticTranscriptEntryPayload::SteeringAcceptedInput {
            accepted_input,
            source_turn,
        } = steering.semantic_entry().payload()
        else {
            return Err(ModelCallCorruption::Inconsistent("steering semantic payload").into());
        };
        if *source_turn != prepared.turn()
            || *accepted_input != steering.accepted_input().id()
            || !matches!(
                steering.accepted_input().disposition(),
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: consuming_call
                } if *consuming_call == call.id()
            )
        {
            return Err(
                ModelCallCorruption::Inconsistent("steering consumption correlation").into(),
            );
        }
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 origin_accepted_input_id, steering_source_turn_id)
             VALUES ($1, $2, 'steering_accepted_input', $3, $4)",
        )
        .bind(session_id_to_uuid(
            steering.semantic_entry().source_session(),
        ))
        .bind(steering.semantic_entry().identity().into_uuid())
        .bind(accepted_input.into_uuid())
        .bind(turn_id_to_uuid(*source_turn))
        .execute(&mut *connection)
        .await?;
    }
    if let Some(snapshot) = prepared.steering_snapshot() {
        insert_snapshot(connection, snapshot).await?;
    }
    for steering in prepared.consumed_steering() {
        let rows = sqlx::query(
            "UPDATE accepted_input
                SET disposition_kind = 'consumed_as_steering',
                    consuming_model_call_id = $1
              WHERE accepted_input_id = $2
                AND session_id = $3
                AND disposition_kind = 'pending_steering'
                AND origin_turn_id IS NULL
                AND consuming_model_call_id IS NULL
                AND delivery_kind = 'next_safe_point'
                AND expected_active_turn_id = $4",
        )
        .bind(call.id().into_uuid())
        .bind(steering.accepted_input().id().into_uuid())
        .bind(session_id_to_uuid(prepared.session()))
        .bind(turn_id_to_uuid(prepared.turn()))
        .execute(&mut *connection)
        .await?
        .rows_affected();
        require_single(rows, "consumed steering accepted input")?;
    }
    let pinned_rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET pinned_provider_model_identity_id = $1
          WHERE turn_id = $2
            AND session_id = $3
            AND current_attempt_id = $4
            AND state_kind = 'active'
            AND active_phase_kind = 'running'
            AND pinned_provider_model_identity_id IS NULL",
    )
    .bind(call.target().identity().into_uuid())
    .bind(turn_id_to_uuid(prepared.turn()))
    .bind(session_id_to_uuid(prepared.session()))
    .bind(prepared.attempt().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(pinned_rows, "turn-level provider target pin")?;
    sqlx::query(
        "INSERT INTO model_call
            (model_call_id, turn_id, session_id, turn_attempt_id,
             selection_kind, direct_model_selection_id, frozen_model_alias_id,
             frozen_alias_selected_direct_id, resolved_provider_model_identity_id,
             context_frontier_id, credential_reference, state_kind,
             terminal_disposition_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'prepared', NULL)",
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
    .bind(credential_reference.as_str())
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

async fn load_call_credential_reference(
    connection: &mut PgConnection,
    session: SessionId,
    call: ModelCallId,
) -> Result<ModelCallCredentialReference, ModelCallRepositoryError> {
    let reference = sqlx::query_scalar::<_, Option<String>>(
        "SELECT credential_reference
           FROM model_call
          WHERE session_id = $1
            AND model_call_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(call.into_uuid())
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing("prepared model call"))?
    .ok_or(ModelCallCorruption::Missing(
        "model-call credential reference",
    ))?;
    Ok(ModelCallCredentialReference::new(reference))
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

pub(crate) async fn persist_stop_requested(
    connection: &mut PgConnection,
    stopped: &StopRequestedModelCallTurn,
) -> Result<(), ModelCallRepositoryError> {
    let proof = stopped.interrupt();
    let attempt_rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'stop_requested',
                interrupt_command_id = $1,
                interrupt_predecessor_turn_id = $2
          WHERE turn_attempt_id = $3
            AND turn_id = $4
            AND session_id = $5
            AND state_kind = 'running'
            AND end_variant IS NULL
            AND end_disposition IS NULL
            AND interrupt_command_id IS NULL
            AND interrupt_predecessor_turn_id IS NULL",
    )
    .bind(durable_command_id_to_uuid(proof.command()))
    .bind(turn_id_to_uuid(proof.predecessor()))
    .bind(stopped.attempt().id().into_uuid())
    .bind(turn_id_to_uuid(stopped.turn()))
    .bind(session_id_to_uuid(stopped.session()))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(attempt_rows, "stop-requested turn attempt")?;

    let call_rows = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'cancellation_requested'
          WHERE model_call_id = $1
            AND turn_id = $2
            AND session_id = $3
            AND turn_attempt_id = $4
            AND state_kind = 'in_flight'
            AND terminal_disposition_kind IS NULL",
    )
    .bind(stopped.call().id().into_uuid())
    .bind(turn_id_to_uuid(stopped.turn()))
    .bind(session_id_to_uuid(stopped.session()))
    .bind(stopped.attempt().id().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(call_rows, "cancellation-requested model call")?;
    outbox::append(
        connection,
        OutboxEvent::ModelCallTransition {
            session: stopped.session(),
            turn: stopped.turn(),
            call: stopped.call().id(),
            state: ModelCallOutboxState::CancellationRequested,
        },
    )
    .await?;
    Ok(())
}

pub(crate) async fn persist_terminal_outcome(
    connection: &mut PgConnection,
    outcome: &ModelCallTerminalOutcome,
) -> Result<(), ModelCallRepositoryError> {
    match outcome {
        ModelCallTerminalOutcome::Completed(completed) => {
            persist_completed(connection, completed).await
        }
        ModelCallTerminalOutcome::Failed(failed) => persist_failed(connection, failed).await,
        ModelCallTerminalOutcome::Cancelled(cancelled) => {
            persist_cancelled(connection, cancelled).await
        }
        ModelCallTerminalOutcome::Refused(refused) => persist_refused(connection, refused).await,
        ModelCallTerminalOutcome::ReconciliationRequired(reconciliation) => {
            persist_reconciliation_required(connection, reconciliation).await
        }
        ModelCallTerminalOutcome::AwaitingRecovery(ambiguous) => {
            persist_ambiguous(connection, ambiguous).await
        }
    }
}

async fn persist_reconciliation_required(
    connection: &mut PgConnection,
    reconciliation: &ReconciliationRequiredModelCallTurn,
) -> Result<(), ModelCallRepositoryError> {
    let call_already_ambiguous = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM model_call
             WHERE model_call_id = $1
               AND turn_id = $2
               AND session_id = $3
               AND turn_attempt_id = $4
               AND state_kind = 'terminal'
               AND terminal_disposition_kind = 'ambiguous'
        )",
    )
    .bind(reconciliation.call().id().into_uuid())
    .bind(turn_id_to_uuid(reconciliation.turn()))
    .bind(session_id_to_uuid(reconciliation.session()))
    .bind(reconciliation.attempt().id().into_uuid())
    .fetch_one(&mut *connection)
    .await?;
    if !call_already_ambiguous {
        persist_ended_call(
            connection,
            reconciliation.session(),
            reconciliation.turn(),
            reconciliation.call(),
        )
        .await?;
    }
    if !call_already_ambiguous {
        persist_ended_attempt(
            connection,
            reconciliation.session(),
            reconciliation.turn(),
            reconciliation.attempt(),
        )
        .await?;
    }
    insert_snapshot(connection, reconciliation.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        reconciliation.session(),
        reconciliation.turn(),
        reconciliation.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        reconciliation.session(),
        reconciliation.turn(),
        "reconciliation_required",
        reconciliation.terminal_snapshot().frontier().snapshot(),
        Some(reconciliation.attempt().id()),
        Some(reconciliation.call().id()),
    )
    .await?;
    if !call_already_ambiguous {
        append_terminal_call_event(
            connection,
            reconciliation.session(),
            reconciliation.turn(),
            reconciliation.call(),
        )
        .await?;
    }
    outbox::append(
        connection,
        OutboxEvent::TurnReconciliationRequired {
            session: reconciliation.session(),
            turn: reconciliation.turn(),
            call: reconciliation.call().id(),
            terminal_frontier: reconciliation.terminal_snapshot().frontier().snapshot(),
        },
    )
    .await?;
    Ok(())
}

async fn persist_cancelled(
    connection: &mut PgConnection,
    cancelled: &CancelledModelCallTurn,
) -> Result<(), ModelCallRepositoryError> {
    if let Some(call) = cancelled.call() {
        persist_ended_call(connection, cancelled.session(), cancelled.turn(), call).await?;
    }
    persist_ended_attempt(
        connection,
        cancelled.session(),
        cancelled.turn(),
        cancelled.attempt(),
    )
    .await?;
    let entry = cancelled.cancellation_entry();
    if !matches!(
        entry.payload(),
        SemanticTranscriptEntryPayload::TurnCancelled { turn } if *turn == cancelled.turn()
    ) {
        return Err(ModelCallCorruption::Inconsistent("cancellation entry payload").into());
    }
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind, cancelled_turn_id)
         VALUES ($1, $2, 'turn_cancelled', $3)",
    )
    .bind(session_id_to_uuid(entry.source_session()))
    .bind(entry.identity().into_uuid())
    .bind(turn_id_to_uuid(cancelled.turn()))
    .execute(&mut *connection)
    .await?;
    insert_snapshot(connection, cancelled.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        cancelled.session(),
        cancelled.turn(),
        cancelled.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        cancelled.session(),
        cancelled.turn(),
        "cancelled",
        cancelled.terminal_snapshot().frontier().snapshot(),
        Some(cancelled.attempt().id()),
        cancelled.call().map(signalbox_domain::EndedModelCall::id),
    )
    .await?;
    if let Some(call) = cancelled.call() {
        append_terminal_call_event(connection, cancelled.session(), cancelled.turn(), call).await?;
    }
    outbox::append(
        connection,
        OutboxEvent::TurnCancelled {
            session: cancelled.session(),
            turn: cancelled.turn(),
            cancellation_entry: entry.identity(),
            terminal_frontier: cancelled.terminal_snapshot().frontier().snapshot(),
        },
    )
    .await?;
    Ok(())
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
    persist_reclassified_pending_steering(
        connection,
        completed.session(),
        completed.turn(),
        completed.reclassified_pending_steering(),
    )
    .await?;
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
    persist_reclassified_pending_steering(
        connection,
        failed.session(),
        failed.turn(),
        failed.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        failed.session(),
        failed.turn(),
        "failed",
        failed.terminal_snapshot().frontier().snapshot(),
        Some(failed.attempt().id()),
        failed.call().map(signalbox_domain::EndedModelCall::id),
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
    persist_reclassified_pending_steering(
        connection,
        refused.session(),
        refused.turn(),
        refused.reclassified_pending_steering(),
    )
    .await?;
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

async fn persist_reclassified_pending_steering(
    connection: &mut PgConnection,
    session: SessionId,
    source_turn: TurnId,
    successors: &[ReclassifiedPendingSteeringTurn],
) -> Result<(), ModelCallRepositoryError> {
    for successor in successors {
        let AcceptedInputDisposition::ReclassifiedAsTurnOrigin { turn, .. } =
            successor.accepted_input().disposition()
        else {
            return Err(ModelCallCorruption::Inconsistent(
                "reclassified accepted-input disposition",
            )
            .into());
        };
        if successor.session() != session
            || successor.source_turn() != source_turn
            || successor.binding().source_turn() != source_turn
            || *turn != successor.turn()
        {
            return Err(
                ModelCallCorruption::Inconsistent("reclassified successor correlation").into(),
            );
        }

        let accepted_rows = sqlx::query(
            "UPDATE accepted_input
                SET disposition_kind = 'reclassified_as_turn_origin',
                    origin_turn_id = $1
              WHERE accepted_input_id = $2
                AND session_id = $3
                AND acceptance_position = $4
                AND delivery_kind = 'next_safe_point'
                AND expected_active_turn_id = $5
                AND disposition_kind = 'pending_steering'
                AND origin_turn_id IS NULL",
        )
        .bind(turn_id_to_uuid(successor.turn()))
        .bind(successor.accepted_input().id().into_uuid())
        .bind(session_id_to_uuid(session))
        .bind(crate::mapping::input_position_to_numeric(
            successor.order().acceptance_position(),
        ))
        .bind(turn_id_to_uuid(source_turn))
        .execute(&mut *connection)
        .await?
        .rows_affected();
        require_single(accepted_rows, "pending-steering reclassification")?;

        let (frozen_kind, frozen_direct, frozen_alias, frozen_alias_selected) =
            match successor.effective_configuration().model() {
                FrozenModelSelection::Direct(selection) => {
                    ("direct", Some(selection.into_uuid()), None, None)
                }
                FrozenModelSelection::FrozenAlias { alias, definition } => (
                    "frozen_alias",
                    None,
                    Some(alias.into_uuid()),
                    Some(definition.selected().into_uuid()),
                ),
            };
        let queue_rows = sqlx::query(
            "WITH RECURSIVE source_configuration AS (
                SELECT stored.*
                  FROM queued_input_origin AS stored
                 WHERE stored.turn_id = $5
                   AND stored.session_id = $3
                UNION
                SELECT ancestor.*
                  FROM source_configuration AS current
                  JOIN queued_input_origin AS ancestor
                    ON ancestor.turn_id = current.source_configuration_turn_id
                   AND ancestor.session_id = current.session_id
             )
             INSERT INTO queued_input_origin
                (turn_id, accepted_input_id, session_id, acceptance_position,
                 priority_kind, source_configuration_turn_id)
             SELECT
                $1, accepted.accepted_input_id, accepted.session_id,
                accepted.acceptance_position, 'ordinary', source.turn_id
               FROM accepted_input AS accepted
               JOIN queued_input_origin AS source
                 ON source.turn_id = $5
                AND source.session_id = accepted.session_id
               JOIN source_configuration AS resolved
                 ON resolved.source_configuration_turn_id IS NULL
              WHERE accepted.accepted_input_id = $2
                AND accepted.session_id = $3
                AND accepted.acceptance_position = $4
                AND accepted.disposition_kind = 'reclassified_as_turn_origin'
                AND accepted.origin_turn_id = $1
                AND accepted.expected_active_turn_id = $5
                AND source.acceptance_position < accepted.acceptance_position
                AND resolved.frozen_model_kind = $6
                AND resolved.frozen_direct_model_selection_id IS NOT DISTINCT FROM $7
                AND resolved.frozen_model_alias_id IS NOT DISTINCT FROM $8
                AND resolved.frozen_alias_selected_direct_id IS NOT DISTINCT FROM $9",
        )
        .bind(turn_id_to_uuid(successor.turn()))
        .bind(successor.accepted_input().id().into_uuid())
        .bind(session_id_to_uuid(session))
        .bind(crate::mapping::input_position_to_numeric(
            successor.order().acceptance_position(),
        ))
        .bind(turn_id_to_uuid(source_turn))
        .bind(frozen_kind)
        .bind(frozen_direct)
        .bind(frozen_alias)
        .bind(frozen_alias_selected)
        .execute(&mut *connection)
        .await?
        .rows_affected();
        require_single(queue_rows, "reclassified successor queue")?;

        let lifecycle_rows = sqlx::query(
            "INSERT INTO turn_lifecycle
                (turn_id, session_id, origin_accepted_input_id,
                 acceptance_position, state_kind)
             VALUES ($1, $2, $3, $4, 'queued')",
        )
        .bind(turn_id_to_uuid(successor.turn()))
        .bind(session_id_to_uuid(session))
        .bind(successor.accepted_input().id().into_uuid())
        .bind(crate::mapping::input_position_to_numeric(
            successor.order().acceptance_position(),
        ))
        .execute(&mut *connection)
        .await?
        .rows_affected();
        require_single(lifecycle_rows, "reclassified successor lifecycle")?;
    }
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
    let (variant, disposition, interrupt_command, interrupt_predecessor) =
        encode_attempt_end(attempt.end())?;
    let rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = $1,
                end_disposition = $2,
                interrupt_command_id = COALESCE(interrupt_command_id, $3),
                interrupt_predecessor_turn_id =
                    COALESCE(interrupt_predecessor_turn_id, $4)
          WHERE turn_attempt_id = $5
            AND turn_id = $6
            AND session_id = $7
            AND (
                (
                    state_kind IN ('prepared', 'running', 'stop_requested')
                    AND end_variant IS NULL
                    AND end_disposition IS NULL
                )
            )
            AND (
                (
                    $3::uuid IS NULL
                    AND interrupt_command_id IS NULL
                    AND interrupt_predecessor_turn_id IS NULL
                )
                OR (
                    $3::uuid IS NOT NULL
                    AND (
                        interrupt_command_id IS NULL
                        OR interrupt_command_id = $3
                    )
                    AND (
                        interrupt_predecessor_turn_id IS NULL
                        OR interrupt_predecessor_turn_id = $4
                    )
                )
            )",
    )
    .bind(variant)
    .bind(disposition)
    .bind(interrupt_command)
    .bind(interrupt_predecessor)
    .bind(attempt.id().into_uuid())
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "terminal model-call attempt")
}

type EncodedAttemptEnd = (&'static str, &'static str, Option<Uuid>, Option<Uuid>);

fn encode_attempt_end(
    end: &signalbox_domain::AttemptEnd,
) -> Result<EncodedAttemptEnd, ModelCallRepositoryError> {
    match end {
        signalbox_domain::AttemptEnd::WithoutStop { disposition } => {
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
            Ok(("without_stop", disposition, None, None))
        }
        signalbox_domain::AttemptEnd::AfterCancellation { cause, disposition } => {
            let disposition = match disposition {
                signalbox_domain::CancellationStopDisposition::TurnCompleted => "turn_completed",
                signalbox_domain::CancellationStopDisposition::TurnRefused => "turn_refused",
                signalbox_domain::CancellationStopDisposition::KnownFailure => "known_failure",
                signalbox_domain::CancellationStopDisposition::Lost => "lost",
                signalbox_domain::CancellationStopDisposition::Cancelled => "cancelled",
                signalbox_domain::CancellationStopDisposition::Ambiguous => "ambiguous",
            };
            Ok((
                "after_cancellation",
                disposition,
                Some(durable_command_id_to_uuid(cause.command())),
                Some(turn_id_to_uuid(cause.predecessor())),
            ))
        }
        signalbox_domain::AttemptEnd::AfterFatalMismatch { .. } => {
            Err(ModelCallRepositoryError::InvalidTransition(
                "initial model execution cannot persist fatal-mismatch attempt history",
            ))
        }
    }
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
            AND (
                (
                    active_phase_kind = 'running'
                    AND recovery_model_call_id IS NULL
                )
                OR (
                    $4 = 'reconciliation_required'
                    AND active_phase_kind = 'awaiting_model_call_recovery'
                    AND recovery_model_call_id = $3
                )
            )",
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
        SubmitInputRepositoryError::ModelExecution(_) => {
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
        Some(
            "accepted_input_origin_turn_id_key"
            | "queued_input_origin_pkey"
            | "turn_lifecycle_pkey",
        ) => Some(ModelCallIdentityCollision::ReclassifiedTurn),
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
    use std::{borrow::Cow, collections::BTreeSet, error::Error, fmt, io};

    use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
    use signalbox_domain::{AssistantText, TurnId};
    use sqlx::{
        error::{DatabaseError, ErrorKind},
        types::Uuid,
    };

    use super::{
        ModelCallIdentityCollision, ModelCallRepositoryError, StoredTerminalFrontierMember,
        commit_failure_is_ambiguous, completed_terminal_frontier_matches,
        failed_terminal_frontier_matches, record_reclassified_turn_candidate,
    };

    /// docs/spec/model-call-execution.md: a source-turn successor candidate is
    /// a retryable minted-ID collision, not a caller transition defect.
    #[test]
    fn generated_successor_source_candidate_is_a_retryable_collision() {
        let source = TurnId::from_uuid(Uuid::from_u128(1));
        let mut proposed = BTreeSet::new();

        assert!(matches!(
            record_reclassified_turn_candidate(source, source, &mut proposed),
            Err(ModelCallRepositoryError::IdentityCollision(
                ModelCallIdentityCollision::ReclassifiedTurn
            ))
        ));
    }

    /// docs/spec/model-call-execution.md: a duplicate successor candidate is a
    /// retryable minted-ID collision, not a caller transition defect.
    #[test]
    fn generated_successor_duplicate_is_a_retryable_collision() {
        let source = TurnId::from_uuid(Uuid::from_u128(1));
        let successor = TurnId::from_uuid(Uuid::from_u128(2));
        let mut proposed = BTreeSet::new();

        record_reclassified_turn_candidate(source, successor, &mut proposed)
            .expect("the first source-safe successor is accepted");
        assert!(matches!(
            record_reclassified_turn_candidate(source, successor, &mut proposed),
            Err(ModelCallRepositoryError::IdentityCollision(
                ModelCallIdentityCollision::ReclassifiedTurn
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
        let transaction_resolution_unknown =
            sqlx::Error::Database(Box::new(ServerCommitFailure { code: "08007" }));
        assert!(commit_failure_is_ambiguous(&transaction_resolution_unknown));

        let statement_completion_unknown =
            sqlx::Error::Database(Box::new(ServerCommitFailure { code: "40003" }));
        assert!(commit_failure_is_ambiguous(&statement_completion_unknown));
    }

    /// docs/spec/model-call-execution.md: a retained completed observation is
    /// present only when the terminal frontier is the exact source prefix,
    /// assistant sequence, and final `TurnCompleted` marker.
    #[test]
    fn completed_reread_requires_exact_terminal_frontier_shape() {
        let session = Uuid::from_u128(1);
        let turn = Uuid::from_u128(2);
        let call = Uuid::from_u128(3);
        let source = vec![(Uuid::from_u128(4), Uuid::from_u128(5))];
        let assistant = vec![
            AssistantText::try_new(String::from("exact reply")).expect("fixture text is admitted"),
        ];
        let prefix = StoredTerminalFrontierMember {
            source_session: source[0].0,
            entry: source[0].1,
            payload_kind: String::from("origin_accepted_input"),
            assistant_text: None,
            producing_call: None,
            completed_turn: None,
            failed_turn: None,
            cancelled_turn: None,
        };
        let assistant_member = StoredTerminalFrontierMember {
            source_session: session,
            entry: Uuid::from_u128(6),
            payload_kind: String::from("assistant_text"),
            assistant_text: Some(String::from("exact reply")),
            producing_call: Some(call),
            completed_turn: None,
            failed_turn: None,
            cancelled_turn: None,
        };
        let completion = StoredTerminalFrontierMember {
            source_session: session,
            entry: Uuid::from_u128(7),
            payload_kind: String::from("turn_completed"),
            assistant_text: None,
            producing_call: None,
            completed_turn: Some(turn),
            failed_turn: None,
            cancelled_turn: None,
        };
        let exact = vec![prefix.clone(), assistant_member.clone(), completion.clone()];
        assert!(completed_terminal_frontier_matches(
            &source, &exact, session, turn, call, &assistant,
        ));

        assert!(!completed_terminal_frontier_matches(
            &source,
            &[prefix.clone(), assistant_member.clone()],
            session,
            turn,
            call,
            &assistant,
        ));
        let mut extra = exact.clone();
        extra.insert(1, prefix.clone());
        assert!(!completed_terminal_frontier_matches(
            &source, &extra, session, turn, call, &assistant,
        ));
        let mut wrong_marker = completion;
        wrong_marker.completed_turn = Some(Uuid::from_u128(8));
        assert!(!completed_terminal_frontier_matches(
            &source,
            &[prefix, assistant_member, wrong_marker],
            session,
            turn,
            call,
            &assistant,
        ));
    }

    /// docs/spec/model-call-execution.md: a retained failed observation is
    /// present only when its terminal frontier is the exact source prefix
    /// plus one matching failure marker.
    #[test]
    fn failed_reread_requires_exact_terminal_frontier_shape() {
        let session = Uuid::from_u128(1);
        let turn = Uuid::from_u128(2);
        let source = vec![(Uuid::from_u128(3), Uuid::from_u128(4))];
        let prefix = StoredTerminalFrontierMember {
            source_session: source[0].0,
            entry: source[0].1,
            payload_kind: String::from("origin_accepted_input"),
            assistant_text: None,
            producing_call: None,
            completed_turn: None,
            failed_turn: None,
            cancelled_turn: None,
        };
        let failure = StoredTerminalFrontierMember {
            source_session: session,
            entry: Uuid::from_u128(5),
            payload_kind: String::from("turn_failed"),
            assistant_text: None,
            producing_call: None,
            completed_turn: None,
            failed_turn: Some(turn),
            cancelled_turn: None,
        };
        assert!(failed_terminal_frontier_matches(
            &source,
            &[prefix.clone(), failure.clone()],
            session,
            turn,
        ));

        let mut wrong_failure = failure;
        wrong_failure.failed_turn = Some(Uuid::from_u128(6));
        assert!(!failed_terminal_frontier_matches(
            &source,
            &[prefix.clone(), wrong_failure],
            session,
            turn,
        ));
        assert!(!failed_terminal_frontier_matches(
            &source,
            &[prefix],
            session,
            turn,
        ));
    }
}
