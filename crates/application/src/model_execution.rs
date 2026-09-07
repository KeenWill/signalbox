//! First text-only model-call execution orchestration.
//!
//! docs/spec/model-call-execution.md owns the staged transaction and
//! provider-effect order. The application keeps persistence, provider
//! capability preparation, send authorization, provider interaction, and
//! terminal observation distinct.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    future::Future,
    num::NonZeroU64,
    sync::{Arc, Weak},
    time::Duration,
};

// The configured automatic tool-round ceiling alone does not bound memory: it
// multiplies against the 32-request batch bound and the 1 MiB argument and
// result bounds, so a 256-round deployment would admit 16 GiB of retained
// argument and result text where 32 rounds admitted 2 GiB. Retained content is
// therefore bounded on its own terms, independently of the round ceiling — and
// of whether a deployment configured one at all. One maximal round retains 32
// requests times 1 MiB of arguments plus 1 MiB of results, so this admits four
// maximal rounds while leaving the round ceiling operative for the
// kilobyte-scale results real executors return. It bounds every kind of content
// a render clones, not tool evidence alone: assistant text carries no length
// bound of its own beyond the transport cap on a single response, so a ceiling
// blind to it would be multiplied by the same round count it is meant to
// contain. It also sits far above any provider context window, so it cannot
// refuse a turn a provider would accept.
const MAX_RETAINED_FRONTIER_CONTENT_BYTES: usize = 256 * 1024 * 1024;

// Worst-case compact JSON for maximum checked metadata, u64 length, and digest.
const MAX_RENDERED_ATTACHMENT_STUB_BYTES: usize = 2_304;

use signalbox_domain::{
    AcceptedInputId, AmbiguousModelCallTurnIdentities, AssistantResponsePart, AssistantText,
    AttachmentKind, AuthorizedModelCall, AvailabilitySuccessorModelCallTurn, BlobDigest,
    CompletedModelCallIdentities, ContextCompactionRange, ContextFrontierId,
    ContextFrontierProjection, ContextFrontierProjectionFailure,
    CorrelatedModelCallTerminalObservation, CredentialPoolExhaustedModelCallTurn,
    DangerousToolAutoApproval, DelegationContent, DelegationMessageId, DelegationOutcome,
    DelegationWaitMode, DirectModelSelection, FailedModelCallTurn, FailedModelCallTurnIdentities,
    ImportedSourceAttestation, ImportedSpeaker, ImportedText, ImportedTranscriptContent,
    ImportedTranscriptEntryId, InitialToolApproval, ModelCallId, ModelCallTerminalIdentities,
    ModelCallTerminalObservation, ModelCallTerminalOutcome,
    PhysicalCancellationModelCallTurnIdentities, PreparedModelCallRequest, ProviderCompactionBlock,
    RecordedUserOverride, RefusedModelCallTurnIdentities, SemanticTranscriptEntryId,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryRef,
    SessionConfigurationDefaultsVersion, SessionId, SessionSystemPrompt,
    StopRequestedModelCallTurn, StoppedToolResponsePartIdentity,
    StoppedToolRoundModelCallIdentities, ToolApprovalDecision, ToolAttemptEnd, ToolDenialReason,
    ToolExecutionError, ToolRequest, ToolRequestId, ToolResponsePartIdentity, ToolResultContent,
    ToolRoundModelCallIdentities, TurnAttemptId, TurnId, UserContent, UserContentPart,
};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::{
    ClassifyOperatorFailure, NoToolCatalog, OperatorFailureClass, ResolvedToolConversationEntry,
    ToolCatalog, ToolDefinition, tool_loop::initial_tool_approval,
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
    const fn with_retained_frontier_content_limit(mut self, limit: usize) -> Self {
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
                Ok(PrepareModelCallOutcome::NoWork) => {
                    return Ok(ModelCallExecutionOutcome::NoWork);
                }
                Ok(PrepareModelCallOutcome::RetryBackoff(delay)) => {
                    return Ok(ModelCallExecutionOutcome::RetryBackoff(delay));
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
                Ok(PrepareModelCallOutcome::Ready {
                    request,
                    credential_reference,
                    dangerous_tool_auto_approval,
                    recorded_user_overrides,
                    system_prompt,
                    tool_entries,
                    reasoning_provenance,
                }) => {
                    break (
                        request,
                        credential_reference,
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
            dangerous_tool_auto_approval,
            recorded_user_overrides,
            system_prompt,
            tool_entries,
            reasoning_provenance,
        ) = prepared;
        let call = prepared.call().id();
        let attempt = prepared.attempt();
        let turn = prepared.turn();
        let prepared_request = (*prepared).clone();
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
            Ok(operation) => operation,
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
        let observation = self
            .provider
            .invoke(
                authorized,
                capability,
                acceptance_possible,
                invocation_cancellation,
            )
            .await;
        let observation = observation.map_err(ModelCallExecutionError::Provider)?;

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
            // Every classified pool trigger evaluates its frozen action, not
            // only the ones that could substitute a member on this turn.
            // `switch_next_turn`, `avoid_new_sessions`, and `quarantine`
            // terminalize the call and persist a durable exclusion, so gating
            // them on substitution proof silently degraded them to `stay`.
            // Persistence still requires the proof before creating a successor.
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

    fn next_terminal_identities(
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
    fn tool_approvals(
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
                    if proposal.is_suppressed() {
                        return Some(InitialToolApproval::RuntimeSafetyDeny);
                    }
                    let definition = advertised_tools
                        .iter()
                        .find(|definition| definition.name() == proposal.name());
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

mod content;
pub use content::{
    ModelAttachmentStub, ModelCallCredentialReference, ModelConversationMessage,
    ModelToolResultContent, ModelUserContent, ModelUserContentPart, ProviderReasoningProvenance,
};
use content::{SerializedAttachmentEnvelope, SerializedAttachmentStub};

mod provider;
pub use provider::{
    AttemptDispatchGate, InProcessAttemptDispatchGate, InProcessAttemptDispatchPermit,
    ModelCallExecutionIdGenerator, ModelCallProvider, UuidV7ModelCallExecutionIdGenerator,
};

mod outcome;
pub use outcome::{ModelCallExecutionError, ModelCallExecutionOutcome};

mod scripted;
pub use scripted::{
    ScriptedModelCallCapability, ScriptedModelCallError, ScriptedModelCallProvider,
    ScriptedModelCallStep,
};

mod render;
#[cfg(test)]
use render::render_frontier_messages;
pub use render::{ModelFrontierRenderingError, render_model_user_content};
use render::{projected_frontier_content_bytes, render_frontier_messages_with_placements};

mod prepared;
pub use prepared::PreparedModelOperation;

mod ports;
use ports::RetainedModelCallExecutionStateKind;
pub use ports::{
    AttachmentPreparationFailure, AuthorizeModelCallOutcome, AuthorizeModelCallTransaction,
    AvailabilitySuccessorOutcome, CommitModelCallObservationTransaction,
    CredentialPoolExhaustedOutcome, FailPreparedModelCallTransaction, ModelCallAuthorizationReread,
    ModelCallCapabilityPreparation, ModelCallInputTokenCount, ModelCallInputTokenCounter,
    ModelCallObservationCommitOutcome, ModelCallTerminalIdentityCandidates,
    PrepareModelCallOutcome, PrepareModelCallTransaction, PreparedModelCallFailureCause,
    RetainedModelCallExecutionState, RetainedModelCallObservationStatus,
    RetainedPreparedFailureStatus,
};

mod report;
use report::{
    TurnTerminalOutcome, automatic_tool_round_count, report_model_call_terminalization,
    report_turn_terminalization,
};

#[cfg(test)]
mod tests;
