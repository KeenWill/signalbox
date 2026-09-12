use super::{
    AmbiguousModelCallTurnIdentities, Arc, AssistantResponsePart, AttachmentPreparationFailure,
    AttemptDispatchGate, AuthorizeModelCallOutcome, AuthorizeModelCallTransaction,
    ClassifyOperatorFailure, CommitModelCallObservationTransaction, CompletedModelCallIdentities,
    CorrelatedModelCallTerminalObservation, CredentialPoolExhaustedOutcome,
    DangerousToolAutoApproval, FailPreparedModelCallTransaction, FailedModelCallTurnIdentities,
    InitialToolApproval, MAX_RETAINED_FRONTIER_CONTENT_BYTES, ModelCallAuthorizationReread,
    ModelCallCapabilityPreparation, ModelCallExecutionError, ModelCallExecutionIdGenerator,
    ModelCallExecutionOutcome, ModelCallId, ModelCallObservationCommitOutcome, ModelCallProvider,
    ModelCallTerminalIdentities, ModelCallTerminalIdentityCandidates, ModelCallTerminalObservation,
    ModelFrontierRenderingError, NoToolCatalog, OperatorFailureClass,
    PhysicalCancellationModelCallTurnIdentities, PrepareModelCallOutcome,
    PrepareModelCallTransaction, PreparedModelCallFailureCause, PreparedModelOperation,
    RecordedUserOverride, RefusedModelCallTurnIdentities, RetainedModelCallExecutionState,
    RetainedModelCallExecutionStateKind, RetainedModelCallObservationStatus,
    RetainedPreparedFailureStatus, SessionId, StoppedToolResponsePartIdentity,
    StoppedToolRoundModelCallIdentities, ToolCatalog, ToolDefinition, ToolResponsePartIdentity,
    ToolRoundModelCallIdentities, TurnId, TurnTerminalOutcome, automatic_tool_round_count,
    initial_tool_approval, report_model_call_terminalization, report_turn_terminalization,
};

/// Coordinates one staged model-call execution invocation.
pub struct ModelCallExecutionService<
    Ids,
    Prepare,
    Failure,
    Authorization,
    Observation,
    Provider,
    Gate,
> {
    pub(super) ids: Ids,
    prepare: Prepare,
    failure: Failure,
    authorization: Authorization,
    observation: Observation,
    provider: Provider,
    gate: Gate,
    pub(super) catalog: Arc<dyn ToolCatalog>,
    retained_state: Option<RetainedModelCallExecutionState>,
    max_automatic_tool_rounds_per_turn: Option<usize>,
    retained_frontier_content_limit: usize,
}

impl<Ids, Prepare, Failure, Authorization, Observation, Provider, Gate>
    ModelCallExecutionService<Ids, Prepare, Failure, Authorization, Observation, Provider, Gate>
{
    /// Composes every purpose-specific effect role.
    #[allow(
        clippy::too_many_arguments,
        reason = "the service keeps each effect role and the required deployment policy explicit"
    )]
    pub fn new(
        ids: Ids,
        prepare: Prepare,
        failure: Failure,
        authorization: Authorization,
        observation: Observation,
        provider: Provider,
        gate: Gate,
        max_automatic_tool_rounds_per_turn: Option<usize>,
    ) -> Self {
        Self {
            ids,
            prepare,
            failure,
            authorization,
            observation,
            provider,
            gate,
            catalog: Arc::new(NoToolCatalog),
            retained_state: None,
            max_automatic_tool_rounds_per_turn,
            retained_frontier_content_limit: MAX_RETAINED_FRONTIER_CONTENT_BYTES,
        }
    }

    /// Replaces the empty compatibility catalog with one tool-capable port.
    pub fn with_tool_catalog(mut self, catalog: impl ToolCatalog + 'static) -> Self {
        self.catalog = Arc::new(catalog);
        self
    }

    /// Narrows the retained-tool-content ceiling for one service.
    ///
    /// Deployments run the module ceiling; this exists so the bound can be
    /// exercised end to end without materializing hundreds of megabytes.
    #[cfg(test)]
    pub(super) const fn with_retained_frontier_content_limit(mut self, limit: usize) -> Self {
        self.retained_frontier_content_limit = limit;
        self
    }

    /// Reconstitutes an explicitly decomposed service without losing evidence.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        ids: Ids,
        prepare: Prepare,
        failure: Failure,
        authorization: Authorization,
        observation: Observation,
        provider: Provider,
        gate: Gate,
        catalog: Arc<dyn ToolCatalog>,
        retained_state: Option<RetainedModelCallExecutionState>,
        max_automatic_tool_rounds_per_turn: Option<usize>,
    ) -> Self {
        Self {
            ids,
            prepare,
            failure,
            authorization,
            observation,
            provider,
            gate,
            catalog,
            retained_state,
            max_automatic_tool_rounds_per_turn,
            retained_frontier_content_limit: MAX_RETAINED_FRONTIER_CONTENT_BYTES,
        }
    }

    /// Returns every owned effect role for explicit composition handoff.
    #[allow(
        clippy::type_complexity,
        reason = "the tuple deliberately preserves the service's explicit independently owned composition roles"
    )]
    pub fn into_parts(
        self,
    ) -> (
        Ids,
        Prepare,
        Failure,
        Authorization,
        Observation,
        Provider,
        Gate,
        Arc<dyn ToolCatalog>,
        Option<RetainedModelCallExecutionState>,
        Option<usize>,
    ) {
        (
            self.ids,
            self.prepare,
            self.failure,
            self.authorization,
            self.observation,
            self.provider,
            self.gate,
            self.catalog,
            self.retained_state,
            self.max_automatic_tool_rounds_per_turn,
        )
    }

    /// Borrows same-incarnation evidence awaiting reconciliation.
    pub const fn retained_state(&self) -> Option<&RetainedModelCallExecutionState> {
        self.retained_state.as_ref()
    }

    /// Borrows the exact observation awaiting authoritative reconciliation.
    pub fn retained_observation(&self) -> Option<&CorrelatedModelCallTerminalObservation> {
        match self.retained_state.as_ref().map(|retained| &retained.state) {
            Some(RetainedModelCallExecutionStateKind::TerminalObservation {
                observation, ..
            }) => Some(observation),
            Some(
                RetainedModelCallExecutionStateKind::PreparedFailure { .. }
                | RetainedModelCallExecutionStateKind::AuthorizationNonConsumption { .. },
            )
            | None => None,
        }
    }
}

impl<Ids, Prepare, Failure, Authorization, Observation, Provider, Gate>
    ModelCallExecutionService<Ids, Prepare, Failure, Authorization, Observation, Provider, Gate>
where
    Ids: ModelCallExecutionIdGenerator + Send,
    Prepare: PrepareModelCallTransaction,
    Failure: FailPreparedModelCallTransaction,
    Authorization: AuthorizeModelCallTransaction,
    Observation: CommitModelCallObservationTransaction,
    Provider: ModelCallProvider,
    Gate: AttemptDispatchGate,
{
    /// Runs at most one provider interaction for one authoritative session hint.
    ///
    /// A newly committed `Prepared` checkpoint ends this invocation. A later
    /// invocation reloads it, prepares the opaque capability outside a
    /// transaction, authorizes send while holding the shared attempt gate,
    /// invokes the provider once, and commits its correlated observation.
    #[allow(
        clippy::result_large_err,
        reason = "The error retains the correlated observation inline for retry."
    )]
    pub async fn execute(
        &mut self,
        mut session: SessionId,
    ) -> Result<
        ModelCallExecutionOutcome,
        ModelCallExecutionError<
            Prepare::Error,
            Failure::Error,
            Authorization::Error,
            Provider::Error,
            Observation::Error,
        >,
    > {
        if let Some(retained) = self.retained_state.take() {
            match retained.state {
                RetainedModelCallExecutionStateKind::PreparedFailure {
                    session,
                    turn,
                    call,
                    cause,
                    attachment_failure,
                } => match self
                    .failure
                    .reread_failure(session, call, attachment_failure)
                    .await
                {
                    Ok(RetainedPreparedFailureStatus::Pending) => {
                        return self
                            .commit_prepared_failure(session, turn, call, cause, attachment_failure)
                            .await;
                    }
                    Ok(RetainedPreparedFailureStatus::AlreadyCommitted) => {
                        report_turn_terminalization(
                            session,
                            turn,
                            TurnTerminalOutcome::from(cause),
                        );
                        return Ok(match cause {
                            PreparedModelCallFailureCause::CapabilityKnownFailure => {
                                ModelCallExecutionOutcome::CapabilityFailureAlreadyCommitted(call)
                            }
                            PreparedModelCallFailureCause::ToolRoundLimitReached => {
                                ModelCallExecutionOutcome::ToolRoundLimitAlreadyCommitted(call)
                            }
                        });
                    }
                    Ok(RetainedPreparedFailureStatus::Cancelled) => {
                        return Ok(ModelCallExecutionOutcome::NoWork);
                    }
                    Err(error) => {
                        self.retained_state = Some(RetainedModelCallExecutionState {
                            state: RetainedModelCallExecutionStateKind::PreparedFailure {
                                session,
                                turn,
                                call,
                                cause,
                                attachment_failure,
                            },
                        });
                        return Err(ModelCallExecutionError::PreparedFailureReread(error));
                    }
                },
                RetainedModelCallExecutionStateKind::AuthorizationNonConsumption {
                    session: retained_session,
                    prepared,
                } => match self
                    .authorization
                    .reread_after_ambiguous_commit(retained_session, &prepared)
                    .await
                {
                    Ok(ModelCallAuthorizationReread::Prepared) => {
                        session = retained_session;
                    }
                    Ok(ModelCallAuthorizationReread::InFlight(authorized)) => {
                        let non_consumption = authorized
                            .observation_correlation()
                            .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
                        return self
                            .commit_terminal_observation(
                                retained_session,
                                non_consumption,
                                Box::new([]),
                            )
                            .await;
                    }
                    Ok(ModelCallAuthorizationReread::CancellationRequested(stopped)) => {
                        let cancellation = stopped
                            .observation_correlation()
                            .bind_terminal_observation(ModelCallTerminalObservation::Cancelled);
                        return self
                            .commit_terminal_observation(
                                retained_session,
                                cancellation,
                                Box::new([]),
                            )
                            .await;
                    }
                    Ok(ModelCallAuthorizationReread::Cancelled) => {
                        return Ok(ModelCallExecutionOutcome::NoWork);
                    }
                    Err(error) => {
                        self.retained_state = Some(RetainedModelCallExecutionState {
                            state:
                                RetainedModelCallExecutionStateKind::AuthorizationNonConsumption {
                                    session: retained_session,
                                    prepared,
                                },
                        });
                        return Err(ModelCallExecutionError::AuthorizationReconciliation(error));
                    }
                },
                RetainedModelCallExecutionStateKind::TerminalObservation {
                    session: retained_session,
                    observation: retained,
                    tool_approvals,
                } => match self
                    .observation
                    .reread_observation(retained_session, &retained)
                    .await
                {
                    Ok(RetainedModelCallObservationStatus::AlreadyCommitted) => {
                        return Ok(ModelCallExecutionOutcome::ObservationAlreadyCommitted(
                            retained.call(),
                        ));
                    }
                    Ok(RetainedModelCallObservationStatus::AvailabilitySuccessorCommitted {
                        retry_backoff,
                    }) => {
                        // The commit landed with its successor, so the turn is
                        // active on a new attempt rather than terminal. Waiting
                        // out the remaining delay returns the caller to ordinary
                        // preparation, which owns the successor from here.
                        return Ok(ModelCallExecutionOutcome::RetryBackoff(retry_backoff));
                    }
                    Ok(RetainedModelCallObservationStatus::Pending) => {
                        return self
                            .commit_terminal_observation(
                                retained_session,
                                *retained,
                                tool_approvals,
                            )
                            .await;
                    }
                    Ok(RetainedModelCallObservationStatus::DiscardedByLogicalTerminal) => {
                        return Ok(ModelCallExecutionOutcome::NoWork);
                    }
                    Err(error) => {
                        self.retained_state = Some(RetainedModelCallExecutionState {
                            state: RetainedModelCallExecutionStateKind::TerminalObservation {
                                session: retained_session,
                                observation: retained.clone(),
                                tool_approvals,
                            },
                        });
                        return Err(ModelCallExecutionError::ObservationCommit {
                            error,
                            retained_observation: *retained,
                        });
                    }
                },
            }
        }

        let prepared = loop {
            let call = self.ids.next_model_call_id();
            let failure_identities = self.next_failed_identities();
            let steering_frontier = self.ids.next_context_frontier_id();
            let prepare = &mut self.prepare;
            let ids = &mut self.ids;
            match prepare
                .prepare(session, call, failure_identities, steering_frontier, |_| {
                    (ids.next_semantic_entry_id(), ids.next_turn_id())
                })
                .await
            {
                Ok(
                    PrepareModelCallOutcome::NoWork | PrepareModelCallOutcome::CredentialWait(_),
                ) => {
                    return Ok(ModelCallExecutionOutcome::NoWork);
                }
                Ok(PrepareModelCallOutcome::RetryBackoff(delay)) => {
                    return Ok(ModelCallExecutionOutcome::RetryBackoff(delay));
                }
                Ok(PrepareModelCallOutcome::WaitFailed(failed)) => {
                    report_turn_terminalization(
                        failed.session(),
                        failed.turn(),
                        TurnTerminalOutcome::Failed,
                    );
                    return Ok(ModelCallExecutionOutcome::WaitFailed(failed));
                }
                Ok(PrepareModelCallOutcome::PoolExhausted(exhausted)) => {
                    report_turn_terminalization(
                        exhausted.failed().session(),
                        exhausted.failed().turn(),
                        TurnTerminalOutcome::Failed,
                    );
                    return Ok(ModelCallExecutionOutcome::PoolExhausted(Box::new(
                        CredentialPoolExhaustedOutcome::BeforeCall(exhausted),
                    )));
                }
                Ok(PrepareModelCallOutcome::Checkpointed(call)) => {
                    return Ok(ModelCallExecutionOutcome::Checkpointed(call));
                }
                Ok(PrepareModelCallOutcome::RetainedContentLimitExceeded { turn, call }) => {
                    return self
                        .commit_prepared_failure(
                            session,
                            turn,
                            call,
                            PreparedModelCallFailureCause::ToolRoundLimitReached,
                            None,
                        )
                        .await;
                }
                Ok(PrepareModelCallOutcome::Ready {
                    request,
                    credential_reference,
                    retained_mapped_target,
                    invocation_capacity_reserved,
                    dangerous_tool_auto_approval,
                    recorded_user_overrides,
                    system_prompt,
                    tool_entries,
                    reasoning_provenance,
                }) => {
                    break (
                        request,
                        credential_reference,
                        retained_mapped_target,
                        invocation_capacity_reserved,
                        dangerous_tool_auto_approval,
                        recorded_user_overrides,
                        system_prompt,
                        tool_entries,
                        reasoning_provenance,
                    );
                }
                Ok(PrepareModelCallOutcome::TargetUnavailable(failed)) => {
                    report_turn_terminalization(
                        failed.session(),
                        failed.turn(),
                        TurnTerminalOutcome::TargetUnavailable,
                    );
                    return Ok(ModelCallExecutionOutcome::TargetUnavailable(failed));
                }
                Err(error)
                    if error.operator_failure_class()
                        == OperatorFailureClass::IdentityCollision =>
                {
                    continue;
                }
                Err(error) => return Err(ModelCallExecutionError::Prepare(error)),
            }
        };

        let (
            prepared,
            credential_reference,
            retained_mapped_target,
            invocation_capacity_reserved,
            dangerous_tool_auto_approval,
            recorded_user_overrides,
            system_prompt,
            tool_entries,
            reasoning_provenance,
        ) = prepared;
        let call = prepared.call().id();
        let attempt = prepared.attempt();
        let turn = prepared.turn();
        let advertised_tools = self.catalog.definitions();
        let operation = match PreparedModelOperation::render_within(
            *prepared,
            credential_reference,
            system_prompt,
            advertised_tools.clone(),
            &tool_entries,
            &reasoning_provenance,
            self.retained_frontier_content_limit,
        ) {
            Ok(mut operation) => {
                operation.retained_mapped_target = retained_mapped_target;
                operation.invocation_capacity_reserved = invocation_capacity_reserved;
                operation
            }
            // The retained-content ceiling is a safety bound on the same
            // automatic tool loop the round ceiling bounds, so it closes the
            // checkpoint through the same terminal contract rather than
            // surfacing as an operator failure. Refusing here, before the
            // messages exist, is what keeps the closure reachable at all.
            Err(ModelFrontierRenderingError::RetainedFrontierContentLimitExceeded {
                observed_bytes,
                limit_bytes,
            }) => {
                tracing::warn!(
                    session_id = %session.as_uuid(),
                    turn_id = %turn.as_uuid(),
                    model_call_id = %call.into_uuid(),
                    retained_frontier_content_limit = limit_bytes,
                    observed_retained_frontier_content_bytes = observed_bytes,
                    "retained frontier content limit reached"
                );
                return self
                    .commit_prepared_failure(
                        session,
                        turn,
                        call,
                        PreparedModelCallFailureCause::ToolRoundLimitReached,
                        None,
                    )
                    .await;
            }
            Err(error) => return Err(ModelCallExecutionError::Render(error)),
        };
        // A deployment that configures no automatic tool-round ceiling leaves the
        // loop bounded by the retained-content ceiling above and by the turn's
        // own liveness watchdogs, so an absent limit admits the round rather than
        // substituting one the operator did not ask for.
        let prepared_request = operation.request().clone();
        let observed_tool_rounds = automatic_tool_round_count(turn, operation.messages());
        if let Some(tool_round_limit) = self.max_automatic_tool_rounds_per_turn
            && observed_tool_rounds >= tool_round_limit
        {
            tracing::warn!(
                session_id = %session.as_uuid(),
                turn_id = %turn.as_uuid(),
                model_call_id = %call.into_uuid(),
                tool_round_limit,
                observed_tool_rounds,
                "automatic tool-round limit reached"
            );
            return self
                .commit_prepared_failure(
                    session,
                    turn,
                    call,
                    PreparedModelCallFailureCause::ToolRoundLimitReached,
                    None,
                )
                .await;
        }
        let preparation_cancellation = self.authorization.cancellation_signal(session, call);
        let capability = match self
            .provider
            .prepare_capability(operation, preparation_cancellation)
            .await
        {
            Ok(ModelCallCapabilityPreparation::Ready(capability)) => capability,
            Ok(ModelCallCapabilityPreparation::Cancelled) => {
                return Ok(ModelCallExecutionOutcome::NoWork);
            }
            Ok(ModelCallCapabilityPreparation::KnownFailure) => {
                return self
                    .commit_prepared_failure(
                        session,
                        turn,
                        call,
                        PreparedModelCallFailureCause::CapabilityKnownFailure,
                        None,
                    )
                    .await;
            }
            Ok(ModelCallCapabilityPreparation::AttachmentFailure(
                AttachmentPreparationFailure::Unavailable,
            )) => {
                return Ok(ModelCallExecutionOutcome::AttachmentUnavailable);
            }
            Ok(ModelCallCapabilityPreparation::AttachmentFailure(
                failure @ (AttachmentPreparationFailure::TooLarge { .. }
                | AttachmentPreparationFailure::Missing
                | AttachmentPreparationFailure::Corrupt),
            )) => {
                return self
                    .commit_prepared_failure(
                        session,
                        turn,
                        call,
                        PreparedModelCallFailureCause::CapabilityKnownFailure,
                        Some(failure),
                    )
                    .await;
            }
            Err(error) => {
                return Err(ModelCallExecutionError::CapabilityPreparation(error));
            }
        };

        let permit = self.gate.acquire(attempt).await;
        let authorized = match self.authorization.authorize(session, call).await {
            Ok(AuthorizeModelCallOutcome::NoSend) => {
                drop(capability);
                drop(permit);
                return Ok(ModelCallExecutionOutcome::NoWork);
            }
            Ok(AuthorizeModelCallOutcome::Authorized(authorized)) => *authorized,
            Err(error)
                if matches!(
                    error.operator_failure_class(),
                    OperatorFailureClass::Infrastructure {
                        commit_ambiguous: true
                    }
                ) =>
            {
                match self
                    .authorization
                    .reread_after_ambiguous_commit(session, &prepared_request)
                    .await
                {
                    Ok(ModelCallAuthorizationReread::Prepared) => {
                        drop(capability);
                        drop(permit);
                        return Err(ModelCallExecutionError::Authorization(error));
                    }
                    Ok(ModelCallAuthorizationReread::InFlight(authorized)) => {
                        drop(capability);
                        drop(permit);
                        let non_consumption = authorized
                            .observation_correlation()
                            .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
                        return self
                            .commit_terminal_observation(session, non_consumption, Box::new([]))
                            .await;
                    }
                    Ok(ModelCallAuthorizationReread::CancellationRequested(stopped)) => {
                        drop(capability);
                        drop(permit);
                        let cancellation = stopped
                            .observation_correlation()
                            .bind_terminal_observation(ModelCallTerminalObservation::Cancelled);
                        return self
                            .commit_terminal_observation(session, cancellation, Box::new([]))
                            .await;
                    }
                    Ok(ModelCallAuthorizationReread::Cancelled) => {
                        drop(capability);
                        drop(permit);
                        return Ok(ModelCallExecutionOutcome::NoWork);
                    }
                    Err(reread_error) => {
                        drop(capability);
                        drop(permit);
                        self.retained_state = Some(RetainedModelCallExecutionState {
                            state:
                                RetainedModelCallExecutionStateKind::AuthorizationNonConsumption {
                                    session,
                                    prepared: Box::new(prepared_request),
                                },
                        });
                        return Err(ModelCallExecutionError::AuthorizationReread {
                            authorization_error: error,
                            reread_error,
                        });
                    }
                }
            }
            Err(error) => return Err(ModelCallExecutionError::Authorization(error)),
        };
        let acceptance_possible = move || drop(permit);
        let invocation_cancellation = self.authorization.cancellation_signal(session, call);
        let correlation = authorized.observation_correlation();
        let started = std::time::Instant::now();
        let observation = self
            .provider
            .invoke(
                authorized,
                capability,
                acceptance_possible,
                invocation_cancellation,
            )
            .await;
        let observation = match observation {
            Ok(observation) => observation,
            Err(error) => {
                let evidence = signalbox_domain::ModelCallAmbiguityEvidence::new(&format!(
                    "classification_point=provider_invocation_error\ncause={}\nelapsed_ms={}\nresponse_progress=unknown",
                    error.operator_failure_cause_code(),
                    started.elapsed().as_millis(),
                ));
                if let Err(record_error) = self
                    .observation
                    .retain_provider_failure_evidence(correlation, evidence)
                    .await
                {
                    tracing::warn!(session_id = %session.as_uuid(), model_call_id = %call.as_uuid(),
                        cause_code = record_error.operator_failure_cause_code(),
                        "provider failure diagnostic could not be retained");
                }
                return Err(ModelCallExecutionError::Provider(error));
            }
        };

        let tool_approvals = self.tool_approvals(
            observation.observation(),
            dangerous_tool_auto_approval,
            &advertised_tools,
            &recorded_user_overrides,
        );
        self.commit_terminal_observation(session, observation, tool_approvals)
            .await
    }

    #[allow(
        clippy::result_large_err,
        reason = "The error retains the correlated observation inline for retry."
    )]
    async fn commit_prepared_failure(
        &mut self,
        session: SessionId,
        turn: TurnId,
        call: ModelCallId,
        cause: PreparedModelCallFailureCause,
        attachment_failure: Option<AttachmentPreparationFailure>,
    ) -> Result<
        ModelCallExecutionOutcome,
        ModelCallExecutionError<
            Prepare::Error,
            Failure::Error,
            Authorization::Error,
            Provider::Error,
            Observation::Error,
        >,
    > {
        loop {
            let identities = self.next_failed_identities();
            let ids = &mut self.ids;
            let next_turn = move |_| ids.next_turn_id();
            match self
                .failure
                .fail_prepared(
                    session,
                    call,
                    cause,
                    attachment_failure,
                    identities,
                    next_turn,
                )
                .await
            {
                Ok(failed) => {
                    let terminal_outcome = TurnTerminalOutcome::from(cause);
                    report_turn_terminalization(failed.session(), failed.turn(), terminal_outcome);
                    return Ok(match cause {
                        PreparedModelCallFailureCause::CapabilityKnownFailure => {
                            ModelCallExecutionOutcome::CapabilityKnownFailure(Box::new(failed))
                        }
                        PreparedModelCallFailureCause::ToolRoundLimitReached => {
                            ModelCallExecutionOutcome::ToolRoundLimitReached(Box::new(failed))
                        }
                    });
                }
                Err(error)
                    if error.operator_failure_class()
                        == OperatorFailureClass::IdentityCollision =>
                {
                    continue;
                }
                Err(error) => {
                    self.retained_state = Some(RetainedModelCallExecutionState {
                        state: RetainedModelCallExecutionStateKind::PreparedFailure {
                            session,
                            turn,
                            call,
                            cause,
                            attachment_failure,
                        },
                    });
                    return Err(ModelCallExecutionError::PreparedFailureCommit(error));
                }
            }
        }
    }

    #[allow(
        clippy::result_large_err,
        reason = "The error retains the correlated observation inline for retry."
    )]
    async fn commit_terminal_observation(
        &mut self,
        session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
        tool_approvals: Box<[InitialToolApproval]>,
    ) -> Result<
        ModelCallExecutionOutcome,
        ModelCallExecutionError<
            Prepare::Error,
            Failure::Error,
            Authorization::Error,
            Provider::Error,
            Observation::Error,
        >,
    > {
        loop {
            let mut identities =
                self.next_terminal_identities(observation.observation(), &tool_approvals);
            // Supply successor identity candidates for classified pool triggers;
            // persistence decides whether to admit a successor or terminalize.
            if matches!(
                observation.provider_failure_cause(),
                Some(
                    signalbox_domain::ProviderModelCallFailureCause::RateLimited
                        | signalbox_domain::ProviderModelCallFailureCause::QuotaExhausted
                        | signalbox_domain::ProviderModelCallFailureCause::Overloaded
                        | signalbox_domain::ProviderModelCallFailureCause::ProviderInternal
                        | signalbox_domain::ProviderModelCallFailureCause::CredentialRejected
                )
            ) && let ModelCallTerminalIdentityCandidates::Exact(
                signalbox_domain::ModelCallTerminalIdentities::Failed(failed),
            ) = identities
            {
                identities = ModelCallTerminalIdentityCandidates::Availability {
                    failed,
                    successor_attempt: self.ids.next_turn_attempt_id(),
                };
            }
            let ids = &mut self.ids;
            let next_turn = move |_| ids.next_turn_id();
            match self
                .observation
                .commit_observation(session, observation.clone(), identities, next_turn)
                .await
            {
                Ok(Some(ModelCallObservationCommitOutcome::CredentialWait(_))) => {
                    return Ok(ModelCallExecutionOutcome::NoWork);
                }
                Ok(Some(ModelCallObservationCommitOutcome::Terminal(outcome))) => {
                    report_model_call_terminalization(&outcome);
                    return Ok(ModelCallExecutionOutcome::ObservationCommitted(outcome));
                }
                Ok(Some(ModelCallObservationCommitOutcome::AvailabilitySuccessor(successor))) => {
                    return Ok(ModelCallExecutionOutcome::AvailabilitySuccessor(successor));
                }
                Ok(Some(ModelCallObservationCommitOutcome::PoolExhausted(exhausted))) => {
                    if let CredentialPoolExhaustedOutcome::AfterCall { terminal, .. } = &exhausted {
                        report_model_call_terminalization(terminal);
                    }
                    return Ok(ModelCallExecutionOutcome::PoolExhausted(Box::new(
                        exhausted,
                    )));
                }
                Ok(None) => return Ok(ModelCallExecutionOutcome::NoWork),
                Err(error)
                    if error.operator_failure_class()
                        == OperatorFailureClass::IdentityCollision =>
                {
                    continue;
                }
                Err(error) => {
                    self.retained_state = Some(RetainedModelCallExecutionState {
                        state: RetainedModelCallExecutionStateKind::TerminalObservation {
                            session,
                            observation: Box::new(observation.clone()),
                            tool_approvals,
                        },
                    });
                    return Err(ModelCallExecutionError::ObservationCommit {
                        error,
                        retained_observation: observation,
                    });
                }
            }
        }
    }

    fn next_failed_identities(&mut self) -> FailedModelCallTurnIdentities {
        FailedModelCallTurnIdentities::new(
            self.ids.next_semantic_entry_id(),
            self.ids.next_context_frontier_id(),
        )
    }

    pub(super) fn next_terminal_identities(
        &mut self,
        observation: &ModelCallTerminalObservation,
        tool_approvals: &[InitialToolApproval],
    ) -> ModelCallTerminalIdentityCandidates {
        let exact = match observation {
            ModelCallTerminalObservation::Completed { assistant_text } => {
                let assistant_entries = (0..assistant_text.len())
                    .map(|_| self.ids.next_semantic_entry_id())
                    .collect();
                ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                    assistant_entries,
                    self.ids.next_semantic_entry_id(),
                    self.ids.next_context_frontier_id(),
                ))
            }
            ModelCallTerminalObservation::CompletedWithProviderCompaction { response, .. }
            | ModelCallTerminalObservation::CompletedWithProviderReasoning { response } => {
                let assistant_entries = (0..response.len())
                    .map(|_| self.ids.next_semantic_entry_id())
                    .collect();
                ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                    assistant_entries,
                    self.ids.next_semantic_entry_id(),
                    self.ids.next_context_frontier_id(),
                ))
            }
            ModelCallTerminalObservation::CompletedWithTools { response, .. } => {
                let mut approval_index = 0usize;
                let mut continuing = Vec::with_capacity(response.parts().len());
                let mut stopped = Vec::with_capacity(response.parts().len());
                let mut every_request_approved = true;
                for part in response.parts() {
                    match part {
                        AssistantResponsePart::Text(_) => {
                            continuing.push(ToolResponsePartIdentity::text(
                                self.ids.next_semantic_entry_id(),
                            ));
                        }
                        AssistantResponsePart::ProviderCompaction(_) => {
                            continuing.push(ToolResponsePartIdentity::provider_compaction(
                                self.ids.next_semantic_entry_id(),
                            ));
                        }
                        AssistantResponsePart::ProviderReasoning(_) => {
                            continuing.push(ToolResponsePartIdentity::provider_reasoning(
                                self.ids.next_semantic_entry_id(),
                            ));
                        }
                        AssistantResponsePart::ToolCall(_) => {
                            // A retained-policy count mismatch is an internal
                            // defect. Confirm is the conservative candidate:
                            // it cannot grant unattended execution, and the
                            // domain still rejects it under blanket posture.
                            let approval = tool_approvals
                                .get(approval_index)
                                .copied()
                                .unwrap_or(InitialToolApproval::Confirm);
                            approval_index += 1;
                            every_request_approved &= !approval.requires_decision();
                            continuing.push(ToolResponsePartIdentity::tool_call(
                                self.ids.next_semantic_entry_id(),
                                self.ids.next_tool_request_id(),
                                approval,
                            ));
                        }
                    }
                }
                debug_assert_eq!(approval_index, tool_approvals.len());
                let continuation_attempt =
                    every_request_approved.then(|| self.ids.next_turn_attempt_id());
                let mut stopped_approval_index = 0usize;
                for part in response.parts() {
                    match part {
                        AssistantResponsePart::Text(_) => {
                            stopped.push(StoppedToolResponsePartIdentity::text(
                                self.ids.next_semantic_entry_id(),
                            ));
                        }
                        AssistantResponsePart::ProviderCompaction(_) => {
                            stopped.push(StoppedToolResponsePartIdentity::provider_compaction(
                                self.ids.next_semantic_entry_id(),
                            ));
                        }
                        AssistantResponsePart::ProviderReasoning(_) => {
                            stopped.push(StoppedToolResponsePartIdentity::provider_reasoning(
                                self.ids.next_semantic_entry_id(),
                            ));
                        }
                        AssistantResponsePart::ToolCall(_) => {
                            let approval = tool_approvals
                                .get(stopped_approval_index)
                                .copied()
                                .unwrap_or(InitialToolApproval::Confirm);
                            stopped_approval_index += 1;
                            stopped.push(StoppedToolResponsePartIdentity::tool_call(
                                self.ids.next_semantic_entry_id(),
                                self.ids.next_tool_request_id(),
                                self.ids.next_semantic_entry_id(),
                                approval,
                            ));
                        }
                    }
                }
                debug_assert_eq!(stopped_approval_index, tool_approvals.len());
                return ModelCallTerminalIdentityCandidates::ToolRound {
                    continuing: ToolRoundModelCallIdentities::new(
                        continuing,
                        self.ids.next_context_frontier_id(),
                        continuation_attempt,
                    ),
                    stopped: StoppedToolRoundModelCallIdentities::new(
                        stopped,
                        self.ids.next_semantic_entry_id(),
                        self.ids.next_context_frontier_id(),
                    ),
                };
            }
            ModelCallTerminalObservation::KnownFailed => {
                ModelCallTerminalIdentities::Failed(self.next_failed_identities())
            }
            ModelCallTerminalObservation::Cancelled => {
                ModelCallTerminalIdentities::PhysicalCancellation(
                    PhysicalCancellationModelCallTurnIdentities::new(
                        self.ids.next_semantic_entry_id(),
                        self.ids.next_context_frontier_id(),
                    ),
                )
            }
            ModelCallTerminalObservation::Refused => ModelCallTerminalIdentities::Refused(
                RefusedModelCallTurnIdentities::new(self.ids.next_context_frontier_id()),
            ),
            ModelCallTerminalObservation::RefusedWithProviderCompaction {
                provider_compaction,
                ..
            } => ModelCallTerminalIdentities::Refused(
                RefusedModelCallTurnIdentities::new(self.ids.next_context_frontier_id())
                    .with_provider_compaction_entries(
                        (0..provider_compaction.len())
                            .map(|_| self.ids.next_semantic_entry_id())
                            .collect(),
                    ),
            ),
            ModelCallTerminalObservation::Ambiguous => ModelCallTerminalIdentities::Ambiguous(
                AmbiguousModelCallTurnIdentities::new(self.ids.next_context_frontier_id()),
            ),
        };
        ModelCallTerminalIdentityCandidates::Exact(exact)
    }

    /// Selects one initial approval per proposal, consuming recorded user
    /// overrides.
    ///
    /// An recorded override substitutes for the judge only where the judge would
    /// otherwise decide: the base selection must be `Delegated`, and the
    /// proposal must re-propose the exact denied command. Each recorded override
    /// is consumed at most once per response — a second identical proposal
    /// parks for the judge again — mirroring the one-shot uniqueness the
    /// decision table enforces durably.
    pub(super) fn tool_approvals(
        &self,
        observation: &ModelCallTerminalObservation,
        posture: DangerousToolAutoApproval,
        advertised_tools: &[ToolDefinition],
        recorded_user_overrides: &[RecordedUserOverride],
    ) -> Box<[InitialToolApproval]> {
        let ModelCallTerminalObservation::CompletedWithTools { response, .. } = observation else {
            return Box::new([]);
        };
        let mut remaining_overrides: Vec<&RecordedUserOverride> =
            recorded_user_overrides.iter().collect();
        response
            .parts()
            .iter()
            .filter_map(|part| match part {
                AssistantResponsePart::Text(_)
                | AssistantResponsePart::ProviderCompaction(_)
                | AssistantResponsePart::ProviderReasoning(_) => None,
                AssistantResponsePart::ToolCall(proposal) => {
                    if proposal.inadmissible_reason().is_some() {
                        return Some(InitialToolApproval::Inadmissible);
                    }
                    if proposal.is_suppressed() {
                        return Some(InitialToolApproval::RuntimeSafetyDeny);
                    }
                    let definition = advertised_tools
                        .iter()
                        .find(|definition| definition.name() == proposal.name());
                    if definition.is_some_and(|definition| {
                        definition.requires_approval_judge(proposal.arguments())
                    }) {
                        return Some(InitialToolApproval::Delegated);
                    }
                    let base = initial_tool_approval(posture, definition);
                    if base != InitialToolApproval::Delegated {
                        return Some(base);
                    }
                    let matched = remaining_overrides
                        .iter()
                        .position(|recorded| recorded.matches_proposal(proposal));
                    Some(match matched {
                        Some(index) => {
                            let recorded = remaining_overrides.remove(index);
                            InitialToolApproval::UserOverride {
                                command: recorded.command(),
                                denied_request: recorded.denied_request(),
                            }
                        }
                        None => base,
                    })
                }
            })
            .collect()
    }
}
