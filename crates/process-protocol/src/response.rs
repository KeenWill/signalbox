//! Response wire representations and validation.

use crate::delegation::{
    DelegationMessageDirection, DelegationOutcome, DelegationPolicy, DelegationProvenance,
    DelegationReason, DelegationWaitMode, direct_child_result_shape_is_valid,
};
use crate::error::{ErrorCode, ErrorDetail};
use crate::event::{SessionEvent, validate_delegation_session_event, validate_settings_event};
use crate::goal::{
    GoalHistoryEvent, GoalLifecycleState, SessionLifecycleEffect, validate_goal_event,
    validate_goal_state, validate_goal_text,
};
use crate::operator_status::{OperatorStatusMessage, validate_operator_status_message};
use crate::request::ToolDecision;
use crate::review::{
    ReviewFindingSnapshot, ReviewFindingStatus, ReviewOrchestrationSnapshot,
    ReviewOrchestrationState, ReviewPassLifecycle, ReviewPassSnapshot, ReviewRunSnapshot,
    ReviewTargetSnapshot,
};
use crate::runner::RunnerProjection;
use crate::scalars::{
    BlobChunk, CanonicalBlobDigest, CanonicalU64, CanonicalUuid, ContentFragment,
    FrameValidationError, MAX_BLOB_READ_BYTES, MAX_MODEL_CAPABILITY_CATALOG_ENTRIES,
    ModelCallDollarCost, ModelCallTokenUsage, SystemPromptMember, SystemPromptText,
    UsageProvenance, deserialize_required_nullable,
};
use crate::session::{
    ConversationCursor, ConversationSummary, MetadataLastWriter, SessionMetadata, SessionPlacement,
    add_metadata_utf8_bytes, canonical_metadata_tags, deserialize_session_metadata_tags,
    validate_nonempty_metadata_text, validate_session_placement_shape,
};
use crate::settings::{
    ModelCapabilities, ModelSelection, ModelSettingsSnapshot, TurnModelSettingsSnapshot,
    snapshot_matches_model,
};
use crate::shared_validation::{
    validate_review_orchestration_snapshot, validate_session_template_name,
    validate_tool_approval_event_shape,
};
use crate::transcript::{
    ImportedContentKind, ImportedSourceSpeaker, ImportedTextPreview, TranscriptEntry,
    TranscriptTextEntry, TurnState, validate_delegation_transcript_entry,
};
use crate::user_input::UserInputContent;
use serde::{Deserialize, Serialize};

/// The selected stop scope and its immutable descendant disposition count.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminationReceipt {
    pub descendant_scope: crate::goal::DescendantTerminationScope,
    pub descendant_count: CanonicalU64,
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
        /// Present exactly for a stop-turn acceptance.
        #[serde(skip_serializing_if = "Option::is_none")]
        termination: Option<TerminationReceipt>,
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
        /// Present exactly for a stop-goal transition.
        #[serde(skip_serializing_if = "Option::is_none")]
        termination: Option<TerminationReceipt>,
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
    pub(crate) fn validate(&self) -> Result<(), FrameValidationError> {
        if let Self::InputSubmitted {
            termination: Some(receipt),
            ..
        }
        | Self::GoalTransitionApplied {
            termination: Some(receipt),
            ..
        } = self
            && receipt.descendant_scope == crate::goal::DescendantTerminationScope::ParentAlone
            && receipt.descendant_count.value() != 0
        {
            return Err(FrameValidationError::DelegationShape);
        }
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
