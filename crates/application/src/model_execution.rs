//! First text-only model-call execution orchestration.
//!
//! ADR-0045 owns the staged transaction and provider-effect order. The
//! application keeps persistence, provider capability preparation, send
//! authorization, provider interaction, and terminal observation distinct.

use std::{
    collections::HashMap,
    error::Error,
    fmt,
    future::Future,
    sync::{Arc, Weak},
};

use signalbox_domain::{
    AcceptedInputId, AssistantText, AuthorizedModelCall, CompletedModelCallIdentities,
    ContextFrontierId, CorrelatedModelCallTerminalObservation, FailedModelCallTurn,
    FailedModelCallTurnIdentities, ModelCallId, ModelCallTerminalIdentities,
    ModelCallTerminalObservation, ModelCallTerminalOutcome, PreparedModelCallRequest,
    RefusedModelCallTurnIdentities, SemanticTranscriptEntryId, SemanticTranscriptEntryPayload,
    SemanticTranscriptEntryRef, SessionId, TurnAttemptId, TurnId, UserContent,
};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::{ClassifyOperatorFailure, OperatorFailureClass};

/// Application rendering of one semantic frontier entry as a provider message.
///
/// The source-qualified semantic entry, rather than a native turn assumption,
/// preserves the provenance of entries inherited across sessions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelConversationMessage {
    /// Exact accepted-input origin content rendered with the user role.
    User {
        /// The source-qualified semantic entry being rendered.
        source: SemanticTranscriptEntryRef,
        /// The immutable accepted input carrying this content.
        accepted_input: AcceptedInputId,
        /// Exact user-owned content.
        content: UserContent,
    },
    /// Exact assistant content rendered with the assistant role.
    Assistant {
        /// The source-qualified semantic entry being rendered.
        source: SemanticTranscriptEntryRef,
        /// The outcome-authoritative call that produced the content.
        producing_call: ModelCallId,
        /// Exact assistant-owned text.
        content: AssistantText,
    },
}

/// A checked prepared call plus its provider-neutral ordered messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedModelOperation {
    request: PreparedModelCallRequest,
    messages: Box<[ModelConversationMessage]>,
}

impl PreparedModelOperation {
    fn render(request: PreparedModelCallRequest) -> Result<Self, ModelFrontierRenderingError> {
        let mut messages = Vec::new();
        for entry in request.frontier_entries() {
            match entry.payload() {
                SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input } => {
                    let content = request.origin_content(*accepted_input).cloned().ok_or(
                        ModelFrontierRenderingError::MissingOriginContent {
                            entry: entry.reference(),
                            accepted_input: *accepted_input,
                        },
                    )?;
                    messages.push(ModelConversationMessage::User {
                        source: entry.reference(),
                        accepted_input: *accepted_input,
                        content,
                    });
                }
                SemanticTranscriptEntryPayload::AssistantText {
                    producing_call,
                    value,
                } => messages.push(ModelConversationMessage::Assistant {
                    source: entry.reference(),
                    producing_call: *producing_call,
                    content: value.clone(),
                }),
                SemanticTranscriptEntryPayload::AssistantToolUse { .. } => {
                    return Err(ModelFrontierRenderingError::UnsupportedAssistantToolUse {
                        entry: entry.reference(),
                    });
                }
                SemanticTranscriptEntryPayload::TurnFailed { .. }
                | SemanticTranscriptEntryPayload::TurnCompleted { .. } => {}
            }
        }
        Ok(Self {
            request,
            messages: messages.into_boxed_slice(),
        })
    }

    /// Borrows the checked durable request facts.
    pub const fn request(&self) -> &PreparedModelCallRequest {
        &self.request
    }

    /// Borrows the exact messages in frontier order.
    pub fn messages(&self) -> &[ModelConversationMessage] {
        &self.messages
    }
}

/// A checked frontier could not be projected into the current text-only input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelFrontierRenderingError {
    /// A frontier origin was missing its reconstituted accepted-input content.
    MissingOriginContent {
        /// The source-qualified origin entry.
        entry: SemanticTranscriptEntryRef,
        /// The accepted input whose content was absent.
        accepted_input: AcceptedInputId,
    },
    /// Tool-use projection is reserved until the tool decisions land.
    UnsupportedAssistantToolUse {
        /// The source-qualified entry that cannot yet be rendered.
        entry: SemanticTranscriptEntryRef,
    },
}

impl fmt::Display for ModelFrontierRenderingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingOriginContent { .. } => {
                formatter.write_str("model frontier origin content is missing")
            }
            Self::UnsupportedAssistantToolUse { .. } => {
                formatter.write_str("model frontier contains unsupported assistant tool use")
            }
        }
    }
}

impl Error for ModelFrontierRenderingError {}

impl ClassifyOperatorFailure for ModelFrontierRenderingError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

/// Result of the authoritative prepare-call transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareModelCallOutcome {
    /// The scheduling hint no longer identifies runnable work.
    NoWork,
    /// A new exact `Prepared` call committed; this invocation stops here.
    Checkpointed(ModelCallId),
    /// A previously committed `Prepared` request may prepare its capability.
    Ready(Box<PreparedModelCallRequest>),
    /// Immutable target resolution failed and the turn closed atomically.
    TargetUnavailable(Box<FailedModelCallTurn>),
    /// Acknowledged steering prevents this model-call safe point.
    PendingSteering {
        /// The earliest accepted input proving that steering remains pending.
        accepted_input: AcceptedInputId,
    },
}

/// Authoritative transaction that prepares or reloads one initial model call.
pub trait PrepareModelCallTransaction {
    /// Adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Runs the serialized prepare role with fresh application candidates.
    fn prepare(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> impl Future<Output = Result<PrepareModelCallOutcome, Self::Error>> + Send;
}

/// Guarded transaction closing a trustworthy local pre-send failure.
pub trait FailPreparedModelCallTransaction {
    /// Adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Closes the exact prepared call without authorizing provider work.
    ///
    /// `next_reclassified_turn` is an application-owned fresh-candidate
    /// supplier. The adapter may call it once for each pending steering input
    /// discovered under its authoritative lock; it must not mint identities.
    fn fail_prepared<NextTurn>(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        identities: FailedModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
    ) -> impl Future<Output = Result<FailedModelCallTurn, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send;
}

/// Distinct transaction that durably authorizes one physical send.
pub trait AuthorizeModelCallTransaction {
    /// Adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Reloads exact authority and commits `Prepared -> InFlight`.
    fn authorize(
        &mut self,
        session: SessionId,
        call: ModelCallId,
    ) -> impl Future<Output = Result<AuthorizedModelCall, Self::Error>> + Send;

    /// Rereads an authorization whose commit acknowledgement was lost.
    fn reread_after_ambiguous_commit(
        &mut self,
        session: SessionId,
        prepared: &PreparedModelCallRequest,
    ) -> impl Future<Output = Result<ModelCallAuthorizationReread, Self::Error>> + Send;
}

/// Authoritative state after an ambiguous send-authorization commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallAuthorizationReread {
    /// The authorization rolled back and the exact call remains Prepared.
    Prepared,
    /// The authorization committed; this exact issued call was not consumed.
    InFlight(Box<AuthorizedModelCall>),
}

/// Fresh transaction committing a provider-neutral terminal observation.
pub trait CommitModelCallObservationTransaction {
    /// Adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Reloads issued authority and atomically applies one observation.
    ///
    /// The successor supplier has the same application-owned, adapter-consumed
    /// contract as [`FailPreparedModelCallTransaction::fail_prepared`].
    fn commit_observation<NextTurn>(
        &mut self,
        session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
        identities: ModelCallTerminalIdentities,
        next_reclassified_turn: NextTurn,
    ) -> impl Future<Output = Result<ModelCallTerminalOutcome, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send;

    /// Rereads whether one retained terminal observation was committed.
    fn reread_observation(
        &mut self,
        session: SessionId,
        observation: &CorrelatedModelCallTerminalObservation,
    ) -> impl Future<Output = Result<RetainedModelCallObservationStatus, Self::Error>> + Send;
}

/// Authoritative status of one unchanged in-memory terminal observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetainedModelCallObservationStatus {
    /// The exact issued call still awaits this observation.
    Pending,
    /// The exact observation is already represented durably.
    AlreadyCommitted,
}

/// Adapter-local result of credential lookup and capability preparation.
pub enum ModelCallCapabilityPreparation<Capability> {
    /// A call-bound one-shot capability is ready to move into provider work.
    Ready(Capability),
    /// A trustworthy ordinary local failure occurred before send authorization.
    KnownFailure,
}

/// Provider adapter boundary surrounding an opaque, one-shot send capability.
pub trait ModelCallProvider {
    /// Adapter-owned capability; application code only moves this value.
    type Capability;
    /// Sanitized adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Resolves credentials internally and prepares an exact call capability.
    fn prepare_capability(
        &mut self,
        operation: PreparedModelOperation,
    ) -> impl Future<Output = Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>> + Send;

    /// Consumes one capability after durable send authorization.
    fn invoke<AcceptancePossible>(
        &mut self,
        authorized: AuthorizedModelCall,
        capability: Self::Capability,
        acceptance_possible: AcceptancePossible,
    ) -> impl Future<Output = Result<CorrelatedModelCallTerminalObservation, Self::Error>> + Send
    where
        AcceptancePossible: FnOnce() + Send;
}

/// Supplies all hub-minted execution candidates.
pub trait ModelCallExecutionIdGenerator {
    /// Generates a distinct model-call candidate.
    fn next_model_call_id(&mut self) -> ModelCallId;
    /// Generates a distinct semantic-entry candidate.
    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId;
    /// Generates a distinct context-frontier candidate.
    fn next_context_frontier_id(&mut self) -> ContextFrontierId;
    /// Generates a distinct reclassified successor-turn candidate.
    fn next_turn_id(&mut self) -> TurnId;
}

/// Production UUIDv7 generator for model-call execution candidates.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7ModelCallExecutionIdGenerator;

impl ModelCallExecutionIdGenerator for UuidV7ModelCallExecutionIdGenerator {
    fn next_model_call_id(&mut self) -> ModelCallId {
        ModelCallId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        ContextFrontierId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_turn_id(&mut self) -> TurnId {
        TurnId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// Process-shared ordering gate between dispatch and attempt-stop transitions.
pub trait AttemptDispatchGate {
    /// Opaque permit retained across the provider acceptance-crossing window.
    type Permit: Send;

    /// Acquires exclusive ordering for one physical attempt.
    fn acquire(&self, attempt: TurnAttemptId) -> impl Future<Output = Self::Permit> + Send;
}

/// Cloneable attempt-keyed in-process dispatch gate.
#[derive(Clone, Debug, Default)]
pub struct InProcessAttemptDispatchGate {
    attempts: Arc<Mutex<HashMap<TurnAttemptId, Weak<Mutex<()>>>>>,
}

/// Opaque permit from [`InProcessAttemptDispatchGate`].
pub struct InProcessAttemptDispatchPermit {
    _guard: OwnedMutexGuard<()>,
}

impl AttemptDispatchGate for InProcessAttemptDispatchGate {
    type Permit = InProcessAttemptDispatchPermit;

    fn acquire(&self, attempt: TurnAttemptId) -> impl Future<Output = Self::Permit> + Send {
        let attempts = Arc::clone(&self.attempts);
        async move {
            let attempt_gate = {
                let mut known = attempts.lock().await;
                known.retain(|_, gate| gate.strong_count() > 0);
                known
                    .get(&attempt)
                    .and_then(Weak::upgrade)
                    .unwrap_or_else(|| {
                        let gate = Arc::new(Mutex::new(()));
                        known.insert(attempt, Arc::downgrade(&gate));
                        gate
                    })
            };
            InProcessAttemptDispatchPermit {
                _guard: attempt_gate.lock_owned().await,
            }
        }
    }
}

/// Completed stage of one service invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallExecutionOutcome {
    /// The scheduling hint no longer identifies runnable work.
    NoWork,
    /// A new prepared checkpoint committed and requires a later invocation.
    Checkpointed(ModelCallId),
    /// Target resolution failed before call creation.
    TargetUnavailable(Box<FailedModelCallTurn>),
    /// Pending steering prevents preparation.
    PendingSteering {
        /// The earliest accepted input proving that steering remains pending.
        accepted_input: AcceptedInputId,
    },
    /// A trustworthy local capability failure closed the prepared call.
    CapabilityKnownFailure(Box<FailedModelCallTurn>),
    /// The provider observation committed its authoritative result.
    ObservationCommitted(Box<ModelCallTerminalOutcome>),
    /// A retained observation's earlier commit was proven to have landed.
    ObservationAlreadyCommitted(ModelCallId),
}

/// Failure annotated with the exact orchestration stage that failed.
#[derive(Debug)]
pub enum ModelCallExecutionError<
    PrepareError,
    FailureError,
    AuthorizationError,
    ProviderError,
    ObservationError,
> {
    /// The prepare-call transaction failed.
    Prepare(PrepareError),
    /// Provider-neutral request rendering failed closed.
    Render(ModelFrontierRenderingError),
    /// Credential lookup or capability preparation failed as an operator error.
    CapabilityPreparation(ProviderError),
    /// The guarded trustworthy-capability-failure transaction failed.
    CapabilityFailureCommit(FailureError),
    /// Durable send authorization failed.
    Authorization(AuthorizationError),
    /// Authoritative reread after an ambiguous authorization also failed.
    AuthorizationReread {
        /// The original commit-ambiguous authorization failure.
        authorization_error: AuthorizationError,
        /// The failure to establish whether authorization committed.
        reread_error: AuthorizationError,
    },
    /// Provider work produced no trustworthy observation.
    Provider(ProviderError),
    /// The terminal-observation transaction failed.
    ObservationCommit {
        /// The failed observation transaction or authoritative reread.
        error: ObservationError,
        /// The unchanged provider observation retained for a later pass.
        retained_observation: CorrelatedModelCallTerminalObservation,
    },
}

impl<PrepareError, FailureError, AuthorizationError, ProviderError, ObservationError> fmt::Display
    for ModelCallExecutionError<
        PrepareError,
        FailureError,
        AuthorizationError,
        ProviderError,
        ObservationError,
    >
where
    PrepareError: fmt::Display,
    FailureError: fmt::Display,
    AuthorizationError: fmt::Display,
    ProviderError: fmt::Display,
    ObservationError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepare(error) => write!(formatter, "model-call prepare stage failed: {error}"),
            Self::Render(error) => write!(formatter, "model-call render stage failed: {error}"),
            Self::CapabilityPreparation(error) => {
                write!(formatter, "model-call capability stage failed: {error}")
            }
            Self::CapabilityFailureCommit(error) => {
                write!(
                    formatter,
                    "model-call capability-failure commit failed: {error}"
                )
            }
            Self::Authorization(error) => {
                write!(formatter, "model-call authorization stage failed: {error}")
            }
            Self::AuthorizationReread { reread_error, .. } => {
                write!(
                    formatter,
                    "model-call authorization reread failed: {reread_error}"
                )
            }
            Self::Provider(error) => write!(formatter, "model-call provider stage failed: {error}"),
            Self::ObservationCommit { error, .. } => {
                write!(formatter, "model-call observation commit failed: {error}")
            }
        }
    }
}

impl<PrepareError, FailureError, AuthorizationError, ProviderError, ObservationError> Error
    for ModelCallExecutionError<
        PrepareError,
        FailureError,
        AuthorizationError,
        ProviderError,
        ObservationError,
    >
where
    PrepareError: Error + 'static,
    FailureError: Error + 'static,
    AuthorizationError: Error + 'static,
    ProviderError: Error + 'static,
    ObservationError: Error + 'static,
{
}

impl<PrepareError, FailureError, AuthorizationError, ProviderError, ObservationError>
    ClassifyOperatorFailure
    for ModelCallExecutionError<
        PrepareError,
        FailureError,
        AuthorizationError,
        ProviderError,
        ObservationError,
    >
where
    PrepareError: ClassifyOperatorFailure,
    FailureError: ClassifyOperatorFailure,
    AuthorizationError: ClassifyOperatorFailure,
    ProviderError: ClassifyOperatorFailure,
    ObservationError: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Prepare(error) => error.operator_failure_class(),
            Self::Render(error) => error.operator_failure_class(),
            Self::CapabilityPreparation(error) | Self::Provider(error) => {
                error.operator_failure_class()
            }
            Self::CapabilityFailureCommit(error) => error.operator_failure_class(),
            Self::Authorization(error) => error.operator_failure_class(),
            Self::AuthorizationReread { reread_error, .. } => reread_error.operator_failure_class(),
            Self::ObservationCommit { error, .. } => error.operator_failure_class(),
        }
    }
}

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
    retained_observation: Option<(SessionId, CorrelatedModelCallTerminalObservation)>,
}

impl<Ids, Prepare, Failure, Authorization, Observation, Provider, Gate>
    ModelCallExecutionService<Ids, Prepare, Failure, Authorization, Observation, Provider, Gate>
{
    /// Composes every purpose-specific effect role.
    pub const fn new(
        ids: Ids,
        prepare: Prepare,
        failure: Failure,
        authorization: Authorization,
        observation: Observation,
        provider: Provider,
        gate: Gate,
    ) -> Self {
        Self {
            ids,
            prepare,
            failure,
            authorization,
            observation,
            provider,
            gate,
            retained_observation: None,
        }
    }

    /// Returns every owned effect role for explicit composition handoff.
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
    ) {
        (
            self.ids,
            self.prepare,
            self.failure,
            self.authorization,
            self.observation,
            self.provider,
            self.gate,
        )
    }

    /// Borrows the exact observation awaiting authoritative reconciliation.
    pub fn retained_observation(&self) -> Option<&CorrelatedModelCallTerminalObservation> {
        self.retained_observation
            .as_ref()
            .map(|(_, observation)| observation)
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
    pub async fn execute(
        &mut self,
        session: SessionId,
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
        if let Some((retained_session, retained)) = self.retained_observation.take() {
            match self
                .observation
                .reread_observation(retained_session, &retained)
                .await
            {
                Ok(RetainedModelCallObservationStatus::AlreadyCommitted) => {
                    return Ok(ModelCallExecutionOutcome::ObservationAlreadyCommitted(
                        retained.call(),
                    ));
                }
                Ok(RetainedModelCallObservationStatus::Pending) => {
                    return self
                        .commit_terminal_observation(retained_session, retained)
                        .await;
                }
                Err(error) => {
                    self.retained_observation = Some((retained_session, retained.clone()));
                    return Err(ModelCallExecutionError::ObservationCommit {
                        error,
                        retained_observation: retained,
                    });
                }
            }
        }

        let prepared = loop {
            let call = self.ids.next_model_call_id();
            let failure_identities = self.next_failed_identities();
            match self
                .prepare
                .prepare(session, call, failure_identities)
                .await
            {
                Ok(PrepareModelCallOutcome::NoWork) => {
                    return Ok(ModelCallExecutionOutcome::NoWork);
                }
                Ok(PrepareModelCallOutcome::Checkpointed(call)) => {
                    return Ok(ModelCallExecutionOutcome::Checkpointed(call));
                }
                Ok(PrepareModelCallOutcome::Ready(request)) => break request,
                Ok(PrepareModelCallOutcome::TargetUnavailable(failed)) => {
                    return Ok(ModelCallExecutionOutcome::TargetUnavailable(failed));
                }
                Ok(PrepareModelCallOutcome::PendingSteering { accepted_input }) => {
                    return Ok(ModelCallExecutionOutcome::PendingSteering { accepted_input });
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

        let call = prepared.call().id();
        let attempt = prepared.attempt();
        let prepared_request = (*prepared).clone();
        let operation =
            PreparedModelOperation::render(*prepared).map_err(ModelCallExecutionError::Render)?;
        let capability = match self.provider.prepare_capability(operation).await {
            Ok(ModelCallCapabilityPreparation::Ready(capability)) => capability,
            Ok(ModelCallCapabilityPreparation::KnownFailure) => loop {
                let identities = self.next_failed_identities();
                let ids = &mut self.ids;
                let next_turn = move |_| ids.next_turn_id();
                match self
                    .failure
                    .fail_prepared(session, call, identities, next_turn)
                    .await
                {
                    Ok(failed) => {
                        return Ok(ModelCallExecutionOutcome::CapabilityKnownFailure(Box::new(
                            failed,
                        )));
                    }
                    Err(error)
                        if error.operator_failure_class()
                            == OperatorFailureClass::IdentityCollision =>
                    {
                        continue;
                    }
                    Err(error) => {
                        return Err(ModelCallExecutionError::CapabilityFailureCommit(error));
                    }
                }
            },
            Err(error) => {
                return Err(ModelCallExecutionError::CapabilityPreparation(error));
            }
        };

        let permit = self.gate.acquire(attempt).await;
        let authorized = loop {
            match self.authorization.authorize(session, call).await {
                Ok(authorized) => break authorized,
                Err(error)
                    if error.operator_failure_class()
                        == OperatorFailureClass::IdentityCollision =>
                {
                    continue;
                }
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
                                .bind_terminal_observation(
                                    ModelCallTerminalObservation::KnownFailed,
                                );
                            return self
                                .commit_terminal_observation(session, non_consumption)
                                .await;
                        }
                        Err(reread_error) => {
                            drop(capability);
                            drop(permit);
                            return Err(ModelCallExecutionError::AuthorizationReread {
                                authorization_error: error,
                                reread_error,
                            });
                        }
                    }
                }
                Err(error) => return Err(ModelCallExecutionError::Authorization(error)),
            }
        };
        let acceptance_possible = move || drop(permit);
        let observation = self
            .provider
            .invoke(authorized, capability, acceptance_possible)
            .await;
        let observation = observation.map_err(ModelCallExecutionError::Provider)?;

        self.commit_terminal_observation(session, observation).await
    }

    async fn commit_terminal_observation(
        &mut self,
        session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
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
            let identities = self.next_terminal_identities(observation.observation());
            let ids = &mut self.ids;
            let next_turn = move |_| ids.next_turn_id();
            match self
                .observation
                .commit_observation(session, observation.clone(), identities, next_turn)
                .await
            {
                Ok(outcome) => {
                    return Ok(ModelCallExecutionOutcome::ObservationCommitted(Box::new(
                        outcome,
                    )));
                }
                Err(error)
                    if error.operator_failure_class()
                        == OperatorFailureClass::IdentityCollision =>
                {
                    continue;
                }
                Err(error) => {
                    self.retained_observation = Some((session, observation.clone()));
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
    ) -> ModelCallTerminalIdentities {
        match observation {
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
            ModelCallTerminalObservation::KnownFailed | ModelCallTerminalObservation::Cancelled => {
                ModelCallTerminalIdentities::Failed(self.next_failed_identities())
            }
            ModelCallTerminalObservation::Refused => ModelCallTerminalIdentities::Refused(
                RefusedModelCallTurnIdentities::new(self.ids.next_context_frontier_id()),
            ),
            ModelCallTerminalObservation::Ambiguous => ModelCallTerminalIdentities::Ambiguous,
        }
    }
}

/// One deterministic scripted-provider action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptedModelCallStep {
    /// Capability preparation returns a trustworthy ordinary failure.
    CapabilityKnownFailure,
    /// Capability preparation reports an operator failure.
    CapabilityOperatorFailure,
    /// Capability succeeds but provider interaction reports no observation.
    InteractionOperatorFailure,
    /// Provider interaction returns this exact terminal observation.
    Return(ModelCallTerminalObservation),
}

/// Sanitized failure from the deterministic scripted provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptedModelCallError {
    /// No scripted action remained for a requested capability.
    ScriptExhausted,
    /// The script explicitly selected a capability-stage operator failure.
    CapabilityOperatorFailure,
    /// The script explicitly selected an interaction-stage operator failure.
    InteractionOperatorFailure,
    /// Issued authorization did not match the prepared capability.
    AuthorizationMismatch,
}

impl fmt::Display for ScriptedModelCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ScriptExhausted => "scripted model-call actions are exhausted",
            Self::CapabilityOperatorFailure => "scripted model-call capability preparation failed",
            Self::InteractionOperatorFailure => "scripted model-call interaction failed",
            Self::AuthorizationMismatch => {
                "scripted model-call authorization does not match its capability"
            }
        })
    }
}

impl Error for ScriptedModelCallError {}

impl ClassifyOperatorFailure for ScriptedModelCallError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

enum ScriptedCapabilityOutcome {
    Return(ModelCallTerminalObservation),
    OperatorFailure,
}

/// Opaque one-shot capability owned by [`ScriptedModelCallProvider`].
pub struct ScriptedModelCallCapability {
    operation: PreparedModelOperation,
    outcome: ScriptedCapabilityOutcome,
}

/// Deterministic in-repository implementation of the provider port.
#[derive(Debug)]
pub struct ScriptedModelCallProvider {
    steps: std::collections::VecDeque<ScriptedModelCallStep>,
    capability_preparation_count: usize,
    interaction_count: usize,
}

impl ScriptedModelCallProvider {
    /// Creates a provider that consumes actions in supplied order.
    pub fn new(steps: impl IntoIterator<Item = ScriptedModelCallStep>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
            capability_preparation_count: 0,
            interaction_count: 0,
        }
    }

    /// Returns how many capability-preparation calls occurred.
    pub const fn capability_preparation_count(&self) -> usize {
        self.capability_preparation_count
    }

    /// Returns how many physical interaction calls occurred.
    pub const fn interaction_count(&self) -> usize {
        self.interaction_count
    }

    /// Returns how many scripted actions remain.
    pub fn remaining_step_count(&self) -> usize {
        self.steps.len()
    }
}

impl ModelCallProvider for ScriptedModelCallProvider {
    type Capability = ScriptedModelCallCapability;
    type Error = ScriptedModelCallError;

    fn prepare_capability(
        &mut self,
        operation: PreparedModelOperation,
    ) -> impl Future<Output = Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>> + Send
    {
        self.capability_preparation_count += 1;
        let step = self.steps.pop_front();
        async move {
            match step.ok_or(ScriptedModelCallError::ScriptExhausted)? {
                ScriptedModelCallStep::CapabilityKnownFailure => {
                    Ok(ModelCallCapabilityPreparation::KnownFailure)
                }
                ScriptedModelCallStep::CapabilityOperatorFailure => {
                    Err(ScriptedModelCallError::CapabilityOperatorFailure)
                }
                ScriptedModelCallStep::InteractionOperatorFailure => Ok(
                    ModelCallCapabilityPreparation::Ready(ScriptedModelCallCapability {
                        operation,
                        outcome: ScriptedCapabilityOutcome::OperatorFailure,
                    }),
                ),
                ScriptedModelCallStep::Return(observation) => Ok(
                    ModelCallCapabilityPreparation::Ready(ScriptedModelCallCapability {
                        operation,
                        outcome: ScriptedCapabilityOutcome::Return(observation),
                    }),
                ),
            }
        }
    }

    fn invoke<AcceptancePossible>(
        &mut self,
        authorized: AuthorizedModelCall,
        capability: Self::Capability,
        acceptance_possible: AcceptancePossible,
    ) -> impl Future<Output = Result<CorrelatedModelCallTerminalObservation, Self::Error>> + Send
    where
        AcceptancePossible: FnOnce() + Send,
    {
        self.interaction_count += 1;
        async move {
            let prepared = capability.operation.request();
            if prepared.session() != authorized.session()
                || prepared.turn() != authorized.turn()
                || prepared.attempt() != authorized.attempt().id()
                || prepared.call().id() != authorized.call().id()
                || prepared.call().target() != authorized.call().target()
                || prepared.call().frontier() != authorized.call().frontier()
            {
                return Err(ScriptedModelCallError::AuthorizationMismatch);
            }
            acceptance_possible();
            match capability.outcome {
                ScriptedCapabilityOutcome::Return(observation) => Ok(authorized
                    .observation_correlation()
                    .bind_terminal_observation(observation)),
                ScriptedCapabilityOutcome::OperatorFailure => {
                    Err(ScriptedModelCallError::InteractionOperatorFailure)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Arc};

    use signalbox_domain::{
        AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
        AcceptedInputSchedulingReconstitutionInput, AcceptedInputTurnActivationIdentities,
        AcceptedInputTurnSchedulingRecord, AcceptedInputTurnSchedulingRecordState, Actor,
        DeliveryRequest, DirectModelSelection, DurableCommandId, FrozenModelSelection,
        ModelCallExecutionReconstitutionInput, ModelCallOriginContent,
        ModelCallReconstitutionInput, ModelCallReconstitutionState, ModelSelectionOverride,
        ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition,
        PerInputConfigurationChoices, PinnedProviderTargetReconstitutionInput,
        ProviderModelIdentity, ResolvedProviderTarget, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionInputPosition, SessionReconstitutionInput, SubmitInput,
        SubmitInputReconstitutionInput, SubmitInputTurnOriginReconstitutionInput,
        TranscriptAncestry,
    };
    use uuid::Uuid;

    use super::*;

    fn identity<Identity>(value: u128, from_uuid: impl FnOnce(Uuid) -> Identity) -> Identity {
        from_uuid(Uuid::from_u128(value))
    }

    fn prepared_fixture() -> (PreparedModelCallRequest, AuthorizedModelCall) {
        let session_id = identity(1, SessionId::from_uuid);
        let direct = identity(2, DirectModelSelection::from_uuid);
        let accepted_input = identity(3, AcceptedInputId::from_uuid);
        let turn_id = identity(4, TurnId::from_uuid);
        let command_id = identity(5, DurableCommandId::from_uuid);
        let version = SessionConfigurationDefaultsVersion::first();
        let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct));
        let session = SessionReconstitutionInput::new(
            session_id,
            session_id,
            SessionCreationProvenance::new(
                SessionCreationCause::OwnerInitiated,
                TranscriptAncestry::None,
            ),
            session_id,
            version,
            session_id,
            version,
            defaults,
        )
        .reconstitute()
        .expect("fixture Session facts are correlated");
        let choices =
            PerInputConfigurationChoices::new(version, ModelSelectionOverride::UseSessionDefault);
        let delivery = DeliveryRequest::StartWhenNoActiveTurn {
            configuration: choices,
        };
        let content = UserContent::try_text(String::from("exact user request"))
            .expect("fixture content is valid");
        let command = SubmitInput::new(command_id, session_id, content.clone(), delivery);
        let position = SessionInputPosition::first();
        let order = AcceptedInputQueueOrder::ordinary(position);
        let lifecycle = AcceptedInputLifecycle::new(
            accepted_input,
            AcceptedInputDisposition::OriginOf(turn_id),
        );
        let receipt = SubmitInputReconstitutionInput::applied_turn_origin(
            command,
            Actor::Owner,
            session_id,
            accepted_input,
            turn_id,
            None,
            command_id,
            accepted_input,
            session_id,
            content,
            delivery,
            position,
            AcceptedInputDisposition::OriginOf(turn_id),
            session_id,
            turn_id,
            order,
            session_id,
            version,
            defaults,
            ModelSelectionRequest::Direct(direct),
            FrozenModelSelection::Direct(direct),
        )
        .reconstitute()
        .expect("fixture receipt facts are correlated");
        let origin = SubmitInputTurnOriginReconstitutionInput::new(
            receipt,
            lifecycle.clone(),
            accepted_input,
            session_id,
            turn_id,
            order,
        );
        let origin_content = ModelCallOriginContent::from_reconstituted_turn_origin(&origin)
            .expect("checked origin carries exact content");
        let checked = session
            .current_configuration_defaults()
            .derive_request(version, ModelSelectionOverride::UseSessionDefault)
            .expect("fixture defaults version is current");
        let configuration = signalbox_domain::OriginConfiguration::freeze(checked, |_| None)
            .expect("a direct selection needs no alias lookup");
        let record = AcceptedInputTurnSchedulingRecord::new(
            session_id,
            turn_id,
            session_id,
            lifecycle,
            session_id,
            turn_id,
            order,
            delivery,
            configuration,
            AcceptedInputTurnSchedulingRecordState::Queued,
        );
        let activation = AcceptedInputSchedulingReconstitutionInput::new(
            session,
            vec![record],
            Vec::new(),
            Vec::new(),
            None,
        )
        .reconstitute()
        .expect("fixture scheduling projection is complete")
        .prepare_earliest_queued_activation(AcceptedInputTurnActivationIdentities::new(
            identity(6, SemanticTranscriptEntryId::from_uuid),
            identity(7, ContextFrontierId::from_uuid),
            identity(8, TurnAttemptId::from_uuid),
        ))
        .expect("the sole queued fixture turn is eligible");
        let (active_turn, origin_entry, starting_snapshot) = activation.into_parts();
        let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
            direct,
            ResolvedProviderTarget::naming(identity(9, ProviderModelIdentity::from_uuid)),
        )])
        .expect("the fixture target key is unique");
        let initial = ModelCallExecutionReconstitutionInput::new(
            active_turn.clone(),
            targets.clone(),
            starting_snapshot.clone(),
            vec![origin_entry.clone()],
            vec![origin_content.clone()],
            None,
            Vec::new(),
        )
        .reconstitute()
        .expect("fixture activation reconstructs execution");
        let prepared = initial
            .prepare_initial_call(identity(10, ModelCallId::from_uuid))
            .expect("fixture call can be prepared");
        let prepared_execution = ModelCallExecutionReconstitutionInput::new(
            active_turn,
            targets,
            starting_snapshot,
            vec![origin_entry],
            vec![origin_content],
            Some(PinnedProviderTargetReconstitutionInput::new(
                prepared.call().turn(),
                prepared.call().target(),
            )),
            vec![ModelCallReconstitutionInput::new(
                prepared.call().id(),
                prepared.call().turn(),
                prepared.call().attempt(),
                prepared.call().selection(),
                prepared.call().target(),
                prepared.call().frontier().snapshot(),
                ModelCallReconstitutionState::Prepared,
            )],
        )
        .reconstitute()
        .expect("fixture Prepared facts reconstruct");
        let request = prepared_execution
            .resume_prepared_call()
            .expect("fixture Prepared request resumes");
        let authorized = prepared_execution
            .authorize_send()
            .expect("fixture Prepared call authorizes");
        (request, authorized)
    }

    #[derive(Debug)]
    struct FixedIds {
        calls: VecDeque<ModelCallId>,
        entries: VecDeque<SemanticTranscriptEntryId>,
        frontiers: VecDeque<ContextFrontierId>,
        turns: VecDeque<TurnId>,
    }

    impl FixedIds {
        fn baseline() -> Self {
            Self {
                calls: [20, 21]
                    .map(|value| identity(value, ModelCallId::from_uuid))
                    .into(),
                entries: (30..40)
                    .map(|value| identity(value, SemanticTranscriptEntryId::from_uuid))
                    .collect(),
                frontiers: (40..50)
                    .map(|value| identity(value, ContextFrontierId::from_uuid))
                    .collect(),
                turns: (50..60)
                    .map(|value| identity(value, TurnId::from_uuid))
                    .collect(),
            }
        }
    }

    impl ModelCallExecutionIdGenerator for FixedIds {
        fn next_model_call_id(&mut self) -> ModelCallId {
            self.calls.pop_front().expect("fixture call identity")
        }

        fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
            self.entries.pop_front().expect("fixture entry identity")
        }

        fn next_context_frontier_id(&mut self) -> ContextFrontierId {
            self.frontiers
                .pop_front()
                .expect("fixture frontier identity")
        }

        fn next_turn_id(&mut self) -> TurnId {
            self.turns.pop_front().expect("fixture turn identity")
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeError {
        IdentityCollision,
        Infrastructure,
        CommitAmbiguous,
    }

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(match self {
                Self::IdentityCollision => "fake identity collision",
                Self::Infrastructure => "fake infrastructure failure",
                Self::CommitAmbiguous => "fake commit-ambiguous failure",
            })
        }
    }

    impl Error for FakeError {}

    impl ClassifyOperatorFailure for FakeError {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            match self {
                Self::IdentityCollision => OperatorFailureClass::IdentityCollision,
                Self::Infrastructure => OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                },
                Self::CommitAmbiguous => OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                },
            }
        }
    }

    #[derive(Debug)]
    struct FakePrepare {
        outcomes: VecDeque<Result<PrepareModelCallOutcome, FakeError>>,
        calls: usize,
    }

    impl PrepareModelCallTransaction for FakePrepare {
        type Error = FakeError;

        async fn prepare(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
            _failure_identities: FailedModelCallTurnIdentities,
        ) -> Result<PrepareModelCallOutcome, Self::Error> {
            self.calls += 1;
            self.outcomes
                .pop_front()
                .expect("one fake prepare outcome per call")
        }
    }

    #[derive(Debug)]
    struct UnusedFailure;

    impl FailPreparedModelCallTransaction for UnusedFailure {
        type Error = FakeError;

        async fn fail_prepared<NextTurn>(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
            _identities: FailedModelCallTurnIdentities,
            _next_reclassified_turn: NextTurn,
        ) -> Result<FailedModelCallTurn, Self::Error>
        where
            NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        {
            panic!("unused failure transaction")
        }
    }

    #[derive(Debug)]
    struct FakeAuthorization {
        outcomes: VecDeque<Result<AuthorizedModelCall, FakeError>>,
        rereads: VecDeque<Result<ModelCallAuthorizationReread, FakeError>>,
        calls: usize,
        reread_calls: usize,
    }

    impl AuthorizeModelCallTransaction for FakeAuthorization {
        type Error = FakeError;

        async fn authorize(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
        ) -> Result<AuthorizedModelCall, Self::Error> {
            self.calls += 1;
            self.outcomes
                .pop_front()
                .expect("one fake authorization outcome")
        }

        async fn reread_after_ambiguous_commit(
            &mut self,
            _session: SessionId,
            _prepared: &PreparedModelCallRequest,
        ) -> Result<ModelCallAuthorizationReread, Self::Error> {
            self.reread_calls += 1;
            self.rereads
                .pop_front()
                .expect("one fake authorization reread")
        }
    }

    #[derive(Debug)]
    struct UnusedAuthorization;

    impl AuthorizeModelCallTransaction for UnusedAuthorization {
        type Error = FakeError;

        async fn authorize(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
        ) -> Result<AuthorizedModelCall, Self::Error> {
            panic!("unused authorization transaction")
        }

        async fn reread_after_ambiguous_commit(
            &mut self,
            _session: SessionId,
            _prepared: &PreparedModelCallRequest,
        ) -> Result<ModelCallAuthorizationReread, Self::Error> {
            panic!("unused authorization reread")
        }
    }

    #[derive(Debug)]
    struct UnusedObservation;

    impl CommitModelCallObservationTransaction for UnusedObservation {
        type Error = FakeError;

        async fn commit_observation<NextTurn>(
            &mut self,
            _session: SessionId,
            _observation: CorrelatedModelCallTerminalObservation,
            _identities: ModelCallTerminalIdentities,
            _next_reclassified_turn: NextTurn,
        ) -> Result<ModelCallTerminalOutcome, Self::Error>
        where
            NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        {
            panic!("unused observation transaction")
        }

        async fn reread_observation(
            &mut self,
            _session: SessionId,
            _observation: &CorrelatedModelCallTerminalObservation,
        ) -> Result<RetainedModelCallObservationStatus, Self::Error> {
            panic!("unused observation reread")
        }
    }

    #[derive(Debug)]
    struct FakeObservation {
        commit_errors: VecDeque<FakeError>,
        rereads: VecDeque<Result<RetainedModelCallObservationStatus, FakeError>>,
        observed: Vec<CorrelatedModelCallTerminalObservation>,
        commit_calls: usize,
        reread_calls: usize,
    }

    impl CommitModelCallObservationTransaction for FakeObservation {
        type Error = FakeError;

        async fn commit_observation<NextTurn>(
            &mut self,
            _session: SessionId,
            observation: CorrelatedModelCallTerminalObservation,
            _identities: ModelCallTerminalIdentities,
            _next_reclassified_turn: NextTurn,
        ) -> Result<ModelCallTerminalOutcome, Self::Error>
        where
            NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        {
            self.commit_calls += 1;
            self.observed.push(observation);
            Err(self
                .commit_errors
                .pop_front()
                .expect("one fake observation commit failure"))
        }

        async fn reread_observation(
            &mut self,
            _session: SessionId,
            _observation: &CorrelatedModelCallTerminalObservation,
        ) -> Result<RetainedModelCallObservationStatus, Self::Error> {
            self.reread_calls += 1;
            self.rereads
                .pop_front()
                .expect("one fake observation reread")
        }
    }

    #[derive(Debug)]
    struct UnusedProvider;

    impl ModelCallProvider for UnusedProvider {
        type Capability = ();
        type Error = FakeError;

        async fn prepare_capability(
            &mut self,
            _operation: PreparedModelOperation,
        ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error> {
            panic!("unused provider capability preparation")
        }

        async fn invoke<AcceptancePossible>(
            &mut self,
            _authorized: AuthorizedModelCall,
            _capability: Self::Capability,
            _acceptance_possible: AcceptancePossible,
        ) -> Result<CorrelatedModelCallTerminalObservation, Self::Error>
        where
            AcceptancePossible: FnOnce() + Send,
        {
            panic!("unused provider interaction")
        }
    }

    #[derive(Debug)]
    struct BoundaryBlockingProvider {
        crossed: Arc<tokio::sync::Notify>,
        finish: Arc<tokio::sync::Notify>,
        interaction_count: usize,
    }

    impl ModelCallProvider for BoundaryBlockingProvider {
        type Capability = PreparedModelOperation;
        type Error = FakeError;

        async fn prepare_capability(
            &mut self,
            operation: PreparedModelOperation,
        ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error> {
            Ok(ModelCallCapabilityPreparation::Ready(operation))
        }

        fn invoke<AcceptancePossible>(
            &mut self,
            _authorized: AuthorizedModelCall,
            _capability: Self::Capability,
            acceptance_possible: AcceptancePossible,
        ) -> impl Future<Output = Result<CorrelatedModelCallTerminalObservation, Self::Error>> + Send
        where
            AcceptancePossible: FnOnce() + Send,
        {
            self.interaction_count += 1;
            let crossed = Arc::clone(&self.crossed);
            let finish = Arc::clone(&self.finish);
            async move {
                acceptance_possible();
                crossed.notify_one();
                finish.notified().await;
                Err(FakeError::Infrastructure)
            }
        }
    }

    /// The owner-selected rendering decision: origin input becomes a user
    /// message carrying the semantic entry's source, in frontier order.
    #[test]
    fn s02_inv015_frontier_rendering_preserves_user_role_order_and_source() {
        let (request, _) = prepared_fixture();
        let operation = PreparedModelOperation::render(request)
            .expect("the baseline origin-only frontier renders");
        assert_eq!(operation.messages().len(), 1);
        let ModelConversationMessage::User {
            source,
            accepted_input,
            content,
        } = &operation.messages()[0]
        else {
            panic!("an origin entry must render as user content")
        };
        assert_eq!(source.source_session(), identity(1, SessionId::from_uuid));
        assert_eq!(*accepted_input, identity(3, AcceptedInputId::from_uuid));
        assert_eq!(content.text().as_str(), "exact user request");
    }

    /// S02 / INV-014: a newly committed Prepared checkpoint ends
    /// the invocation before capability preparation or authorization.
    #[tokio::test]
    async fn s02_inv014_checkpoint_stops_before_every_later_port() {
        let checkpoint = identity(70, ModelCallId::from_uuid);
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(PrepareModelCallOutcome::Checkpointed(checkpoint))].into(),
                calls: 0,
            },
            UnusedFailure,
            UnusedAuthorization,
            UnusedObservation,
            UnusedProvider,
            InProcessAttemptDispatchGate::default(),
        );
        assert_eq!(
            service
                .execute(identity(1, SessionId::from_uuid))
                .await
                .expect("checkpointing succeeds"),
            ModelCallExecutionOutcome::Checkpointed(checkpoint)
        );
        let (_, prepare, ..) = service.into_parts();
        assert_eq!(prepare.calls, 1);
    }

    /// S02 / INV-014: a proven fresh-identity collision retries only the
    /// rolled-back prepare transaction with fresh candidates.
    #[tokio::test]
    async fn s02_inv014_prepare_identity_collision_retries_transaction_only() {
        let checkpoint = identity(71, ModelCallId::from_uuid);
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [
                    Err(FakeError::IdentityCollision),
                    Ok(PrepareModelCallOutcome::Checkpointed(checkpoint)),
                ]
                .into(),
                calls: 0,
            },
            UnusedFailure,
            UnusedAuthorization,
            UnusedObservation,
            UnusedProvider,
            InProcessAttemptDispatchGate::default(),
        );
        assert_eq!(
            service
                .execute(identity(1, SessionId::from_uuid))
                .await
                .expect("proven collision is retryable"),
            ModelCallExecutionOutcome::Checkpointed(checkpoint)
        );
        let (_, prepare, ..) = service.into_parts();
        assert_eq!(prepare.calls, 2);
    }

    /// A resumed prepared call constructs one opaque
    /// capability, commits InFlight first, and invokes the provider once. An
    /// operator failure produces no fabricated observation commit.
    #[tokio::test]
    async fn resumed_provider_failure_stays_at_provider_stage() {
        let (request, authorized) = prepared_fixture();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(PrepareModelCallOutcome::Ready(Box::new(request)))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Ok(authorized)].into(),
                rereads: VecDeque::new(),
                calls: 0,
                reread_calls: 0,
            },
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::InteractionOperatorFailure]),
            InProcessAttemptDispatchGate::default(),
        );
        let error = service
            .execute(identity(1, SessionId::from_uuid))
            .await
            .expect_err("the script reports no trustworthy observation");
        assert!(matches!(
            error,
            ModelCallExecutionError::Provider(ScriptedModelCallError::InteractionOperatorFailure)
        ));
        let (_, prepare, _, authorization, _, provider, _) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(authorization.calls, 1);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 1);
    }

    /// S02 / INV-014 / INV-034: a non-collision observation failure retains
    /// the exact result; later passes authoritatively resubmit it unchanged
    /// while absent and stop once the original commit is observed.
    #[tokio::test]
    async fn s02_inv014_inv034_failed_observation_commit_is_retained_and_reread() {
        let (request, authorized) = prepared_fixture();
        let call = authorized.call().id();
        let session = authorized.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(PrepareModelCallOutcome::Ready(Box::new(request)))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Ok(authorized)].into(),
                rereads: VecDeque::new(),
                calls: 0,
                reread_calls: 0,
            },
            FakeObservation {
                commit_errors: [FakeError::Infrastructure, FakeError::Infrastructure].into(),
                rereads: [
                    Ok(RetainedModelCallObservationStatus::Pending),
                    Ok(RetainedModelCallObservationStatus::AlreadyCommitted),
                ]
                .into(),
                observed: Vec::new(),
                commit_calls: 0,
                reread_calls: 0,
            },
            ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
                ModelCallTerminalObservation::KnownFailed,
            )]),
            InProcessAttemptDispatchGate::default(),
        );

        let error = service
            .execute(session)
            .await
            .expect_err("the first observation commit fails");
        let retained = match error {
            ModelCallExecutionError::ObservationCommit {
                error: FakeError::Infrastructure,
                retained_observation,
            } => retained_observation,
            error => panic!("unexpected failure: {error}"),
        };
        assert_eq!(retained.call(), call);
        assert_eq!(service.retained_observation(), Some(&retained));

        let error = service
            .execute(session)
            .await
            .expect_err("the unchanged resubmission also fails");
        let resubmitted = match error {
            ModelCallExecutionError::ObservationCommit {
                error: FakeError::Infrastructure,
                retained_observation,
            } => retained_observation,
            error => panic!("unexpected failure: {error}"),
        };
        assert_eq!(resubmitted, retained);

        assert_eq!(
            service
                .execute(session)
                .await
                .expect("the authoritative reread proves the retained commit landed"),
            ModelCallExecutionOutcome::ObservationAlreadyCommitted(call)
        );
        assert!(service.retained_observation().is_none());
        let (_, _, _, authorization, observation, provider, _) = service.into_parts();
        assert_eq!(authorization.calls, 1);
        assert_eq!(authorization.reread_calls, 0);
        assert_eq!(observation.commit_calls, 2);
        assert_eq!(observation.reread_calls, 2);
        assert_eq!(observation.observed, vec![retained.clone(), retained]);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 1);
    }

    /// S02 / INV-014 / INV-034: when authorization acknowledgement is lost,
    /// the still-owned capability proves `invoke` was never entered. An
    /// authoritative InFlight reread becomes a correlated known-failure
    /// observation without any provider interaction.
    #[tokio::test]
    async fn s02_inv014_inv034_ambiguous_authorization_classifies_unconsumed_in_flight() {
        let (request, authorized) = prepared_fixture();
        let call = authorized.call().id();
        let session = authorized.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(PrepareModelCallOutcome::Ready(Box::new(request)))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Err(FakeError::CommitAmbiguous)].into(),
                rereads: [Ok(ModelCallAuthorizationReread::InFlight(Box::new(
                    authorized,
                )))]
                .into(),
                calls: 0,
                reread_calls: 0,
            },
            FakeObservation {
                commit_errors: [FakeError::Infrastructure].into(),
                rereads: VecDeque::new(),
                observed: Vec::new(),
                commit_calls: 0,
                reread_calls: 0,
            },
            ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
                ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new(String::from("must not be sent"))
                            .expect("fixture text is valid"),
                    ],
                },
            )]),
            InProcessAttemptDispatchGate::default(),
        );

        let error = service
            .execute(session)
            .await
            .expect_err("the fake non-consumption commit fails visibly");
        let retained = match error {
            ModelCallExecutionError::ObservationCommit {
                error: FakeError::Infrastructure,
                retained_observation,
            } => retained_observation,
            error => panic!("unexpected failure: {error}"),
        };
        assert_eq!(retained.call(), call);
        assert_eq!(
            retained.observation(),
            &ModelCallTerminalObservation::KnownFailed
        );
        let (_, _, _, authorization, observation, provider, _) = service.into_parts();
        assert_eq!(authorization.calls, 1);
        assert_eq!(authorization.reread_calls, 1);
        assert_eq!(observation.commit_calls, 1);
        assert_eq!(observation.observed, vec![retained]);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 0);
    }

    /// S02 / INV-009 / INV-014: the attempt gate transfers into the provider
    /// interaction and is released at its acceptance-capable boundary while
    /// the slow terminal response remains pending.
    #[tokio::test]
    async fn s02_inv009_inv014_dispatch_gate_releases_at_acceptance_boundary() {
        let (request, authorized) = prepared_fixture();
        let session = authorized.session();
        let attempt = authorized.attempt().id();
        let crossed = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let gate = InProcessAttemptDispatchGate::default();
        let gate_probe = gate.clone();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(PrepareModelCallOutcome::Ready(Box::new(request)))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Ok(authorized)].into(),
                rereads: VecDeque::new(),
                calls: 0,
                reread_calls: 0,
            },
            UnusedObservation,
            BoundaryBlockingProvider {
                crossed: Arc::clone(&crossed),
                finish: Arc::clone(&finish),
                interaction_count: 0,
            },
            gate,
        );
        {
            let execution = service.execute(session);
            tokio::pin!(execution);

            tokio::select! {
                () = crossed.notified() => {}
                result = &mut execution => panic!("provider returned before boundary probe: {result:?}"),
            }
            let after_boundary = tokio::time::timeout(
                std::time::Duration::from_millis(10),
                gate_probe.acquire(attempt),
            )
            .await
            .expect("the same-attempt gate is released at provider acceptance");
            drop(after_boundary);
            finish.notify_one();
            assert!(matches!(
                execution.as_mut().await,
                Err(ModelCallExecutionError::Provider(FakeError::Infrastructure))
            ));
        }
        let (_, _, _, _, _, provider, _) = service.into_parts();
        assert_eq!(provider.interaction_count, 1);
    }

    #[test]
    fn in_process_gate_serializes_the_same_attempt_but_not_distinct_attempts() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime builds");
        runtime.block_on(async {
            let gate = InProcessAttemptDispatchGate::default();
            let attempt = identity(80, TurnAttemptId::from_uuid);
            let other = identity(81, TurnAttemptId::from_uuid);
            let first = gate.acquire(attempt).await;
            let same = gate.acquire(attempt);
            tokio::pin!(same);
            assert!(
                tokio::time::timeout(std::time::Duration::ZERO, &mut same)
                    .await
                    .is_err()
            );
            let distinct =
                tokio::time::timeout(std::time::Duration::from_millis(10), gate.acquire(other))
                    .await
                    .expect("distinct attempts do not block one another");
            drop(distinct);
            drop(first);
            tokio::time::timeout(std::time::Duration::from_millis(10), same)
                .await
                .expect("same attempt proceeds after permit release");
        });
    }
}
