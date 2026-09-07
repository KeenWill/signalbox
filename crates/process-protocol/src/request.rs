//! Request wire representations and validation.

use crate::delegation::{DelegationPolicy, DelegationWaitMode};
use crate::goal::{
    CommissionedSessionFence, DescendantTerminationScope, FinishCondition, SessionFailureCause,
    SessionLifecycleMembers, validate_goal_text,
};
use crate::review::{
    ReviewConcernTerminalOutcome, ReviewExternalObjectKind, ReviewFindingEvent, ReviewFindingInput,
    ReviewImportTerminalOutcome, ReviewJudgmentEffectTerminalOutcome, ReviewJudgmentPlanMember,
    ReviewOrchestrationConcernInput, ReviewPassTerminalOutcome, ReviewPublicationOutcome,
    ReviewPublicationTerminalOutcome, ReviewRepairOutcome, ReviewRepairTerminalOutcome,
    ReviewTargetSubject, ReviewWorkflow,
};
use crate::scalars::{
    BlobChunk, CanonicalBlobDigest, CanonicalDigest, CanonicalU64, CanonicalUuid, CommandId,
    ConversationImportFormat, ConversationImportSource, FrameValidationError, InputContent,
    MAX_BLOB_CHUNK_BYTES, MAX_CONVERSATION_IMPORT_CHUNK_BYTES, MAX_REVIEW_ORCHESTRATION_MEMBERS,
    SystemPromptMember, deserialize_required_nullable,
};
use crate::session::{
    ConversationCursor, ConversationOriginFilter, ImportedSessionRelationship, InputDelivery,
    SessionMetadata, SessionPlacement, add_metadata_utf8_bytes, canonical_metadata_tags,
    deserialize_present_input_delivery, deserialize_required_metadata_tags,
    validate_nonempty_metadata_text, validate_session_placement_shape,
};
use crate::settings::{ModelSelection, ModelSettingsOverlay};
use crate::shared_validation::{
    validate_review_finding_event, validate_review_judgment_disposition, validate_review_key,
    validate_session_template_name,
};
use crate::user_input::UserInputContent;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Closed versioned request family.

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientRequest {
    /// Register a directory resolved by the daemon operator boundary.
    RegisterWorkspace { command_id: CommandId, root: String },
    /// Mint an HTTPS Git remote for a registered workspace.
    MintGitRemote {
        command_id: CommandId,
        workspace_id: CanonicalUuid,
        name: String,
        url: String,
    },
    /// Withdraw exactly one Git remote mint.
    WithdrawGitRemote {
        command_id: CommandId,
        mint_id: CanonicalUuid,
    },
    /// Recover the named session on its pending successor runner.
    ReplaceLostRunner {
        /// User-global mutation identity.
        command_id: CommandId,
        /// Session whose runner is lost.
        session_id: CanonicalUuid,
        /// Full lowercase SHA-1 or SHA-256 checkout revision, or retained recovery facts.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        revision: Option<String>,
    },
    /// Retire a lost placement after the session's active turn has ended.
    AbandonLostRunner {
        /// User-global mutation identity.
        command_id: CommandId,
        /// Session whose runner is lost.
        session_id: CanonicalUuid,
    },
    /// Activate one pending runner without moving any session.
    PromotePendingRunner {
        /// User-global mutation identity.
        command_id: CommandId,
        /// Exact runner-created pending enrollment request.
        enrollment_request_id: CanonicalUuid,
    },
    /// Begin an operator-authorized OAuth device exchange.
    ProvisionOauthCredential {
        /// User-global durable command identity.
        command_id: CommandId,
        /// Configured credential profile name.
        profile: String,
    },
    /// Replace a stored OAuth authorization.
    ReprovisionOauthCredential {
        /// User-global durable command identity.
        command_id: CommandId,
        /// Configured credential profile name.
        profile: String,
    },
    /// Delete retained OAuth authorization by profile identity.
    DeleteOauthCredential {
        /// User-global durable command identity.
        command_id: CommandId,
        /// Credential profile identity, including a retired declaration.
        profile: String,
    },
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
    pub(crate) fn validate(&self) -> Result<(), FrameValidationError> {
        match self {
            Self::ReplaceLostRunner { revision, .. } => {
                if let Some(revision) = revision {
                    signalbox_domain::WorkspaceRevision::try_new(revision.clone())
                        .map_err(|_| FrameValidationError::PlacementShape)?;
                }
            }
            Self::AbandonLostRunner { .. } | Self::PromotePendingRunner { .. } => {}

            Self::ProvisionOauthCredential { profile, .. }
            | Self::ReprovisionOauthCredential { profile, .. }
            | Self::DeleteOauthCredential { profile, .. } => {
                crate::response::validate_oauth_profile(profile)?;
            }
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
            | Self::RegisterWorkspace { .. }
            | Self::MintGitRemote { .. }
            | Self::WithdrawGitRemote { .. }
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
