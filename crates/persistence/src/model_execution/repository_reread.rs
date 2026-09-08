use super::credential_pool::committed_availability_successor_backoff;
use super::delegated_result::{
    ExpectedDelegatedChildResult, delegated_observation_result_matches,
    delegated_terminal_result_matches,
};
use super::delegation_lock::{
    lock_model_call_terminal_frontier, locked_delegation_logical_terminal,
};
use super::live_turn::{lock_session, require_live_execution, require_live_execution_for_restart};
use super::load::{decode_model_call, require_exact_call};
use super::persist_disposition::{decode_stored_model_call_observation, encode_token_usage};
use super::persist_terminal::{
    persist_failed_with_delegated_child_result, persist_terminal_outcome,
};
use super::reread::{
    failed_turn_closure_matches, load_frontier_members, pending_reclassification_candidates,
    prepared_cancellation_closure_matches, prepared_matches_authorized, prepared_matches_stopped,
    terminal_observation_closure_matches,
};
use super::{
    ModelCallCorruption, ModelCallRepositoryError, PostgresModelCallRepository, encode_disposition,
    encode_provider_failure_cause, finish_commit, prepared_failure_cause,
};
use crate::mapping::{session_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid};
use rust_decimal::Decimal;
use signalbox_application::{
    AttachmentPreparationFailure, ModelCallAuthorizationReread, PreparedModelCallFailureCause,
    RetainedModelCallObservationStatus, RetainedPreparedFailureStatus,
};
use signalbox_domain::{
    AcceptedInputId, CorrelatedModelCallTerminalObservation, FailedModelCallTurn,
    FailedModelCallTurnIdentities, ModelCallDisposition, ModelCallId, ModelCallReconstitutionState,
    ModelCallTerminalOutcome, ProviderReportedTokenUsage, SessionId, TurnId, TurnTerminalCause,
};
use sqlx::PgConnection;
use sqlx::types::Uuid;

impl PostgresModelCallRepository {
    /// Atomically closes a trustworthy prepared failure before send.
    pub async fn fail_prepared_call<NextTurn>(
        &self,
        session: SessionId,
        call: ModelCallId,
        cause: PreparedModelCallFailureCause,
        attachment_failure: Option<AttachmentPreparationFailure>,
        identities: FailedModelCallTurnIdentities,
        mut next_reclassified_turn: NextTurn,
    ) -> Result<FailedModelCallTurn, ModelCallRepositoryError>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_model_call_terminal_frontier(&mut transaction, session, call).await?;
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
                        "prepared failure requires a Prepared call",
                    )
                })?;
            persist_failed_with_delegated_child_result(
                &mut transaction,
                &failed,
                prepared_failure_cause(cause, attachment_failure),
                ProviderReportedTokenUsage::unreported(),
                None,
                attachment_failure,
            )
            .await?;
            Ok(failed)
        }
        .await;
        let result = match result {
            Ok(outcome) => {
                self.settle_runner_replacement_after_observation(&mut transaction, session, None)
                    .await?;
                Ok(outcome)
            }
            Err(error) => Err(error),
        };
        finish_commit(transaction, result).await
    }

    /// Closes a freshly activated call-free turn after required automatic
    /// context compaction failed in the same transaction.
    pub(crate) async fn fail_automatic_compaction_in_transaction(
        &self,
        connection: &mut PgConnection,
        session: SessionId,
        turn: TurnId,
        identities: FailedModelCallTurnIdentities,
        terminal_cause: TurnTerminalCause,
        recovery_cause: Option<crate::goal::GoalExecutionFailureRecoveryCause>,
    ) -> Result<FailedModelCallTurn, ModelCallRepositoryError> {
        let execution = require_live_execution(connection, session, &self.targets).await?;
        if execution.turn() != turn || execution.current_call().is_some() {
            return Err(ModelCallRepositoryError::InvalidTransition(
                "automatic compaction failure does not match fresh call-free execution",
            ));
        }
        let failed = execution
            .fail_automatic_context_compaction(identities)
            .map_err(|_| {
                ModelCallRepositoryError::InvalidTransition(
                    "automatic compaction failure could not close fresh execution",
                )
            })?;
        persist_failed_with_delegated_child_result(
            connection,
            &failed,
            terminal_cause,
            ProviderReportedTokenUsage::unreported(),
            None,
            None,
        )
        .await?;
        if let Some(cause) = recovery_cause {
            crate::goal::record_execution_failure_recovery_cause(connection, session, turn, cause)
                .await?;
        }
        Ok(failed)
    }

    /// Rereads whether an unchanged pre-send prepared failure committed.
    pub async fn reread_prepared_failure(
        &self,
        session: SessionId,
        call: ModelCallId,
        attachment_failure: Option<AttachmentPreparationFailure>,
    ) -> Result<RetainedPreparedFailureStatus, ModelCallRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            lock_session(&mut transaction, session).await?;
            let stored = sqlx::query_as::<
                _,
                (
                    Uuid,
                    Uuid,
                    Uuid,
                    String,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    Option<Decimal>,
                ),
            >(
                "SELECT turn_id, turn_attempt_id, context_frontier_id, state_kind,
                        terminal_disposition_kind, terminal_provider_failure_cause,
                        terminal_attachment_preparation_failure_cause,
                        terminal_attachment_preparation_failure_maximum_bytes
                   FROM model_call
                  WHERE session_id = $1
                    AND model_call_id = $2",
            )
            .bind(session_id_to_uuid(session))
            .bind(call.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(ModelCallCorruption::Missing(
                "retained prepared-failure model call",
            ))?;
            let (
                turn,
                attempt,
                source_frontier,
                state,
                disposition,
                provider_failure_cause,
                stored_attachment_failure,
                stored_attachment_maximum,
            ) = stored;
            match (state.as_str(), disposition.as_deref()) {
                ("prepared", None) => {
                    let execution = require_exact_call(
                        require_live_execution(&mut transaction, session, &self.targets).await?,
                        call,
                    )?;
                    execution.resume_prepared_call().map_err(|_| {
                        ModelCallRepositoryError::InvalidTransition(
                            "retained prepared failure could not resume Prepared",
                        )
                    })?;
                    Ok(RetainedPreparedFailureStatus::Pending)
                }
                ("terminal", Some("known_failed")) => {
                    let (expected_attachment_failure, expected_attachment_maximum) =
                        match attachment_failure {
                            Some(AttachmentPreparationFailure::TooLarge { maximum_bytes }) => (
                                Some("too_large"),
                                Some(Decimal::from(maximum_bytes)),
                            ),
                            Some(AttachmentPreparationFailure::Missing) => (Some("missing"), None),
                            Some(AttachmentPreparationFailure::Corrupt) => (Some("corrupt"), None),
                            Some(AttachmentPreparationFailure::Unavailable) => {
                                return Err(ModelCallRepositoryError::InvalidTransition(
                                    "retryable attachment unavailability cannot have a terminal closure",
                                ));
                            }
                            None => (None, None),
                        };
                    if provider_failure_cause.is_some()
                        || stored_attachment_failure.as_deref() != expected_attachment_failure
                        || stored_attachment_maximum != expected_attachment_maximum
                    {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "retained capability failure durable cause changed",
                        ));
                    }
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
                    let delegated_result_matches = delegated_terminal_result_matches(
                        &mut transaction,
                        session,
                        turn_id_from_uuid(turn),
                        &ExpectedDelegatedChildResult::Failed,
                    )
                    .await?;
                    if transition_history_matches && closure_matches && delegated_result_matches {
                        Ok(RetainedPreparedFailureStatus::AlreadyCommitted)
                    } else {
                        Err(ModelCallRepositoryError::InvalidTransition(
                            "retained prepared failure durable closure is incomplete",
                        ))
                    }
                }
                ("terminal", Some("cancelled")) => {
                    if prepared_cancellation_closure_matches(
                        &mut transaction,
                        session,
                        turn,
                        attempt,
                        call.into_uuid(),
                        source_frontier,
                    )
                    .await?
                    {
                        Ok(RetainedPreparedFailureStatus::Cancelled)
                    } else {
                        Err(ModelCallRepositoryError::InvalidTransition(
                            "retained prepared failure cancellation closure is incomplete",
                        ))
                    }
                }
                _ => Err(ModelCallRepositoryError::InvalidTransition(
                    "retained prepared failure durable state changed",
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
            let stored = sqlx::query(
                "SELECT call.model_call_id, call.turn_id, call.turn_attempt_id,
                        call.selection_kind, call.direct_model_selection_id,
                        call.frozen_model_alias_id, call.frozen_alias_selected_direct_id,
                        call.resolved_provider_model_identity_id, call.context_frontier_id,
                        call.state_kind, call.terminal_disposition_kind,
                        manifest.turn_instruction_manifest_id,
                        manifest.boundary_kind AS instruction_manifest_boundary_kind,
                        manifest.eligibility_hash_algorithm
                            AS instruction_eligibility_hash_algorithm,
                        manifest.eligibility_hash AS instruction_eligibility_hash,
                        manifest.admitted_set_hash_algorithm
                            AS instruction_admitted_set_hash_algorithm,
                        manifest.admitted_set_hash AS instruction_admitted_set_hash,
                        manifest.manifest_hash_algorithm
                            AS instruction_manifest_hash_algorithm,
                        manifest.manifest_hash AS instruction_manifest_hash,
                        discovery.scan_complete AS instruction_discovery_complete
                   FROM model_call AS call
              LEFT JOIN turn_instruction_manifest AS manifest
                     ON manifest.turn_instruction_manifest_id = call.turn_instruction_manifest_id
                    AND manifest.session_id = call.session_id
                    AND manifest.turn_id = call.turn_id
              LEFT JOIN instruction_discovery AS discovery
                     ON discovery.instruction_discovery_id = manifest.instruction_discovery_id
                  WHERE call.session_id = $1
                    AND call.model_call_id = $2",
            )
            .bind(session_id_to_uuid(session))
            .bind(prepared.call().id().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(ModelCallCorruption::Missing(
                "ambiguous authorization model call",
            ))?;
            let stored = decode_model_call(stored, session)?;
            if stored.state()
                == ModelCallReconstitutionState::Terminal(ModelCallDisposition::Cancelled)
            {
                let stored_members =
                    load_frontier_members(&mut transaction, session, stored.frontier().into_uuid())
                        .await?;
                let exact_request = prepared.session() == session
                    && prepared.turn() == stored.turn()
                    && prepared.attempt() == stored.attempt()
                    && prepared.call().id() == stored.id()
                    && prepared.call().selection() == stored.selection()
                    && prepared.call().target() == stored.target()
                    && prepared.call().frontier().snapshot() == stored.frontier()
                    && prepared
                        .frontier_entries()
                        .map(|entry| {
                            (
                                session_id_to_uuid(entry.source_session()),
                                entry.identity().into_uuid(),
                            )
                        })
                        .eq(stored_members);
                if !exact_request {
                    return Err(ModelCallRepositoryError::InvalidTransition(
                        "ambiguous authorization reread changed terminal request",
                    ));
                }
                if prepared_cancellation_closure_matches(
                    &mut transaction,
                    session,
                    stored.turn().into_uuid(),
                    stored.attempt().into_uuid(),
                    stored.id().into_uuid(),
                    stored.frontier().into_uuid(),
                )
                .await?
                {
                    return Ok(ModelCallAuthorizationReread::Cancelled);
                }
                return Err(ModelCallRepositoryError::InvalidTransition(
                    "ambiguous authorization terminal cancellation closure is incomplete",
                ));
            }
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
            let correlation = observation.correlation();
            if correlation.session() != session {
                return Err(ModelCallRepositoryError::InvalidTransition(
                    "retained observation session changed",
                ));
            }
            let delegation_logically_terminal =
                locked_delegation_logical_terminal(&mut transaction, session, observation.call())
                    .await?;
            let stored_row = sqlx::query(
                "SELECT session_id, turn_id, turn_attempt_id,
                        resolved_provider_model_identity_id, context_frontier_id,
                        state_kind, terminal_disposition_kind,
                        terminal_provider_failure_cause,
                        usage_input_tokens, usage_output_tokens,
                        usage_cache_creation_input_tokens,
                        usage_cache_read_input_tokens,
                        retained_input_tokens, retained_output_tokens
                   FROM model_call
                  WHERE model_call_id = $1",
            )
            .bind(observation.call().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(ModelCallCorruption::Missing(
                "retained observation model call",
            ))?;
            let stored = decode_stored_model_call_observation(&stored_row)?;
            if stored.session != session_id_to_uuid(correlation.session())
                || stored.turn != turn_id_to_uuid(correlation.turn())
                || stored.attempt != correlation.attempt().into_uuid()
                || stored.target != correlation.target().identity().into_uuid()
                || stored.frontier != correlation.frontier().into_uuid()
            {
                return Err(ModelCallRepositoryError::InvalidTransition(
                    "retained observation correlation changed",
                ));
            }
            if delegation_logically_terminal {
                return Ok(if stored.state == "terminal" {
                    RetainedModelCallObservationStatus::DiscardedByLogicalTerminal
                } else {
                    RetainedModelCallObservationStatus::Pending
                });
            }
            match (stored.state.as_str(), stored.disposition.as_deref()) {
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
                    .bind(stored.turn)
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
                        == encode_disposition(observation.observation().disposition())
                        && stored.provider_failure_cause.as_deref()
                            == observation
                                .provider_failure_cause()
                                .map(encode_provider_failure_cause)
                        && stored.usage == encode_token_usage(observation.usage())
                        && stored.retained_input_tokens
                            == observation
                                .observation()
                                .retained_input_tokens()
                                .map(Decimal::from)
                        && stored.retained_output_tokens
                            == observation
                                .observation()
                                .retained_output_tokens()
                                .map(Decimal::from) =>
                {
                    // A commit-ambiguous driver error can hide a commit that
                    // durably created an availability successor. The
                    // predecessor is then terminal while its turn stays active
                    // on the successor attempt, which is not the terminal
                    // failed turn the ordinary closure predicate requires.
                    if let Some(retry_backoff) = committed_availability_successor_backoff(
                        &mut transaction,
                        observation.call(),
                    )
                    .await?
                    {
                        return Ok(
                            RetainedModelCallObservationStatus::AvailabilitySuccessorCommitted {
                                retry_backoff,
                            },
                        );
                    }
                    if !terminal_observation_closure_matches(&mut transaction, session, observation)
                        .await?
                    {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "retained observation terminal closure changed",
                        ));
                    }
                    if !delegated_observation_result_matches(&mut transaction, session, observation)
                        .await?
                    {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "retained observation delegated result closure changed",
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
            lock_model_call_terminal_frontier(&mut transaction, session, call).await?;
            let execution = require_exact_call(
                require_live_execution_for_restart(&mut transaction, session).await?,
                call,
            )?;
            let outcome = execution.recover_after_restart(identities).map_err(|_| {
                ModelCallRepositoryError::InvalidTransition(
                    "startup recovery requires a live Prepared or issued call",
                )
            })?;
            persist_terminal_outcome(
                &mut transaction,
                &outcome,
                Some(TurnTerminalCause::AbandonedAtRestart),
            )
            .await?;
            Ok(outcome)
        }
        .await;
        let result = match result {
            Ok(outcome) => {
                self.settle_runner_replacement_after_observation(&mut transaction, session, None)
                    .await?;
                Ok(outcome)
            }
            Err(error) => Err(error),
        };
        finish_commit(transaction, result).await
    }
}
