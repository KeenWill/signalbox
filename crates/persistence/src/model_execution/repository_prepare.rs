use super::continuation::resolve_session_credential;
use super::credential_pool::{
    DurablePoolExclusions, SelectedRuntimePoolCredential, acquire_model_call_outbox_order_guard,
    consume_pool_member_actions, decode_prepared_usage_limit, load_availability_successor_backoff,
    load_call_pool_policy, load_durable_pool_exclusions, lock_credential_pool_action_heads,
    persist_credential_pool_member_action, prepared_serving_configuration_is_compatible,
    prepared_serving_evidence, retain_call_capacity_policy_observation,
    select_runtime_pool_credential,
};
use super::delegation_lock::{
    lock_delegated_child_endpoint_sessions, locked_delegation_logical_terminal,
};
use super::live_turn::{lock_session, require_live_execution};
use super::load::require_exact_call;
use super::persist_terminal::{
    persist_authorization, persist_failed_with_delegated_child_result,
    persist_terminal_outcome_with_usage,
};
use super::persist_tool_round::{
    availability_retry_backoff, count_turn_credential_attempts,
    insert_credential_pool_terminal_exhaustion, is_same_credential_retry_cause,
    persist_availability_successor, persist_credential_pool_exhaustion,
    persist_observed_tool_round, persist_tool_round_observation,
};
use super::prepared::{
    insert_prepared_call, load_call_credential_reference, load_call_user_overrides,
    load_frozen_epoch_system_prompt, load_provider_reasoning_provenance,
    load_tool_conversation_entries, resolve_runner_placement_entries,
};
use super::reread::{
    attach_pending_reclassification_candidates, record_reclassified_turn_candidate,
    select_terminal_identity_candidates,
};
use super::{
    CredentialPoolRuntimeAction, ModelCallCorruption, ModelCallIdentityCollision,
    ModelCallRepositoryError, PostgresModelCallRepository, PrepareInitialModelCallOutcome,
    encode_provider_failure_cause, finish_commit, finish_optional_commit,
};
use crate::mapping::{session_id_to_uuid, turn_id_to_uuid};
use crate::outbox;
use signalbox_application::{
    AuthorizeModelCallOutcome, AvailabilitySuccessorOutcome, CredentialPoolExhaustedOutcome,
    ModelCallObservationCommitOutcome, ModelCallTerminalIdentityCandidates,
};
use signalbox_domain::{
    AcceptedInputId, CorrelatedModelCallTerminalObservation, FailedModelCallTurnIdentities,
    ModelCallId, ModelCallPreparationFailure, ModelCallTerminalIdentities,
    ModelCallTerminalOutcome, PendingSteeringReclassificationIdentity, ProviderModelIdentity,
    ProviderReportedTokenUsage, ResolvedProviderTarget, SessionId, TurnId, TurnTerminalCause,
};
use sqlx::{Row, types::Uuid};
use std::collections::BTreeSet;
use std::sync::Arc;

impl PostgresModelCallRepository {
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
            lock_delegated_child_endpoint_sessions(&mut transaction, session).await?;
            lock_session(&mut transaction, session).await?;
            let waiting_turn: Option<Uuid> = sqlx::query_scalar(
                "SELECT turn_id FROM credential_availability_wait
                  WHERE session_id = $1 AND consumed_by_attempt_id IS NULL",
            )
            .bind(session.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
            let mut wait_steering_candidates = if let Some(turn) = waiting_turn {
                let pending: Vec<Uuid> = sqlx::query_scalar(
                    "SELECT accepted_input_id FROM accepted_input
                      WHERE session_id = $1 AND expected_active_turn_id = $2
                        AND disposition_kind = 'pending_steering'
                      ORDER BY acceptance_position",
                )
                .bind(session.into_uuid())
                .bind(turn)
                .fetch_all(&mut *transaction)
                .await?;
                let candidates = pending
                    .into_iter()
                    .map(|input| {
                        let input = AcceptedInputId::from_uuid(input);
                        (input, next_steering_identities(input))
                    })
                    .collect::<std::collections::BTreeMap<_, _>>();
                super::reserve_frontier_write_identities(
                    &mut transaction,
                    candidates
                        .values()
                        .map(|(entry, _)| entry.into_uuid())
                        .chain([
                            steering_frontier.into_uuid(),
                            failure_identities.failure_entry().into_uuid(),
                            failure_identities.terminal_frontier().into_uuid(),
                        ]),
                )
                .await?;
                Some(candidates)
            } else {
                None
            };
            if let Some(wait) = super::credential_wait::prepare_release(
                &mut transaction,
                self,
                session,
                signalbox_domain::TurnAttemptId::from_uuid(call.into_uuid()),
            )
            .await?
            {
                return Ok((true, PrepareInitialModelCallOutcome::CredentialWait(wait)));
            }
            let execution =
                require_live_execution(&mut transaction, session, &self.targets).await?;
            if execution.current_call().is_none()
                && let Some(delay) = load_availability_successor_backoff(
                    &mut transaction,
                    execution.current_attempt().id(),
                )
                .await?
            {
                return Ok((false, PrepareInitialModelCallOutcome::RetryBackoff(delay)));
            }
            if let Some(current_call) = execution.current_call() {
                return match current_call.state() {
                    signalbox_domain::CurrentModelCallState::Prepared => {
                        let current_call_id = current_call.id();
                        let mut request = execution.resume_prepared_call().map_err(|_| {
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
                        let dangerous_tool_auto_approval = execution
                            .active_turn()
                            .configuration()
                            .effective()
                            .dangerous_tool_auto_approval();
                        let system_prompt = load_frozen_epoch_system_prompt(
                            &mut transaction,
                            session,
                            execution
                                .active_turn()
                                .configuration()
                                .session_defaults_version(),
                        )
                        .await?;
                        resolve_runner_placement_entries(transaction.as_mut(), &mut request)
                            .await?;
                        let Some(tool_entries) =
                            load_tool_conversation_entries(&mut transaction, &request).await?
                        else {
                            return Ok((
                                false,
                                PrepareInitialModelCallOutcome::RetainedContentLimitExceeded {
                                    turn: request.turn(),
                                    call: current_call_id,
                                },
                            ));
                        };
                        let reasoning_provenance =
                            load_provider_reasoning_provenance(&mut transaction, &request).await?;
                        let recorded_user_overrides =
                            load_call_user_overrides(&mut transaction, session, current_call_id)
                                .await?;
                        Ok((
                            false,
                            PrepareInitialModelCallOutcome::Ready {
                                request: Box::new(request),
                                credential_reference,
                                retained_mapped_target:
                                    super::credential_wait::retained_mapped_target(
                                        &mut transaction,
                                        execution.current_attempt().id(),
                                    )
                                    .await?,
                                invocation_capacity_reserved: sqlx::query_scalar(
                                    "SELECT EXISTS (SELECT 1 FROM credential_invocation_reservation WHERE model_call_id = $1 AND released_at IS NULL)",
                                )
                                .bind(current_call_id.into_uuid())
                                .fetch_one(&mut *transaction)
                                .await?,
                                dangerous_tool_auto_approval,
                                recorded_user_overrides,
                                system_prompt,
                                tool_entries,
                                reasoning_provenance,
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
                let (entry, turn) =
                    match &mut wait_steering_candidates {
                        Some(candidates) => candidates.remove(&accepted_input).ok_or(
                            ModelCallCorruption::Missing(
                                "reserved credential-wait steering identities",
                            ),
                        )?,
                        None => next_steering_identities(accepted_input),
                    };
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
            super::reserve_frontier_write_identities(
                &mut transaction,
                steering_entries
                    .iter()
                    .map(|entry| entry.into_uuid())
                    .chain(steering_snapshot.map(|frontier| frontier.into_uuid()))
                    .chain([
                        failure_identities.failure_entry().into_uuid(),
                        failure_identities.terminal_frontier().into_uuid(),
                    ]),
            )
            .await?;
            let fast_mode = execution
                .configuration()
                .effective()
                .model_settings()
                .effective()
                .fast_mode();
            let selected = if let Ok(resolved) = self
                .targets
                .resolve(*execution.configuration().effective().model())
            {
                let credential_reference = resolve_session_credential(
                    &mut transaction,
                    session,
                    resolved.target(),
                    fast_mode,
                    &self.credential_reference,
                    self.credential_families.as_ref(),
                )
                .await?;
                acquire_model_call_outbox_order_guard(&mut transaction).await?;
                let serving_evidence = prepared_serving_evidence(
                    self.credential_families.as_ref(),
                    &self.continuation_usage_limits,
                    resolved.target(),
                    fast_mode,
                );
                let serving_evidence = super::credential_wait::retain_serving_target(
                    &mut transaction,
                    execution.current_attempt().id(),
                    self.credential_families.as_ref(),
                    serving_evidence,
                )
                .await?;
                let selected = Some(
                    select_runtime_pool_credential(
                        &mut transaction,
                        session,
                        execution.turn(),
                        execution.current_attempt().id(),
                        serving_evidence,
                        credential_reference,
                        &self.credential_pools,
                    )
                    .await?,
                );
                outbox::lock_sequence_allocator(&mut transaction).await?;
                selected
            } else {
                None
            };
            if let Some(wait) = super::credential_wait::park_initial(
                &mut transaction,
                &execution,
                selected.as_ref(),
            )
            .await?
            {
                return Ok((true, PrepareInitialModelCallOutcome::CredentialWait(wait)));
            }
            if let Some(failed) = super::credential_wait::fail_released_chain(
                &mut transaction,
                &execution,
                selected.as_ref(),
                failure_identities
                    .clone()
                    .with_pending_steering_reclassifications(
                        steering_identities
                            .iter()
                            .map(|(_, identity)| *identity)
                            .collect(),
                    ),
            )
            .await?
            {
                return Ok((
                    true,
                    PrepareInitialModelCallOutcome::WaitFailed(Box::new(failed)),
                ));
            }
            if let Some(SelectedRuntimePoolCredential {
                reference: None,
                policy: Some(policy),
                ..
            }) = selected.as_ref()
            {
                let source_turn = execution.turn();
                let reclassifications = steering_identities
                    .iter()
                    .map(|(_, reclassification)| *reclassification)
                    .collect::<Vec<_>>();
                let mut proposed_turns = BTreeSet::new();
                for reclassification in &reclassifications {
                    record_reclassified_turn_candidate(
                        source_turn,
                        reclassification.turn(),
                        &mut proposed_turns,
                    )?;
                }
                let exhausted = execution
                    .fail_credential_pool_exhausted(
                        policy.name().to_owned(),
                        failure_identities
                            .clone()
                            .with_pending_steering_reclassifications(reclassifications),
                    )
                    .map_err(|_| {
                        ModelCallRepositoryError::InvalidTransition(
                            "credential-pool exhaustion could not close fresh execution state",
                        )
                    })?;
                persist_credential_pool_exhaustion(&mut transaction, &exhausted).await?;
                return Ok((
                    true,
                    PrepareInitialModelCallOutcome::PoolExhausted(Box::new(exhausted)),
                ));
            }
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
                    persist_failed_with_delegated_child_result(
                        &mut transaction,
                        &failed,
                        TurnTerminalCause::ModelTargetUnavailable,
                        ProviderReportedTokenUsage::unreported(),
                        None,
                        None,
                    )
                    .await?;
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
            let selected = selected.ok_or(ModelCallRepositoryError::InvalidTransition(
                "resolved initial call omitted credential selection",
            ))?;
            let credential_reference =
                selected
                    .reference
                    .as_ref()
                    .ok_or(ModelCallRepositoryError::InvalidTransition(
                        "admitted credential pool omitted its selected member",
                    ))?;
            let serving_evidence = prepared_serving_evidence(
                self.credential_families.as_ref(),
                &self.continuation_usage_limits,
                prepared.call().target(),
                fast_mode,
            );
            let serving_evidence = super::credential_wait::retain_serving_target(
                &mut transaction,
                prepared.attempt(),
                self.credential_families.as_ref(),
                serving_evidence,
            )
            .await?;
            insert_prepared_call(
                &mut transaction,
                &prepared,
                credential_reference,
                selected.policy.as_ref(),
                self.cache_inclusive_input_targets
                    .contains(&prepared.call().target()),
                serving_evidence,
            )
            .await?;
            consume_pool_member_actions(
                &mut transaction,
                prepared.turn(),
                &selected.pending_consumed_actions,
            )
            .await?;
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
            let Some(current) = execution.current_call() else {
                return Ok((false, AuthorizeModelCallOutcome::NoSend));
            };
            if current.id() != call
                || current.state() != signalbox_domain::CurrentModelCallState::Prepared
            {
                return Ok((false, AuthorizeModelCallOutcome::NoSend));
            }
            let fast_mode = execution
                .configuration()
                .effective()
                .model_settings()
                .effective()
                .fast_mode();
            let current_serving_evidence = prepared_serving_evidence(
                self.credential_families.as_ref(),
                &self.continuation_usage_limits,
                current.target(),
                fast_mode,
            );
            let current_serving_evidence = super::credential_wait::retain_serving_target(
                &mut transaction,
                execution.current_attempt().id(),
                self.credential_families.as_ref(),
                current_serving_evidence,
            )
            .await?;
            let current_effective_target = current_serving_evidence.effective_target;
            let stored_serving_evidence = sqlx::query(
                "SELECT effective_provider_model_identity_id,
                        prepared_credential_model_family,
                        prepared_max_output_tokens,
                        prepared_context_window_tokens,
                        prepared_provider_compaction_replay
                   FROM model_call
                  WHERE model_call_id = $1",
            )
            .bind(call.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
            let stored_effective_target =
                ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                    stored_serving_evidence.try_get("effective_provider_model_identity_id")?,
                ));
            let stored_credential_model_family = stored_serving_evidence
                .try_get::<Option<String>, _>("prepared_credential_model_family")?;
            let stored_limit =
                decode_prepared_usage_limit(&stored_serving_evidence, stored_effective_target)?;
            if !prepared_serving_configuration_is_compatible(
                stored_effective_target,
                stored_credential_model_family.as_deref(),
                stored_limit,
                current_serving_evidence,
            ) {
                return Ok((false, AuthorizeModelCallOutcome::NoSend));
            }
            let authorized = execution.authorize_send().map_err(|_| {
                ModelCallCorruption::Inconsistent("checked Prepared call could not authorize send")
            })?;
            persist_authorization(&mut transaction, &authorized, current_effective_target).await?;
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
        next_reclassified_turn: NextTurn,
    ) -> Result<ModelCallTerminalOutcome, ModelCallRepositoryError>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        let outcome = self
            .apply_terminal_observation_candidates(
                session,
                observation,
                ModelCallTerminalIdentityCandidates::Exact(identities),
                next_reclassified_turn,
            )
            .await?
            .ok_or(ModelCallRepositoryError::InvalidTransition(
                "provider observation was discarded by logical delegation terminalization",
            ))?;
        match outcome {
            ModelCallObservationCommitOutcome::Terminal(outcome) => Ok(*outcome),
            ModelCallObservationCommitOutcome::AvailabilitySuccessor(_)
            | ModelCallObservationCommitOutcome::CredentialWait(_) => {
                Err(ModelCallRepositoryError::InvalidTransition(
                    "exact terminal candidates produced an availability successor",
                ))
            }
            ModelCallObservationCommitOutcome::PoolExhausted(_) => {
                Err(ModelCallRepositoryError::InvalidTransition(
                    "exact terminal candidates produced pool exhaustion",
                ))
            }
        }
    }

    pub(super) async fn apply_terminal_observation_candidates<NextTurn>(
        &self,
        session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
        identities: ModelCallTerminalIdentityCandidates,
        mut next_reclassified_turn: NextTurn,
    ) -> Result<Option<ModelCallObservationCommitOutcome>, ModelCallRepositoryError>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        let mut notifications = self
            .runner_recovery
            .as_ref()
            .and_then(crate::runner_protocol::RunnerProtocolStore::recovery_notifications);
        loop {
            let mut transaction = self.pool.begin().await?;
            let result = async {
                let observation = observation.clone();
                let identities = identities.clone();
                if locked_delegation_logical_terminal(&mut transaction, session, observation.call())
                    .await?
                {
                    super::delegation_lock::retire_logically_terminal_observation(
                        &mut transaction,
                        session,
                        &observation,
                    )
                    .await?;
                    return Ok(None);
                }
                let execution = require_exact_call(
                    require_live_execution(&mut transaction, session, &self.targets).await?,
                    observation.call(),
                )?;
                let identities = select_terminal_identity_candidates(identities, &execution);
                let identities = attach_pending_reclassification_candidates(
                    identities,
                    &execution,
                    &mut next_reclassified_turn,
                )?;
                let usage = observation.usage();
                let (entries, frontier) = match &identities {
                    ModelCallTerminalIdentityCandidates::Exact(identities) => {
                        identities.frontier_identity_candidates()
                    }
                    ModelCallTerminalIdentityCandidates::Availability { failed, .. } => {
                        (vec![failed.failure_entry()], failed.terminal_frontier())
                    }
                    ModelCallTerminalIdentityCandidates::ToolRound { .. } => {
                        return Err(ModelCallRepositoryError::InvalidTransition(
                            "terminal candidate selection retained a nonterminal alternative",
                        ));
                    }
                };
                super::reserve_frontier_write_identities(
                    &mut transaction,
                    entries
                        .into_iter()
                        .map(|entry| entry.into_uuid())
                        .chain([frontier.into_uuid()]),
                )
                .await?;
                if let Some(snapshot) = observation.rate_limits() {
                    retain_call_capacity_policy_observation(
                        &mut transaction,
                        &observation,
                        snapshot,
                    )
                    .await?;
                }
                let retained_input_tokens = observation.observation().retained_input_tokens();
                let retained_output_tokens = observation.observation().retained_output_tokens();
                let provider_failure_cause = observation.provider_failure_cause();
                let retry_after = observation.retry_after();
                if let ModelCallTerminalIdentityCandidates::Availability {
                    failed,
                    successor_attempt,
                } = identities
                {
                    let cause = provider_failure_cause.ok_or(
                        ModelCallRepositoryError::InvalidTransition(
                            "availability candidates require a classified provider failure",
                        ),
                    )?;
                    let policy =
                        load_call_pool_policy(&mut transaction, observation.call().into_uuid())
                            .await?;
                    let Some(policy) = policy else {
                        outbox::lock_sequence_allocator(&mut transaction).await?;
                        // The call carried no credential pool, so no configured
                        // action governs this availability cause. Close the turn on
                        // the ordinary terminal path rather than failing the commit.
                        let outcome = execution
                            .apply_terminal_observation(
                                observation,
                                ModelCallTerminalIdentities::Failed(failed),
                            )
                            .map_err(|_| {
                                ModelCallRepositoryError::InvalidTransition(
                                    "terminal observation does not match fresh issued state",
                                )
                            })?;
                        persist_terminal_outcome_with_usage(
                            &mut transaction,
                            &outcome,
                            Some(TurnTerminalCause::ModelCallFailed),
                            usage,
                            provider_failure_cause,
                            retained_input_tokens,
                            retained_output_tokens,
                        )
                        .await?;
                        return Ok(Some(ModelCallObservationCommitOutcome::Terminal(Box::new(
                            outcome,
                        ))));
                    };
                    acquire_model_call_outbox_order_guard(&mut transaction).await?;
                    lock_credential_pool_action_heads(&mut transaction, &policy).await?;
                    outbox::lock_sequence_allocator(&mut transaction).await?;
                    let recovery = observation.credential_recovery();
                    let action = if recovery.is_some() {
                        CredentialPoolRuntimeAction::SwitchNow
                    } else {
                        policy.action(cause)
                    };
                    let mut pool_exhausted_name = None;
                    let current_reference = sqlx::query_scalar::<_, String>(
                        "SELECT credential_reference
                       FROM model_call
                      WHERE model_call_id = $1",
                    )
                    .bind(observation.call().into_uuid())
                    .fetch_one(&mut *transaction)
                    .await?;
                    let stop_requested = matches!(
                        execution.current_attempt().state(),
                        signalbox_domain::CurrentTurnAttemptState::StopRequested { .. }
                    );
                    let same_credential_attempts = count_turn_credential_attempts(
                        &mut transaction,
                        session,
                        observation.correlation().turn(),
                        &current_reference,
                    )
                    .await?;
                    let retry_candidate = (recovery
                        == Some(signalbox_domain::CredentialRejectionRecovery::Refreshed)
                        || (is_same_credential_retry_cause(cause)
                            && self
                                .same_credential_attempt_bound
                                .is_none_or(|bound| same_credential_attempts < bound.get())))
                        && !stop_requested;
                    let rotation_candidate =
                        action == CredentialPoolRuntimeAction::SwitchNow && !stop_requested;
                    let mut durable_exclusions = if retry_candidate || rotation_candidate {
                        Some(
                            load_durable_pool_exclusions(
                                &mut transaction,
                                session,
                                observation.correlation().turn(),
                                &policy,
                            )
                            .await?,
                        )
                    } else {
                        None
                    };
                    let retrying_same_credential = retry_candidate
                        && durable_exclusions.as_ref().is_some_and(|exclusions| {
                            !exclusions.excluded.contains(&current_reference)
                        });
                    // The failed credential itself must still be admitted for a
                    // retry. Otherwise only the pinned action may authorize a
                    // rotation; every other action follows the terminal path.
                    let rotating = !retrying_same_credential && rotation_candidate;
                    if retrying_same_credential || rotating {
                        let Some(DurablePoolExclusions {
                            mut excluded,
                            headroom,
                            ..
                        }) = durable_exclusions.take()
                        else {
                            return Err(ModelCallRepositoryError::InvalidTransition(
                                "availability successor omitted pool exclusions",
                            ));
                        };
                        let quota_excluded_by_capacity = cause
                            == signalbox_domain::ProviderModelCallFailureCause::QuotaExhausted
                            && policy
                                .members()
                                .iter()
                                .find(|member| member.credential_reference() == current_reference)
                                .and_then(|member| {
                                    member
                                        .headroom_reserve_percent
                                        .or(policy.headroom_reserve_percent)
                                })
                                .is_some_and(|reserve| {
                                    headroom
                                        .get(&current_reference)
                                        .copied()
                                        .flatten()
                                        .is_some_and(|remaining| remaining <= i64::from(reserve))
                                });
                        if rotating && !quota_excluded_by_capacity {
                            sqlx::query(
                                "INSERT INTO credential_pool_chain_exclusion
                            (session_id, turn_id, credential_reference,
                             predecessor_model_call_id, cause_kind)
                         VALUES ($1, $2, $3, $4, $5)
                         ON CONFLICT (predecessor_model_call_id) DO NOTHING",
                            )
                            .bind(session_id_to_uuid(session))
                            .bind(turn_id_to_uuid(observation.correlation().turn()))
                            .bind(&current_reference)
                            .bind(observation.call().into_uuid())
                            .bind(encode_provider_failure_cause(cause))
                            .execute(&mut *transaction)
                            .await?;
                            excluded.insert(current_reference.clone());
                        }
                        pool_exhausted_name = Some(Arc::<str>::from(policy.name()));
                        if policy
                            .members()
                            .iter()
                            .any(|member| !excluded.contains(member.credential_reference()))
                        {
                            let backoff = availability_retry_backoff(
                                cause,
                                retry_after,
                                if retrying_same_credential {
                                    same_credential_attempts
                                } else {
                                    1
                                },
                                observation.call(),
                            );
                            let successor = execution
                                .apply_availability_successor(observation, successor_attempt)
                                .map_err(|_| {
                                    ModelCallRepositoryError::InvalidTransition(
                                        "availability successor does not match fresh issued state",
                                    )
                                })?;
                            persist_availability_successor(
                                &mut transaction,
                                &successor,
                                usage,
                                cause,
                                backoff,
                            )
                            .await?;
                            return Ok(Some(
                                ModelCallObservationCommitOutcome::AvailabilitySuccessor(Box::new(
                                    AvailabilitySuccessorOutcome::new(successor, backoff),
                                )),
                            ));
                        }
                        if let Some(outcome) = super::credential_wait::park_failed(
                            &mut transaction,
                            &execution,
                            &policy,
                            &observation,
                            successor_attempt,
                            cause,
                            &self.targets,
                        )
                        .await?
                        {
                            return Ok(Some(outcome));
                        }
                        insert_credential_pool_terminal_exhaustion(
                            &mut transaction,
                            observation.correlation().attempt(),
                            session,
                            observation.correlation().turn(),
                            policy.name(),
                            Some(observation.call()),
                            Some(cause),
                        )
                        .await?;
                    } else if action != CredentialPoolRuntimeAction::Stay
                        && action != CredentialPoolRuntimeAction::SwitchNow
                    {
                        persist_credential_pool_member_action(
                            &mut transaction,
                            &policy,
                            action,
                            current_reference,
                            &observation,
                            encode_provider_failure_cause(cause),
                        )
                        .await?;
                    }
                    let outcome = execution
                        .apply_terminal_observation(
                            observation,
                            ModelCallTerminalIdentities::Failed(failed),
                        )
                        .map_err(|_| {
                            ModelCallRepositoryError::InvalidTransition(
                                "terminal observation does not match fresh issued state",
                            )
                        })?;
                    // Exhausting the pool's last member is why this turn ended,
                    // so the durable exhaustion record and the cause agree.
                    let terminal_cause = match pool_exhausted_name {
                        Some(_) => TurnTerminalCause::CredentialPoolExhausted,
                        None => TurnTerminalCause::ModelCallFailed,
                    };
                    persist_terminal_outcome_with_usage(
                        &mut transaction,
                        &outcome,
                        Some(terminal_cause),
                        usage,
                        provider_failure_cause,
                        retained_input_tokens,
                        retained_output_tokens,
                    )
                    .await?;
                    if let Some(pool_name) = pool_exhausted_name {
                        return Ok(Some(ModelCallObservationCommitOutcome::PoolExhausted(
                            CredentialPoolExhaustedOutcome::AfterCall {
                                pool_name,
                                terminal: Box::new(outcome),
                            },
                        )));
                    }
                    return Ok(Some(ModelCallObservationCommitOutcome::Terminal(Box::new(
                        outcome,
                    ))));
                }
                let ModelCallTerminalIdentityCandidates::Exact(identities) = identities else {
                    return Err(ModelCallRepositoryError::InvalidTransition(
                        "terminal candidate selection retained a nonterminal alternative",
                    ));
                };
                let outcome = execution
                    .apply_terminal_observation(observation, identities)
                    .map_err(|_| {
                        ModelCallRepositoryError::InvalidTransition(
                            "terminal observation does not match fresh issued state",
                        )
                    })?;
                if let ModelCallTerminalOutcome::ToolRound(round) = &outcome {
                    persist_tool_round_observation(
                        &mut transaction,
                        round,
                        usage,
                        retained_input_tokens,
                        retained_output_tokens,
                    )
                    .await?;
                    let relocation = if let Some(runner) = &self.runner_recovery {
                        runner
                            .settle_replacement_at_boundary(
                                &mut transaction,
                                session,
                                Some(round.yielded_snapshot()),
                            )
                            .await
                            .map_err(|error| match error {
                                crate::runner_protocol::RunnerProtocolStoreError::Database(
                                    source,
                                ) => ModelCallRepositoryError::from(source),
                                _ => ModelCallCorruption::Inconsistent(
                                    "runner replacement tool observation boundary",
                                )
                                .into(),
                            })?
                            .1
                    } else {
                        None
                    };
                    let boundary = relocation
                        .as_ref()
                        .map_or(round.yielded_snapshot(), |boundary| boundary.frontier());
                    persist_observed_tool_round(&mut transaction, round, boundary).await?;
                } else {
                    persist_terminal_outcome_with_usage(
                        &mut transaction,
                        &outcome,
                        Some(TurnTerminalCause::ModelCallFailed),
                        usage,
                        provider_failure_cause,
                        retained_input_tokens,
                        retained_output_tokens,
                    )
                    .await?;
                }
                Ok(Some(ModelCallObservationCommitOutcome::Terminal(Box::new(
                    outcome,
                ))))
            }
            .await;
            let result = match result {
                Ok(outcome) => {
                    let observation_frontier = match &outcome {
                        Some(ModelCallObservationCommitOutcome::AvailabilitySuccessor(outcome)) => {
                            Some(outcome.successor().predecessor_call().frontier().snapshot())
                        }
                        _ => None,
                    };
                    let settled = self
                        .settle_runner_replacement_after_observation(
                            &mut transaction,
                            session,
                            observation_frontier,
                        )
                        .await?;
                    if !settled {
                        transaction.rollback().await?;
                        notifications
                            .as_mut()
                            .ok_or(ModelCallRepositoryError::InvalidTransition(
                                "runner recovery notifications are not configured",
                            ))?
                            .changed()
                            .await
                            .map_err(|_| {
                                ModelCallRepositoryError::InvalidTransition(
                                    "runner recovery notifications closed",
                                )
                            })?;
                        continue;
                    }
                    Ok(outcome)
                }
                Err(error) => Err(error),
            };
            return finish_commit(transaction, result).await;
        }
    }
}
