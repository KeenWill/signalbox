use super::session::ProcessRunnerProjection;
use crate::outbox::{
    DispatchedDelegationOutcome, DispatchedDelegationProvenance, DispatchedDelegationReason,
    DispatchedDelegationWaitMode,
};
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, DelegationMessageId, DirectModelSelection,
    ImportedConversationId, ImportedTranscriptEntryId, ModelCallId, ResolvedProviderTarget,
    RunnerGeneration, RunnerId, SemanticTranscriptEntryId, SessionId, ToolApprovalDecider,
    ToolApprovalDecision, ToolAttemptId, ToolDecisionRationale, ToolRequestId, TurnAttemptId,
    TurnId, TurnModelSettingsResolved, UserContent,
};

/// Durable state of the current model call attached to an active turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessCurrentModelCallState {
    /// Provider work has not been authorized.
    Prepared,
    /// Provider work was authorized and may have happened.
    InFlight,
    /// Cancellation was durably requested for issued provider work.
    CancellationRequested,
}

/// Current model call attached to the active turn attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessCurrentModelCall {
    pub(super) call: ModelCallId,
    pub(super) state: ProcessCurrentModelCallState,
}

impl ProcessCurrentModelCall {
    /// Returns the current model-call identity.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the exact durable call state.
    pub const fn state(&self) -> ProcessCurrentModelCallState {
        self.state
    }
}

/// Terminal model-call dispositions admitted by a failed turn projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessFailedModelCallDisposition {
    /// The provider interaction definitively failed.
    KnownFailed,
    /// The provider call was cancelled without terminalizing the turn as
    /// cancelled.
    Cancelled,
}

/// Persistence-owned closed classification of a definitive provider error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessProviderModelCallFailureCause {
    /// The provider rejected the request credential.
    CredentialRejected,
    /// The credential lacked permission.
    PermissionDenied,
    /// The provider judged the request invalid.
    InvalidRequest,
    /// The requested model or resource was not found.
    TargetNotFound,
    /// The request exceeded a provider size limit.
    RequestTooLarge,
    /// The provider applied a transient rate limit.
    RateLimited,
    /// The account's available quota was exhausted.
    QuotaExhausted,
    /// The provider reported overload.
    Overloaded,
    /// The provider reported an internal error.
    ProviderInternal,
    /// The adapter did not recognize the definitive provider error.
    Unrecognized,
}

/// Persistence-owned closed classification of an unsent attachment-preparation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessAttachmentPreparationFailureCause {
    /// Distinct rendered attachment bytes exceeded the deployment ceiling.
    TooLarge,
    /// No recorded replica contained the required attachment.
    Missing,
    /// Recorded replicas failed length or digest verification.
    Corrupt,
}

/// Optional terminal model-call evidence for a failed turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessFailedTerminalModelCall {
    pub(super) call: ModelCallId,
    pub(super) disposition: ProcessFailedModelCallDisposition,
    pub(super) provider_failure_cause: Option<ProcessProviderModelCallFailureCause>,
    pub(super) attachment_preparation_failure_cause:
        Option<ProcessAttachmentPreparationFailureCause>,
}

impl ProcessFailedTerminalModelCall {
    /// Returns the terminal model-call identity.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the exact terminal model-call disposition.
    pub const fn disposition(&self) -> ProcessFailedModelCallDisposition {
        self.disposition
    }

    /// Returns the closed provider classification when this call retained one.
    pub const fn provider_failure_cause(&self) -> Option<ProcessProviderModelCallFailureCause> {
        self.provider_failure_cause
    }

    /// Returns the closed local attachment-preparation cause when retained.
    pub const fn attachment_preparation_failure_cause(
        &self,
    ) -> Option<ProcessAttachmentPreparationFailureCause> {
        self.attachment_preparation_failure_cause
    }
}

/// Whether a session can owe a user reconciliation decision right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessModelCallRecoveryPrecondition {
    /// No such session exists in this snapshot.
    SessionAbsent,
    /// The session exists but no active turn is parked on a model call.
    NoParkedTurn,
    /// The session's active turn is parked on this exact ambiguous call.
    Parked {
        /// The active turn holding the slot until reconciliation.
        turn: TurnId,
    },
}

/// Authoritative lifecycle state for one projected turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessTurnState {
    /// Accepted work has not activated.
    Queued {
        /// Accepted input that created the queued turn.
        accepted_input: AcceptedInputId,
        /// Exact accepted ordered user content.
        content: UserContent,
    },
    /// Delegated work has not activated.
    QueuedDelegated {
        /// Tool request that spawned the delegated session.
        spawning_request: ToolRequestId,
        /// Parent session that issued the spawn request.
        parent_session: SessionId,
        /// Parent turn that issued the spawn request.
        parent_turn: TurnId,
        /// Exact delegated task text.
        content: String,
    },
    /// A contiguous range of delivered delegation content is waiting to wake
    /// an otherwise idle recipient.
    QueuedDelegationWake {
        /// First recipient-wide delivery sequence included by the wake.
        first_delivery_sequence: u64,
        /// Last recipient-wide delivery sequence included by the wake.
        through_delivery_sequence: u64,
    },
    /// Parent policy logically terminalized the delegated root while any
    /// retained physical execution evidence remains inert.
    DelegationTerminated {
        /// Tool request that spawned the terminalized child.
        spawning_request: ToolRequestId,
        /// Typed stopped or cancelled outcome.
        outcome: DispatchedDelegationOutcome,
        /// Exact parent terminal reason.
        reason: DispatchedDelegationReason,
        /// Exact parent-command provenance.
        provenance: DispatchedDelegationProvenance,
    },
    /// The current attempt is running.
    ActiveRunning {
        /// Current live attempt.
        current_attempt: TurnAttemptId,
        /// Current provider call, when one has been prepared or authorized.
        current_model_call: Option<ProcessCurrentModelCall>,
    },
    /// The ended attempt is parked on an ambiguous model call.
    ActiveAwaitingModelCallRecovery {
        /// Ended attempt whose call is ambiguous.
        ended_attempt: TurnAttemptId,
        /// Ambiguous call awaiting recovery.
        recovery_call: ModelCallId,
        /// Durable automatic attempts already claimed.
        automatic_reconciliation_attempts: u32,
        /// True only after the automatic attempt budget is exhausted.
        operator_action_required: bool,
    },
    /// The yielded tool batch is parked on a user decision.
    ActiveAwaitingToolApproval {
        /// Earliest undecided tool request.
        request: ToolRequestId,
    },
    /// The yielded foreground await is parked on one exact delegated child.
    ActiveAwaitingChild {
        /// Tool request that issued the foreground await.
        awaiting_request: ToolRequestId,
        /// Spawn request naming the relationship.
        spawning_request: ToolRequestId,
        /// Exact child whose terminal result releases this turn.
        child: SessionId,
    },
    /// The yielded tool batch is parked on an ambiguous external effect.
    ActiveAwaitingToolRecovery {
        /// Ended turn attempt that issued the tool effect.
        ended_attempt: TurnAttemptId,
        /// Ambiguous tool attempt awaiting recovery.
        recovery_attempt: ToolAttemptId,
        /// Durable automatic attempts already claimed.
        automatic_reconciliation_attempts: u32,
        /// True only after the automatic attempt budget is exhausted.
        operator_action_required: bool,
    },
    /// The turn is parked on replacement of one exact lost runner placement.
    ActiveAwaitingRunnerRecovery {
        /// Runner whose durable loss owns this wait.
        runner: RunnerId,
        /// Positive placement revision against which loss was projected.
        placement_revision: RunnerGeneration,
        /// Physical tool attempt interrupted by loss, when one exists.
        interrupted_tool_attempt: Option<ToolAttemptId>,
    },
    /// The turn terminalized as failed.
    Failed {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Terminal physical attempt, absent only for an evidence-free
        /// recovery failure.
        terminal_attempt: Option<TurnAttemptId>,
        /// Terminal call evidence, absent when no call existed.
        terminal_model_call: Option<ProcessFailedTerminalModelCall>,
    },
    /// The turn terminalized as completed.
    Completed {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Outcome-authoritative attempt.
        terminal_attempt: TurnAttemptId,
        /// Outcome-authoritative model call.
        terminal_call: ModelCallId,
    },
    /// The turn terminalized as refused.
    Refused {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Outcome-authoritative attempt.
        terminal_attempt: TurnAttemptId,
        /// Outcome-authoritative model call.
        terminal_call: ModelCallId,
    },
    /// The turn terminalized after confirmed cancellation.
    Cancelled {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Outcome-authoritative attempt.
        terminal_attempt: TurnAttemptId,
        /// Terminal call, absent when cancellation preceded preparation.
        terminal_call: Option<ModelCallId>,
    },
    /// The turn terminalized requiring external reconciliation.
    ReconciliationRequired {
        /// Exact terminal semantic frontier.
        terminal_frontier: ContextFrontierId,
        /// Outcome-authoritative attempt.
        terminal_attempt: TurnAttemptId,
        /// Exact ambiguous terminal operation.
        operation: ProcessReconciliationOperation,
    },
}

/// Exact ambiguous operation exposed by a process transcript projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessReconciliationOperation {
    /// Ambiguous provider call.
    ModelCall(ModelCallId),
    /// Ambiguous tool attempt.
    ToolAttempt(ToolAttemptId),
}

#[derive(signalbox_derive::Accessors)]
/// One turn in acceptance order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessTranscriptTurn {
    pub(super) turn: TurnId,
    pub(super) acceptance_position: u64,
    /// Returns the authoritative lifecycle state.
    #[get]
    pub(super) state: ProcessTurnState,
    pub(super) model_settings: Option<TurnModelSettingsResolved>,
}

#[derive(signalbox_derive::Accessors)]
/// Exact token fields for one terminal model call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessModelCallTokenUsage {
    /// Returns the input-token count when present.
    #[get(copy)]
    pub(super) input_tokens: Option<u64>,
    /// Returns the output-token count when present.
    #[get(copy)]
    pub(super) output_tokens: Option<u64>,
    /// Returns the cache-creation input-token count when present.
    #[get(copy)]
    pub(super) cache_creation_input_tokens: Option<u64>,
    /// Returns the cache-read input-token count when present.
    #[get(copy)]
    pub(super) cache_read_input_tokens: Option<u64>,
}

/// Closed provenance of one terminal model call's usage fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessModelCallUsageProvenance {
    /// Counts reported by the provider or adapter stream.
    Reported,
    /// Counts produced by an explicit estimator.
    Estimated,
}

impl ProcessModelCallUsageProvenance {
    pub(super) fn from_storage(value: &str) -> Option<Self> {
        match value {
            "reported" => Some(Self::Reported),
            "estimated" => Some(Self::Estimated),
            _ => None,
        }
    }
}

/// Closed meaning of a provider-reported model-call input-token count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessModelCallInputTokenSemantics {
    /// The input count excludes the separately reported cache axes.
    CacheExclusive,
    /// The input count includes the separately reported cache axes.
    CacheInclusive,
}

impl ProcessModelCallInputTokenSemantics {
    pub(super) const fn from_storage(value: Option<bool>) -> Option<Self> {
        match value {
            Some(true) => Some(Self::CacheInclusive),
            Some(false) => Some(Self::CacheExclusive),
            None => None,
        }
    }
}

#[derive(signalbox_derive::Accessors)]
/// One terminal model call's typed token evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessTranscriptModelCallUsage {
    pub(super) turn: TurnId,
    pub(super) call: ModelCallId,
    pub(super) target: ResolvedProviderTarget,
    /// Returns the event-sourced credential profile pinned into this call.
    #[get(str)]
    pub(super) credential_profile: String,
    pub(super) input_token_semantics: Option<ProcessModelCallInputTokenSemantics>,
    pub(super) provenance: ProcessModelCallUsageProvenance,
    pub(super) usage: ProcessModelCallTokenUsage,
}

impl ProcessTranscriptModelCallUsage {
    /// Returns the owning turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the terminal model-call identity.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the immutable provider target whose configured rates apply.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }

    /// Returns the pinned meaning of this call's reported input-token count.
    ///
    /// Absence identifies a call prepared before that semantic pin existed.
    pub const fn input_token_semantics(&self) -> Option<ProcessModelCallInputTokenSemantics> {
        self.input_token_semantics
    }

    /// Returns the closed provenance of this call's token fields.
    pub const fn provenance(&self) -> ProcessModelCallUsageProvenance {
        self.provenance
    }

    /// Returns the exact independently optional provider fields.
    pub const fn usage(&self) -> ProcessModelCallTokenUsage {
        self.usage
    }
}

impl ProcessTranscriptTurn {
    /// Returns the immutable turn identity.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the immutable positive acceptance position.
    pub const fn acceptance_position(&self) -> u64 {
        self.acceptance_position
    }

    /// Returns complete frozen settings evidence when the turn was committed
    /// after settings persistence became available.
    pub const fn model_settings(&self) -> Option<&TurnModelSettingsResolved> {
        self.model_settings.as_ref()
    }
}

/// Session ancestry relevant to process-protocol compatibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessSessionAncestry {
    /// User-initiated native session.
    UserInitiated,
    /// Session seeded from one immutable imported frontier.
    ImportedConversation,
}

/// Exact source-speaker attestation in the conservative process projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessImportedSourceSpeaker {
    /// The source omitted the speaker field.
    NotAttested,
    /// The source explicitly supplied no speaker.
    AttestedAbsent,
    /// The source attested user authorship.
    User,
    /// The source attested assistant authorship.
    Assistant,
}

/// Conservative imported content kind exposed by the process read boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessImportedContentKind {
    /// One source event.
    SourceEvent,
    /// One source-defined message block.
    SourceMessageBlock,
    /// Text whose value is unattested or explicitly absent.
    Text,
    /// One tool call.
    ToolCall,
    /// One tool result.
    ToolResult,
    /// One thinking block.
    Thinking,
    /// One redacted-thinking block.
    RedactedThinking,
    /// One document block.
    Document,
    /// One typed message-content absence.
    MessageContentAbsent,
}

/// Typed outcome of an executed tool-result transcript entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessToolExecutionResultDisposition {
    /// The executor returned admitted result content.
    Completed,
    /// The executor returned definitive typed failure evidence.
    KnownFailed,
}

/// One ordered member of the latest authoritative semantic frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessTranscriptEntry {
    /// Reference-only successor placement boundary.
    RunnerPlacementChanged {
        /// Zero-based frontier position.
        entry_index: u64,
        /// Session owning the placement record.
        source_session: SessionId,
        /// Semantic boundary identity.
        entry: SemanticTranscriptEntryId,
        /// Exact successor placement revision.
        placement_revision: signalbox_domain::RunnerGeneration,
    },
    /// Exact delegated task that opened one child session.
    DelegatedTask {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Tool request that spawned the child.
        spawning_request: ToolRequestId,
        /// Parent session that issued the spawn request.
        parent_session: SessionId,
        /// Parent turn that issued the spawn request.
        parent_turn: TurnId,
        /// Exact delegated task text.
        content: String,
    },
    /// Exact bidirectional delegation message delivered to this frontier.
    DelegationMessage {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Relationship identity.
        spawning_request: ToolRequestId,
        /// Immutable message identity.
        message: DelegationMessageId,
        /// Sending session.
        sender: SessionId,
        /// Receiving session.
        recipient: SessionId,
        /// Relationship-local message ordinal.
        ordinal: u64,
        /// Recipient-wide delivery sequence.
        delivery_sequence: u64,
        /// Exact delivered content.
        content: String,
    },
    /// Exact child result delivered through one registered wait.
    DelegationResult {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Await request receiving this result.
        awaiting_request: ToolRequestId,
        /// Relationship identity.
        spawning_request: ToolRequestId,
        /// Terminal child session.
        child: SessionId,
        /// Foreground or background delivery mode.
        mode: DispatchedDelegationWaitMode,
        /// Recipient-wide position for background delivery only.
        delivery_sequence: Option<u64>,
        /// Typed terminal result outcome.
        outcome: DispatchedDelegationOutcome,
        /// Delivered content for a successful result only.
        content: Option<String>,
        /// Typed lifecycle reason.
        reason: DispatchedDelegationReason,
        /// Exact child-turn or parent-command proof.
        provenance: DispatchedDelegationProvenance,
    },
    /// Injected boundary declaring the model identity newly in force.
    ModelIdentityChanged {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Turn whose start first observes the identity.
        turn: TurnId,
        /// Immutable defaults epoch bound by that turn.
        defaults_version: u64,
        /// Exact direct model identity frozen for that turn.
        selected: DirectModelSelection,
    },
    /// Model-produced summary of one exact earlier semantic range.
    ContextSummary {
        /// Zero-based position in the complete frontier.
        entry_index: u64,
        /// Session that owns the immutable summary entry.
        source_session: SessionId,
        /// Semantic summary-entry identity.
        entry: SemanticTranscriptEntryId,
        /// Dedicated producing model call.
        model_call: ModelCallId,
        /// Inclusive summarized-range first entry.
        first: signalbox_domain::SemanticTranscriptEntryRef,
        /// Inclusive summarized-range final entry.
        through: signalbox_domain::SemanticTranscriptEntryRef,
        /// Exact model-produced summary text.
        content: String,
    },
    /// Exact accepted user input.
    User {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Accepted-input identity.
        accepted_input: AcceptedInputId,
        /// Origin turn.
        turn: TurnId,
        /// Exact admitted ordered user content.
        content: UserContent,
    },
    /// Exact committed assistant text.
    Assistant {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Owning turn.
        turn: TurnId,
        /// Producing model call.
        model_call: ModelCallId,
        /// Exact committed assistant text.
        content: String,
    },
    /// Opaque provider compaction retained without exposing replay bytes.
    ProviderCompaction {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Owning turn.
        turn: TurnId,
        /// Producing model call.
        model_call: ModelCallId,
    },
    /// Opaque provider reasoning retained without exposing replay bytes.
    ProviderReasoning {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Owning turn.
        turn: TurnId,
        /// Producing model call.
        model_call: ModelCallId,
    },
    /// Assistant tool proposal.
    AssistantToolUse {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Owning turn.
        turn: TurnId,
        /// Producing model call.
        model_call: ModelCallId,
        /// Exact logical tool request.
        request: ToolRequestId,
        /// Exact stored tool name.
        name: String,
        /// Exact stored normalized or scrubbed undecodable arguments.
        arguments: String,
        /// Explicit decision provenance, absent while pending and for automatic policy.
        approval: Option<ProcessToolApproval>,
    },
    /// Executed tool result reference.
    ToolExecutionResult {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Exact logical tool request.
        request: ToolRequestId,
        /// Exact physical tool attempt.
        attempt: ToolAttemptId,
        /// Typed terminal outcome of the exact physical attempt.
        disposition: ProcessToolExecutionResultDisposition,
        /// Exact provider-visible result content.
        content: String,
    },
    /// User or policy denied one tool request.
    ToolDenied {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Exact denied request.
        request: ToolRequestId,
        /// Exact provider-visible denial content.
        content: String,
        /// Whether this denial already has its one permitted user override.
        override_recorded: bool,
    },
    /// The turn ended before one tool request resolved ordinarily.
    ToolInadmissible {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Exact inadmissible request.
        request: ToolRequestId,
        /// Exact provider-visible inadmissibility content.
        content: String,
    },
    /// The request closed when its turn ended.
    ToolClosed {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Exact closed request.
        request: ToolRequestId,
        /// Exact provider-visible terminal-closure content.
        content: String,
        /// Whether an approval was recorded before the request closed.
        approved_before_close: bool,
    },
    /// Explicit failed-turn marker.
    TurnFailed {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Failed turn.
        turn: TurnId,
    },
    /// Explicit completed-turn marker.
    TurnCompleted {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Completed turn.
        turn: TurnId,
    },
    /// Explicit cancelled-turn marker.
    TurnCancelled {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the immutable semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Cancelled turn.
        turn: TurnId,
    },
    /// Imported text whose value was explicitly source-attested.
    ImportedText {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the projected semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Owning imported conversation.
        imported_conversation: ImportedConversationId,
        /// Exact imported entry identity.
        imported_entry: ImportedTranscriptEntryId,
        /// Exact source-speaker attestation.
        source_speaker: ProcessImportedSourceSpeaker,
        /// Exact source-attested text.
        content: String,
    },
    /// Conservative imported entry without rendered text.
    Imported {
        /// Zero-based position in the projected frontier.
        entry_index: u64,
        /// Session that owns the projected semantic entry.
        source_session: SessionId,
        /// Semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Owning imported conversation.
        imported_conversation: ImportedConversationId,
        /// Exact imported entry identity.
        imported_entry: ImportedTranscriptEntryId,
        /// Exact source-speaker attestation.
        source_speaker: ProcessImportedSourceSpeaker,
        /// Conservative normalized content kind.
        content_kind: ProcessImportedContentKind,
    },
}

#[derive(signalbox_derive::Accessors)]
/// One explicit approval decision projected with an assistant tool proposal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessToolApproval {
    /// Borrows the exact recorded decision.
    #[get]
    pub(super) decision: ToolApprovalDecision,
    pub(super) decider: ToolApprovalDecider,
    pub(super) rationale: Option<ToolDecisionRationale>,
}

impl ProcessToolApproval {
    /// Returns the exact user or delegate provenance.
    pub const fn decider(&self) -> ToolApprovalDecider {
        self.decider
    }

    /// Borrows the delegate rationale, absent for a user decision.
    pub const fn rationale(&self) -> Option<&ToolDecisionRationale> {
        self.rationale.as_ref()
    }
}

#[derive(signalbox_derive::Accessors)]
/// One complete transcript and cursor observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessTranscriptSnapshot {
    pub(super) session: SessionId,
    pub(super) cursor: u64,
    pub(super) runner: Option<ProcessRunnerProjection>,
    /// Borrows turns in immutable acceptance order.
    #[get(slice)]
    pub(super) turns: Vec<ProcessTranscriptTurn>,
    /// Borrows terminal model-call usage in turn and call identity order.
    #[get(slice)]
    pub(super) model_call_usage: Vec<ProcessTranscriptModelCallUsage>,
    /// Borrows the latest semantic frontier in member order.
    #[get(slice)]
    pub(super) entries: Vec<ProcessTranscriptEntry>,
}

impl ProcessTranscriptSnapshot {
    /// Returns the selected session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the global last committed outbox sequence from this snapshot.
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Borrows the current runner placement, absent for a daemon-only session.
    pub const fn runner(&self) -> Option<&ProcessRunnerProjection> {
        self.runner.as_ref()
    }
}

/// One bounded-memory item yielded from a repeatable-read transcript snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessTranscriptItem {
    /// One turn in acceptance order.
    Turn(ProcessTranscriptTurn),
    /// One terminal model call's typed token evidence.
    ModelCallUsage(ProcessTranscriptModelCallUsage),
    /// One semantic entry in frontier order.
    Entry(ProcessTranscriptEntry),
}

/// Counts and cursor observed after a transcript reader reaches its committed
/// end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessTranscriptSummary {
    pub(super) session: SessionId,
    pub(super) cursor: u64,
    pub(super) turn_count: u64,
    pub(super) model_call_count: u64,
    pub(super) entry_count: u64,
}

impl ProcessTranscriptSummary {
    /// Returns the selected session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the global outbox cursor from the repeatable-read snapshot.
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Returns the exact number of yielded turns.
    pub const fn turn_count(&self) -> u64 {
        self.turn_count
    }

    /// Returns the exact number of yielded terminal model calls.
    pub const fn model_call_count(&self) -> u64 {
        self.model_call_count
    }

    /// Returns the exact number of yielded semantic entries.
    pub const fn entry_count(&self) -> u64 {
        self.entry_count
    }
}
