use super::{
    AcceptedInputId, Arc, AuthorizedModelCall, AvailabilitySuccessorModelCallTurn,
    ClassifyOperatorFailure, ContextFrontierId, CorrelatedModelCallTerminalObservation,
    CredentialPoolExhaustedModelCallTurn, DangerousToolAutoApproval, Duration, FailedModelCallTurn,
    FailedModelCallTurnIdentities, Future, InitialToolApproval, ModelCallCredentialReference,
    ModelCallId, ModelCallTerminalIdentities, ModelCallTerminalOutcome, PreparedModelCallRequest,
    PreparedModelOperation, ProviderReasoningProvenance, RecordedUserOverride,
    ResolvedToolConversationEntry, SemanticTranscriptEntryId, SessionId, SessionSystemPrompt,
    StopRequestedModelCallTurn, StoppedToolRoundModelCallIdentities, ToolRoundModelCallIdentities,
    TurnAttemptId, TurnId,
};

/// Result of the authoritative prepare-call transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareModelCallOutcome {
    /// Admission of a released wait failed with its predecessor provider evidence.
    WaitFailed(Box<FailedModelCallTurn>),
    /// Credential admission retained the turn without preparing a call.
    CredentialWait(signalbox_domain::CredentialAvailabilityWait),
    /// The scheduling hint no longer identifies runnable work.
    NoWork,
    /// A durable availability-successor deadline has not elapsed.
    RetryBackoff(Duration),
    /// No credential-pool member was available for this call-free attempt.
    PoolExhausted(Box<CredentialPoolExhaustedModelCallTurn>),
    /// A new exact `Prepared` call committed; this invocation stops here.
    Checkpointed(ModelCallId),
    /// A previously committed `Prepared` request may prepare its capability.
    Ready {
        /// Checked durable request facts.
        request: Box<PreparedModelCallRequest>,
        /// Non-secret credential reference captured with the call.
        credential_reference: ModelCallCredentialReference,
        /// Retained mapped serving target whose fast-mode mapping is already applied.
        retained_mapped_target: Option<signalbox_domain::ResolvedProviderTarget>,
        /// Frozen dangerous blanket posture for initial request decisions.
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
        /// Recorded, not-yet-consumed user overrides of delegate denials, frozen
        /// for this call in the same transaction as the blanket posture.
        recorded_user_overrides: Box<[RecordedUserOverride]>,
        /// Exact optional session system prompt on the turn's frozen epoch.
        system_prompt: Option<SessionSystemPrompt>,
        /// Exact durable authority for every tool-related frontier entry.
        tool_entries: Box<[ResolvedToolConversationEntry]>,
        /// Durable producing-target and credential facts for retained reasoning.
        reasoning_provenance: Box<[ProviderReasoningProvenance]>,
    },
    /// Immutable target resolution failed and the turn closed atomically.
    TargetUnavailable(Box<FailedModelCallTurn>),
}

/// Authoritative transaction that prepares or reloads one initial model call.
pub trait PrepareModelCallTransaction {
    /// Adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Runs the serialized prepare role with fresh application candidates.
    fn prepare<NextSteeringIdentities>(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        failure_identities: FailedModelCallTurnIdentities,
        steering_frontier: ContextFrontierId,
        next_steering_identities: NextSteeringIdentities,
    ) -> impl Future<Output = Result<PrepareModelCallOutcome, Self::Error>> + Send
    where
        NextSteeringIdentities:
            FnMut(AcceptedInputId) -> (SemanticTranscriptEntryId, TurnId) + Send;
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
        cause: PreparedModelCallFailureCause,
        attachment_failure: Option<AttachmentPreparationFailure>,
        identities: FailedModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
    ) -> impl Future<Output = Result<FailedModelCallTurn, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send;

    /// Rereads whether a retained prepared-call failure closure committed.
    fn reread_failure(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        attachment_failure: Option<AttachmentPreparationFailure>,
    ) -> impl Future<Output = Result<RetainedPreparedFailureStatus, Self::Error>> + Send;
}

/// Application-owned reason for closing a prepared call before provider entry.
///
/// This vocabulary stays separate from provider-runtime cause codes because no
/// physical model call has been dispatched when either variant applies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparedModelCallFailureCause {
    /// Provider capability preparation reported a trustworthy local failure.
    CapabilityKnownFailure,
    /// The current turn already contains the maximum automatic tool rounds.
    ToolRoundLimitReached,
}

/// Authoritative status of one retained pre-send prepared-call failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetainedPreparedFailureStatus {
    /// The exact call remains `Prepared`; the closure may be resubmitted.
    Pending,
    /// The exact known-failure closure is already represented durably.
    AlreadyCommitted,
    /// A racing interrupt authoritatively cancelled the prepared call.
    Cancelled,
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
    ) -> impl Future<Output = Result<AuthorizeModelCallOutcome, Self::Error>> + Send;

    /// Rereads an authorization whose commit acknowledgement was lost.
    fn reread_after_ambiguous_commit(
        &mut self,
        session: SessionId,
        prepared: &PreparedModelCallRequest,
    ) -> impl Future<Output = Result<ModelCallAuthorizationReread, Self::Error>> + Send;

    /// Returns a same-call signal that resolves when durable state forbids
    /// continuing provider work.
    ///
    /// The returned future owns its adapter state so it can outlive this
    /// borrow and race capability preparation or physical invocation.
    fn cancellation_signal(
        &self,
        session: SessionId,
        call: ModelCallId,
    ) -> impl Future<Output = ()> + Send + 'static;
}

/// Result of freshly rechecking one send-authorization hint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthorizeModelCallOutcome {
    /// The exact prepared authority is stale or has stopped; no send may begin.
    NoSend,
    /// The exact prepared call committed `InFlight` and may enter its provider.
    Authorized(Box<AuthorizedModelCall>),
}

/// Authoritative state after an ambiguous send-authorization commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallAuthorizationReread {
    /// The authorization rolled back and the exact call remains Prepared.
    Prepared,
    /// The authorization committed; this exact issued call was not consumed.
    InFlight(Box<AuthorizedModelCall>),
    /// The authorization committed, but an interrupt stopped it before this
    /// process entered the provider.
    CancellationRequested(Box<StopRequestedModelCallTurn>),
    /// An interrupt already terminalized this exact unsent call as Cancelled.
    Cancelled,
}

/// Fresh identity candidates for a terminal observation.
///
/// A tool-using response carries both legal closures because an interrupt can
/// race after provider acceptance. The authoritative transaction selects the
/// continuing or stopped shape only after locking fresh lifecycle state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallTerminalIdentityCandidates {
    /// One lifecycle-independent terminal identity shape.
    Exact(ModelCallTerminalIdentities),
    /// Both legal closures for one tool-using response.
    ToolRound {
        /// Nonterminal same-turn continuation identities.
        continuing: ToolRoundModelCallIdentities,
        /// Applied-interrupt terminal closure identities.
        stopped: StoppedToolRoundModelCallIdentities,
    },
    /// Both legal closures for one classified availability failure.
    ///
    /// Persistence validates the call-pinned pool policy and retry bound under
    /// its lock, then consumes the identities for the authorized ending.
    Availability {
        /// Ordinary terminal failure when policy does not authorize a successor.
        failed: FailedModelCallTurnIdentities,
        /// Fresh physical attempt for an authorized availability successor.
        successor_attempt: TurnAttemptId,
    },
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
        identities: ModelCallTerminalIdentityCandidates,
        next_reclassified_turn: NextTurn,
    ) -> impl Future<Output = Result<Option<ModelCallObservationCommitOutcome>, Self::Error>> + Send
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
    /// The observation committed and its availability successor is durable.
    ///
    /// Distinct from `AlreadyCommitted` because the turn is still active on
    /// the successor attempt: the caller must keep driving it after the
    /// enclosed remaining delay rather than treating the turn as finished.
    AvailabilitySuccessorCommitted {
        /// Remaining wait before the successor attempt may prepare.
        retry_backoff: Duration,
    },
    /// A newer logical terminal proof made the retained provider result inert.
    DiscardedByLogicalTerminal,
}

/// Opaque same-incarnation evidence retained across a failed orchestration stage.
///
/// This state prevents a later service invocation or explicit composition
/// handoff from repeating credential work, losing proof that provider entry
/// never occurred, or dropping an unchanged terminal observation.
/// docs/spec/model-call-execution.md requires a linear handoff token: callers
/// may move it between service `into_parts` and `from_parts` handoffs, but
/// cannot construct or clone evidence.
///
/// ```compile_fail
/// use signalbox_application::RetainedModelCallExecutionState;
///
/// let _forged = RetainedModelCallExecutionState {};
/// ```
///
/// ```compile_fail
/// use signalbox_application::RetainedModelCallExecutionState;
///
/// fn duplicate(state: RetainedModelCallExecutionState) {
///     let _replayed: RetainedModelCallExecutionState = state.clone();
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct RetainedModelCallExecutionState {
    pub(super) state: RetainedModelCallExecutionStateKind,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum RetainedModelCallExecutionStateKind {
    /// A provider-neutral prepared-call failure remains to be reconciled.
    PreparedFailure {
        /// Session owning the exact prepared call.
        session: SessionId,
        /// Turn closed by the exact prepared call.
        turn: TurnId,
        /// Prepared call whose guarded failure closure remains pending.
        call: ModelCallId,
        /// Exact application reason that must survive the retained retry.
        cause: PreparedModelCallFailureCause,
        /// Distinct attachment-preparation cause, absent for ordinary capability failure.
        attachment_failure: Option<AttachmentPreparationFailure>,
    },
    /// Ambiguous authorization still has same-incarnation proof of no send.
    AuthorizationNonConsumption {
        /// Session owning the exact prepared request.
        session: SessionId,
        /// Unchanged request used to reread whether authorization committed.
        prepared: Box<PreparedModelCallRequest>,
    },
    /// One unchanged provider observation awaits authoritative reconciliation.
    TerminalObservation {
        /// Session owning the exact issued call.
        session: SessionId,
        /// Unchanged correlated observation returned by provider work.
        observation: Box<CorrelatedModelCallTerminalObservation>,
        /// Frozen policy outcomes for each tool proposal, in proposal order.
        tool_approvals: Box<[InitialToolApproval]>,
    },
}

/// Closed result of preparing rendered attachment authority before provider work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachmentPreparationFailure {
    /// Distinct rendered attachments exceed the deployment verification bound.
    TooLarge {
        /// Deployment maximum applied before store I/O.
        maximum_bytes: u64,
    },
    /// No recorded replica contains the required attachment.
    Missing,
    /// Recorded replicas were readable but failed identity verification.
    Corrupt,
    /// No replica verified and at least one candidate was temporarily unavailable.
    Unavailable,
}

/// Adapter-local result of credential lookup and capability preparation.
pub enum ModelCallCapabilityPreparation<Capability> {
    /// A call-bound one-shot capability is ready to move into provider work.
    Ready(Capability),
    /// Durable authority changed while the capability was being prepared.
    Cancelled,
    /// A trustworthy ordinary local failure occurred before send authorization.
    KnownFailure,
    /// Attachment preparation could not establish authority for the request.
    AttachmentFailure(AttachmentPreparationFailure),
}

/// Outcome of one provider-native prospective input-token estimate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallInputTokenCount {
    /// Provider-reported estimate for the rendered operation.
    Counted(u64),
    /// Authority or caller cancellation won before a count completed.
    Cancelled,
    /// Attachment authority is temporarily unavailable, so the queued turn
    /// must be retried before activation rather than sent without a count.
    AttachmentUnavailable,
    /// Attachment preparation found a definitive request-local failure.
    AttachmentFailure(AttachmentPreparationFailure),
    /// No trustworthy provider-native estimate is available.
    Unavailable,
}

/// Provider adapter boundary for prospective input-token estimation.
pub trait ModelCallInputTokenCounter {
    /// Sanitized adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Estimates the same provider-native operation shape later prepared for send.
    fn count_input_tokens<Cancellation>(
        &self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> impl Future<Output = Result<ModelCallInputTokenCount, Self::Error>> + Send
    where
        Cancellation: Future<Output = ()> + Send + 'static;
}

/// One durable result of committing a correlated model-call observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallObservationCommitOutcome {
    /// A failed call's chain yielded to a call-free credential admission wait.
    CredentialWait(signalbox_domain::CredentialAvailabilityWait),
    /// The observation reached an ordinary terminal or durable-wait outcome.
    Terminal(Box<ModelCallTerminalOutcome>),
    /// Pool policy authorized a distinct availability successor attempt.
    AvailabilitySuccessor(Box<AvailabilitySuccessorOutcome>),
    /// Every member is unavailable; the pool, not one member, terminalized.
    PoolExhausted(CredentialPoolExhaustedOutcome),
}

/// Typed pool-wide terminal cause, distinct from one account's failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CredentialPoolExhaustedOutcome {
    /// Selection found no member before creating a call.
    BeforeCall(Box<CredentialPoolExhaustedModelCallTurn>),
    /// A qualifying member failure consumed the last available member.
    AfterCall {
        /// Deployment-owned pool name.
        pool_name: Arc<str>,
        /// Ordinary terminal projection retaining the last call's evidence.
        terminal: Box<ModelCallTerminalOutcome>,
    },
}

/// One committed availability successor and its capped retry delay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailabilitySuccessorOutcome {
    successor: AvailabilitySuccessorModelCallTurn,
    backoff: Duration,
}

impl AvailabilitySuccessorOutcome {
    /// Creates the application result after persistence freezes the deadline.
    pub const fn new(successor: AvailabilitySuccessorModelCallTurn, backoff: Duration) -> Self {
        Self { successor, backoff }
    }

    /// Borrows the exact predecessor/successor lifecycle transition.
    pub const fn successor(&self) -> &AvailabilitySuccessorModelCallTurn {
        &self.successor
    }

    /// Returns the capped delay frozen with the durable successor.
    pub const fn backoff(&self) -> Duration {
        self.backoff
    }
}
