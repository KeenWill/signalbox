//! Closed versioned JSON-lines process protocol.
//!
//! This crate owns wire representations and frame validation only. Domain,
//! persistence, and client presentation values remain distinct mappings
//! (docs/spec/process-protocol.md).

mod delegation;
mod goal;
mod operator_status;
mod review;
mod runner;
mod scalars;
mod session;
mod settings;
mod shared_validation;
mod transcript;
mod user_input;

pub use delegation::*;
pub use goal::*;
pub use operator_status::*;
pub use review::*;
pub use runner::*;
pub use scalars::*;
pub use session::*;
pub use settings::*;
pub use transcript::*;
pub use user_input::*;

use crate::shared_validation::{
    validate_review_finding_event, validate_review_judgment_disposition, validate_review_key,
    validate_review_orchestration_snapshot, validate_session_template_name,
    validate_tool_approval_event_shape,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;

/// Closed versioned request family.

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientRequest {
    /// Create a user-initiated session.
    CreateSession {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Initial session model-selection defaults.
        initial_model_selection: ModelSelection,
        /// Initial session-layer settings contribution.
        model_settings: ModelSettingsOverlay,
        /// Optional initial system prompt; required null-or-text member.
        #[serde(default, skip_serializing_if = "SystemPromptMember::is_absent")]
        system_prompt: SystemPromptMember,
        /// Explicit opt-in placement, defaulting to legacy pathless behavior.
        #[serde(default, skip_serializing_if = "SessionPlacement::is_pathless")]
        placement: SessionPlacement,
        /// Start gate, ownership, and finish condition.
        #[serde(default, skip_serializing_if = "SessionLifecycleMembers::is_default")]
        lifecycle: SessionLifecycleMembers,
    },
    /// Create a user-initiated session from one daemon-held template.
    CreateSessionFromTemplate {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Validated static template name.
        template_name: String,
        /// Explicit opt-in placement, defaulting to legacy pathless behavior.
        #[serde(default, skip_serializing_if = "SessionPlacement::is_pathless")]
        placement: SessionPlacement,
        /// Start gate, ownership, and finish condition.
        #[serde(default, skip_serializing_if = "SessionLifecycleMembers::is_default")]
        lifecycle: SessionLifecycleMembers,
    },
    /// Atomically commission one session from a daemon-held template: create
    /// it under a recorded immutable authority fence, attach its goal, and
    /// submit its first input through the start-when-idle path.
    CommissionSession {
        /// Durable mutation identity for the whole composite.
        command_id: CommandId,
        /// Validated static template name.
        template_name: String,
        /// Immutable authority fence recorded for the created session.
        fence: CommissionedSessionFence,
        /// Exact immutable goal statement.
        statement: String,
        /// Exact first-input text carried to the created session.
        content: InputContent,
    },
    /// List available static templates by name and version.
    ListTemplates {},
    /// Read client-relevant deployment policy for this connection.
    ReadDeploymentLimits {},
    /// List current sessions.
    ListSessions {},
    /// Read one coherent operator-status snapshot.
    ReadOperatorStatus {},
    /// Append one explicit immutable session-placement update event.
    UpdateSessionPlacement {
        command_id: CommandId,
        session_id: CanonicalUuid,
        expected_placement_version: CanonicalU64,
        replacement: SessionPlacement,
    },
    /// Attach one immutable commissioned goal statement.
    AttachGoal {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Exact immutable statement.
        statement: String,
    },
    /// Read the current goal projection and complete ordered event history.
    ReadGoal {
        /// Target session.
        session_id: CanonicalUuid,
    },
    /// Resume a blocked goal with optional next-turn guidance.
    ResumeGoal {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Optional exact next-turn guidance.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        guidance: Option<String>,
    },
    /// Explicitly stop a pursuing or blocked goal.
    StopGoal {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Explicit delegated-child scope.
        descendant_scope: DescendantTerminationScope,
    },
    /// Atomically replace the active immutable statement.
    SupersedeGoal {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Newly commissioned immutable statement.
        statement: String,
    },
    /// Close a session `stopped{sticky}` from any non-terminal state.
    StopSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
        /// Whether re-dispatch stays suppressed until the source is updated.
        sticky: bool,
        /// Explicit delegated-child scope.
        descendant_scope: DescendantTerminationScope,
    },
    /// Close a session `superseded{by}` in favour of its successor.
    SupersedeSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
        /// The session that takes the work.
        successor_session_id: CanonicalUuid,
    },
    /// Write off a parked session as `abandoned`.
    AbandonSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
    },
    /// Close a parked session as failed; null closes with its standing cause.
    CloseSessionFailed {
        command_id: CommandId,
        session_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        cause: Option<SessionFailureCause>,
    },
    /// Return a parked session whose goal is not blocked to its mapped state.
    ResumeSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
    },
    /// Take the liveness obligation, optionally supplying a finish condition.
    AdoptSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        finish_condition: Option<FinishCondition>,
    },
    /// Drop the liveness obligation.
    ReleaseSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
    },
    /// Open a held start gate so queued admission work may dispatch.
    ReleaseStart {
        command_id: CommandId,
        session_id: CanonicalUuid,
    },
    /// Submit user input with an admitted delivery treatment.
    SubmitInput {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Exact ordered user parts.
        content: UserInputContent,
        /// Caller-observed defaults version, or null for configuration-free
        /// steering.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        expected_defaults_version: Option<CanonicalU64>,
        /// Per-call settings contribution; steering must inherit every knob.
        model_settings: ModelSettingsOverlay,
        /// Optional delivery treatment; absence selects the start-when-idle default.
        #[serde(
            default,
            deserialize_with = "deserialize_present_input_delivery",
            skip_serializing_if = "Option::is_none"
        )]
        delivery: Option<InputDelivery>,
    },
    /// Compact one session's model-visible history without rewriting it.
    CompactSession {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Optional one-based semantic position to summarize through; null
        /// selects the latest safe boundary.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        through_position: Option<CanonicalU64>,
    },
    /// Read one durable transcript snapshot.
    ReadTranscript {
        /// Target session.
        session_id: CanonicalUuid,
    },
    /// Read a snapshot and follow later durable updates.
    FollowSession {
        /// Target session.
        session_id: CanonicalUuid,
    },
    /// Execute one exact already-issued delegated-session spawn request.
    SpawnSession {
        /// Invoking parent session.
        session_id: CanonicalUuid,
        /// Turn that issued the tool request.
        turn_id: CanonicalUuid,
        /// Exact logical spawn tool request.
        tool_request_id: CanonicalUuid,
        /// Exact bounded child task.
        task: String,
        /// Parent-chosen lifecycle relationship.
        relationship: DelegationPolicy,
    },
    /// Register delivery for one related child.
    AwaitSession {
        /// Invoking parent session.
        session_id: CanonicalUuid,
        /// Turn that issued the tool request.
        turn_id: CanonicalUuid,
        /// Exact logical await tool request.
        tool_request_id: CanonicalUuid,
        /// Related child whose result is awaited.
        child_session_id: CanonicalUuid,
        /// Foreground or background delivery mode.
        mode: DelegationWaitMode,
    },
    /// Send one bounded message across an existing delegation relationship.
    SendSessionMessage {
        /// Invoking session.
        session_id: CanonicalUuid,
        /// Turn that issued the tool request.
        turn_id: CanonicalUuid,
        /// Exact logical message tool request.
        tool_request_id: CanonicalUuid,
        /// Related peer receiving the message.
        peer_session_id: CanonicalUuid,
        /// Exact bounded message content.
        content: String,
    },
    /// Read one filtered bounded metadata-summary page.
    ListSessionMetadata {
        /// Exact tags every result must carry.
        #[serde(deserialize_with = "deserialize_required_metadata_tags")]
        required_tags: Vec<String>,
        /// Optional exact case-sensitive title substring.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title_contains: Option<String>,
        /// Whether archived sessions participate.
        include_archived: bool,
        /// Inclusive result bound from one through one hundred.
        page_size: CanonicalU64,
        /// Exclusive session-identity cursor.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        after_session_id: Option<CanonicalUuid>,
    },
    /// Read one filtered bounded unified conversation-summary page.
    ListConversations {
        /// Optional exact case-sensitive title substring.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title_contains: Option<String>,
        /// Which origin classes participate.
        origin: ConversationOriginFilter,
        /// Whether archived native sessions participate.
        include_archived: bool,
        /// Inclusive result bound from one through one hundred.
        page_size: CanonicalU64,
        /// Exclusive unified keyset cursor.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        after: Option<ConversationCursor>,
    },
    /// Read the deployment's complete configured model-alias catalog.
    ListModelAliases {},
    /// Read the deployment's complete per-model capability catalog.
    ListModelCapabilities {},
    /// Read one complete current metadata snapshot.
    ReadSessionMetadata {
        /// Target session.
        session_id: CanonicalUuid,
    },
    /// Durably replace one complete metadata snapshot.
    ReplaceSessionMetadata {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Complete replacement object.
        metadata: SessionMetadata,
    },
    /// Replace one session's complete defaults with a new immutable epoch.
    ReplaceSessionDefaults {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Exact caller-observed current epoch.
        expected_defaults_version: CanonicalU64,
        /// Complete replacement model selection.
        model_selection: ModelSelection,
        /// Complete replacement session-layer settings contribution.
        model_settings: ModelSettingsOverlay,
        /// Complete replacement dangerous-tool blanket-auto posture.
        dangerous_tool_auto_approval: bool,
        /// Complete replacement system prompt; required null-or-text member.
        #[serde(default, skip_serializing_if = "SystemPromptMember::is_absent")]
        system_prompt: SystemPromptMember,
    },
    /// Read one session's complete current or named immutable defaults epoch.
    ReadSessionDefaults {
        /// Target session.
        session_id: CanonicalUuid,
        /// Exact immutable epoch to read, or null for the current epoch.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        defaults_version: Option<CanonicalU64>,
    },
    /// Import one complete external conversation snapshot.
    ImportConversation {
        /// Explicit format-versioned converter selection.
        format: ConversationImportFormat,
        /// Exact complete source bytes.
        source: ConversationImportSource,
    },
    /// Begin one per-connection chunked conversation import.
    BeginConversationImport {
        /// Explicit format-versioned converter selection.
        format: ConversationImportFormat,
        /// Exact total source size the caller will append.
        declared_size_bytes: CanonicalU64,
    },
    /// Append one source chunk to the connection's in-progress import.
    AppendConversationImport {
        /// Next exact source bytes in physical order.
        chunk: ConversationImportSource,
    },
    /// Convert and store the connection's completely appended source.
    CommitConversationImport {},
    /// Discard the connection's in-progress import without conversion.
    AbortConversationImport {},
    /// Begin one connection-local immutable user-attachment upload.
    BeginBlobUpload {
        /// Exact content identity the caller computed before upload.
        expected_digest: CanonicalBlobDigest,
        /// Exact positive byte length the caller will append.
        expected_length_bytes: CanonicalU64,
    },
    /// Append one bounded chunk to the connection's active blob upload.
    AppendBlobUpload {
        /// Next exact bytes in physical order.
        chunk: BlobChunk,
    },
    /// Verify, publish, and catalogue the active blob upload.
    CommitBlobUpload {},
    /// Discard the connection's active blob upload.
    AbortBlobUpload {},
    /// Read bounded catalog metadata for one immutable blob.
    ReadBlobMetadata { digest: CanonicalBlobDigest },
    /// Read one exact bounded range after full replica verification.
    ReadBlobChunk {
        digest: CanonicalBlobDigest,
        offset_bytes: CanonicalU64,
        length_bytes: CanonicalU64,
    },
    /// Read one immutable imported conversation's complete entry inventory.
    ///
    /// The read exposes the ordinals `create_session_from_imported_frontier`
    /// consumes; it creates nothing and seeds nothing.
    ReadImportedConversation {
        /// Immutable imported conversation to inspect.
        imported_conversation_id: CanonicalUuid,
    },
    /// Create a live session from one inclusive imported entry boundary.
    CreateSessionFromImportedFrontier {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Immutable imported conversation to continue.
        imported_conversation_id: CanonicalUuid,
        /// Inclusive one-based imported entry position.
        through_position: CanonicalU64,
        /// Creation-time resume or fork intent.
        relationship: ImportedSessionRelationship,
        /// Initial session model-selection defaults.
        initial_model_selection: ModelSelection,
        /// Initial session-layer settings contribution.
        model_settings: ModelSettingsOverlay,
    },
    /// Reconcile the exact active turn parked on an ambiguous model call.
    ///
    /// The named turn must be the session's active turn and must be parked in
    /// the model-call recovery wait. The request supplies the user interrupt
    /// authority that turn's terminal disposition requires and carries the
    /// successor input the session continues with.
    ReconcileTurn {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// The turn the caller observed parked awaiting reconciliation.
        expected_active_turn_id: CanonicalUuid,
        /// Exact ordered user parts for the immediate successor turn.
        content: UserInputContent,
        /// Caller-observed defaults version.
        expected_defaults_version: CanonicalU64,
        /// Per-call settings contribution for the immediate successor origin.
        model_settings: ModelSettingsOverlay,
    },
    /// Register one immutable external review target snapshot.
    CreateReviewTarget {
        command_id: CommandId,
        target_id: CanonicalUuid,
        provider: String,
        repository: String,
        subject: ReviewTargetSubject,
        head_revision: String,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        base_revision: Option<String>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        stack_parent_target_id: Option<CanonicalUuid>,
    },
    /// Admit one run and its sole session-backed pass.
    StartReviewRun {
        command_id: CommandId,
        target_id: CanonicalUuid,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        workflow: ReviewWorkflow,
        session_id: CanonicalUuid,
        accepted_input_id: CanonicalUuid,
    },
    /// Atomically bind one queued run and pass to their already-active turn.
    ActivateReviewPass {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Owning run.
        run_id: CanonicalUuid,
        /// Pass to activate.
        pass_id: CanonicalUuid,
        /// Canonical active turn created from the pass's accepted input.
        turn_id: CanonicalUuid,
    },
    /// Conclude one pass that carries no other typed result payload.
    CompleteReviewPass {
        command_id: CommandId,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        turn_id: Option<CanonicalUuid>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        output_frontier_id: Option<CanonicalUuid>,
        outcome: ReviewPassTerminalOutcome,
    },
    RecordReviewFindings {
        command_id: CommandId,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        output_frontier_id: CanonicalUuid,
        findings: Vec<ReviewFindingInput>,
    },
    /// Atomically conclude a result-bearing pass and append one finding event.
    RecordReviewFindingEvent {
        command_id: CommandId,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        output_frontier_id: Option<CanonicalUuid>,
        finding_id: CanonicalUuid,
        /// Exact contiguous event ordinal the appended disposition occupies.
        event_ordinal: CanonicalU64,
        event: ReviewFindingEvent,
    },
    /// Reserve one provider object identity before an external write.
    ReserveReviewExternalLink {
        command_id: CommandId,
        external_link_id: CanonicalUuid,
        finding_id: CanonicalUuid,
        provider: String,
        object_kind: ReviewExternalObjectKind,
    },
    /// Attach a provider identity through an exact publish-pass result.
    AttachReviewExternalLink {
        command_id: CommandId,
        external_link_id: CanonicalUuid,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        output_frontier_id: CanonicalUuid,
        external_object: String,
        event_ordinal: CanonicalU64,
    },
    /// Read one immutable target snapshot.
    ReadReviewTarget { target_id: CanonicalUuid },
    /// Read one run and its sole pass projection.
    ReadReviewRun { run_id: CanonicalUuid },
    /// Read one complete finding aggregate projection.
    ReadReviewFinding { finding_id: CanonicalUuid },
    /// List findings produced by one exact run in identity order.
    ListReviewFindings { run_id: CanonicalUuid },
    /// Start one immutable client-driven orchestration attempt.
    StartReviewOrchestration {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        target_id: CanonicalUuid,
        concern_set_version: String,
        import_template_name: String,
        judgment_template_name: String,
        repair_template_name: String,
        publication_template_name: String,
        concerns: Vec<ReviewOrchestrationConcernInput>,
    },
    /// Seal the import stage outcome.
    RecordReviewImportOutcome {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        pass_id: Option<CanonicalUuid>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        external_link_id: Option<CanonicalUuid>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        context_digest: Option<CanonicalDigest>,
        outcome: ReviewImportTerminalOutcome,
    },
    /// Seal one frozen concern member outcome.
    RecordReviewConcernOutcome {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        concern: String,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        pass_id: Option<CanonicalUuid>,
        outcome: ReviewConcernTerminalOutcome,
    },
    /// Seal the complete judgment plan over a succeeded fan-out.
    RecordReviewJudgmentPlan {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        analysis_pass_id: CanonicalUuid,
        members: Vec<ReviewJudgmentPlanMember>,
    },
    /// Seal the result of applying one judgment-plan member.
    RecordReviewJudgmentEffect {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        finding_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        event_pass_id: Option<CanonicalUuid>,
        outcome: ReviewJudgmentEffectTerminalOutcome,
    },
    /// Seal the complete repair-stage member inventory.
    RecordReviewRepairOutcomes {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        outcomes: Vec<ReviewRepairOutcome>,
    },
    /// Seal the complete publication-stage member inventory.
    RecordReviewPublicationOutcomes {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        outcomes: Vec<ReviewPublicationOutcome>,
    },
    /// Read one complete orchestration attempt projection.
    ReadReviewOrchestration { attempt_id: CanonicalUuid },
    /// Stop the exact active turn through the accepted interrupt treatment.
    ///
    /// The request applies the `Interrupt` delivery to the named active turn:
    /// its stop is durably requested and terminalization flows through the
    /// existing lifecycle, while `content` becomes the immediate-successor
    /// origin the session continues with. No standalone cancellation command
    /// exists; this verb is the interrupt treatment on the wire.
    StopTurn {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// The turn the caller observed active in the session.
        expected_active_turn_id: CanonicalUuid,
        /// Exact ordered user parts for the immediate successor turn.
        content: UserInputContent,
        /// Caller-observed defaults version.
        expected_defaults_version: CanonicalU64,
        /// Explicit delegated-child scope.
        descendant_scope: DescendantTerminationScope,
        /// Per-call settings contribution for the immediate successor origin.
        model_settings: ModelSettingsOverlay,
    },
    /// Supply the user decision for one pending tool request.
    DecideToolRequest {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Session the caller expects to own the request.
        session_id: CanonicalUuid,
        /// Exact logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact closed approval decision.
        decision: ToolDecision,
    },
    /// Record one one-shot user override of a delegate-denied tool request.
    OverrideDeniedToolRequest {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Session the override covers; part of the canonical payload.
        session_id: CanonicalUuid,
        /// Exact delegate-denied logical tool request.
        tool_request_id: CanonicalUuid,
    },
}

/// One closed wire approval decision for a pending tool request.
///
/// The wire surface requires a denial reason; the daemon validates it against
/// the domain's denial-reason contract before command construction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolDecision {
    /// Execution is permitted subject to current aggregate guards.
    Approve {},
    /// Execution is permanently prohibited for this request.
    Deny {
        /// Exact user explanation rendered to the model.
        reason: String,
    },
}

impl ClientRequest {
    fn validate(&self) -> Result<(), FrameValidationError> {
        match self {
            Self::AttachGoal { statement, .. }
            | Self::SupersedeGoal { statement, .. }
            | Self::CommissionSession { statement, .. } => {
                validate_goal_text(statement)?;
            }
            Self::ResumeGoal {
                guidance: Some(guidance),
                ..
            } => validate_goal_text(guidance)?,
            Self::AdoptSession {
                finish_condition: Some(FinishCondition::Declared { statement }),
                ..
            } => validate_goal_text(statement)?,
            Self::CreateSession {
                lifecycle:
                    SessionLifecycleMembers {
                        finish_condition: Some(FinishCondition::Declared { statement }),
                        ..
                    },
                ..
            }
            | Self::CreateSessionFromTemplate {
                lifecycle:
                    SessionLifecycleMembers {
                        finish_condition: Some(FinishCondition::Declared { statement }),
                        ..
                    },
                ..
            } => validate_goal_text(statement)?,
            Self::CreateSession { .. }
            | Self::CreateSessionFromTemplate { .. }
            | Self::ListTemplates {}
            | Self::ReadDeploymentLimits {}
            | Self::ListSessions {}
            | Self::ReadOperatorStatus {}
            | Self::UpdateSessionPlacement { .. }
            | Self::ReadGoal { .. }
            | Self::ResumeGoal { guidance: None, .. }
            | Self::StopGoal { .. }
            | Self::StopSession { .. }
            | Self::SupersedeSession { .. }
            | Self::AbandonSession { .. }
            | Self::CloseSessionFailed { .. }
            | Self::ResumeSession { .. }
            | Self::AdoptSession {
                finish_condition: None,
                ..
            }
            | Self::AdoptSession {
                finish_condition: Some(FinishCondition::ExternalGate),
                ..
            }
            | Self::ReleaseSession { .. }
            | Self::ReleaseStart { .. }
            | Self::SubmitInput { .. }
            | Self::CompactSession { .. }
            | Self::ReadTranscript { .. }
            | Self::FollowSession { .. }
            | Self::SpawnSession { .. }
            | Self::AwaitSession { .. }
            | Self::SendSessionMessage { .. }
            | Self::ListSessionMetadata { .. }
            | Self::ListConversations { .. }
            | Self::ListModelAliases {}
            | Self::ListModelCapabilities {}
            | Self::ReadSessionMetadata { .. }
            | Self::ReplaceSessionMetadata { .. }
            | Self::ReplaceSessionDefaults { .. }
            | Self::ReadSessionDefaults { .. }
            | Self::ImportConversation { .. }
            | Self::BeginConversationImport { .. }
            | Self::AppendConversationImport { .. }
            | Self::CommitConversationImport {}
            | Self::AbortConversationImport {}
            | Self::BeginBlobUpload { .. }
            | Self::AppendBlobUpload { .. }
            | Self::CommitBlobUpload {}
            | Self::AbortBlobUpload {}
            | Self::ReadBlobMetadata { .. }
            | Self::ReadBlobChunk { .. }
            | Self::ReadImportedConversation { .. }
            | Self::CreateSessionFromImportedFrontier { .. }
            | Self::ReconcileTurn { .. }
            | Self::CreateReviewTarget { .. }
            | Self::StartReviewRun { .. }
            | Self::ActivateReviewPass { .. }
            | Self::CompleteReviewPass { .. }
            | Self::RecordReviewFindings { .. }
            | Self::RecordReviewFindingEvent { .. }
            | Self::ReserveReviewExternalLink { .. }
            | Self::AttachReviewExternalLink { .. }
            | Self::ReadReviewTarget { .. }
            | Self::ReadReviewRun { .. }
            | Self::ReadReviewFinding { .. }
            | Self::ListReviewFindings { .. }
            | Self::StartReviewOrchestration { .. }
            | Self::RecordReviewImportOutcome { .. }
            | Self::RecordReviewConcernOutcome { .. }
            | Self::RecordReviewJudgmentPlan { .. }
            | Self::RecordReviewJudgmentEffect { .. }
            | Self::RecordReviewRepairOutcomes { .. }
            | Self::RecordReviewPublicationOutcomes { .. }
            | Self::ReadReviewOrchestration { .. }
            | Self::StopTurn { .. }
            | Self::DecideToolRequest { .. }
            | Self::OverrideDeniedToolRequest { .. } => {}
        }
        match self {
            Self::CreateSession { placement, .. }
            | Self::CreateSessionFromTemplate { placement, .. }
            | Self::UpdateSessionPlacement {
                replacement: placement,
                ..
            } => validate_session_placement_shape(placement)?,
            _ => {}
        }
        if let Self::UpdateSessionPlacement {
            expected_placement_version,
            ..
        } = self
            && expected_placement_version.value() == 0
        {
            return Err(FrameValidationError::PlacementShape);
        }
        if let Self::CommissionSession {
            fence:
                CommissionedSessionFence::PullRequest {
                    pull_request: number,
                    ..
                },
            ..
        } = self
            && number.value() == 0
        {
            return Err(FrameValidationError::DispatchFenceShape);
        }
        if let Self::SubmitInput {
            expected_defaults_version,
            delivery,
            model_settings,
            content,
            ..
        } = self
        {
            content.validate()?;
            let valid = matches!(
                (delivery, expected_defaults_version),
                (None | Some(InputDelivery::StartWhenIdle {}), Some(_))
                    | (Some(InputDelivery::Steer { .. }), None)
                    | (Some(InputDelivery::Queue { .. }), Some(_))
            );
            if !valid {
                return Err(FrameValidationError::InputDeliveryShape);
            }
            if matches!(delivery, Some(InputDelivery::Steer { .. }))
                && *model_settings != ModelSettingsOverlay::inherit_all()
            {
                return Err(FrameValidationError::ModelSettingsShape);
            }
        }
        if let Self::ReconcileTurn { content, .. } | Self::StopTurn { content, .. } = self {
            content.validate()?;
        }
        if let Self::AppendConversationImport { chunk } = self
            && (chunk.as_bytes().is_empty()
                || chunk.as_bytes().len() > MAX_CONVERSATION_IMPORT_CHUNK_BYTES)
        {
            return Err(FrameValidationError::ConversationImportShape);
        }
        if let Self::AppendBlobUpload { chunk } = self
            && (chunk.as_bytes().is_empty() || chunk.as_bytes().len() > MAX_BLOB_CHUNK_BYTES)
        {
            return Err(FrameValidationError::BlobUploadShape);
        }
        if let Self::CreateSessionFromImportedFrontier {
            through_position, ..
        } = self
            && through_position.value() == 0
        {
            return Err(FrameValidationError::ImportedFrontierShape);
        }
        if let Self::CompactSession {
            through_position: Some(position),
            ..
        } = self
            && position.value() == 0
        {
            return Err(FrameValidationError::ContextCompactionShape);
        }
        if let Self::ListSessionMetadata {
            required_tags,
            title_contains,
            ..
        } = self
        {
            let canonical_tags = canonical_metadata_tags(required_tags.clone(), None)
                .map_err(|_| FrameValidationError::MetadataShape)?;
            let mut total_utf8_bytes = 0usize;
            for tag in &canonical_tags {
                add_metadata_utf8_bytes(&mut total_utf8_bytes, tag)
                    .map_err(|_| FrameValidationError::MetadataShape)?;
            }
            if let Some(query) = title_contains {
                validate_nonempty_metadata_text(query)
                    .map_err(|_| FrameValidationError::MetadataShape)?;
                add_metadata_utf8_bytes(&mut total_utf8_bytes, query)
                    .map_err(|_| FrameValidationError::MetadataShape)?;
            }
        }
        if let Self::ListConversations {
            title_contains: Some(query),
            ..
        } = self
        {
            validate_nonempty_metadata_text(query)
                .map_err(|_| FrameValidationError::ConversationListShape)?;
            let mut total_utf8_bytes = 0usize;
            add_metadata_utf8_bytes(&mut total_utf8_bytes, query)
                .map_err(|_| FrameValidationError::ConversationListShape)?;
        }
        if let Self::CreateSessionFromTemplate { template_name, .. } = self {
            validate_session_template_name(template_name)?;
        }
        if let Self::CommissionSession { template_name, .. } = self {
            validate_session_template_name(template_name)?;
        }
        if let Self::CompleteReviewPass {
            turn_id,
            output_frontier_id,
            outcome,
            ..
        } = self
        {
            let valid = matches!(
                (outcome, turn_id, output_frontier_id),
                (ReviewPassTerminalOutcome::Succeeded, Some(_), Some(_))
                    | (
                        ReviewPassTerminalOutcome::Failed | ReviewPassTerminalOutcome::Blocked,
                        Some(_),
                        None
                    )
                    | (ReviewPassTerminalOutcome::Cancelled, _, None)
            );
            if !valid {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::RecordReviewFindings { findings, .. } = self
            && findings.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS
        {
            return Err(FrameValidationError::ReviewShape);
        }
        if let Self::RecordReviewFindingEvent {
            finding_id,
            output_frontier_id,
            event,
            ..
        } = self
        {
            validate_review_finding_event(event)?;
            let blocked = matches!(event, ReviewFindingEvent::BlockedWithReason { .. });
            if blocked == output_frontier_id.is_some() {
                return Err(FrameValidationError::ReviewShape);
            }

            let self_reference = match event {
                ReviewFindingEvent::Duplicate {
                    canonical_finding_id,
                } => *canonical_finding_id == *finding_id,
                ReviewFindingEvent::Superseded {
                    successor_finding_id,
                } => *successor_finding_id == *finding_id,
                _ => false,
            };
            if self_reference {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::StartReviewOrchestration {
            concern_set_version,
            import_template_name,
            judgment_template_name,
            repair_template_name,
            publication_template_name,
            concerns,
            ..
        } = self
        {
            validate_review_key(concern_set_version)?;
            validate_session_template_name(import_template_name)?;
            validate_session_template_name(judgment_template_name)?;
            validate_session_template_name(repair_template_name)?;
            validate_session_template_name(publication_template_name)?;
            if concerns.is_empty() || concerns.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
                return Err(FrameValidationError::ReviewShape);
            }
            let mut keys = HashSet::new();
            let mut templates = HashSet::new();
            for concern in concerns {
                validate_review_key(&concern.key)?;
                validate_session_template_name(&concern.template_name)?;
                if !keys.insert(&concern.key) || !templates.insert(&concern.template_name) {
                    return Err(FrameValidationError::ReviewShape);
                }
            }
        }
        if let Self::RecordReviewImportOutcome {
            pass_id,
            external_link_id,
            context_digest,
            outcome,
            ..
        } = self
        {
            let valid = match outcome {
                ReviewImportTerminalOutcome::Succeeded => {
                    pass_id.is_some() && context_digest.is_some()
                }
                ReviewImportTerminalOutcome::Failed | ReviewImportTerminalOutcome::Blocked => {
                    pass_id.is_some() && external_link_id.is_none() && context_digest.is_none()
                }
                ReviewImportTerminalOutcome::Cancelled => {
                    external_link_id.is_none() && context_digest.is_none()
                }
            };
            if !valid
                || (*outcome != ReviewImportTerminalOutcome::Succeeded
                    && external_link_id.is_some())
            {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::RecordReviewConcernOutcome {
            concern,
            pass_id,
            outcome,
            ..
        } = self
        {
            validate_review_key(concern)?;
            if *outcome != ReviewConcernTerminalOutcome::Cancelled && pass_id.is_none() {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::RecordReviewJudgmentPlan { members, .. } = self {
            if members.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
                return Err(FrameValidationError::ReviewShape);
            }
            let mut findings = HashSet::new();
            for member in members {
                validate_review_judgment_disposition(&member.disposition)?;
                if !findings.insert(member.finding_id) {
                    return Err(FrameValidationError::ReviewShape);
                }
            }
        }
        if let Self::RecordReviewJudgmentEffect {
            event_pass_id,
            outcome,
            ..
        } = self
        {
            let valid = (*outcome == ReviewJudgmentEffectTerminalOutcome::Applied)
                == event_pass_id.is_some();
            if !valid {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::RecordReviewRepairOutcomes { outcomes, .. } = self {
            if outcomes.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
                return Err(FrameValidationError::ReviewShape);
            }
            let mut findings = HashSet::new();
            for outcome in outcomes {
                let valid = (outcome.outcome == ReviewRepairTerminalOutcome::Fixed)
                    == outcome.event_pass_id.is_some();
                if !valid || !findings.insert(outcome.finding_id) {
                    return Err(FrameValidationError::ReviewShape);
                }
            }
        }
        if let Self::RecordReviewPublicationOutcomes { outcomes, .. } = self {
            if outcomes.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
                return Err(FrameValidationError::ReviewShape);
            }
            let mut findings = HashSet::new();
            for outcome in outcomes {
                let valid = (outcome.outcome == ReviewPublicationTerminalOutcome::Published)
                    == outcome.external_link_id.is_some();
                if !valid || !findings.insert(outcome.finding_id) {
                    return Err(FrameValidationError::ReviewShape);
                }
            }
        }
        Ok(())
    }
}

#[derive(signalbox_derive::Accessors)]
/// One validated client frame.

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientFrame {
    version: ProtocolVersion,
    request_id: RequestId,
    /// Borrows the closed request.
    #[get]
    request: ClientRequest,
}

impl ClientFrame {
    /// Constructs a single-version frame with a correlated request identity.
    pub fn try_new(
        request_id: RequestId,
        request: ClientRequest,
    ) -> Result<Self, FrameValidationError> {
        Self::try_new_for_version(ProtocolVersion::One, request_id, request)
    }

    /// Constructs a frame in one admitted protocol version.
    pub fn try_new_for_version(
        version: ProtocolVersion,
        request_id: RequestId,
        request: ClientRequest,
    ) -> Result<Self, FrameValidationError> {
        let frame = Self {
            version,
            request_id,
            request,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// Returns the admitted protocol version.
    pub const fn version(&self) -> ProtocolVersion {
        self.version
    }

    /// Returns the correlation identity.
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    /// Transfers the admitted version, correlation identity, and closed
    /// request out of the frame.
    pub fn into_parts(self) -> (ProtocolVersion, RequestId, ClientRequest) {
        (self.version, self.request_id, self.request)
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if !self.request_id.is_correlated() {
            return Err(FrameValidationError::UncorrelatedClientRequest);
        }
        if let ClientRequest::CreateSession { system_prompt, .. }
        | ClientRequest::ReplaceSessionDefaults { system_prompt, .. } = &self.request
        {
            validate_system_prompt_member(system_prompt)?;
        }
        self.request.validate()
    }
}

/// Requires the presence-checked system-prompt member.
fn validate_system_prompt_member(member: &SystemPromptMember) -> Result<(), FrameValidationError> {
    if member.is_absent() {
        return Err(FrameValidationError::SystemPromptShape);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClientFrame {
    version: ProtocolVersion,
    request_id: RequestId,
    request: ClientRequest,
}

impl<'de> Deserialize<'de> for ClientFrame {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let raw = RawClientFrame::deserialize(deserializer)?;
        let frame = Self {
            version: raw.version,
            request_id: raw.request_id,
            request: raw.request,
        };
        frame.validate().map_err(serde::de::Error::custom)?;
        Ok(frame)
    }
}

/// Stable server error code.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// JSON, UTF-8, framing, field, or size validation failed.
    MalformedFrame,
    /// Frame version is not admitted by this implementation.
    UnsupportedVersion,
    /// A boundary value cannot construct the application input.
    InvalidRequest,
    /// A read target does not exist.
    NotFound,
    /// Every recorded replica was proven absent.
    BlobMissing,
    /// Every usable recorded replica failed content verification.
    BlobCorrupt,
    /// A durable identity already names different intent.
    ConflictingReuse,
    /// Canonical command handling recorded a typed rejection.
    Rejected,
    /// A follower fell behind bounded fan-out.
    ResyncRequired,
    /// Infrastructure prevented completion.
    Unavailable,
    /// A remote store may have accepted a deterministic publication.
    PublicationAmbiguous,
    /// Infrastructure obscured whether a requested mutation committed.
    CommitAmbiguous,
    /// Fail-closed corruption or a hub defect stopped the request.
    Internal,
}

/// Closed connection-local holder of the process-wide bulk-ingest permit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkIngestKind {
    ConversationImport,
    BlobUpload,
}

impl BulkIngestKind {
    /// Returns the exact lowercase wire token for terminal diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConversationImport => "conversation_import",
            Self::BlobUpload => "blob_upload",
        }
    }
}

/// Typed durable submit rejection details.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RejectionDetail {
    /// Another chunked bulk-ingest kind already owns this connection.
    BulkIngestAlreadyInProgress { active_kind: BulkIngestKind },
    /// An explicit reasoning value is unsupported by the selected model.
    UnsupportedReasoningLevel {
        selection_id: CanonicalUuid,
        requested: ReasoningLevel,
    },
    /// Enabled fast mode is unsupported by the selected model.
    UnsupportedFastMode { selection_id: CanonicalUuid },
    /// An explicit service tier is unsupported by the selected model.
    UnsupportedServiceTier {
        selection_id: CanonicalUuid,
        requested: ServiceTier,
    },
    /// The target session did not exist at command handling.
    SessionNotFound {
        /// Absent target.
        session_id: CanonicalUuid,
    },
    /// An attachment digest had no catalogued verified replica.
    AttachmentBlobNotFound {
        /// The unavailable immutable byte identity.
        digest: CanonicalBlobDigest,
    },
    /// Distinct attachment bytes exceeded the deployment admission ceiling.
    AttachmentByteBudgetExceeded {
        /// Configured maximum aggregate byte count.
        maximum_bytes: PositiveCanonicalU64,
    },
    /// The placement head advanced beyond the caller-observed version.
    SessionPlacementCurrentVersionMismatch {
        session_id: CanonicalUuid,
        expected_placement_version: CanonicalU64,
        current_placement_version: CanonicalU64,
    },
    /// The positive placement-version space was exhausted.
    SessionPlacementVersionExhausted {
        session_id: CanonicalUuid,
        current_placement_version: CanonicalU64,
    },
    /// A durable goal command was rejected by current goal state.
    GoalCommandRejected {
        /// Target session.
        session_id: CanonicalUuid,
        /// Closed goal-specific reason.
        reason: GoalCommandRejection,
    },
    /// A turn already held the session slot.
    ActiveTurnPresent {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
    },
    /// A commissioned target already has a live session.
    CommissionTargetBusy {
        /// Authoritative live session currently owning the target.
        session_id: CanonicalUuid,
    },
    /// The caller named a turn that no longer holds the session slot.
    ActiveTurnMismatch {
        /// Target session.
        session_id: CanonicalUuid,
        /// Turn the caller expected to be active.
        expected_active_turn_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
    },
    /// No turn held the session slot when the caller named one.
    NoActiveTurn {
        /// Target session.
        session_id: CanonicalUuid,
        /// Turn the caller expected to be active.
        expected_active_turn_id: CanonicalUuid,
    },
    /// The named turn is not parked on the model-call recovery wait, so no
    /// reconciliation decision is owed for it.
    ///
    /// This precondition is refused before a durable command is recorded; a
    /// caller that races the authoritative state instead receives one of the
    /// recorded rejections above.
    TurnNotAwaitingReconciliation {
        /// Target session.
        session_id: CanonicalUuid,
        /// Turn the caller named.
        turn_id: CanonicalUuid,
    },
    /// A distinct earlier stop was already applied to the active turn.
    InterruptAlreadyApplied {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
        /// Command whose applied result already carries the stop proof.
        existing_command_id: CanonicalUuid,
    },
    /// The active turn is parked on a tool-approval wait, which a stop can
    /// neither decide nor bypass; the caller denies the pending request first.
    InterruptUnavailableWhileAwaitingApproval {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
    },
    /// A next-safe-point input targeted a turn that is already stopping.
    SafePointUnavailableWhileStopping {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative stopping turn.
        active_turn_id: CanonicalUuid,
        /// Command whose applied result already carries the stop proof.
        existing_command_id: CanonicalUuid,
    },
    /// No logical tool request had the named identity.
    ToolRequestNotFound {
        /// Absent logical tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The named tool request already had a terminal approval resolution.
    ToolRequestAlreadyResolved {
        /// Resolved logical tool request.
        tool_request_id: CanonicalUuid,
    },
    /// An earlier request in the same batch still awaited its decision.
    ToolRequestNotEarliestUndecided {
        /// Named logical tool request.
        tool_request_id: CanonicalUuid,
        /// Earliest undecided request owed a decision first.
        earliest_tool_request_id: CanonicalUuid,
    },
    /// The named tool request is not owned by the named session, so no
    /// decision is admitted for it.
    ///
    /// This precondition is refused before a durable command is recorded; a
    /// correctly correlated request instead reaches the canonical decision
    /// command and its recorded rejections above.
    ToolRequestNotInSession {
        /// Session the caller named.
        session_id: CanonicalUuid,
        /// Tool request the caller named.
        tool_request_id: CanonicalUuid,
    },
    /// The named tool request carries no delegate denial, so no override is
    /// admitted for it.
    ToolRequestNotDelegateDenied {
        /// Tool request without a delegate denial.
        tool_request_id: CanonicalUuid,
    },
    /// The named delegate denial has not reached its terminal denied result.
    ToolRequestNotTerminallyDenied {
        /// Tool request whose denial is still resolving.
        tool_request_id: CanonicalUuid,
    },
    /// An override is already recorded for the named delegate denial.
    ToolDenialAlreadyOverridden {
        /// Already-overridden tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The named delegation request belongs to another turn.
    DelegationRequestNotInTurn {
        /// Session the caller named.
        session_id: CanonicalUuid,
        /// Turn the caller named.
        turn_id: CanonicalUuid,
        /// Delegation request owned by another turn.
        tool_request_id: CanonicalUuid,
    },
    /// A first execution named a request without executable attempt authority.
    DelegationToolRequestNotExecutable {
        /// Logical delegation tool request.
        tool_request_id: CanonicalUuid,
        /// Exact durable state that prevented first execution.
        state: DelegationToolRequestState,
    },
    /// A spawn request replay changed its immutable arguments.
    DelegationSpawnConflict {
        /// Conflicting logical spawn request.
        tool_request_id: CanonicalUuid,
    },
    /// A generated child identity was already occupied.
    DelegatedChildIdentityCollision {
        /// Colliding child identity.
        child_session_id: CanonicalUuid,
    },
    /// No delegation relationship joined the named session and peer.
    DelegationRelationNotFound {
        /// Invoking session.
        session_id: CanonicalUuid,
        /// Named related peer.
        peer_session_id: CanonicalUuid,
    },
    /// An await request replay changed its immutable arguments.
    DelegationAwaitConflict {
        /// Conflicting logical await request.
        tool_request_id: CanonicalUuid,
    },
    /// A message request replay changed its immutable arguments.
    DelegationMessageConflict {
        /// Conflicting logical message request.
        tool_request_id: CanonicalUuid,
    },
    /// A daemon-minted message identity was already claimed.
    DelegationMessageIdentityCollision {
        /// Colliding message identity.
        message_id: CanonicalUuid,
    },
    /// A relationship cannot allocate another positive event ordinal.
    DelegationEventOrdinalExhausted {
        /// Relationship's spawning request identity.
        spawning_request_id: CanonicalUuid,
        /// Last representable event ordinal.
        last: CanonicalU64,
    },
    /// A recipient cannot allocate another positive delivery sequence.
    DelegationDeliverySequenceExhausted {
        /// Recipient whose delivery sequence is exhausted.
        recipient_session_id: CanonicalUuid,
        /// Last representable delivery sequence.
        last: CanonicalU64,
    },
    /// The caller observed stale defaults.
    DefaultsVersionMismatch {
        /// Target session.
        session_id: CanonicalUuid,
        /// Caller version.
        expected: CanonicalU64,
        /// Current authoritative version.
        current: CanonicalU64,
    },
    /// The selected alias had no current definition.
    UnknownModelAlias {
        /// Target session.
        session_id: CanonicalUuid,
        /// Unknown alias.
        alias_id: CanonicalUuid,
    },
    /// The session acceptance ordinal was exhausted.
    AcceptancePositionExhausted {
        /// Target session.
        session_id: CanonicalUuid,
        /// Last representable position.
        last: CanonicalU64,
    },
    /// The session defaults epoch ordinal was exhausted.
    DefaultsVersionExhausted {
        /// Target session.
        session_id: CanonicalUuid,
        /// Last representable epoch.
        current: CanonicalU64,
    },
    /// No imported conversation had the named identity.
    ///
    /// The absent target is an imported conversation, never a session: an
    /// imported conversation is durable record and creates no session.
    ImportedConversationNotFound {
        /// Absent imported conversation.
        imported_conversation_id: CanonicalUuid,
    },
    /// The named imported conversation exists but has no such position.
    ///
    /// Imported positions are the one-based contiguous sequence
    /// `1..=last_position`; the identity was valid and only the ordinal was
    /// outside it.
    ImportedFrontierPositionOutOfRange {
        /// Imported conversation whose positions bound the request.
        imported_conversation_id: CanonicalUuid,
        /// Exact position the caller named.
        requested_position: CanonicalU64,
        /// Greatest selectable position on that conversation.
        last_position: CanonicalU64,
    },
    /// This connection already has one in-progress conversation import.
    ConversationImportAlreadyInProgress {},
    /// This connection has no in-progress conversation import.
    ConversationImportNotInProgress {},
    /// The declared or observed source size exceeds the configured total bound.
    ConversationImportSourceTooLarge {
        /// Configured maximum assembled source size.
        limit_bytes: CanonicalU64,
        /// Exact total source size declared at begin.
        declared_size_bytes: CanonicalU64,
        /// Exact observed size at append or commit, or null at begin.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        actual_size_bytes: Option<CanonicalU64>,
    },
    /// The observed source size did not equal the size declared at begin.
    ConversationImportSourceSizeMismatch {
        /// Exact total source size declared at begin.
        declared_size_bytes: CanonicalU64,
        /// Exact number of source bytes observed across append requests.
        actual_size_bytes: CanonicalU64,
    },
    /// A converter rejected the complete source with content-silent evidence.
    ConversationImportConversionFailed {
        /// Closed converter failure class.
        class: ConversationImportRejectionClass,
        /// One-based offending physical record, or null when not applicable.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        record_ordinal: Option<CanonicalU64>,
    },
    /// This connection already has one in-progress blob upload.
    BlobUploadAlreadyInProgress {},
    /// This connection has no in-progress blob upload.
    BlobUploadNotInProgress {},
    /// The declared blob length fell outside the configured inclusive range.
    BlobUploadLengthOutOfRange {
        min_length_bytes: CanonicalU64,
        max_length_bytes: CanonicalU64,
        declared_length_bytes: CanonicalU64,
    },
    /// Appending the chunk would exceed the length declared at begin.
    BlobUploadSizeExceeded {
        expected_length_bytes: CanonicalU64,
        actual_length_bytes: CanonicalU64,
    },
    /// The appended byte count differed from the length declared at begin.
    BlobUploadLengthMismatch {
        expected_length_bytes: CanonicalU64,
        actual_length_bytes: CanonicalU64,
    },
    /// The assembled bytes differed from the digest declared at begin.
    BlobUploadDigestMismatch {
        expected_digest: CanonicalBlobDigest,
        actual_digest: CanonicalBlobDigest,
    },
    /// The requested direct-read length fell outside the inclusive wire bound.
    BlobReadLengthOutOfRange {
        min_length_bytes: CanonicalU64,
        max_length_bytes: CanonicalU64,
        requested_length_bytes: CanonicalU64,
    },
    /// The requested exact half-open range is not contained by the blob.
    BlobReadRangeOutOfBounds {
        offset_bytes: CanonicalU64,
        length_bytes: CanonicalU64,
        blob_length_bytes: CanonicalU64,
    },
    /// A durable session-lifecycle command was rejected by current state.
    SessionLifecycleCommandRejected {
        /// Target session.
        session_id: CanonicalUuid,
        /// Closed reason.
        reason: SessionLifecycleCommandRejection,
    },
}

impl RejectionDetail {
    const fn is_bulk_ingest(self) -> bool {
        matches!(self, Self::BulkIngestAlreadyInProgress { .. })
    }

    const fn is_blob_upload(self) -> bool {
        matches!(
            self,
            Self::BlobUploadAlreadyInProgress {}
                | Self::BlobUploadNotInProgress {}
                | Self::BlobUploadLengthOutOfRange { .. }
                | Self::BlobUploadSizeExceeded { .. }
                | Self::BlobUploadLengthMismatch { .. }
                | Self::BlobUploadDigestMismatch { .. }
        )
    }

    const fn is_blob_read(self) -> bool {
        matches!(
            self,
            Self::BlobReadLengthOutOfRange { .. } | Self::BlobReadRangeOutOfBounds { .. }
        )
    }

    const fn is_conversation_import(self) -> bool {
        match self {
            Self::ConversationImportAlreadyInProgress {}
            | Self::ConversationImportNotInProgress {}
            | Self::ConversationImportSourceTooLarge { .. }
            | Self::ConversationImportSourceSizeMismatch { .. }
            | Self::ConversationImportConversionFailed { .. } => true,
            Self::BlobUploadAlreadyInProgress {}
            | Self::BlobUploadNotInProgress {}
            | Self::BlobUploadLengthOutOfRange { .. }
            | Self::BlobUploadSizeExceeded { .. }
            | Self::BlobUploadLengthMismatch { .. }
            | Self::BlobUploadDigestMismatch { .. }
            | Self::BlobReadLengthOutOfRange { .. }
            | Self::BlobReadRangeOutOfBounds { .. }
            | Self::BulkIngestAlreadyInProgress { .. }
            | Self::SessionNotFound { .. }
            | Self::AttachmentBlobNotFound { .. }
            | Self::AttachmentByteBudgetExceeded { .. }
            | Self::UnsupportedReasoningLevel { .. }
            | Self::UnsupportedFastMode { .. }
            | Self::UnsupportedServiceTier { .. }
            | Self::SessionPlacementCurrentVersionMismatch { .. }
            | Self::SessionPlacementVersionExhausted { .. }
            | Self::GoalCommandRejected { .. }
            | Self::SessionLifecycleCommandRejected { .. }
            | Self::ActiveTurnPresent { .. }
            | Self::CommissionTargetBusy { .. }
            | Self::ActiveTurnMismatch { .. }
            | Self::NoActiveTurn { .. }
            | Self::TurnNotAwaitingReconciliation { .. }
            | Self::InterruptAlreadyApplied { .. }
            | Self::InterruptUnavailableWhileAwaitingApproval { .. }
            | Self::SafePointUnavailableWhileStopping { .. }
            | Self::ToolRequestNotFound { .. }
            | Self::ToolRequestAlreadyResolved { .. }
            | Self::ToolRequestNotEarliestUndecided { .. }
            | Self::ToolRequestNotInSession { .. }
            | Self::ToolRequestNotDelegateDenied { .. }
            | Self::ToolRequestNotTerminallyDenied { .. }
            | Self::ToolDenialAlreadyOverridden { .. }
            | Self::DelegationRequestNotInTurn { .. }
            | Self::DelegationToolRequestNotExecutable { .. }
            | Self::DelegationSpawnConflict { .. }
            | Self::DelegatedChildIdentityCollision { .. }
            | Self::DelegationRelationNotFound { .. }
            | Self::DelegationAwaitConflict { .. }
            | Self::DelegationMessageConflict { .. }
            | Self::DelegationMessageIdentityCollision { .. }
            | Self::DelegationEventOrdinalExhausted { .. }
            | Self::DelegationDeliverySequenceExhausted { .. }
            | Self::DefaultsVersionMismatch { .. }
            | Self::UnknownModelAlias { .. }
            | Self::AcceptancePositionExhausted { .. }
            | Self::DefaultsVersionExhausted { .. }
            | Self::ImportedConversationNotFound { .. }
            | Self::ImportedFrontierPositionOutOfRange { .. } => false,
        }
    }
}

#[derive(signalbox_derive::Accessors)]
/// Presence-checked rejection detail on an error message.
///
/// An absent value omits the JSON member. A present JSON `null` is rejected
/// rather than being treated as absence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ErrorDetail(
    /// Returns the typed rejection detail when present.
    #[get(copy, as = "value")]
    Option<RejectionDetail>,
);

impl ErrorDetail {
    /// Omits rejection detail from a non-rejection error.
    pub const fn none() -> Self {
        Self(None)
    }

    /// Includes exact durable-rejection detail.
    pub const fn rejected(detail: RejectionDetail) -> Self {
        Self(Some(detail))
    }

    /// Includes typed import evidence on an invalid request.
    pub const fn invalid_request(detail: RejectionDetail) -> Self {
        Self(Some(detail))
    }

    const fn is_absent(&self) -> bool {
        self.0.is_none()
    }
}

impl Serialize for ErrorDetail {
    fn serialize<SerializerT>(
        &self,
        serializer: SerializerT,
    ) -> Result<SerializerT::Ok, SerializerT::Error>
    where
        SerializerT: Serializer,
    {
        match self.0 {
            Some(detail) => detail.serialize(serializer),
            None => serializer.serialize_unit(),
        }
    }
}

impl<'de> Deserialize<'de> for ErrorDetail {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        RejectionDetail::deserialize(deserializer).map(Self::rejected)
    }
}

/// Closed durable update event family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionEvent {
    /// Session creation committed.
    SessionCreated {},
    /// One defaults replacement changed model selection or settings.
    SessionModelSettingsChanged {
        command_id: CommandId,
        prior_defaults_version: CanonicalU64,
        installed_defaults_version: CanonicalU64,
        prior_model: ModelSelection,
        installed_model: ModelSelection,
        prior_settings: ModelSettingsSnapshot,
        installed_settings: ModelSettingsSnapshot,
        caller_override: ModelSettingsOverlay,
        adjustments: Vec<ModelChangeAdjustment>,
    },
    /// One accepted origin turn froze complete model settings.
    TurnModelSettingsResolved {
        accepted_input_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        defaults_version: CanonicalU64,
        requested_model: ModelSelection,
        selected_direct_id: CanonicalUuid,
        per_call_override: ModelSettingsOverlay,
        settings: ModelSettingsSnapshot,
        adjusted_from_selection_id: Option<CanonicalUuid>,
        adjustments: Vec<ModelChangeAdjustment>,
    },
    /// User input acceptance and its queued turn committed.
    InputAccepted {
        /// Accepted input.
        accepted_input_id: CanonicalUuid,
        /// Queued origin turn.
        turn_id: CanonicalUuid,
        /// Immutable session acceptance position.
        acceptance_position: CanonicalU64,
        /// Exact ordered accepted user parts.
        content: UserInputContent,
    },
    /// A queued goal turn became intentionally ineligible.
    GoalTurnRetired {
        /// Exact immutable queued turn retired by a goal transition.
        turn_id: CanonicalUuid,
    },
    /// A queued turn became active.
    TurnActivated {
        /// Activated turn.
        turn_id: CanonicalUuid,
        /// Initial current attempt.
        current_attempt_id: CanonicalUuid,
    },
    /// Model call advanced.
    ModelCallTransition {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Advancing call.
        model_call_id: CanonicalUuid,
        /// Exact committed state.
        state: ModelCallState,
    },
    /// A tool batch crossed one durable presentation boundary.
    ToolBatchTransition {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Model call that proposed the batch.
        model_call_id: CanonicalUuid,
        /// Exact committed batch state.
        state: ToolBatchState,
    },
    /// A runner placement or its exact connection changed follower-visible state.
    RunnerStateTransition {
        /// Exact runner named by the transition.
        runner_id: CanonicalUuid,
        /// Positive placement revision whose immutable facts are projected.
        placement_revision: RunnerPlacementRevision,
        /// Placement-selected sandbox profile.
        sandbox_profile: RunnerSandboxProfile,
        /// Caller-selected directory, null when the runner default was selected.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        working_directory: Option<RunnerWorkingDirectory>,
        /// Exact closed transition state.
        state: RunnerStateTransitionState,
    },
    /// One explicit tool approval decision committed with full provenance.
    ToolApprovalDecided {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Exact logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact recorded decision.
        decision: ToolApprovalEventDecision,
        /// Exact user or delegate decider.
        decider: ToolApprovalEventDecider,
        /// Exact judge rationale, absent for a user decision.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        rationale: Option<String>,
    },
    /// One append-only context compaction committed.
    ContextCompacted {
        /// Exact compaction provenance record.
        context_compaction_id: CanonicalUuid,
        /// Dedicated producing model call.
        model_call_id: CanonicalUuid,
        /// One-based final summarized position.
        through_position: CanonicalU64,
        /// Appended semantic summary entry.
        summary_entry_id: CanonicalUuid,
        /// Complete result frontier.
        result_frontier_id: CanonicalUuid,
    },
    /// Turn completed.
    TurnCompleted {
        /// Completed turn.
        turn_id: CanonicalUuid,
        /// Outcome-authoritative call.
        model_call_id: CanonicalUuid,
        /// Final completion marker.
        completion_entry_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn failed.
    TurnFailed {
        /// Failed turn.
        turn_id: CanonicalUuid,
        /// Failure marker.
        failure_entry_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn was refused.
    TurnRefused {
        /// Refused turn.
        turn_id: CanonicalUuid,
        /// Outcome-authoritative call.
        model_call_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn was cancelled.
    TurnCancelled {
        /// Cancelled turn.
        turn_id: CanonicalUuid,
        /// Semantic cancellation marker.
        cancellation_entry_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn stopped with an ambiguous model call requiring reconciliation.
    TurnReconciliationRequired {
        /// Reconciliation-required turn.
        turn_id: CanonicalUuid,
        /// Exact ambiguous terminal model call.
        model_call_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn stopped with an ambiguous tool attempt requiring reconciliation.
    TurnToolReconciliationRequired {
        /// Reconciliation-required turn.
        turn_id: CanonicalUuid,
        /// Exact ambiguous terminal tool attempt.
        tool_attempt_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// A parent committed one child relationship and lifecycle policy.
    ChildSpawned {
        /// Exact spawning tool request and relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Spawned child session.
        child_session_id: CanonicalUuid,
        /// Parent-chosen relationship lifecycle policy.
        relationship: DelegationPolicy,
    },
    /// A parent registered one foreground or background wait.
    ChildWaiting {
        /// Exact await tool request.
        await_request_id: CanonicalUuid,
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Child being awaited.
        child_session_id: CanonicalUuid,
        /// Wait delivery mode.
        mode: DelegationWaitMode,
    },
    /// One bidirectional relationship message became durable for its recipient.
    SessionMessage {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Message identity.
        message_id: CanonicalUuid,
        /// Sending session.
        sender_session_id: CanonicalUuid,
        /// Receiving session.
        recipient_session_id: CanonicalUuid,
        /// Relationship-local message ordinal.
        ordinal: CanonicalU64,
        /// Recipient-wide delivery sequence.
        delivery_sequence: CanonicalU64,
        /// Exact delivered content.
        content: String,
    },
    /// A terminal child result became durable for its parent.
    ChildResult {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Terminal child.
        child_session_id: CanonicalUuid,
        /// Typed terminal result outcome.
        outcome: DelegationOutcome,
        /// Delivered content for a successful result only.
        content: Option<String>,
        /// Typed reason for the terminal result.
        reason: DelegationReason,
        /// Exact child-turn or parent-command provenance.
        provenance: DelegationProvenance,
    },
    /// Parent termination evaluated one relationship edge.
    ChildLifecycleDisposition {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Evaluated child.
        child_session_id: CanonicalUuid,
        /// Typed relationship outcome.
        outcome: DelegationOutcome,
        /// Typed reason for evaluating this relationship edge.
        reason: DelegationReason,
        /// Exact parent command provenance.
        provenance: DelegationProvenance,
    },
}

fn validate_delegation_session_event(
    session_id: CanonicalUuid,
    event: &SessionEvent,
) -> Result<(), FrameValidationError> {
    let valid = match event {
        SessionEvent::ChildSpawned {
            child_session_id, ..
        }
        | SessionEvent::ChildWaiting {
            child_session_id, ..
        } => *child_session_id != session_id,
        SessionEvent::SessionMessage {
            sender_session_id,
            recipient_session_id,
            ordinal,
            delivery_sequence,
            content,
            ..
        } => {
            *recipient_session_id == session_id
                && sender_session_id != recipient_session_id
                && ordinal.value() > 0
                && delivery_sequence.value() > 0
                && delegation_content_is_valid(content)
        }
        SessionEvent::ChildResult {
            child_session_id,
            outcome,
            content,
            reason,
            provenance,
            ..
        } => {
            *child_session_id != session_id
                && child_result_shape_is_valid(
                    session_id,
                    *child_session_id,
                    *outcome,
                    content,
                    *reason,
                    provenance,
                )
        }
        SessionEvent::ChildLifecycleDisposition {
            child_session_id,
            outcome,
            reason,
            provenance,
            ..
        } => {
            matches!(
                reason,
                DelegationReason::ParentStopped | DelegationReason::ParentCancelled
            ) && if *child_session_id == session_id {
                // A descendant cascade also addresses the terminalization to
                // the child itself so that live child followers observe it.
                // That row carries the parent's cascade provenance, so the
                // provenance parent is a different session than this header.
                matches!(
                    outcome,
                    DelegationOutcome::Stopped | DelegationOutcome::Cancelled
                ) && delegation_provenance_parent(provenance)
                    .is_some_and(|parent| parent != session_id)
                    && parent_delegation_provenance_has_cascade(provenance)
            } else {
                matches!(
                    outcome,
                    DelegationOutcome::Stopped
                        | DelegationOutcome::Cancelled
                        | DelegationOutcome::AlreadyTerminal
                        | DelegationOutcome::ContinueRunning
                ) && parent_delegation_provenance_is_cascade(session_id, provenance)
            }
        }
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::DelegationShape)
    }
}

fn validate_settings_event(event: &SessionEvent) -> Result<(), FrameValidationError> {
    match event {
        SessionEvent::SessionModelSettingsChanged {
            prior_defaults_version,
            installed_defaults_version,
            prior_model,
            installed_model,
            prior_settings,
            installed_settings,
            caller_override,
            adjustments,
            ..
        } => {
            prior_settings.validate_defaults()?;
            installed_settings.validate_defaults()?;
            validate_adjustments(adjustments)?;
            let validation_changed = matches!(
                (
                    prior_settings.validated_for_selection_id,
                    installed_settings.validated_for_selection_id,
                ),
                (Some(prior), Some(installed)) if prior != installed
            );
            let copied_precedence = ModelSettingsPrecedence {
                per_call: prior_settings.precedence.per_call,
                session: prior_settings.precedence.session,
                profile: installed_settings.precedence.profile,
                global_default: installed_settings.precedence.global_default,
            };
            let unadjusted_precedence = ModelSettingsPrecedence {
                session: overlay_inheriting_from(
                    *caller_override,
                    prior_settings.precedence.session,
                ),
                ..copied_precedence
            };
            let provenance_matches = apply_wire_adjustments(unadjusted_precedence, adjustments)
                .is_some_and(|expected| expected == installed_settings.precedence);
            if prior_defaults_version.value() == 0
                || prior_defaults_version.value().checked_add(1)
                    != Some(installed_defaults_version.value())
                || (prior_model == installed_model && prior_settings == installed_settings)
                || !snapshot_matches_model(prior_model, prior_settings)
                || !snapshot_matches_model(installed_model, installed_settings)
                || !provenance_matches
                || (!adjustments.is_empty() && !validation_changed)
                || adjustments_target_explicit_overlay(*caller_override, adjustments)
            {
                return Err(FrameValidationError::ModelSettingsShape);
            }
        }
        SessionEvent::TurnModelSettingsResolved {
            defaults_version,
            requested_model,
            selected_direct_id,
            per_call_override,
            settings,
            adjusted_from_selection_id,
            adjustments,
            ..
        } => validate_turn_settings_payload(
            *defaults_version,
            requested_model,
            *selected_direct_id,
            *per_call_override,
            settings,
            *adjusted_from_selection_id,
            adjustments,
        )?,
        SessionEvent::ToolApprovalDecided {
            decision,
            decider,
            rationale,
            ..
        } => validate_tool_approval_event_shape(decision, decider, rationale)?,
        SessionEvent::InputAccepted { content, .. } => content.validate()?,
        SessionEvent::SessionCreated {}
        | SessionEvent::GoalTurnRetired { .. }
        | SessionEvent::TurnActivated { .. }
        | SessionEvent::ModelCallTransition { .. }
        | SessionEvent::ToolBatchTransition { .. }
        | SessionEvent::RunnerStateTransition { .. }
        | SessionEvent::ContextCompacted { .. }
        | SessionEvent::TurnCompleted { .. }
        | SessionEvent::TurnFailed { .. }
        | SessionEvent::TurnRefused { .. }
        | SessionEvent::TurnCancelled { .. }
        | SessionEvent::TurnReconciliationRequired { .. }
        | SessionEvent::TurnToolReconciliationRequired { .. }
        | SessionEvent::ChildSpawned { .. }
        | SessionEvent::ChildWaiting { .. }
        | SessionEvent::SessionMessage { .. }
        | SessionEvent::ChildResult { .. }
        | SessionEvent::ChildLifecycleDisposition { .. } => {}
    }
    Ok(())
}

/// Closed versioned server message family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerMessage {
    /// Session creation receipt.
    SessionCreated {
        /// Created session.
        session_id: CanonicalUuid,
        /// Complete settings snapshot installed as defaults version one.
        model_settings: ModelSettingsSnapshot,
    },
    /// Commissioned-session receipt: the composite committed or replayed.
    SessionCommissioned {
        /// Created session.
        session_id: CanonicalUuid,
        /// Append-only commissioned-dispatch record carrying the fence.
        dispatch_id: CanonicalUuid,
    },
    /// A durable session-lifecycle command applied.
    SessionLifecycleCommandApplied {
        /// Target session.
        session_id: CanonicalUuid,
        /// What the command did.
        effect: SessionLifecycleEffect,
    },
    /// One delegated child spawn was recorded or equally replayed.
    SessionSpawned {
        /// Exact logical spawn tool request.
        tool_request_id: CanonicalUuid,
        /// Created child identity.
        child_session_id: CanonicalUuid,
        /// Exact immutable relationship policy.
        relationship: DelegationPolicy,
    },
    /// One child-delivery registration was recorded or equally replayed.
    SessionAwaitRegistered {
        /// Exact logical await tool request.
        tool_request_id: CanonicalUuid,
        /// Related child identity.
        child_session_id: CanonicalUuid,
        /// Exact registered delivery mode.
        mode: DelegationWaitMode,
    },
    /// One child outcome was delivered directly to a foreground await.
    ChildResult {
        /// Exact logical await tool request receiving the result.
        await_request_id: CanonicalUuid,
        /// Logical tool request that created the relationship.
        spawning_request_id: CanonicalUuid,
        /// Child whose terminal result was delivered.
        child_session_id: CanonicalUuid,
        /// Closed result outcome.
        outcome: DelegationOutcome,
        /// Exact returned content only for `returned`.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        content: Option<String>,
        /// Closed reason correlated with the outcome.
        reason: DelegationReason,
        /// Exact child-turn or parent-command authority.
        provenance: DelegationProvenance,
    },
    /// One relationship message was recorded or equally replayed.
    SessionMessageSent {
        /// Exact logical message tool request.
        tool_request_id: CanonicalUuid,
        /// Immutable message identity.
        message_id: CanonicalUuid,
        /// Exact relationship direction.
        direction: DelegationMessageDirection,
        /// Positive contiguous relationship event ordinal.
        ordinal: CanonicalU64,
        /// Positive recipient-wide delivery sequence.
        delivery_sequence: CanonicalU64,
    },
    /// One immutable placement update was appended or equally replayed.
    SessionPlacementUpdated {
        session_id: CanonicalUuid,
        placement_version: CanonicalU64,
        placement: SessionPlacement,
    },
    /// Input acceptance receipt.
    InputSubmitted {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Accepted input.
        accepted_input_id: CanonicalUuid,
        /// Immutable acceptance position.
        acceptance_position: CanonicalU64,
        /// Created origin turn.
        turn_id: CanonicalUuid,
        /// Complete settings snapshot frozen for the origin turn.
        model_settings: ModelSettingsSnapshot,
    },
    /// Configuration-free steering acceptance receipt.
    SteeringSubmitted {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Accepted input.
        accepted_input_id: CanonicalUuid,
        /// Immutable acceptance position.
        acceptance_position: CanonicalU64,
        /// Exact active turn the steering is bound to.
        source_turn_id: CanonicalUuid,
    },
    /// A durable user goal command appended one event.
    GoalTransitionApplied {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Appended event position.
        event_ordinal: CanonicalU64,
        /// Generation acted on by the event.
        generation: CanonicalU64,
    },
    /// Begins one complete goal-history snapshot.
    GoalHistoryStart {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Current immutable statement generation.
        current_generation: CanonicalU64,
        /// Current immutable statement.
        current_statement: String,
    },
    /// Carries the current lifecycle state in a frame bounded independently from text.
    GoalHistoryState {
        /// Current derived lifecycle state.
        current_state: GoalLifecycleState,
    },
    /// One ordered event in a goal-history snapshot.
    GoalHistoryItem {
        /// Positive contiguous event position.
        event_ordinal: CanonicalU64,
        /// Statement generation acted on by the event.
        generation: CanonicalU64,
        /// Exact event payload and provenance.
        event: GoalHistoryEvent,
    },
    /// Completes one goal-history snapshot.
    GoalHistoryEnd {
        /// Number of preceding history items.
        event_count: CanonicalU64,
    },
    /// Begins a session-summary sequence.
    SessionsStart {},
    /// One current session summary.
    SessionSummary {
        /// Session identity.
        session_id: CanonicalUuid,
        /// Current defaults version.
        defaults_version: CanonicalU64,
        /// Current model-selection request.
        model_selection: ModelSelection,
        /// Current immutable placement-history version.
        placement_version: CanonicalU64,
        /// Current opt-in placement decision.
        placement: SessionPlacement,
        /// Complete current runner projection, null for daemon-only sessions.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        runner: Option<RunnerProjection>,
    },
    /// Completes a session-summary sequence.
    SessionsEnd {
        /// Number of preceding summaries.
        session_count: CanonicalU64,
    },
    /// One member of a coherent operator-status snapshot.
    OperatorStatus(Box<OperatorStatusMessage>),
    /// Begins the available-template sequence.
    TemplatesStart {},
    /// One available static template summary.
    TemplateSummary {
        /// Validated template name.
        name: String,
        /// Positive operator-assigned bundle version.
        version: CanonicalU64,
    },
    /// Completes the available-template sequence.
    TemplatesEnd {
        /// Number of preceding summaries.
        template_count: CanonicalU64,
    },
    /// Client-relevant deployment policy, with null denoting unbounded.
    DeploymentLimits {
        #[serde(deserialize_with = "deserialize_required_nullable")]
        max_message_utf8_bytes: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        max_system_prompt_utf8_bytes: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        terminal_input_channel_capacity: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        min_metadata_page_size: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        max_metadata_page_size: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        max_review_findings_per_run: Option<CanonicalU64>,
    },
    /// Begins one bounded metadata-summary page.
    SessionMetadataPageStart {},
    /// One current session metadata summary.
    SessionMetadataSummary {
        /// Session identity.
        session_id: CanonicalUuid,
        /// Current defaults version.
        defaults_version: CanonicalU64,
        /// Current model-selection request.
        model_selection: ModelSelection,
        /// Whether the current defaults blanket-approve dangerous tools.
        dangerous_tool_auto_approval: bool,
        /// Optional exact title.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title: Option<String>,
        /// Exact sorted flat tags.
        #[serde(deserialize_with = "deserialize_session_metadata_tags")]
        tags: Vec<String>,
        /// Whether the session is archived.
        archived: bool,
        /// Last replacement writer, absent only before the first write.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        last_writer: Option<MetadataLastWriter>,
    },
    /// Completes one bounded metadata-summary page.
    SessionMetadataPageEnd {
        /// Number of preceding summaries.
        session_count: CanonicalU64,
        /// Exclusive cursor for another page, or null when no later match
        /// existed in this page snapshot.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        next_after_session_id: Option<CanonicalUuid>,
    },
    /// Begins one bounded unified conversation-summary page.
    ConversationPageStart {},
    /// One unified conversation summary.
    ConversationSummary {
        /// Closed per-origin summary.
        conversation: ConversationSummary,
    },
    /// Completes one bounded unified conversation-summary page.
    ConversationPageEnd {
        /// Number of preceding summaries.
        conversation_count: CanonicalU64,
        /// Exclusive cursor for another page, or null when no later match
        /// existed in this page snapshot.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        next_after: Option<ConversationCursor>,
    },
    /// Begins the configured model-alias sequence.
    ModelAliasesStart {},
    /// One configured alias and the direct selection it currently names.
    ModelAliasSummary {
        /// Stable alias identity selectable by creation commands.
        alias_id: CanonicalUuid,
        /// Current deployment-owned direct selection target.
        selection_id: CanonicalUuid,
    },
    /// Completes the configured model-alias sequence.
    ModelAliasesEnd {
        /// Number of preceding alias summaries.
        alias_count: CanonicalU64,
    },
    /// Begins the configured model-capability sequence.
    ModelCapabilitiesStart {},
    /// One direct selection and its exact client-visible capabilities.
    ModelCapabilityItem {
        selection_id: CanonicalUuid,
        capabilities: ModelCapabilities,
    },
    /// Completes the configured model-capability sequence.
    ModelCapabilitiesEnd { capability_count: CanonicalU64 },
    /// One complete current metadata read.
    SessionMetadata {
        /// Selected session.
        session_id: CanonicalUuid,
        /// Complete current metadata object.
        metadata: SessionMetadata,
        /// Last replacement writer, absent only before the first write.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        last_writer: Option<MetadataLastWriter>,
    },
    /// One successful complete metadata replacement receipt.
    SessionMetadataReplaced {
        /// Updated session.
        session_id: CanonicalUuid,
        /// Complete committed metadata object.
        metadata: SessionMetadata,
        /// Non-null last replacement writer.
        last_writer: MetadataLastWriter,
    },
    /// One successful forward-only session-defaults replacement receipt.
    SessionDefaultsReplaced {
        /// Updated session.
        session_id: CanonicalUuid,
        /// Newly installed immutable defaults epoch.
        defaults_version: CanonicalU64,
        /// Complete committed model selection.
        model_selection: ModelSelection,
        /// Complete settings snapshot installed on the new epoch.
        model_settings: ModelSettingsSnapshot,
        /// Complete committed dangerous-tool blanket-auto posture.
        dangerous_tool_auto_approval: bool,
        /// Complete committed system prompt; required null-or-text member.
        #[serde(default, skip_serializing_if = "SystemPromptMember::is_absent")]
        system_prompt: SystemPromptMember,
    },
    /// One complete current or named immutable session-defaults epoch.
    SessionDefaults {
        /// Selected session.
        session_id: CanonicalUuid,
        /// The read immutable defaults epoch.
        defaults_version: CanonicalU64,
        /// Complete model selection on that epoch.
        model_selection: ModelSelection,
        /// Complete settings snapshot stored on the selected epoch.
        model_settings: ModelSettingsSnapshot,
        /// Complete dangerous-tool blanket-auto posture on that epoch.
        dangerous_tool_auto_approval: bool,
        /// Exact optional system prompt on that epoch.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        system_prompt: Option<SystemPromptText>,
    },
    /// One recorded user tool-decision receipt.
    ///
    /// The receipt mirrors the recorded applied result exactly; an equal
    /// command replay returns this same projection.
    ToolRequestDecided {
        /// Decided logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact recorded decision.
        decision: ToolDecision,
    },
    /// One recorded recorded-override receipt.
    ///
    /// The receipt mirrors the recorded applied result exactly; an equal
    /// command replay returns this same projection.
    ToolDenialOverridden {
        /// Overridden delegate-denied tool request.
        tool_request_id: CanonicalUuid,
    },
    /// One completed append-only context-compaction receipt.
    SessionCompacted {
        /// Compacted session.
        session_id: CanonicalUuid,
        /// Immutable compaction identity.
        context_compaction_id: CanonicalUuid,
        /// Dedicated producing model call.
        model_call_id: CanonicalUuid,
        /// One-based exact through position in the source frontier.
        through_position: CanonicalU64,
        /// Appended summary semantic entry.
        summary_entry_id: CanonicalUuid,
        /// Complete source-plus-summary result frontier.
        result_frontier_id: CanonicalUuid,
    },
    /// One new immutable imported conversation was inserted.
    ConversationImportInserted {
        /// Newly durable imported-conversation identity.
        imported_conversation_id: CanonicalUuid,
    },
    /// The exact imported snapshot was already durable.
    ConversationImportAlreadyImported {
        /// Existing durable imported-conversation identity.
        imported_conversation_id: CanonicalUuid,
    },
    /// One per-connection chunked import was initialized.
    ConversationImportBegun {
        /// Exact total source size admitted from the begin request.
        declared_size_bytes: CanonicalU64,
    },
    /// One source chunk was appended to the in-progress import.
    ConversationImportAppended {
        /// Exact total source bytes observed after this append.
        assembled_size_bytes: CanonicalU64,
    },
    /// One per-connection chunked import was discarded.
    ConversationImportAborted {},
    /// One connection-local immutable-blob upload was initialized.
    BlobUploadBegun {
        expected_digest: CanonicalBlobDigest,
        expected_length_bytes: CanonicalU64,
    },
    /// The routed store already held a verified replica, so no chunks are owed.
    BlobUploadAlreadyPresent {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
    },
    /// One bounded chunk was appended to the connection-local spool.
    BlobUploadAppended {
        assembled_length_bytes: CanonicalU64,
    },
    /// The exact assembled bytes were published and catalogued.
    BlobUploadCommitted {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
    },
    /// One connection-local immutable-blob upload was discarded.
    BlobUploadAborted {},
    /// Bounded catalog facts for one immutable identity.
    BlobMetadata {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
        replica_count: CanonicalU64,
    },
    /// One exact verified byte range.
    #[serde(rename = "blob_chunk")]
    BlobChunkRead {
        digest: CanonicalBlobDigest,
        offset_bytes: CanonicalU64,
        bytes: BlobChunk,
    },
    /// Begins one imported-conversation entry sequence.
    ImportedConversationStart {
        /// Inspected imported conversation.
        imported_conversation_id: CanonicalUuid,
    },
    /// One imported entry as the inspection projection presents it.
    ImportedConversationEntry {
        /// One-based imported position, exactly the ordinal
        /// `create_session_from_imported_frontier` consumes.
        position: CanonicalU64,
        /// Immutable imported entry identity.
        imported_entry_id: CanonicalUuid,
        /// Exact source-speaker attestation.
        source_speaker: ImportedSourceSpeaker,
        /// Normalized content variant.
        content_kind: ImportedContentKind,
        /// Bounded preview of exact attested text, or null when this entry
        /// carries no exact attested text.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        text_preview: Option<ImportedTextPreview>,
    },
    /// Completes one imported-conversation entry sequence.
    ImportedConversationEnd {
        /// Inspected imported conversation.
        imported_conversation_id: CanonicalUuid,
        /// Number of preceding entries, equal to the greatest selectable
        /// position.
        entry_count: CanonicalU64,
    },
    /// Begins one transcript snapshot sequence.
    TranscriptSnapshotStart {
        /// Selected session.
        session_id: CanonicalUuid,
        /// Snapshot outbox cursor.
        cursor: CanonicalU64,
        /// Complete current runner placement, or null for a daemon-only session.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        runner: Option<RunnerProjection>,
    },
    /// One authoritative turn projection.
    TranscriptTurn {
        /// Immutable turn identity.
        turn_id: CanonicalUuid,
        /// Immutable acceptance order.
        acceptance_position: CanonicalU64,
        /// Complete frozen settings for a settings-aware turn, or null for a
        /// turn committed before settings evidence existed.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        model_settings: Option<TurnModelSettingsSnapshot>,
        /// Exact lifecycle state.
        state: TurnState,
    },
    /// Exact independently nullable token fields for one terminal model call.
    TranscriptModelCallUsage {
        /// Zero-based model-call evidence index in this snapshot.
        model_call_index: CanonicalU64,
        /// Turn that owns the terminal model call.
        turn_id: CanonicalUuid,
        /// Immutable model-call identity.
        model_call_id: CanonicalUuid,
        /// Closed source vocabulary for the independently nullable counts.
        usage_provenance: UsageProvenance,
        /// Exact independently nullable fields from the named provenance.
        usage: ModelCallTokenUsage,
        /// Read-time configured-rate derivation, required null when unavailable.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        cost: Option<ModelCallDollarCost>,
    },
    /// Completes the model-call evidence section of one transcript snapshot.
    TranscriptModelCallsEnd {
        /// Number of preceding model-call usage messages.
        model_call_count: CanonicalU64,
    },
    /// One non-text frontier member.
    TranscriptEntry {
        /// Zero-based frontier member index.
        entry_index: CanonicalU64,
        /// Entry source session.
        source_session_id: CanonicalUuid,
        /// Semantic entry identity.
        entry_id: CanonicalUuid,
        /// Exact marker payload.
        entry: TranscriptEntry,
    },
    /// One atomic native user entry with exact ordered multipart content.
    TranscriptUserEntry {
        /// Zero-based frontier member index.
        entry_index: CanonicalU64,
        /// Entry source session.
        source_session_id: CanonicalUuid,
        /// Semantic entry identity.
        entry_id: CanonicalUuid,
        /// Exact accepted input.
        accepted_input_id: CanonicalUuid,
        /// Origin turn.
        turn_id: CanonicalUuid,
        /// Canonical ordered user content.
        content: UserInputContent,
    },
    /// Begins one text-bearing frontier member.
    TranscriptTextEntry {
        /// Zero-based frontier member index.
        entry_index: CanonicalU64,
        /// Entry source session.
        source_session_id: CanonicalUuid,
        /// Semantic entry identity.
        entry_id: CanonicalUuid,
        /// Exact text-entry metadata.
        entry: TranscriptTextEntry,
    },
    /// One bounded text fragment.
    TranscriptContent {
        /// Frontier member index.
        entry_index: CanonicalU64,
        /// Zero-based fragment index.
        fragment_index: CanonicalU64,
        /// Whether this is the entry's final fragment.
        final_fragment: bool,
        /// Exact content fragment.
        content_fragment: ContentFragment,
    },
    /// Completes one transcript snapshot.
    TranscriptSnapshotEnd {
        /// Selected session.
        session_id: CanonicalUuid,
        /// Snapshot outbox cursor.
        cursor: CanonicalU64,
        /// Number of preceding turn messages.
        turn_count: CanonicalU64,
        /// Number of complete semantic entries.
        entry_count: CanonicalU64,
    },
    /// One committed update after a follow snapshot.
    SessionEvent {
        /// Global durable cursor.
        cursor: CanonicalU64,
        /// Owning session.
        session_id: CanonicalUuid,
        /// Exact typed update.
        event: SessionEvent,
    },
    /// One cursorless, process-local provider text fragment.
    ProviderTextDelta {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Active turn receiving the provider response.
        turn_id: CanonicalUuid,
        /// Correlated model call producing the response.
        model_call_id: CanonicalUuid,
        /// Provider part position this fragment extends.
        part_index: CanonicalU64,
        /// One bounded fragment of already-redacted provider text.
        content: ContentFragment,
    },
    /// One immutable target registration was recorded or equally replayed.
    ReviewTargetCreated {
        /// Registered target.
        target_id: CanonicalUuid,
    },
    /// One run and its sole pass were admitted or equally replayed.
    ReviewRunStarted {
        /// Admitted run.
        run_id: CanonicalUuid,
        /// Admitted pass.
        pass_id: CanonicalUuid,
    },
    /// One queued run and pass were atomically activated or equally replayed.
    ReviewPassActivated {
        /// Activated run.
        run_id: CanonicalUuid,
        /// Activated pass.
        pass_id: CanonicalUuid,
    },
    /// One pass without another typed result was terminalized.
    ReviewPassCompleted {
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        state: ReviewPassLifecycle,
    },
    /// One read-only result and complete finding inventory were committed.
    ReviewFindingsRecorded {
        /// Concluding run.
        run_id: CanonicalUuid,
        /// Concluding pass.
        pass_id: CanonicalUuid,
        /// Exact committed finding count.
        finding_count: CanonicalU64,
    },
    /// One finding disposition was committed.
    ReviewFindingEventRecorded {
        /// Updated finding.
        finding_id: CanonicalUuid,
        /// Current derived status.
        status: ReviewFindingStatus,
    },
    /// One pre-effect external-link reservation was recorded.
    ReviewExternalLinkReserved {
        /// Stable reservation identity.
        external_link_id: CanonicalUuid,
    },
    /// One provider object identity was attached.
    ReviewExternalLinkAttached {
        /// Consumed reservation identity.
        external_link_id: CanonicalUuid,
        /// Canonical provider object key.
        external_object: String,
    },
    /// One immutable target read.
    ReviewTarget {
        /// Complete target snapshot.
        target: ReviewTargetSnapshot,
    },
    /// One run and its optional pass read.
    ReviewRun {
        /// Complete run snapshot.
        run: ReviewRunSnapshot,
        /// Complete pass snapshot after admission.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        pass: Option<ReviewPassSnapshot>,
    },
    /// One complete finding read.
    ReviewFinding {
        /// Complete finding snapshot.
        finding: ReviewFindingSnapshot,
    },
    /// Begins one finding list sequence.
    ReviewFindingsStart {
        /// Selected run.
        run_id: CanonicalUuid,
    },
    /// One finding in identity order.
    ReviewFindingItem {
        /// Complete finding snapshot.
        finding: ReviewFindingSnapshot,
    },
    /// Completes one finding list sequence.
    ReviewFindingsEnd {
        /// Number of preceding items.
        finding_count: CanonicalU64,
    },
    /// One orchestration attempt was admitted or equally replayed.
    ReviewOrchestrationStarted { attempt_id: CanonicalUuid },
    /// One orchestration attempt advanced or equally replayed.
    ReviewOrchestrationAdvanced {
        attempt_id: CanonicalUuid,
        state: ReviewOrchestrationState,
    },
    /// One complete orchestration attempt read.
    ReviewOrchestration {
        snapshot: ReviewOrchestrationSnapshot,
    },
    /// Stable, sanitized failure.
    Error {
        /// Stable error code.
        code: ErrorCode,
        /// Non-sensitive human diagnostic.
        message: String,
        /// Typed durable-rejection or conversation-import failure evidence.
        #[serde(default, skip_serializing_if = "ErrorDetail::is_absent")]
        detail: ErrorDetail,
    },
}

impl ServerMessage {
    fn validate(&self) -> Result<(), FrameValidationError> {
        validate_operator_status_message(self)?;
        match self {
            Self::SessionCreated { model_settings, .. } => model_settings.validate_defaults()?,
            Self::SessionAwaitRegistered {
                mode: DelegationWaitMode::Foreground,
                ..
            } => {
                return Err(FrameValidationError::DelegationShape);
            }
            Self::ChildResult {
                await_request_id,
                spawning_request_id,
                child_session_id,
                outcome,
                content,
                reason,
                provenance,
            } if await_request_id == spawning_request_id
                || !direct_child_result_shape_is_valid(
                    *child_session_id,
                    *outcome,
                    content,
                    *reason,
                    provenance,
                ) =>
            {
                return Err(FrameValidationError::DelegationShape);
            }
            Self::SessionMessageSent {
                ordinal,
                delivery_sequence,
                ..
            } if ordinal.value() < 2 || delivery_sequence.value() == 0 => {
                return Err(FrameValidationError::DelegationShape);
            }
            Self::SessionAwaitRegistered {
                mode: DelegationWaitMode::Background,
                ..
            }
            | Self::ChildResult { .. }
            | Self::SessionMessageSent { .. } => {}
            Self::InputSubmitted { model_settings, .. } => model_settings.validate()?,
            Self::SessionDefaultsReplaced {
                model_selection,
                model_settings,
                ..
            }
            | Self::SessionDefaults {
                model_selection,
                model_settings,
                ..
            } => {
                model_settings.validate_defaults()?;
                if !snapshot_matches_model(model_selection, model_settings) {
                    return Err(FrameValidationError::ModelSettingsShape);
                }
            }
            Self::SessionEvent {
                session_id, event, ..
            } => {
                validate_settings_event(event)?;
                validate_delegation_session_event(*session_id, event)?;
            }
            Self::TranscriptTurn {
                turn_id,
                model_settings,
                state,
                ..
            } => {
                if let TurnState::Queued { content, .. } = state {
                    content.validate()?;
                }
                if let Some(settings) = model_settings {
                    settings.validate()?;
                    if settings.turn_id != *turn_id
                        || (matches!(
                            state,
                            TurnState::Queued {
                                accepted_input_id,
                                ..
                            } if settings.accepted_input_id != *accepted_input_id
                        ))
                    {
                        return Err(FrameValidationError::ModelSettingsShape);
                    }
                }
            }
            Self::TranscriptEntry {
                entry:
                    TranscriptEntry::AssistantToolUse {
                        approval: Some(approval),
                        ..
                    },
                ..
            } => validate_tool_approval_event_shape(
                &approval.decision,
                &approval.decider,
                &approval.rationale,
            )?,
            Self::TranscriptUserEntry { content, .. } => content.validate()?,
            Self::GoalTransitionApplied {
                event_ordinal,
                generation,
                ..
            }
            | Self::GoalHistoryItem {
                event_ordinal,
                generation,
                ..
            } if event_ordinal.value() == 0 || generation.value() == 0 => {
                return Err(FrameValidationError::GoalShape);
            }
            Self::GoalHistoryStart {
                current_generation,
                current_statement,
                ..
            } => {
                if current_generation.value() == 0 {
                    return Err(FrameValidationError::GoalShape);
                }
                validate_goal_text(current_statement)?;
            }
            Self::GoalHistoryState { current_state } => validate_goal_state(current_state)?,
            Self::GoalHistoryItem { event, .. } => validate_goal_event(event)?,
            Self::GoalHistoryEnd { event_count } if event_count.value() == 0 => {
                return Err(FrameValidationError::GoalShape);
            }
            Self::SessionMetadataSummary {
                title,
                tags,
                archived,
                last_writer,
                ..
            } => {
                let mut total_utf8_bytes = 0usize;
                if let Some(title) = title {
                    validate_nonempty_metadata_text(title)
                        .map_err(|_| FrameValidationError::MetadataShape)?;
                    add_metadata_utf8_bytes(&mut total_utf8_bytes, title)
                        .map_err(|_| FrameValidationError::MetadataShape)?;
                }
                let canonical = canonical_metadata_tags(tags.clone(), None)
                    .map_err(|_| FrameValidationError::MetadataShape)?;
                if canonical != *tags {
                    return Err(FrameValidationError::MetadataShape);
                }
                for tag in tags {
                    add_metadata_utf8_bytes(&mut total_utf8_bytes, tag)
                        .map_err(|_| FrameValidationError::MetadataShape)?;
                }
                if last_writer.is_none() && (title.is_some() || !tags.is_empty() || *archived) {
                    return Err(FrameValidationError::MetadataShape);
                }
            }
            Self::SessionMetadataPageEnd {
                session_count,
                next_after_session_id,
            } => {
                if next_after_session_id.is_some() && session_count.value() == 0 {
                    return Err(FrameValidationError::MetadataShape);
                }
            }
            Self::ConversationSummary { conversation } => conversation.validate()?,
            Self::ModelCapabilityItem { capabilities, .. } => capabilities.validate()?,
            Self::ModelCapabilitiesEnd { capability_count }
                if capability_count.value() > MAX_MODEL_CAPABILITY_CATALOG_ENTRIES as u64 =>
            {
                return Err(FrameValidationError::ModelSettingsShape);
            }
            Self::ReviewPassCompleted {
                state: ReviewPassLifecycle::Queued | ReviewPassLifecycle::Running,
                ..
            } => {
                return Err(FrameValidationError::ReviewShape);
            }
            Self::ReviewOrchestration { snapshot } => {
                validate_review_orchestration_snapshot(snapshot)?;
            }
            Self::TemplateSummary { name, version } => {
                validate_session_template_name(name)?;
                if version.value() == 0 {
                    return Err(FrameValidationError::TemplateShape);
                }
            }
            Self::SessionSummary {
                placement_version,
                placement,
                ..
            } => {
                if placement_version.value() == 0 {
                    return Err(FrameValidationError::PlacementShape);
                }
                validate_session_placement_shape(placement)?;
            }
            Self::SessionPlacementUpdated {
                placement_version,
                placement,
                ..
            } => {
                if placement_version.value() == 0 {
                    return Err(FrameValidationError::PlacementShape);
                }
                validate_session_placement_shape(placement)?;
            }
            Self::ConversationPageEnd {
                conversation_count,
                next_after,
            } => {
                if next_after.is_some() && conversation_count.value() == 0 {
                    return Err(FrameValidationError::ConversationListShape);
                }
            }
            Self::SessionMetadata {
                metadata,
                last_writer,
                ..
            } if last_writer.is_none() && !metadata.is_initial() => {
                return Err(FrameValidationError::MetadataShape);
            }
            Self::ImportedConversationEntry {
                position,
                content_kind,
                text_preview,
                ..
            } => {
                if position.value() == 0 {
                    return Err(FrameValidationError::ImportedConversationEntryShape);
                }
                if let Some(preview) = text_preview {
                    // Only `Text` content has an exact attested text to
                    // preview, so a preview on any other kind contradicts the
                    // kind it accompanies.
                    if *content_kind != ImportedContentKind::Text {
                        return Err(FrameValidationError::ImportedConversationEntryShape);
                    }
                    preview.validate()?;
                }
            }
            Self::ConversationImportAppended {
                assembled_size_bytes,
            } if assembled_size_bytes.value() == 0 => {
                return Err(FrameValidationError::ConversationImportShape);
            }
            Self::BlobUploadBegun {
                expected_length_bytes,
                ..
            }
            | Self::BlobUploadAlreadyPresent {
                byte_length: expected_length_bytes,
                ..
            }
            | Self::BlobUploadCommitted {
                byte_length: expected_length_bytes,
                ..
            }
            | Self::BlobUploadAppended {
                assembled_length_bytes: expected_length_bytes,
            } if expected_length_bytes.value() == 0 => {
                return Err(FrameValidationError::BlobUploadShape);
            }
            Self::BlobMetadata { byte_length, .. } if byte_length.value() == 0 => {
                return Err(FrameValidationError::BlobReadShape);
            }
            Self::BlobChunkRead {
                offset_bytes,
                bytes,
                ..
            } if bytes.as_bytes().is_empty()
                || bytes.as_bytes().len() > MAX_BLOB_READ_BYTES
                || u64::try_from(bytes.as_bytes().len()).map_or(true, |length_bytes| {
                    offset_bytes.value().checked_add(length_bytes).is_none()
                }) =>
            {
                return Err(FrameValidationError::BlobReadShape);
            }
            Self::TranscriptModelCallUsage { usage, cost, .. }
                if cost.is_some()
                    && usage.input_tokens.is_none()
                    && usage.output_tokens.is_none()
                    && usage.cache_creation_input_tokens.is_none()
                    && usage.cache_read_input_tokens.is_none() =>
            {
                return Err(FrameValidationError::ModelCallUsageShape);
            }
            Self::TranscriptEntry {
                source_session_id,
                entry,
                ..
            } => validate_delegation_transcript_entry(*source_session_id, entry)?,
            Self::SessionSpawned { .. } => {}
            _ => {}
        }
        Ok(())
    }
}

#[derive(signalbox_derive::Accessors)]
/// One validated server frame.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerFrame {
    version: ProtocolVersion,
    request_id: RequestId,
    /// Borrows the closed server message.
    #[get]
    message: ServerMessage,
}

impl ServerFrame {
    /// Constructs a single-version response frame.
    pub fn try_new(
        request_id: RequestId,
        message: ServerMessage,
    ) -> Result<Self, FrameValidationError> {
        Self::try_new_for_version(ProtocolVersion::One, request_id, message)
    }

    /// Constructs one response in an admitted protocol version.
    pub fn try_new_for_version(
        version: ProtocolVersion,
        request_id: RequestId,
        message: ServerMessage,
    ) -> Result<Self, FrameValidationError> {
        let frame = Self {
            version,
            request_id,
            message,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// Returns the admitted protocol version.
    pub const fn version(&self) -> ProtocolVersion {
        self.version
    }

    /// Returns the request correlation identity.
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if let ServerMessage::TranscriptTurn { state, .. } = &self.message {
            state.validate()?;
        }
        if let ServerMessage::SessionDefaultsReplaced { system_prompt, .. } = &self.message {
            validate_system_prompt_member(system_prompt)?;
        }
        self.message.validate()?;
        match &self.message {
            ServerMessage::Error { code, detail, .. } => {
                if !self.request_id.is_correlated()
                    && !matches!(
                        code,
                        ErrorCode::MalformedFrame | ErrorCode::UnsupportedVersion
                    )
                {
                    return Err(FrameValidationError::UncorrelatedApplicationError);
                }
                if let Some(RejectionDetail::ImportedFrontierPositionOutOfRange {
                    requested_position,
                    last_position,
                    ..
                }) = detail.value()
                {
                    // An imported conversation's positions are the contiguous
                    // sequence `1..=last_position`, so a nonpositive bound or a
                    // requested ordinal inside that range contradicts the
                    // rejection the detail states.
                    if last_position.value() == 0
                        || requested_position.value() <= last_position.value()
                    {
                        return Err(FrameValidationError::ImportedFrontierRangeShape);
                    }
                }
                if let Some(detail) = detail.value() {
                    if detail.is_bulk_ingest() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                    } else if detail.is_conversation_import() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                        validate_conversation_import_detail(detail)?;
                    } else if detail.is_blob_upload() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                        validate_blob_upload_detail(detail)?;
                    } else if detail.is_blob_read() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                        validate_blob_read_detail(detail)?;
                    } else if *code != ErrorCode::Rejected {
                        return Err(FrameValidationError::ErrorDetailShape);
                    } else {
                        validate_rejection_detail(detail)?;
                    }
                } else if *code == ErrorCode::Rejected {
                    return Err(FrameValidationError::ErrorDetailShape);
                }
            }
            _ if !self.request_id.is_correlated() => {
                return Err(FrameValidationError::UncorrelatedSuccess);
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServerFrame {
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
}

impl<'de> Deserialize<'de> for ServerFrame {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let raw = RawServerFrame::deserialize(deserializer)?;
        let frame = Self {
            version: raw.version,
            request_id: raw.request_id,
            message: raw.message,
        };
        frame.validate().map_err(serde::de::Error::custom)?;
        Ok(frame)
    }
}

fn validate_rejection_detail(detail: RejectionDetail) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::SessionPlacementCurrentVersionMismatch {
            expected_placement_version,
            current_placement_version,
            ..
        } => {
            expected_placement_version.value() > 0
                && current_placement_version.value() > 0
                && expected_placement_version != current_placement_version
        }
        RejectionDetail::SessionPlacementVersionExhausted {
            current_placement_version,
            ..
        } => current_placement_version.value() == u64::MAX,
        RejectionDetail::DelegationEventOrdinalExhausted { last, .. } => last.value() == u64::MAX,
        RejectionDetail::DelegationDeliverySequenceExhausted { last, .. } => {
            last.value() == u64::MAX
        }
        RejectionDetail::SessionNotFound { .. }
        | RejectionDetail::AttachmentBlobNotFound { .. }
        | RejectionDetail::AttachmentByteBudgetExceeded { .. }
        | RejectionDetail::UnsupportedReasoningLevel { .. }
        | RejectionDetail::UnsupportedFastMode { .. }
        | RejectionDetail::UnsupportedServiceTier { .. }
        | RejectionDetail::GoalCommandRejected { .. }
        | RejectionDetail::SessionLifecycleCommandRejected { .. }
        | RejectionDetail::ActiveTurnPresent { .. }
        | RejectionDetail::CommissionTargetBusy { .. }
        | RejectionDetail::ActiveTurnMismatch { .. }
        | RejectionDetail::NoActiveTurn { .. }
        | RejectionDetail::TurnNotAwaitingReconciliation { .. }
        | RejectionDetail::InterruptAlreadyApplied { .. }
        | RejectionDetail::InterruptUnavailableWhileAwaitingApproval { .. }
        | RejectionDetail::SafePointUnavailableWhileStopping { .. }
        | RejectionDetail::ToolRequestNotFound { .. }
        | RejectionDetail::ToolRequestAlreadyResolved { .. }
        | RejectionDetail::ToolRequestNotEarliestUndecided { .. }
        | RejectionDetail::ToolRequestNotInSession { .. }
        | RejectionDetail::ToolRequestNotDelegateDenied { .. }
        | RejectionDetail::ToolRequestNotTerminallyDenied { .. }
        | RejectionDetail::ToolDenialAlreadyOverridden { .. }
        | RejectionDetail::DelegationRequestNotInTurn { .. }
        | RejectionDetail::DelegationToolRequestNotExecutable { .. }
        | RejectionDetail::DelegationSpawnConflict { .. }
        | RejectionDetail::DelegatedChildIdentityCollision { .. }
        | RejectionDetail::DelegationRelationNotFound { .. }
        | RejectionDetail::DelegationAwaitConflict { .. }
        | RejectionDetail::DelegationMessageConflict { .. }
        | RejectionDetail::DelegationMessageIdentityCollision { .. }
        | RejectionDetail::DefaultsVersionMismatch { .. }
        | RejectionDetail::UnknownModelAlias { .. }
        | RejectionDetail::AcceptancePositionExhausted { .. }
        | RejectionDetail::DefaultsVersionExhausted { .. }
        | RejectionDetail::ImportedConversationNotFound { .. }
        | RejectionDetail::ImportedFrontierPositionOutOfRange { .. } => true,
        RejectionDetail::ConversationImportAlreadyInProgress {}
        | RejectionDetail::ConversationImportNotInProgress {}
        | RejectionDetail::ConversationImportSourceTooLarge { .. }
        | RejectionDetail::ConversationImportSourceSizeMismatch { .. }
        | RejectionDetail::ConversationImportConversionFailed { .. }
        | RejectionDetail::BulkIngestAlreadyInProgress { .. }
        | RejectionDetail::BlobUploadAlreadyInProgress {}
        | RejectionDetail::BlobUploadNotInProgress {}
        | RejectionDetail::BlobUploadLengthOutOfRange { .. }
        | RejectionDetail::BlobUploadSizeExceeded { .. }
        | RejectionDetail::BlobUploadLengthMismatch { .. }
        | RejectionDetail::BlobUploadDigestMismatch { .. }
        | RejectionDetail::BlobReadLengthOutOfRange { .. }
        | RejectionDetail::BlobReadRangeOutOfBounds { .. } => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::ErrorDetailShape)
    }
}

fn validate_conversation_import_detail(
    detail: RejectionDetail,
) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::ConversationImportAlreadyInProgress {}
        | RejectionDetail::ConversationImportNotInProgress {} => true,
        RejectionDetail::ConversationImportSourceTooLarge {
            limit_bytes,
            declared_size_bytes,
            actual_size_bytes,
        } => {
            limit_bytes.value() > 0
                && match actual_size_bytes {
                    Some(actual) => {
                        actual.value() > limit_bytes.value()
                            && (declared_size_bytes.value() <= limit_bytes.value()
                                || declared_size_bytes == actual)
                    }
                    None => declared_size_bytes.value() > limit_bytes.value(),
                }
        }
        RejectionDetail::ConversationImportSourceSizeMismatch {
            declared_size_bytes,
            actual_size_bytes,
        } => declared_size_bytes != actual_size_bytes,
        RejectionDetail::ConversationImportConversionFailed {
            class,
            record_ordinal,
        } => match class {
            ConversationImportRejectionClass::EmptySource => record_ordinal.is_none(),
            ConversationImportRejectionClass::BlankLine
            | ConversationImportRejectionClass::InvalidUtf8
            | ConversationImportRejectionClass::InvalidJson
            | ConversationImportRejectionClass::JsonDepthExceeded
            | ConversationImportRejectionClass::TopLevelNotObject
            | ConversationImportRejectionClass::InvalidRecordType
            | ConversationImportRejectionClass::InvalidSourceMetadata
            | ConversationImportRejectionClass::InvalidMessageEnvelope
            | ConversationImportRejectionClass::InvalidMessageRole
            | ConversationImportRejectionClass::MessageRoleMismatch
            | ConversationImportRejectionClass::InvalidMessageContent
            | ConversationImportRejectionClass::InvalidContentBlock
            | ConversationImportRejectionClass::InvalidToolResultBlock
            | ConversationImportRejectionClass::InvalidReasoning
            | ConversationImportRejectionClass::InvalidToolCall
            | ConversationImportRejectionClass::InvalidToolResult => {
                record_ordinal.is_some_and(|ordinal| ordinal.value() > 0)
            }
        },
        RejectionDetail::SessionNotFound { .. }
        | RejectionDetail::AttachmentBlobNotFound { .. }
        | RejectionDetail::AttachmentByteBudgetExceeded { .. }
        | RejectionDetail::UnsupportedReasoningLevel { .. }
        | RejectionDetail::UnsupportedFastMode { .. }
        | RejectionDetail::UnsupportedServiceTier { .. }
        | RejectionDetail::SessionPlacementCurrentVersionMismatch { .. }
        | RejectionDetail::SessionPlacementVersionExhausted { .. }
        | RejectionDetail::GoalCommandRejected { .. }
        | RejectionDetail::SessionLifecycleCommandRejected { .. }
        | RejectionDetail::ActiveTurnPresent { .. }
        | RejectionDetail::CommissionTargetBusy { .. }
        | RejectionDetail::ActiveTurnMismatch { .. }
        | RejectionDetail::NoActiveTurn { .. }
        | RejectionDetail::TurnNotAwaitingReconciliation { .. }
        | RejectionDetail::InterruptAlreadyApplied { .. }
        | RejectionDetail::InterruptUnavailableWhileAwaitingApproval { .. }
        | RejectionDetail::SafePointUnavailableWhileStopping { .. }
        | RejectionDetail::ToolRequestNotFound { .. }
        | RejectionDetail::ToolRequestAlreadyResolved { .. }
        | RejectionDetail::ToolRequestNotEarliestUndecided { .. }
        | RejectionDetail::ToolRequestNotInSession { .. }
        | RejectionDetail::ToolRequestNotDelegateDenied { .. }
        | RejectionDetail::ToolRequestNotTerminallyDenied { .. }
        | RejectionDetail::ToolDenialAlreadyOverridden { .. }
        | RejectionDetail::DelegationRequestNotInTurn { .. }
        | RejectionDetail::DelegationToolRequestNotExecutable { .. }
        | RejectionDetail::DelegationSpawnConflict { .. }
        | RejectionDetail::DelegatedChildIdentityCollision { .. }
        | RejectionDetail::DelegationRelationNotFound { .. }
        | RejectionDetail::DelegationAwaitConflict { .. }
        | RejectionDetail::DelegationMessageConflict { .. }
        | RejectionDetail::DelegationMessageIdentityCollision { .. }
        | RejectionDetail::DelegationEventOrdinalExhausted { .. }
        | RejectionDetail::DelegationDeliverySequenceExhausted { .. }
        | RejectionDetail::DefaultsVersionMismatch { .. }
        | RejectionDetail::UnknownModelAlias { .. }
        | RejectionDetail::AcceptancePositionExhausted { .. }
        | RejectionDetail::DefaultsVersionExhausted { .. }
        | RejectionDetail::ImportedConversationNotFound { .. }
        | RejectionDetail::ImportedFrontierPositionOutOfRange { .. } => false,
        RejectionDetail::BulkIngestAlreadyInProgress { .. }
        | RejectionDetail::BlobUploadAlreadyInProgress {}
        | RejectionDetail::BlobUploadNotInProgress {}
        | RejectionDetail::BlobUploadLengthOutOfRange { .. }
        | RejectionDetail::BlobUploadSizeExceeded { .. }
        | RejectionDetail::BlobUploadLengthMismatch { .. }
        | RejectionDetail::BlobUploadDigestMismatch { .. }
        | RejectionDetail::BlobReadLengthOutOfRange { .. }
        | RejectionDetail::BlobReadRangeOutOfBounds { .. } => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::ConversationImportShape)
    }
}

fn validate_blob_upload_detail(detail: RejectionDetail) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::BlobUploadAlreadyInProgress {}
        | RejectionDetail::BlobUploadNotInProgress {} => true,
        RejectionDetail::BlobUploadLengthOutOfRange {
            min_length_bytes,
            max_length_bytes,
            declared_length_bytes,
        } => {
            min_length_bytes.value() > 0
                && min_length_bytes.value() <= max_length_bytes.value()
                && (declared_length_bytes.value() < min_length_bytes.value()
                    || declared_length_bytes.value() > max_length_bytes.value())
        }
        RejectionDetail::BlobUploadSizeExceeded {
            expected_length_bytes,
            actual_length_bytes,
        } => {
            expected_length_bytes.value() > 0
                && actual_length_bytes.value() > expected_length_bytes.value()
        }
        RejectionDetail::BlobUploadLengthMismatch {
            expected_length_bytes,
            actual_length_bytes,
        } => expected_length_bytes.value() > 0 && expected_length_bytes != actual_length_bytes,
        RejectionDetail::BlobUploadDigestMismatch {
            expected_digest,
            actual_digest,
        } => expected_digest != actual_digest,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::BlobUploadShape)
    }
}

fn validate_blob_read_detail(detail: RejectionDetail) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::BlobReadLengthOutOfRange {
            min_length_bytes,
            max_length_bytes,
            requested_length_bytes,
        } => {
            min_length_bytes.value() == 1
                && max_length_bytes.value() == MAX_BLOB_READ_BYTES as u64
                && (requested_length_bytes.value() < min_length_bytes.value()
                    || requested_length_bytes.value() > max_length_bytes.value())
        }
        RejectionDetail::BlobReadRangeOutOfBounds {
            offset_bytes,
            length_bytes,
            blob_length_bytes,
            ..
        } => {
            (1..=MAX_BLOB_READ_BYTES as u64).contains(&length_bytes.value())
                && blob_length_bytes.value() > 0
                && (offset_bytes
                    .value()
                    .checked_add(length_bytes.value())
                    .is_none_or(|end| end > blob_length_bytes.value()))
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::BlobReadShape)
    }
}

/// Decodes and validates one complete client line including its final newline.
pub fn decode_client_line(line: &[u8]) -> Result<ClientFrame, FrameDecodeError> {
    let content = checked_line_content(line, false)?;
    let header = probe_header(content, "request", false)?;
    let frame: ClientFrame = serde_json::from_slice(content)
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    frame
        .validate()
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    Ok(frame)
}

/// Decodes and validates one complete server line including its final newline.
pub fn decode_server_line(line: &[u8]) -> Result<ServerFrame, FrameDecodeError> {
    let content = checked_line_content(line, true)?;
    let header = probe_header(content, "message", true)?;
    let frame: ServerFrame = serde_json::from_slice(content)
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    frame
        .validate()
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    Ok(frame)
}

/// Encodes one validated client frame with its final newline.
pub fn encode_client_line(frame: &ClientFrame) -> Result<Vec<u8>, FrameEncodeError> {
    frame.validate()?;
    encode_line(frame)
}

/// Encodes one validated server frame with its final newline.
pub fn encode_server_line(frame: &ServerFrame) -> Result<Vec<u8>, FrameEncodeError> {
    frame.validate()?;
    encode_line(frame)
}

fn encode_line<T: Serialize>(frame: &T) -> Result<Vec<u8>, FrameEncodeError> {
    let mut encoded = serde_json::to_vec(frame)?;
    encoded.push(b'\n');
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(FrameEncodeError::OversizedFrame);
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests;
