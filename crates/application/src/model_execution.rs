//! First text-only model-call execution orchestration.
//!
//! docs/spec/model-call-execution.md owns the staged transaction and
//! provider-effect order. The application keeps persistence, provider
//! capability preparation, send authorization, provider interaction, and
//! terminal observation distinct.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    error::Error,
    fmt,
    future::Future,
    sync::{Arc, Weak},
};

const MAX_AUTOMATIC_TOOL_ROUNDS_PER_TURN: usize = 32;

use signalbox_domain::{
    AcceptedInputId, AmbiguousModelCallTurnIdentities, AssistantResponsePart, AssistantText,
    AuthorizedModelCall, CompletedModelCallIdentities, ContextFrontierId,
    CorrelatedModelCallTerminalObservation, DangerousToolAutoApproval, FailedModelCallTurn,
    FailedModelCallTurnIdentities, InitialToolApproval, ModelCallId, ModelCallTerminalIdentities,
    ModelCallTerminalObservation, ModelCallTerminalOutcome,
    PhysicalCancellationModelCallTurnIdentities, PreparedModelCallRequest,
    RefusedModelCallTurnIdentities, SemanticTranscriptEntryId, SemanticTranscriptEntryPayload,
    SemanticTranscriptEntryRef, SessionId, StopRequestedModelCallTurn,
    StoppedToolResponsePartIdentity, StoppedToolRoundModelCallIdentities, ToolApprovalDecision,
    ToolAttemptEnd, ToolDenialReason, ToolExecutionError, ToolRequest, ToolRequestId,
    ToolResponsePartIdentity, ToolResultContent, ToolRoundModelCallIdentities, TurnAttemptId,
    TurnId, UserContent,
};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::{
    ClassifyOperatorFailure, NoToolCatalog, OperatorFailureClass, ResolvedToolConversationEntry,
    ToolCatalog, ToolDefinition, tool_loop::initial_tool_approval,
};

/// Non-secret durable name of the credential pinned for one model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallCredentialReference(String);

impl ModelCallCredentialReference {
    /// Preserves the deployment-owned reference spelling exactly.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the non-secret reference text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

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
    /// One durable assistant tool proposal.
    AssistantToolUse {
        /// The source-qualified semantic entry being rendered.
        source: SemanticTranscriptEntryRef,
        /// The outcome-authoritative call that proposed the request.
        producing_call: ModelCallId,
        /// Immutable request content and hub correlation.
        request: ToolRequest,
    },
    /// One durable result corresponding to an earlier assistant proposal.
    ToolResult {
        /// The source-qualified semantic entry being rendered.
        source: SemanticTranscriptEntryRef,
        /// The logical request whose provider-visible correlation this resolves.
        request: ToolRequestId,
        /// Exact durable result classification and content.
        content: ModelToolResultContent,
    },
}

/// Provider-neutral result content resolved from durable request/attempt facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelToolResultContent {
    /// Exact admitted executor success content.
    Success(ToolResultContent),
    /// Exact terminal executor error evidence.
    ExecutionError(ToolExecutionError),
    /// Exact durable owner denial.
    Denied {
        /// Optional bounded sanitized owner explanation.
        reason: Option<ToolDenialReason>,
    },
    /// The turn ended before this request received a decision.
    ClosedByTurnEnd,
}

fn render_frontier_messages<'a>(
    entries: impl IntoIterator<
        Item = (
            SemanticTranscriptEntryRef,
            &'a SemanticTranscriptEntryPayload,
        ),
    >,
    mut origin_content: impl FnMut(AcceptedInputId) -> Option<UserContent>,
    tool_entries: impl IntoIterator<Item = &'a ResolvedToolConversationEntry>,
) -> Result<Box<[ModelConversationMessage]>, ModelFrontierRenderingError> {
    let mut resolved_tools = BTreeMap::new();
    for evidence in tool_entries {
        if resolved_tools.insert(evidence.source(), evidence).is_some() {
            return Err(ModelFrontierRenderingError::DuplicateToolEvidence {
                entry: evidence.source(),
            });
        }
    }
    let mut messages = Vec::new();
    for (source, payload) in entries {
        match payload {
            SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { accepted_input, .. } => {
                let content = origin_content(*accepted_input).ok_or(
                    ModelFrontierRenderingError::MissingOriginContent {
                        entry: source,
                        accepted_input: *accepted_input,
                    },
                )?;
                messages.push(ModelConversationMessage::User {
                    source,
                    accepted_input: *accepted_input,
                    content,
                });
            }
            SemanticTranscriptEntryPayload::AssistantText {
                producing_call,
                value,
            } => messages.push(ModelConversationMessage::Assistant {
                source,
                producing_call: *producing_call,
                content: value.clone(),
            }),
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            } => {
                let Some(ResolvedToolConversationEntry::AssistantToolUse {
                    request: record, ..
                }) = resolved_tools.remove(&source)
                else {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                };
                if record.id() != *request
                    || record.producing_call() != *producing_call
                    || record.session() != source.source_session()
                {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                }
                messages.push(ModelConversationMessage::AssistantToolUse {
                    source,
                    producing_call: *producing_call,
                    request: record.clone(),
                });
            }
            SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } => {
                let Some(ResolvedToolConversationEntry::ExecutionResult {
                    request,
                    attempt: ended,
                    ..
                }) = resolved_tools.remove(&source)
                else {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                };
                if ended.attempt() != *attempt
                    || ended.request() != request.id()
                    || ended.session() != source.source_session()
                    || ended.turn() != request.turn()
                    || request.session() != source.source_session()
                {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                }
                let content = match ended.end() {
                    ToolAttemptEnd::Completed { result } => {
                        ModelToolResultContent::Success(result.clone())
                    }
                    ToolAttemptEnd::KnownFailed { error } => {
                        ModelToolResultContent::ExecutionError(error.clone())
                    }
                    ToolAttemptEnd::Ambiguous => {
                        return Err(ModelFrontierRenderingError::UnrenderableToolResult {
                            entry: source,
                        });
                    }
                };
                messages.push(ModelConversationMessage::ToolResult {
                    source,
                    request: request.id(),
                    content,
                });
            }
            SemanticTranscriptEntryPayload::ToolDenied { request } => {
                let Some(ResolvedToolConversationEntry::Denied {
                    request: record,
                    approval,
                    ..
                }) = resolved_tools.remove(&source)
                else {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                };
                let ToolApprovalDecision::Deny { reason } = approval.decision() else {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                };
                if record.id() != *request
                    || approval.request() != *request
                    || record.session() != source.source_session()
                {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                }
                messages.push(ModelConversationMessage::ToolResult {
                    source,
                    request: *request,
                    content: ModelToolResultContent::Denied {
                        reason: reason.clone(),
                    },
                });
            }
            SemanticTranscriptEntryPayload::ToolClosed { request } => {
                let Some(ResolvedToolConversationEntry::Closed {
                    request: record, ..
                }) = resolved_tools.remove(&source)
                else {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                };
                if record.id() != *request || record.session() != source.source_session() {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                }
                messages.push(ModelConversationMessage::ToolResult {
                    source,
                    request: *request,
                    content: ModelToolResultContent::ClosedByTurnEnd,
                });
            }
            SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. } => {}
        }
    }
    if let Some(entry) = resolved_tools.into_keys().next() {
        return Err(ModelFrontierRenderingError::UnexpectedToolEvidence { entry });
    }
    Ok(messages.into_boxed_slice())
}

/// A checked prepared call plus its provider-neutral ordered messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedModelOperation {
    request: PreparedModelCallRequest,
    credential_reference: ModelCallCredentialReference,
    messages: Box<[ModelConversationMessage]>,
    tools: Box<[ToolDefinition]>,
}

impl PreparedModelOperation {
    fn render(
        request: PreparedModelCallRequest,
        credential_reference: ModelCallCredentialReference,
        tools: Box<[ToolDefinition]>,
        tool_entries: &[ResolvedToolConversationEntry],
    ) -> Result<Self, ModelFrontierRenderingError> {
        let messages = render_frontier_messages(
            request
                .frontier_entries()
                .map(|entry| (entry.reference(), entry.payload())),
            |accepted_input| request.origin_content(accepted_input).cloned(),
            tool_entries.iter(),
        )?;
        Ok(Self {
            request,
            credential_reference,
            messages,
            tools,
        })
    }

    /// Borrows the checked durable request facts.
    pub const fn request(&self) -> &PreparedModelCallRequest {
        &self.request
    }

    /// Borrows the exact durable credential reference pinned with the call.
    pub const fn credential_reference(&self) -> &ModelCallCredentialReference {
        &self.credential_reference
    }

    /// Borrows the exact messages in frontier order.
    pub fn messages(&self) -> &[ModelConversationMessage] {
        &self.messages
    }

    /// Borrows the exact model-facing catalog snapshot.
    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
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
    /// Two storage evidence values claimed the same semantic entry.
    DuplicateToolEvidence {
        /// Duplicated source-qualified entry.
        entry: SemanticTranscriptEntryRef,
    },
    /// Reference-only tool history lacks exact correlated durable authority.
    MissingOrMismatchedToolEvidence {
        /// Source-qualified entry whose evidence is absent or cross-wired.
        entry: SemanticTranscriptEntryRef,
    },
    /// Durable ambiguity cannot be projected as an ordinary model-visible result.
    UnrenderableToolResult {
        /// Source-qualified result entry.
        entry: SemanticTranscriptEntryRef,
    },
    /// Storage supplied evidence not named by the checked frontier.
    UnexpectedToolEvidence {
        /// Extra source-qualified entry.
        entry: SemanticTranscriptEntryRef,
    },
}

impl fmt::Display for ModelFrontierRenderingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingOriginContent { .. } => {
                formatter.write_str("model frontier origin content is missing")
            }
            Self::DuplicateToolEvidence { .. } => {
                formatter.write_str("model frontier tool evidence is duplicated")
            }
            Self::MissingOrMismatchedToolEvidence { .. } => {
                formatter.write_str("model frontier tool evidence is missing or mismatched")
            }
            Self::UnrenderableToolResult { .. } => {
                formatter.write_str("model frontier contains an unrenderable tool result")
            }
            Self::UnexpectedToolEvidence { .. } => {
                formatter.write_str("model frontier tool evidence is not referenced")
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
    Ready {
        /// Checked durable request facts.
        request: Box<PreparedModelCallRequest>,
        /// Non-secret credential reference captured with the call.
        credential_reference: ModelCallCredentialReference,
        /// Frozen dangerous blanket posture for initial request decisions.
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
        /// Exact durable authority for every tool-related frontier entry.
        tool_entries: Box<[ResolvedToolConversationEntry]>,
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
        identities: FailedModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
    ) -> impl Future<Output = Result<FailedModelCallTurn, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send;

    /// Rereads whether a retained capability-failure closure committed.
    fn reread_failure(
        &mut self,
        session: SessionId,
        call: ModelCallId,
    ) -> impl Future<Output = Result<RetainedCapabilityFailureStatus, Self::Error>> + Send;
}

/// Authoritative status of one retained pre-send capability failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetainedCapabilityFailureStatus {
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

/// Opaque same-incarnation evidence retained across a failed orchestration stage.
///
/// This state prevents a later service invocation or explicit composition
/// handoff from repeating credential work, losing proof that provider entry
/// never occurred, or dropping an unchanged terminal observation. INV-014 and
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
    state: RetainedModelCallExecutionStateKind,
}

#[derive(Debug, Eq, PartialEq)]
enum RetainedModelCallExecutionStateKind {
    /// Capability preparation proved an ordinary pre-send known failure.
    CapabilityKnownFailure {
        /// Session owning the exact prepared call.
        session: SessionId,
        /// Prepared call whose guarded known-failure closure remains pending.
        call: ModelCallId,
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
        observation: CorrelatedModelCallTerminalObservation,
        /// Frozen policy outcomes for each tool proposal, in proposal order.
        tool_approvals: Box<[InitialToolApproval]>,
    },
}

/// Adapter-local result of credential lookup and capability preparation.
pub enum ModelCallCapabilityPreparation<Capability> {
    /// A call-bound one-shot capability is ready to move into provider work.
    Ready(Capability),
    /// Durable authority changed while the capability was being prepared.
    Cancelled,
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
    fn prepare_capability<Cancellation>(
        &mut self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> impl Future<Output = Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>> + Send
    where
        Cancellation: Future<Output = ()> + Send + 'static;

    /// Consumes one capability after durable send authorization.
    fn invoke<AcceptancePossible, Cancellation>(
        &mut self,
        authorized: AuthorizedModelCall,
        capability: Self::Capability,
        acceptance_possible: AcceptancePossible,
        cancellation: Cancellation,
    ) -> impl Future<Output = Result<CorrelatedModelCallTerminalObservation, Self::Error>> + Send
    where
        AcceptancePossible: FnOnce() + Send,
        Cancellation: Future<Output = ()> + Send + 'static;
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
    /// Generates a distinct logical tool-request candidate.
    fn next_tool_request_id(&mut self) -> ToolRequestId;
    /// Generates a distinct same-turn continuation attempt candidate.
    fn next_tool_continuation_attempt_id(&mut self) -> TurnAttemptId;
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

    fn next_tool_request_id(&mut self) -> ToolRequestId {
        ToolRequestId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_tool_continuation_attempt_id(&mut self) -> TurnAttemptId {
        TurnAttemptId::from_uuid(uuid::Uuid::now_v7())
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
    /// Compatibility-only result retained for existing exhaustive callers.
    ///
    /// Atomic steering consumption means this execution service no longer
    /// produces the variant; callers must not treat it as a reachable
    /// preparation-blocked state.
    PendingSteering {
        /// The earliest accepted input proving that steering remains pending.
        accepted_input: AcceptedInputId,
    },
    /// A trustworthy local capability failure closed the prepared call.
    CapabilityKnownFailure(Box<FailedModelCallTurn>),
    /// A retained capability failure's earlier commit was proven to have landed.
    CapabilityFailureAlreadyCommitted(ModelCallId),
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
    /// Authoritative reread of a retained capability failure failed.
    CapabilityFailureReread(FailureError),
    /// Durable send authorization failed.
    Authorization(AuthorizationError),
    /// Authoritative reread after an ambiguous authorization also failed.
    AuthorizationReread {
        /// The original commit-ambiguous authorization failure.
        authorization_error: AuthorizationError,
        /// The failure to establish whether authorization committed.
        reread_error: AuthorizationError,
    },
    /// A later pass still could not reconcile retained non-consumption proof.
    AuthorizationReconciliation(AuthorizationError),
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
            Self::CapabilityFailureReread(error) => {
                write!(
                    formatter,
                    "model-call capability-failure reread failed: {error}"
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
            Self::AuthorizationReconciliation(error) => {
                write!(
                    formatter,
                    "model-call authorization reconciliation failed: {error}"
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
            Self::CapabilityFailureCommit(error) | Self::CapabilityFailureReread(error) => {
                error.operator_failure_class()
            }
            Self::Authorization(error) => error.operator_failure_class(),
            Self::AuthorizationReread { reread_error, .. } => reread_error.operator_failure_class(),
            Self::AuthorizationReconciliation(error) => error.operator_failure_class(),
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
    catalog: Arc<dyn ToolCatalog>,
    retained_state: Option<RetainedModelCallExecutionState>,
}

impl<Ids, Prepare, Failure, Authorization, Observation, Provider, Gate>
    ModelCallExecutionService<Ids, Prepare, Failure, Authorization, Observation, Provider, Gate>
{
    /// Composes every purpose-specific effect role.
    pub fn new(
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
            catalog: Arc::new(NoToolCatalog),
            retained_state: None,
        }
    }

    /// Replaces the empty compatibility catalog with one tool-capable port.
    pub fn with_tool_catalog(mut self, catalog: impl ToolCatalog + 'static) -> Self {
        self.catalog = Arc::new(catalog);
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
                RetainedModelCallExecutionStateKind::CapabilityKnownFailure { .. }
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
                RetainedModelCallExecutionStateKind::CapabilityKnownFailure { session, call } => {
                    match self.failure.reread_failure(session, call).await {
                        Ok(RetainedCapabilityFailureStatus::Pending) => {
                            return self.commit_capability_known_failure(session, call).await;
                        }
                        Ok(RetainedCapabilityFailureStatus::AlreadyCommitted) => {
                            return Ok(
                                ModelCallExecutionOutcome::CapabilityFailureAlreadyCommitted(call),
                            );
                        }
                        Ok(RetainedCapabilityFailureStatus::Cancelled) => {
                            return Ok(ModelCallExecutionOutcome::NoWork);
                        }
                        Err(error) => {
                            self.retained_state = Some(RetainedModelCallExecutionState {
                                state:
                                    RetainedModelCallExecutionStateKind::CapabilityKnownFailure {
                                        session,
                                        call,
                                    },
                            });
                            return Err(ModelCallExecutionError::CapabilityFailureReread(error));
                        }
                    }
                }
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
                    Ok(RetainedModelCallObservationStatus::Pending) => {
                        return self
                            .commit_terminal_observation(retained_session, retained, tool_approvals)
                            .await;
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
                            retained_observation: retained,
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
                Ok(PrepareModelCallOutcome::Checkpointed(call)) => {
                    return Ok(ModelCallExecutionOutcome::Checkpointed(call));
                }
                Ok(PrepareModelCallOutcome::Ready {
                    request,
                    credential_reference,
                    dangerous_tool_auto_approval,
                    tool_entries,
                }) => {
                    break (
                        request,
                        credential_reference,
                        dangerous_tool_auto_approval,
                        tool_entries,
                    );
                }
                Ok(PrepareModelCallOutcome::TargetUnavailable(failed)) => {
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

        let (prepared, credential_reference, dangerous_tool_auto_approval, tool_entries) = prepared;
        let call = prepared.call().id();
        let attempt = prepared.attempt();
        let turn = prepared.turn();
        let prepared_request = (*prepared).clone();
        let advertised_tools = self.catalog.definitions();
        let operation = PreparedModelOperation::render(
            *prepared,
            credential_reference,
            advertised_tools.clone(),
            &tool_entries,
        )
        .map_err(ModelCallExecutionError::Render)?;
        if automatic_tool_round_limit_reached(turn, operation.messages()) {
            return self.commit_capability_known_failure(session, call).await;
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
                return self.commit_capability_known_failure(session, call).await;
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
        );
        self.commit_terminal_observation(session, observation, tool_approvals)
            .await
    }

    async fn commit_capability_known_failure(
        &mut self,
        session: SessionId,
        call: ModelCallId,
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
                    self.retained_state = Some(RetainedModelCallExecutionState {
                        state: RetainedModelCallExecutionStateKind::CapabilityKnownFailure {
                            session,
                            call,
                        },
                    });
                    return Err(ModelCallExecutionError::CapabilityFailureCommit(error));
                }
            }
        }
    }

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
            let identities =
                self.next_terminal_identities(observation.observation(), &tool_approvals);
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
                    self.retained_state = Some(RetainedModelCallExecutionState {
                        state: RetainedModelCallExecutionStateKind::TerminalObservation {
                            session,
                            observation: observation.clone(),
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
            ModelCallTerminalObservation::CompletedWithTools { response } => {
                let mut approval_index = 0usize;
                let mut continuing = Vec::with_capacity(response.parts().len());
                let mut stopped = Vec::with_capacity(response.parts().len());
                let mut every_request_approved = true;
                for part in response.parts() {
                    let entry = self.ids.next_semantic_entry_id();
                    match part {
                        AssistantResponsePart::Text(_) => {
                            continuing.push(ToolResponsePartIdentity::text(entry));
                            stopped.push(StoppedToolResponsePartIdentity::text(entry));
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
                            every_request_approved &= approval != InitialToolApproval::Confirm;
                            let request = self.ids.next_tool_request_id();
                            continuing.push(ToolResponsePartIdentity::tool_call(
                                entry, request, approval,
                            ));
                            stopped.push(StoppedToolResponsePartIdentity::tool_call(
                                entry,
                                request,
                                self.ids.next_semantic_entry_id(),
                            ));
                        }
                    }
                }
                debug_assert_eq!(approval_index, tool_approvals.len());
                let continuation_attempt =
                    every_request_approved.then(|| self.ids.next_tool_continuation_attempt_id());
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
            ModelCallTerminalObservation::Ambiguous => ModelCallTerminalIdentities::Ambiguous(
                AmbiguousModelCallTurnIdentities::new(self.ids.next_context_frontier_id()),
            ),
        };
        ModelCallTerminalIdentityCandidates::Exact(exact)
    }

    fn tool_approvals(
        &self,
        observation: &ModelCallTerminalObservation,
        posture: DangerousToolAutoApproval,
        advertised_tools: &[ToolDefinition],
    ) -> Box<[InitialToolApproval]> {
        let ModelCallTerminalObservation::CompletedWithTools { response } = observation else {
            return Box::new([]);
        };
        response
            .parts()
            .iter()
            .filter_map(|part| match part {
                AssistantResponsePart::Text(_) => None,
                AssistantResponsePart::ToolCall(proposal) => {
                    let definition = advertised_tools
                        .iter()
                        .find(|definition| definition.name() == proposal.name());
                    Some(initial_tool_approval(posture, definition))
                }
            })
            .collect()
    }
}

fn automatic_tool_round_limit_reached(turn: TurnId, messages: &[ModelConversationMessage]) -> bool {
    messages
        .iter()
        .filter_map(|message| match message {
            ModelConversationMessage::AssistantToolUse {
                producing_call,
                request,
                ..
            } if request.turn() == turn => Some(*producing_call),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .len()
        >= MAX_AUTOMATIC_TOOL_ROUNDS_PER_TURN
}

/// One deterministic scripted-provider action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptedModelCallStep {
    /// Capability preparation returns a trustworthy ordinary failure.
    CapabilityKnownFailure,
    /// Capability preparation observes durable cancellation.
    CapabilityCancelled,
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

/// Opaque one-shot capability owned by [`ScriptedModelCallProvider`].
pub struct ScriptedModelCallCapability {
    operation: PreparedModelOperation,
    step: ScriptedModelCallStep,
}

/// Deterministic in-repository implementation of the provider port.
#[derive(Debug)]
pub struct ScriptedModelCallProvider {
    steps: std::collections::VecDeque<ScriptedModelCallStep>,
    capability_preparation_count: usize,
    interaction_count: usize,
    last_prepared_messages: Option<Box<[ModelConversationMessage]>>,
}

impl ScriptedModelCallProvider {
    /// Creates a provider that consumes actions in supplied order.
    ///
    /// Capability-stage actions are consumed during preparation. Interaction
    /// actions remain queued until their prepared capability is invoked, so a
    /// proven authorization rollback can prepare the same action again.
    pub fn new(steps: impl IntoIterator<Item = ScriptedModelCallStep>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
            capability_preparation_count: 0,
            interaction_count: 0,
            last_prepared_messages: None,
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

    /// Borrows the exact messages most recently presented for capability
    /// preparation.
    pub fn last_prepared_messages(&self) -> Option<&[ModelConversationMessage]> {
        self.last_prepared_messages.as_deref()
    }
}

impl ModelCallProvider for ScriptedModelCallProvider {
    type Capability = ScriptedModelCallCapability;
    type Error = ScriptedModelCallError;

    fn prepare_capability<Cancellation>(
        &mut self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> impl Future<Output = Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>> + Send
    where
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        drop(cancellation);
        self.capability_preparation_count += 1;
        self.last_prepared_messages = Some(operation.messages().to_vec().into_boxed_slice());
        let step = self.steps.front().cloned();
        if matches!(
            &step,
            Some(
                ScriptedModelCallStep::CapabilityKnownFailure
                    | ScriptedModelCallStep::CapabilityCancelled
                    | ScriptedModelCallStep::CapabilityOperatorFailure
            )
        ) {
            self.steps.pop_front();
        }
        async move {
            match step.ok_or(ScriptedModelCallError::ScriptExhausted)? {
                ScriptedModelCallStep::CapabilityKnownFailure => {
                    Ok(ModelCallCapabilityPreparation::KnownFailure)
                }
                ScriptedModelCallStep::CapabilityCancelled => {
                    Ok(ModelCallCapabilityPreparation::Cancelled)
                }
                ScriptedModelCallStep::CapabilityOperatorFailure => {
                    Err(ScriptedModelCallError::CapabilityOperatorFailure)
                }
                step @ (ScriptedModelCallStep::InteractionOperatorFailure
                | ScriptedModelCallStep::Return(_)) => Ok(ModelCallCapabilityPreparation::Ready(
                    ScriptedModelCallCapability { operation, step },
                )),
            }
        }
    }

    fn invoke<AcceptancePossible, Cancellation>(
        &mut self,
        authorized: AuthorizedModelCall,
        capability: Self::Capability,
        acceptance_possible: AcceptancePossible,
        cancellation: Cancellation,
    ) -> impl Future<Output = Result<CorrelatedModelCallTerminalObservation, Self::Error>> + Send
    where
        AcceptancePossible: FnOnce() + Send,
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        drop(cancellation);
        self.interaction_count += 1;
        let prepared = capability.operation.request();
        let step = if prepared.session() != authorized.session()
            || prepared.turn() != authorized.turn()
            || prepared.attempt() != authorized.attempt().id()
            || prepared.call().id() != authorized.call().id()
            || prepared.call().selection() != authorized.call().selection()
            || prepared.call().target() != authorized.call().target()
            || prepared.call().frontier() != authorized.call().frontier()
        {
            Err(ScriptedModelCallError::AuthorizationMismatch)
        } else {
            match self.steps.front() {
                None => Err(ScriptedModelCallError::ScriptExhausted),
                Some(step) if step != &capability.step => {
                    Err(ScriptedModelCallError::AuthorizationMismatch)
                }
                Some(_) => self
                    .steps
                    .pop_front()
                    .ok_or(ScriptedModelCallError::ScriptExhausted),
            }
        };
        async move {
            let step = step?;
            acceptance_possible();
            match step {
                ScriptedModelCallStep::Return(observation) => Ok(authorized
                    .observation_correlation()
                    .bind_terminal_observation(observation)),
                ScriptedModelCallStep::InteractionOperatorFailure => {
                    Err(ScriptedModelCallError::InteractionOperatorFailure)
                }
                ScriptedModelCallStep::CapabilityKnownFailure
                | ScriptedModelCallStep::CapabilityCancelled
                | ScriptedModelCallStep::CapabilityOperatorFailure => {
                    Err(ScriptedModelCallError::ScriptExhausted)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Arc};

    use expect_test::expect;
    use signalbox_domain::{
        AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
        AcceptedInputSchedulingReconstitutionInput, AcceptedInputTurnActivationIdentities,
        AcceptedInputTurnSchedulingRecord, AcceptedInputTurnSchedulingRecordState, Actor,
        DeliveryRequest, DirectModelSelection, DurableCommandId, FrozenModelSelection,
        ModelCallExecutionReconstitutionInput, ModelCallOriginContent,
        ModelCallReconstitutionInput, ModelCallReconstitutionState, ModelSelectionOverride,
        ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition, NormalizedToolArguments,
        PerInputConfigurationChoices, PinnedProviderTargetReconstitutionInput,
        ProviderModelIdentity, ResolvedProviderTarget, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionInputPosition, SessionReconstitutionInput, SubmitInput,
        SubmitInputReconstitutionInput, SubmitInputTurnOriginReconstitutionInput,
        ToolApprovalResolutionReconstitutionInput, ToolAttemptReconstitutionInput,
        ToolAttemptReconstitutionState, ToolDecisionSource, ToolDispatchGeneration,
        ToolEffectClass, ToolName, ToolRequestOrdinal, ToolRequestReconstitutionInput,
        ToolResultText, TranscriptAncestry,
    };
    use uuid::Uuid;

    use super::*;

    fn identity<Identity>(value: u128, from_uuid: impl FnOnce(Uuid) -> Identity) -> Identity {
        from_uuid(Uuid::from_u128(value))
    }

    fn credential_reference() -> ModelCallCredentialReference {
        ModelCallCredentialReference::new("fixture-provider-primary")
    }

    fn ready(request: PreparedModelCallRequest) -> PrepareModelCallOutcome {
        PrepareModelCallOutcome::Ready {
            request: Box::new(request),
            credential_reference: credential_reference(),
            dangerous_tool_auto_approval: DangerousToolAutoApproval::Disabled,
            tool_entries: Box::new([]),
        }
    }

    fn tool_response() -> ModelCallTerminalObservation {
        let arguments =
            signalbox_domain::NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("fixture arguments are valid");
        let parts = vec![
            AssistantResponsePart::Text(
                AssistantText::try_new(String::from("checking"))
                    .expect("fixture assistant text is valid"),
            ),
            AssistantResponsePart::ToolCall(signalbox_domain::ToolCallProposal::new(
                signalbox_domain::ToolName::try_new(String::from("automatic"))
                    .expect("fixture tool name is valid"),
                arguments.clone(),
            )),
            AssistantResponsePart::ToolCall(signalbox_domain::ToolCallProposal::new(
                signalbox_domain::ToolName::try_new(String::from("unknown"))
                    .expect("fixture tool name is valid"),
                arguments,
            )),
        ];
        ModelCallTerminalObservation::CompletedWithTools {
            response: signalbox_domain::ToolUsingAssistantResponse::try_from_parts(parts)
                .expect("fixture response contains tools"),
        }
    }

    /// One request in the canonical model-rendering session, turn, and call.
    ///
    /// The request identity derives from the ordinal and is deliberately in a
    /// different UUID range so an implementation cannot confuse the two.
    fn model_tool_request(ordinal: u32) -> ToolRequest {
        ToolRequestReconstitutionInput::new(
            identity(100 + u128::from(ordinal), ToolRequestId::from_uuid),
            identity(1, SessionId::from_uuid),
            identity(2, TurnId::from_uuid),
            identity(3, ModelCallId::from_uuid),
            ToolRequestOrdinal::from_u32(ordinal),
            ToolName::try_new(format!("tool_{ordinal}")).expect("fixture tool name is valid"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("fixture arguments are valid"),
        )
        .into_request()
    }

    fn model_tool_use_message(
        request_identity: u128,
        turn_identity: u128,
        call_identity: u128,
        ordinal: u32,
    ) -> ModelConversationMessage {
        let session = identity(1, SessionId::from_uuid);
        let turn = identity(turn_identity, TurnId::from_uuid);
        let producing_call = identity(call_identity, ModelCallId::from_uuid);
        let request = ToolRequestReconstitutionInput::new(
            identity(request_identity, ToolRequestId::from_uuid),
            session,
            turn,
            producing_call,
            ToolRequestOrdinal::from_u32(ordinal),
            ToolName::try_new(String::from("known")).expect("fixture tool name is valid"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("fixture arguments are valid"),
        )
        .into_request();
        ModelConversationMessage::AssistantToolUse {
            source: SemanticTranscriptEntryRef::from_source(
                session,
                identity(
                    request_identity + 10_000,
                    SemanticTranscriptEntryId::from_uuid,
                ),
            ),
            producing_call,
            request,
        }
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
        tool_requests: VecDeque<ToolRequestId>,
        tool_attempts: VecDeque<TurnAttemptId>,
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
                tool_requests: (60..70)
                    .map(|value| identity(value, ToolRequestId::from_uuid))
                    .collect(),
                tool_attempts: (70..80)
                    .map(|value| identity(value, TurnAttemptId::from_uuid))
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

        fn next_tool_request_id(&mut self) -> ToolRequestId {
            self.tool_requests
                .pop_front()
                .expect("fixture tool request identity")
        }

        fn next_tool_continuation_attempt_id(&mut self) -> TurnAttemptId {
            self.tool_attempts
                .pop_front()
                .expect("fixture tool continuation attempt identity")
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

        async fn prepare<NextSteeringIdentities>(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
            _failure_identities: FailedModelCallTurnIdentities,
            _steering_frontier: ContextFrontierId,
            _next_steering_identities: NextSteeringIdentities,
        ) -> Result<PrepareModelCallOutcome, Self::Error>
        where
            NextSteeringIdentities:
                FnMut(AcceptedInputId) -> (SemanticTranscriptEntryId, TurnId) + Send,
        {
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

        async fn reread_failure(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
        ) -> Result<RetainedCapabilityFailureStatus, Self::Error> {
            panic!("unused capability-failure reread")
        }
    }

    #[derive(Debug)]
    struct FakeFailure {
        errors: VecDeque<FakeError>,
        rereads: VecDeque<Result<RetainedCapabilityFailureStatus, FakeError>>,
        calls: usize,
        reread_calls: usize,
    }

    impl FailPreparedModelCallTransaction for FakeFailure {
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
            self.calls += 1;
            Err(self
                .errors
                .pop_front()
                .expect("one fake failure-commit error"))
        }

        async fn reread_failure(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
        ) -> Result<RetainedCapabilityFailureStatus, Self::Error> {
            self.reread_calls += 1;
            self.rereads
                .pop_front()
                .expect("one fake capability-failure reread")
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
        ) -> Result<AuthorizeModelCallOutcome, Self::Error> {
            self.calls += 1;
            self.outcomes
                .pop_front()
                .expect("one fake authorization outcome")
                .map(|authorized| AuthorizeModelCallOutcome::Authorized(Box::new(authorized)))
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

        fn cancellation_signal(
            &self,
            _session: SessionId,
            _call: ModelCallId,
        ) -> impl Future<Output = ()> + Send + 'static {
            std::future::pending()
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
        ) -> Result<AuthorizeModelCallOutcome, Self::Error> {
            panic!("unused authorization transaction")
        }

        async fn reread_after_ambiguous_commit(
            &mut self,
            _session: SessionId,
            _prepared: &PreparedModelCallRequest,
        ) -> Result<ModelCallAuthorizationReread, Self::Error> {
            panic!("unused authorization reread")
        }

        fn cancellation_signal(
            &self,
            _session: SessionId,
            _call: ModelCallId,
        ) -> impl Future<Output = ()> + Send + 'static {
            std::future::pending()
        }
    }

    #[derive(Debug)]
    struct NoSendAuthorization {
        calls: usize,
    }

    impl AuthorizeModelCallTransaction for NoSendAuthorization {
        type Error = FakeError;

        async fn authorize(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
        ) -> Result<AuthorizeModelCallOutcome, Self::Error> {
            self.calls += 1;
            Ok(AuthorizeModelCallOutcome::NoSend)
        }

        async fn reread_after_ambiguous_commit(
            &mut self,
            _session: SessionId,
            _prepared: &PreparedModelCallRequest,
        ) -> Result<ModelCallAuthorizationReread, Self::Error> {
            panic!("a known no-send result needs no reread")
        }

        fn cancellation_signal(
            &self,
            _session: SessionId,
            _call: ModelCallId,
        ) -> impl Future<Output = ()> + Send + 'static {
            std::future::pending()
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
            _identities: ModelCallTerminalIdentityCandidates,
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
            _identities: ModelCallTerminalIdentityCandidates,
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

        async fn prepare_capability<Cancellation>(
            &mut self,
            _operation: PreparedModelOperation,
            _cancellation: Cancellation,
        ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>
        where
            Cancellation: Future<Output = ()> + Send + 'static,
        {
            panic!("unused provider capability preparation")
        }

        async fn invoke<AcceptancePossible, Cancellation>(
            &mut self,
            _authorized: AuthorizedModelCall,
            _capability: Self::Capability,
            _acceptance_possible: AcceptancePossible,
            _cancellation: Cancellation,
        ) -> Result<CorrelatedModelCallTerminalObservation, Self::Error>
        where
            AcceptancePossible: FnOnce() + Send,
            Cancellation: Future<Output = ()> + Send + 'static,
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

        async fn prepare_capability<Cancellation>(
            &mut self,
            operation: PreparedModelOperation,
            _cancellation: Cancellation,
        ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>
        where
            Cancellation: Future<Output = ()> + Send + 'static,
        {
            Ok(ModelCallCapabilityPreparation::Ready(operation))
        }

        fn invoke<AcceptancePossible, Cancellation>(
            &mut self,
            _authorized: AuthorizedModelCall,
            _capability: Self::Capability,
            acceptance_possible: AcceptancePossible,
            _cancellation: Cancellation,
        ) -> impl Future<Output = Result<CorrelatedModelCallTerminalObservation, Self::Error>> + Send
        where
            AcceptancePossible: FnOnce() + Send,
            Cancellation: Future<Output = ()> + Send + 'static,
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
        let credential_reference = credential_reference();
        let operation = PreparedModelOperation::render(
            request,
            credential_reference.clone(),
            Box::new([]),
            &[],
        )
        .expect("the baseline origin-only frontier renders");
        assert_eq!(operation.credential_reference(), &credential_reference);
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

    /// The recorded turn-wide availability bound counts validated producing
    /// calls, not requests or inherited tool history.
    #[test]
    fn s15_automatic_tool_round_bound_counts_current_turn_producing_calls() {
        let current_turn = identity(2, TurnId::from_uuid);
        let mut messages = (0..31_u128)
            .map(|round| model_tool_use_message(1_000 + round, 2, 2_000 + round, 0))
            .collect::<Vec<_>>();
        assert!(!automatic_tool_round_limit_reached(current_turn, &messages));

        messages.push(model_tool_use_message(1_031, 2, 2_031, 0));
        assert!(automatic_tool_round_limit_reached(current_turn, &messages));

        let one_multi_request_round = (0..32_u32)
            .map(|ordinal| model_tool_use_message(3_000 + u128::from(ordinal), 2, 4_000, ordinal))
            .chain(
                (0..32_u128)
                    .map(|round| model_tool_use_message(5_000 + round, 99, 6_000 + round, 0)),
            )
            .collect::<Vec<_>>();
        assert!(
            !automatic_tool_round_limit_reached(current_turn, &one_multi_request_round),
            "one current-turn batch and inherited history consume one round"
        );
    }

    /// S10 / INV-001 / INV-020: one identity is minted per ordered response
    /// part/request, approval stays pinned to the advertised catalog snapshot,
    /// mixed auto/confirm policy parks without a continuation attempt, and the
    /// adapter still receives a stopped race closure.
    #[test]
    fn s10_inv001_inv020_tool_response_candidates_preserve_order_and_policy() {
        let schema =
            crate::ToolInputSchema::try_new(String::from(r#"{"properties":{},"type":"object"}"#))
                .expect("fixture schema is valid");
        let definition = crate::ToolDefinition::new(
            signalbox_domain::ToolName::try_new(String::from("automatic"))
                .expect("fixture name is valid"),
            String::from("Runs automatically."),
            schema,
            signalbox_domain::ToolPermissionDefault::Auto,
            signalbox_domain::ToolEffectClass::EffectFree,
        );
        let catalog = crate::CompiledToolCatalog::try_new([crate::CompiledTool::new(
            definition,
            |_: &signalbox_domain::NormalizedToolArguments| Ok(()),
        )])
        .expect("one tool is unambiguous");
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: VecDeque::new(),
                calls: 0,
            },
            UnusedFailure,
            UnusedAuthorization,
            UnusedObservation,
            UnusedProvider,
            InProcessAttemptDispatchGate::default(),
        )
        .with_tool_catalog(catalog);
        let observation = tool_response();
        let advertised_tools = service.catalog.definitions();
        service.catalog = Arc::new(NoToolCatalog);
        let approvals = service.tool_approvals(
            &observation,
            DangerousToolAutoApproval::Disabled,
            &advertised_tools,
        );
        assert_eq!(
            approvals.as_ref(),
            [
                InitialToolApproval::PolicyAuto,
                InitialToolApproval::Confirm
            ]
        );

        let ModelCallTerminalIdentityCandidates::ToolRound {
            continuing,
            stopped: _,
        } = service.next_terminal_identities(&observation, &approvals)
        else {
            panic!("tool response requires both race-safe closures");
        };
        assert_eq!(continuing.response_parts().len(), 3);
        assert_eq!(continuing.continuation_attempt(), None);
        let requests = continuing
            .response_parts()
            .iter()
            .filter_map(|part| match part {
                ToolResponsePartIdentity::Text { .. } => None,
                ToolResponsePartIdentity::ToolCall {
                    request, approval, ..
                } => Some((*request, *approval)),
            })
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].1, InitialToolApproval::PolicyAuto);
        assert_eq!(requests[1].1, InitialToolApproval::Confirm);
    }

    /// S02 / INV-015: mixed semantic content keeps exact role order and
    /// source-qualified provenance, including entries created by a different
    /// session; terminal markers do not invent provider-visible messages.
    #[test]
    fn s02_inv015_frontier_rendering_preserves_mixed_roles_and_inherited_sources() {
        let inherited_session = identity(90, SessionId::from_uuid);
        let current_session = identity(1, SessionId::from_uuid);
        let inherited_input = identity(91, AcceptedInputId::from_uuid);
        let current_input = identity(92, AcceptedInputId::from_uuid);
        let failed_input = identity(99, AcceptedInputId::from_uuid);
        let producing_call = identity(93, ModelCallId::from_uuid);
        let inherited_content =
            UserContent::try_text(String::from("inherited user request")).expect("valid text");
        let current_content =
            UserContent::try_text(String::from("current user request")).expect("valid text");
        let failed_content =
            UserContent::try_text(String::from("failed user request")).expect("valid text");
        let assistant_text = AssistantText::try_new(String::from("inherited assistant reply"))
            .expect("valid assistant text");
        let origin_contents = std::collections::HashMap::from([
            (inherited_input, inherited_content.clone()),
            (current_input, current_content.clone()),
            (failed_input, failed_content.clone()),
        ]);
        let entries = [
            (
                SemanticTranscriptEntryRef::from_source(
                    inherited_session,
                    identity(94, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::OriginAcceptedInput {
                    accepted_input: inherited_input,
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    inherited_session,
                    identity(95, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::AssistantText {
                    producing_call,
                    value: assistant_text.clone(),
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    inherited_session,
                    identity(96, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::TurnCompleted {
                    turn: identity(97, TurnId::from_uuid),
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    current_session,
                    identity(98, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::OriginAcceptedInput {
                    accepted_input: failed_input,
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    current_session,
                    identity(100, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::TurnFailed {
                    turn: identity(101, TurnId::from_uuid),
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    current_session,
                    identity(102, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::OriginAcceptedInput {
                    accepted_input: current_input,
                },
            ),
        ];

        let messages = render_frontier_messages(
            entries.iter().map(|(source, payload)| (*source, payload)),
            |accepted_input| origin_contents.get(&accepted_input).cloned(),
            [],
        )
        .expect("the admitted mixed text frontier renders");

        expect![[r#"
            [
                User {
                    source: SemanticTranscriptEntryRef {
                        source_session: SessionId(
                            00000000-0000-0000-0000-00000000005a,
                        ),
                        entry: SemanticTranscriptEntryId(
                            00000000-0000-0000-0000-00000000005e,
                        ),
                    },
                    accepted_input: AcceptedInputId(
                        00000000-0000-0000-0000-00000000005b,
                    ),
                    content: Text {
                        value: NonEmptyUnicodeText(
                            "inherited user request",
                        ),
                    },
                },
                Assistant {
                    source: SemanticTranscriptEntryRef {
                        source_session: SessionId(
                            00000000-0000-0000-0000-00000000005a,
                        ),
                        entry: SemanticTranscriptEntryId(
                            00000000-0000-0000-0000-00000000005f,
                        ),
                    },
                    producing_call: ModelCallId(
                        00000000-0000-0000-0000-00000000005d,
                    ),
                    content: AssistantText(
                        NonEmptyUnicodeText(
                            "inherited assistant reply",
                        ),
                    ),
                },
                User {
                    source: SemanticTranscriptEntryRef {
                        source_session: SessionId(
                            00000000-0000-0000-0000-000000000001,
                        ),
                        entry: SemanticTranscriptEntryId(
                            00000000-0000-0000-0000-000000000062,
                        ),
                    },
                    accepted_input: AcceptedInputId(
                        00000000-0000-0000-0000-000000000063,
                    ),
                    content: Text {
                        value: NonEmptyUnicodeText(
                            "failed user request",
                        ),
                    },
                },
                User {
                    source: SemanticTranscriptEntryRef {
                        source_session: SessionId(
                            00000000-0000-0000-0000-000000000001,
                        ),
                        entry: SemanticTranscriptEntryId(
                            00000000-0000-0000-0000-000000000066,
                        ),
                    },
                    accepted_input: AcceptedInputId(
                        00000000-0000-0000-0000-00000000005c,
                    ),
                    content: Text {
                        value: NonEmptyUnicodeText(
                            "current user request",
                        ),
                    },
                },
            ]
        "#]]
        .assert_debug_eq(&messages);
        assert_eq!(
            &messages[0],
            &ModelConversationMessage::User {
                source: entries[0].0,
                accepted_input: inherited_input,
                content: inherited_content,
            }
        );
        assert_eq!(
            &messages[1],
            &ModelConversationMessage::Assistant {
                source: entries[1].0,
                producing_call,
                content: assistant_text,
            }
        );
        assert_eq!(
            &messages[2],
            &ModelConversationMessage::User {
                source: entries[3].0,
                accepted_input: failed_input,
                content: failed_content,
            }
        );
        assert_eq!(
            &messages[3],
            &ModelConversationMessage::User {
                source: entries[5].0,
                accepted_input: current_input,
                content: current_content,
            }
        );
    }

    /// S02 / INV-015: durable request, attempt, and denial authority renders
    /// reference-only tool semantics into their exact provider-visible roles
    /// without changing source order.
    #[test]
    fn s02_inv015_frontier_rendering_resolves_exact_tool_roles_in_source_order() {
        let completed_request = model_tool_request(0);
        let denied_request = model_tool_request(1);
        let closed_request = model_tool_request(2);
        let completed_use_source = SemanticTranscriptEntryRef::from_source(
            completed_request.session(),
            identity(110, SemanticTranscriptEntryId::from_uuid),
        );
        let completed_result_source = SemanticTranscriptEntryRef::from_source(
            completed_request.session(),
            identity(111, SemanticTranscriptEntryId::from_uuid),
        );
        let denied_use_source = SemanticTranscriptEntryRef::from_source(
            denied_request.session(),
            identity(112, SemanticTranscriptEntryId::from_uuid),
        );
        let denied_result_source = SemanticTranscriptEntryRef::from_source(
            denied_request.session(),
            identity(113, SemanticTranscriptEntryId::from_uuid),
        );
        let closed_use_source = SemanticTranscriptEntryRef::from_source(
            closed_request.session(),
            identity(114, SemanticTranscriptEntryId::from_uuid),
        );
        let closed_result_source = SemanticTranscriptEntryRef::from_source(
            closed_request.session(),
            identity(115, SemanticTranscriptEntryId::from_uuid),
        );
        let completed_result = ToolResultContent::Text(
            ToolResultText::try_new(String::from(r#"{"timezone":"UTC"}"#))
                .expect("fixture result is valid"),
        );
        let attempt_id = identity(116, signalbox_domain::ToolAttemptId::from_uuid);
        let signalbox_domain::ReconstitutedToolAttempt::Ended(completed_attempt) =
            ToolAttemptReconstitutionInput::new(
                attempt_id,
                completed_request.id(),
                completed_request.session(),
                completed_request.turn(),
                identity(117, TurnAttemptId::from_uuid),
                ToolEffectClass::EffectFree,
                ToolDispatchGeneration::first(),
                ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Completed {
                    result: completed_result.clone(),
                }),
            )
            .reconstitute()
        else {
            panic!("terminal fixture reconstitutes as ended")
        };
        let denial_reason = ToolDenialReason::try_new(String::from("owner declined"))
            .expect("fixture denial reason is valid");
        let denial = ToolApprovalResolutionReconstitutionInput::new(
            denied_request.id(),
            ToolApprovalDecision::Deny {
                reason: Some(denial_reason.clone()),
            },
            ToolDecisionSource::OwnerCommand,
        )
        .reconstitute()
        .expect("owner denial provenance is implemented");
        let entries = [
            (
                completed_use_source,
                SemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call: completed_request.producing_call(),
                    request: completed_request.id(),
                },
            ),
            (
                completed_result_source,
                SemanticTranscriptEntryPayload::ToolExecutionResult {
                    attempt: attempt_id,
                },
            ),
            (
                denied_use_source,
                SemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call: denied_request.producing_call(),
                    request: denied_request.id(),
                },
            ),
            (
                denied_result_source,
                SemanticTranscriptEntryPayload::ToolDenied {
                    request: denied_request.id(),
                },
            ),
            (
                closed_use_source,
                SemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call: closed_request.producing_call(),
                    request: closed_request.id(),
                },
            ),
            (
                closed_result_source,
                SemanticTranscriptEntryPayload::ToolClosed {
                    request: closed_request.id(),
                },
            ),
        ];
        let evidence = [
            ResolvedToolConversationEntry::AssistantToolUse {
                source: completed_use_source,
                request: completed_request.clone(),
            },
            ResolvedToolConversationEntry::ExecutionResult {
                source: completed_result_source,
                request: completed_request.clone(),
                attempt: completed_attempt,
            },
            ResolvedToolConversationEntry::AssistantToolUse {
                source: denied_use_source,
                request: denied_request.clone(),
            },
            ResolvedToolConversationEntry::Denied {
                source: denied_result_source,
                request: denied_request.clone(),
                approval: denial,
            },
            ResolvedToolConversationEntry::AssistantToolUse {
                source: closed_use_source,
                request: closed_request.clone(),
            },
            ResolvedToolConversationEntry::Closed {
                source: closed_result_source,
                request: closed_request.clone(),
            },
        ];

        let messages = render_frontier_messages(
            entries.iter().map(|(source, payload)| (*source, payload)),
            |_| None,
            evidence.iter(),
        )
        .expect("exact tool evidence renders");

        assert_eq!(
            messages.as_ref(),
            [
                ModelConversationMessage::AssistantToolUse {
                    source: completed_use_source,
                    producing_call: completed_request.producing_call(),
                    request: completed_request.clone(),
                },
                ModelConversationMessage::ToolResult {
                    source: completed_result_source,
                    request: completed_request.id(),
                    content: ModelToolResultContent::Success(completed_result),
                },
                ModelConversationMessage::AssistantToolUse {
                    source: denied_use_source,
                    producing_call: denied_request.producing_call(),
                    request: denied_request.clone(),
                },
                ModelConversationMessage::ToolResult {
                    source: denied_result_source,
                    request: denied_request.id(),
                    content: ModelToolResultContent::Denied {
                        reason: Some(denial_reason),
                    },
                },
                ModelConversationMessage::AssistantToolUse {
                    source: closed_use_source,
                    producing_call: closed_request.producing_call(),
                    request: closed_request.clone(),
                },
                ModelConversationMessage::ToolResult {
                    source: closed_result_source,
                    request: closed_request.id(),
                    content: ModelToolResultContent::ClosedByTurnEnd,
                },
            ]
        );
    }

    /// S02 / INV-015: a terminal attempt from another turn cannot supply
    /// authority for a tool-result semantic entry.
    #[test]
    fn s02_inv015_frontier_rendering_rejects_cross_turn_tool_result_evidence() {
        let request = model_tool_request(0);
        let source = SemanticTranscriptEntryRef::from_source(
            request.session(),
            identity(120, SemanticTranscriptEntryId::from_uuid),
        );
        let attempt_id = identity(121, signalbox_domain::ToolAttemptId::from_uuid);
        let signalbox_domain::ReconstitutedToolAttempt::Ended(cross_turn_attempt) =
            ToolAttemptReconstitutionInput::new(
                attempt_id,
                request.id(),
                request.session(),
                identity(122, TurnId::from_uuid),
                identity(123, TurnAttemptId::from_uuid),
                ToolEffectClass::EffectFree,
                ToolDispatchGeneration::first(),
                ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(String::from("cross-wired"))
                            .expect("fixture result is valid"),
                    ),
                }),
            )
            .reconstitute()
        else {
            panic!("terminal fixture reconstitutes as ended")
        };
        let payload = SemanticTranscriptEntryPayload::ToolExecutionResult {
            attempt: attempt_id,
        };
        let evidence = ResolvedToolConversationEntry::ExecutionResult {
            source,
            request,
            attempt: cross_turn_attempt,
        };

        let error = render_frontier_messages([(source, &payload)], |_| None, [&evidence])
            .expect_err("cross-turn tool evidence must fail closed");

        assert_eq!(
            error,
            ModelFrontierRenderingError::MissingOrMismatchedToolEvidence { entry: source }
        );
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

    /// INV-037: durable cancellation during capability preparation is
    /// authoritative no-work, not a local capability failure to terminalize.
    #[tokio::test]
    async fn inv037_capability_preparation_cancellation_stops_without_failure_commit() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityCancelled]),
            InProcessAttemptDispatchGate::default(),
        );

        assert_eq!(
            service
                .execute(session)
                .await
                .expect("durable cancellation is authoritative"),
            ModelCallExecutionOutcome::NoWork
        );
        let (_, prepare, _, _, _, provider, ..) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(provider.capability_preparation_count(), 1);
    }

    /// docs/spec/model-call-execution.md: a trustworthy capability failure
    /// survives a failed guarded closure and explicit service decomposition,
    /// then resubmits without repeating capability preparation.
    #[tokio::test]
    async fn capability_failure_commit_retains_evidence_across_handoff() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let call = request.call().id();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            FakeFailure {
                errors: [FakeError::Infrastructure, FakeError::Infrastructure].into(),
                rereads: [Ok(RetainedCapabilityFailureStatus::Pending)].into(),
                calls: 0,
                reread_calls: 0,
            },
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
            InProcessAttemptDispatchGate::default(),
        );

        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::CapabilityFailureCommit(
                FakeError::Infrastructure
            ))
        ));
        assert!(matches!(
            service.retained_state(),
            Some(RetainedModelCallExecutionState {
                state: RetainedModelCallExecutionStateKind::CapabilityKnownFailure {
                    session: retained_session,
                    call: retained_call,
                },
            }) if *retained_session == session && *retained_call == call
        ));

        let (ids, prepare, failure, authorization, observation, provider, gate, catalog, retained) =
            service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(failure.calls, 1);
        assert_eq!(failure.reread_calls, 0);
        assert_eq!(provider.capability_preparation_count(), 1);
        let mut resumed = ModelCallExecutionService::from_parts(
            ids,
            prepare,
            failure,
            authorization,
            observation,
            provider,
            gate,
            catalog,
            retained,
        );
        assert!(matches!(
            resumed.execute(identity(99, SessionId::from_uuid)).await,
            Err(ModelCallExecutionError::CapabilityFailureCommit(
                FakeError::Infrastructure
            ))
        ));
        let (_, prepare, failure, _, _, provider, _, _, retained) = resumed.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(failure.calls, 2);
        assert_eq!(failure.reread_calls, 1);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert!(matches!(
            retained,
            Some(RetainedModelCallExecutionState {
                state: RetainedModelCallExecutionStateKind::CapabilityKnownFailure {
                    session: retained_session,
                    call: retained_call,
                },
            }) if retained_session == session && retained_call == call
        ));
    }

    /// INV-037: if an interrupt wins after capability preparation reported a
    /// known failure, the retained reread accepts the durable cancellation as
    /// authoritative no-work rather than retrying failure closure forever.
    #[tokio::test]
    async fn inv037_capability_failure_race_rereads_cancellation_as_no_work() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            FakeFailure {
                errors: [FakeError::Infrastructure].into(),
                rereads: [Ok(RetainedCapabilityFailureStatus::Cancelled)].into(),
                calls: 0,
                reread_calls: 0,
            },
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
            InProcessAttemptDispatchGate::default(),
        );

        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::CapabilityFailureCommit(
                FakeError::Infrastructure
            ))
        ));
        assert_eq!(
            service
                .execute(identity(99, SessionId::from_uuid))
                .await
                .expect("the cancellation reread is authoritative"),
            ModelCallExecutionOutcome::NoWork
        );
        let (_, prepare, failure, _, _, provider, _, _, retained) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(failure.calls, 1);
        assert_eq!(failure.reread_calls, 1);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert!(retained.is_none());
    }

    /// docs/spec/model-call-execution.md: a commit-ambiguous
    /// capability-failure closure is reread before any resubmission, and a
    /// landed closure ends reconciliation without repeating credential
    /// preparation or the guarded transaction.
    #[tokio::test]
    async fn ambiguous_capability_failure_commit_is_reread_before_resubmission() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let call = request.call().id();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            FakeFailure {
                errors: [FakeError::CommitAmbiguous].into(),
                rereads: [Ok(RetainedCapabilityFailureStatus::AlreadyCommitted)].into(),
                calls: 0,
                reread_calls: 0,
            },
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
            InProcessAttemptDispatchGate::default(),
        );

        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::CapabilityFailureCommit(
                FakeError::CommitAmbiguous
            ))
        ));
        assert_eq!(
            service
                .execute(identity(99, SessionId::from_uuid))
                .await
                .expect("the authoritative reread proves the closure landed"),
            ModelCallExecutionOutcome::CapabilityFailureAlreadyCommitted(call)
        );
        let (_, prepare, failure, _, _, provider, _, _, retained) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(failure.calls, 1);
        assert_eq!(failure.reread_calls, 1);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert!(retained.is_none());
    }

    /// docs/spec/model-call-execution.md: send authorization has no fresh
    /// candidate to replace after an identity-collision classification, so
    /// the same session/call pair is not retried in place.
    #[tokio::test]
    async fn authorization_identity_collision_returns_without_retrying_same_call() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Err(FakeError::IdentityCollision)].into(),
                rereads: VecDeque::new(),
                calls: 0,
                reread_calls: 0,
            },
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
                ModelCallTerminalObservation::KnownFailed,
            )]),
            InProcessAttemptDispatchGate::default(),
        );

        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::Authorization(
                FakeError::IdentityCollision
            ))
        ));
        let (_, prepare, _, authorization, _, provider, _, _, retained) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(authorization.calls, 1);
        assert_eq!(authorization.reread_calls, 0);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 0);
        assert!(retained.is_none());
    }

    /// docs/spec/model-call-execution.md: stale or stopped authority is an
    /// ordinary no-send result, not a caller/hub defect and never provider
    /// entry.
    #[tokio::test]
    async fn stale_authorization_returns_no_work_without_provider_entry() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            NoSendAuthorization { calls: 0 },
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
                ModelCallTerminalObservation::KnownFailed,
            )]),
            InProcessAttemptDispatchGate::default(),
        );

        assert_eq!(
            service
                .execute(session)
                .await
                .expect("stale authority is a normal no-send result"),
            ModelCallExecutionOutcome::NoWork
        );
        let (_, _, _, authorization, _, provider, _, _, retained) = service.into_parts();
        assert_eq!(authorization.calls, 1);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 0);
        assert!(retained.is_none());
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
                outcomes: [Ok(ready(request))].into(),
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
        let (_, prepare, _, authorization, _, provider, _, _, _) = service.into_parts();
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
                outcomes: [Ok(ready(request))].into(),
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
        let (_, _, _, authorization, observation, provider, _, _, _) = service.into_parts();
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
                outcomes: [Ok(ready(request))].into(),
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
        let (_, _, _, authorization, observation, provider, _, _, _) = service.into_parts();
        assert_eq!(authorization.calls, 1);
        assert_eq!(authorization.reread_calls, 1);
        assert_eq!(observation.commit_calls, 1);
        assert_eq!(observation.observed, vec![retained]);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 0);
    }

    /// INV-014 / INV-037: an ambiguous authorization reread accepts a complete
    /// concurrent direct cancellation of the exact unsent call as
    /// authoritative no-work without entering the provider.
    #[tokio::test]
    async fn inv014_inv037_ambiguous_authorization_accepts_terminal_cancellation() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Err(FakeError::CommitAmbiguous)].into(),
                rereads: [Ok(ModelCallAuthorizationReread::Cancelled)].into(),
                calls: 0,
                reread_calls: 0,
            },
            FakeObservation {
                commit_errors: VecDeque::new(),
                rereads: VecDeque::new(),
                observed: Vec::new(),
                commit_calls: 0,
                reread_calls: 0,
            },
            ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
                ModelCallTerminalObservation::KnownFailed,
            )]),
            InProcessAttemptDispatchGate::default(),
        );

        assert_eq!(
            service
                .execute(session)
                .await
                .expect("the complete terminal cancellation is authoritative"),
            ModelCallExecutionOutcome::NoWork
        );
        let (_, _, _, authorization, observation, provider, _, _, retained) = service.into_parts();
        assert_eq!(authorization.calls, 1);
        assert_eq!(authorization.reread_calls, 1);
        assert_eq!(observation.commit_calls, 0);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 0);
        assert!(retained.is_none());
    }

    /// docs/spec/model-call-execution.md: a failed ambiguous-authorization
    /// reread retains the exact non-consumption proof across handoff and
    /// later classifies a committed `InFlight` authorization without invoking
    /// the provider.
    #[tokio::test]
    async fn ambiguous_authorization_reread_retains_non_consumption_across_handoff() {
        let (request, authorized) = prepared_fixture();
        let session = request.session();
        let call = request.call().id();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request.clone()))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Err(FakeError::CommitAmbiguous)].into(),
                rereads: [
                    Err(FakeError::Infrastructure),
                    Ok(ModelCallAuthorizationReread::InFlight(Box::new(authorized))),
                ]
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

        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::AuthorizationReread {
                authorization_error: FakeError::CommitAmbiguous,
                reread_error: FakeError::Infrastructure,
            })
        ));
        assert!(matches!(
            service.retained_state(),
            Some(RetainedModelCallExecutionState {
                state: RetainedModelCallExecutionStateKind::AuthorizationNonConsumption {
                    session: retained_session,
                    prepared,
                },
            }) if *retained_session == session && **prepared == request
        ));

        let (ids, prepare, failure, authorization, observation, provider, gate, catalog, retained) =
            service.into_parts();
        let mut resumed = ModelCallExecutionService::from_parts(
            ids,
            prepare,
            failure,
            authorization,
            observation,
            provider,
            gate,
            catalog,
            retained,
        );
        let error = resumed
            .execute(identity(99, SessionId::from_uuid))
            .await
            .expect_err("the retained known-failure observation commit is visible");
        let retained_observation = match error {
            ModelCallExecutionError::ObservationCommit {
                error: FakeError::Infrastructure,
                retained_observation,
            } => retained_observation,
            error => panic!("unexpected reconciliation error: {error}"),
        };
        assert_eq!(retained_observation.call(), call);
        assert_eq!(
            retained_observation.observation(),
            &ModelCallTerminalObservation::KnownFailed
        );
        let (_, prepare, _, authorization, observation, provider, _, _, retained) =
            resumed.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(authorization.calls, 1);
        assert_eq!(authorization.reread_calls, 2);
        assert_eq!(observation.commit_calls, 1);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 0);
        assert!(matches!(
            retained,
            Some(RetainedModelCallExecutionState {
                state: RetainedModelCallExecutionStateKind::TerminalObservation {
                    observation,
                    ..
                },
            }) if observation == retained_observation
        ));
    }

    /// INV-014: when an ambiguous authorization is proven to have rolled
    /// back to Prepared, the unconsumed scripted interaction action can
    /// prepare again and still produces exactly one physical interaction.
    #[tokio::test]
    async fn s02_inv014_authorization_rollback_reprepares_one_scripted_interaction_action() {
        let (request, authorized) = prepared_fixture();
        let session = request.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request.clone())), Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Err(FakeError::CommitAmbiguous), Ok(authorized)].into(),
                rereads: [
                    Err(FakeError::Infrastructure),
                    Ok(ModelCallAuthorizationReread::Prepared),
                ]
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
                ModelCallTerminalObservation::KnownFailed,
            )]),
            InProcessAttemptDispatchGate::default(),
        );

        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::AuthorizationReread {
                authorization_error: FakeError::CommitAmbiguous,
                reread_error: FakeError::Infrastructure,
            })
        ));
        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::ObservationCommit {
                error: FakeError::Infrastructure,
                ..
            })
        ));

        let (_, prepare, _, authorization, observation, provider, _, _, _) = service.into_parts();
        assert_eq!(prepare.calls, 2);
        assert_eq!(authorization.calls, 2);
        assert_eq!(authorization.reread_calls, 2);
        assert_eq!(observation.commit_calls, 1);
        assert_eq!(provider.capability_preparation_count(), 2);
        assert_eq!(provider.interaction_count(), 1);
        assert_eq!(provider.remaining_step_count(), 0);
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
                outcomes: [Ok(ready(request))].into(),
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
        let (_, _, _, _, _, provider, _, _, _) = service.into_parts();
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
