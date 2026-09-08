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
    ModelCallTerminalOutcome, PendingSteeringReclassificationIdentity,
    ProviderModelCallFailureCause, ProviderModelIdentity, ProviderReportedTokenUsage,
    ResolvedProviderTarget, SessionId, TurnId, TurnTerminalCause,
};
use sqlx::Row;
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
                        let tool_entries =
                            load_tool_conversation_entries(&mut transaction, &request).await?;
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
            ModelCallObservationCommitOutcome::AvailabilitySuccessor(_) => {
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
        if let Some(notifications) = &mut notifications {
            notifications.borrow_and_update();
        }
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
                    let action = policy.action(cause);
                    let mut pool_exhausted_name = None;
                    let current_reference = sqlx::query_scalar::<_, String>(
                        "SELECT credential_reference
                       FROM model_call
                      WHERE model_call_id = $1",
                    )
                    .bind(observation.call().into_uuid())
                    .fetch_one(&mut *transaction)
                    .await?;
                    // A successor reissues the request, so availability failures
                    // need the adapter's proof that the failed request was never
                    // accepted. Credential rejection is the one exception: the
                    // authentication refusal itself authorizes rotation, but never
                    // a retry on the rejected credential.
                    // A stop already requested on this attempt forbids the reissue
                    // outright: the successor would reload an attempt the domain
                    // admits only while running.
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
                    let retry_candidate = is_same_credential_retry_cause(cause)
                        && same_credential_attempts < self.same_credential_attempt_bound.get()
                        && observation.non_acceptance_proven()
                        && !stop_requested;
                    let rotation_candidate = action == CredentialPoolRuntimeAction::SwitchNow
                        && (observation.non_acceptance_proven()
                            || cause == ProviderModelCallFailureCause::CredentialRejected)
                        && !stop_requested;
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
                        let Some(DurablePoolExclusions { mut excluded, .. }) =
                            durable_exclusions.take()
                        else {
                            return Err(ModelCallRepositoryError::InvalidTransition(
                                "availability successor omitted pool exclusions",
                            ));
                        };
                        if rotating {
                            sqlx::query(
                                "INSERT INTO credential_pool_chain_exclusion
                            (session_id, turn_id, credential_reference,
                             predecessor_model_call_id, cause_kind)
                         VALUES ($1, $2, $3, $4, $5)
                         ON CONFLICT (session_id, turn_id, credential_reference) DO NOTHING",
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
