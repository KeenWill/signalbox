//! Application orchestration boundary for Signalbox.
//!
//! This crate coordinates domain decisions and external effects while
//! depending inward on `signalbox-domain`.

mod approval_judge;
mod attention;
mod blob_derivation;
mod commissioned_dispatch;
mod convergence_reconciliation;
mod conversation_import;
mod create_session;
mod create_session_from_imported_frontier;
mod list_conversations;
mod load_session;
mod model_execution;
mod operator_failure;
mod replace_session_defaults;
mod repo_watch;
mod review_orchestration;
mod review_workflow;
mod scheduler;
mod search;
mod session_delegation;
mod session_live;
mod session_metadata;
mod session_timeline;
mod start_eligible_turn;
mod startup_scan;
mod submit_input;
mod tool_dispatch_gate;
#[cfg(feature = "test-support")]
mod tool_execution_test_support;
mod tool_loop;
mod tool_loop_ports;
mod turn_liveness;
mod update_session_placement;
mod usage;
mod workspace_instructions;

pub use approval_judge::{
    ApprovalJudgeAuthorization, ApprovalJudgeBranchAuthority, ApprovalJudgeBranchAuthorityInput,
    ApprovalJudgeCompletionIdentities, ApprovalJudgeDispatchAuthority,
    ApprovalJudgeDispatchProvenance, ApprovalJudgePullRequestAuthority,
    ApprovalJudgePullRequestAuthorityInput,
};
pub use attention::{
    AttentionAction, AttentionActivity, AttentionActivityKind, AttentionBlockedReason,
    AttentionChanges, AttentionContinuation, AttentionCursor, AttentionGoalBlock,
    AttentionJudgeFacts, AttentionLifecycleState, AttentionQuery, AttentionQueryError,
    AttentionReader, AttentionSnapshot, AttentionSort, AttentionState, AttentionSummary,
    max_attention_change_items, max_attention_filter_tags, max_attention_filter_utf8_bytes,
    max_attention_goal_summary_characters, max_attention_snapshot_items,
    max_attention_title_characters,
};
pub use blob_derivation::{
    BlobDerivationIdGenerator, BlobDerivationRecordOutcome, BlobDerivationServiceError,
    BlobDerivationServiceOutcome, BlobDerivationStore, DeterministicBlobDerivationRequest,
    DeterministicBlobDerivationService, DeterministicBlobProducer, UuidV7BlobDerivationIdGenerator,
};
pub use commissioned_dispatch::{
    CommissionDispatchPreparationError, CommissionDispatchRequest, CommissionedDispatchFence,
    CommissionedDispatchIdGenerator, PreparedCommissionedDispatch,
    UuidV7CommissionedDispatchIdGenerator,
};
pub use convergence_reconciliation::{
    PullRequestCheck, PullRequestCheckState, PullRequestConvergence, PullRequestConvergenceBlocker,
    PullRequestConvergenceFacts, PullRequestDraftState, evaluate_pull_request_convergence,
};
pub use conversation_import::{
    ImportConversationError, ImportConversationOutcome, ImportConversationReport,
    ImportConversationService, ImportedConversationConversionReport, ImportedConversationConverter,
    ImportedConversationIdGenerator, ImportedConversationSkippedRecord, ImportedConversationStore,
    ImportedConversationStoreOutcome, ResilientImportedConversationConverter,
    UuidV7ImportedConversationIdGenerator,
};
pub use create_session::{
    CreateSessionError, CreateSessionOutcome, CreateSessionRequest, CreateSessionService,
    CreateSessionTransaction, InvalidDurableCommandId, SessionIdGenerator,
    UuidV7SessionIdGenerator,
};
pub use create_session_from_imported_frontier::{
    CreateSessionFromImportedFrontierIdGenerator, CreateSessionFromImportedFrontierOutcome,
    CreateSessionFromImportedFrontierRequest, CreateSessionFromImportedFrontierService,
    CreateSessionFromImportedFrontierTransaction,
    UuidV7CreateSessionFromImportedFrontierIdGenerator,
};
pub use list_conversations::{
    ConversationListCursor, ConversationListItem, ConversationListQuery,
    ConversationListQueryError, ConversationLister, ConversationOriginFilter,
    ConversationPageReader, ListConversationsService,
};
pub use load_session::{LoadSessionService, SessionReader};
pub use model_execution::{
    AttachmentPreparationFailure, AttemptDispatchGate, AuthorizeModelCallOutcome,
    AuthorizeModelCallTransaction, AvailabilitySuccessorOutcome,
    CommitModelCallObservationTransaction, CredentialPoolExhaustedOutcome,
    FailPreparedModelCallTransaction, InProcessAttemptDispatchGate, InProcessAttemptDispatchPermit,
    ModelAttachmentStub, ModelCallAuthorizationReread, ModelCallCapabilityPreparation,
    ModelCallCredentialReference, ModelCallExecutionError, ModelCallExecutionIdGenerator,
    ModelCallExecutionOutcome, ModelCallExecutionService, ModelCallInputTokenCount,
    ModelCallInputTokenCounter, ModelCallObservationCommitOutcome, ModelCallProvider,
    ModelCallTerminalIdentityCandidates, ModelConversationMessage, ModelFrontierRenderingError,
    ModelToolResultContent, ModelUserContent, ModelUserContentPart, PrepareModelCallOutcome,
    PrepareModelCallTransaction, PreparedModelCallFailureCause, PreparedModelOperation,
    RetainedModelCallExecutionState, RetainedModelCallObservationStatus,
    RetainedPreparedFailureStatus, ScriptedModelCallCapability, ScriptedModelCallError,
    ScriptedModelCallProvider, ScriptedModelCallStep, UuidV7ModelCallExecutionIdGenerator,
    render_model_user_content,
};
pub use operator_failure::{ClassifyOperatorFailure, OperatorFailureClass};
pub use replace_session_defaults::{
    PromptMemberStatement, ReplaceSessionDefaultsOutcome, ReplaceSessionDefaultsRequest,
    ReplaceSessionDefaultsService, ReplaceSessionDefaultsTransaction,
};
pub use repo_watch::{
    RepoWatchBranchHead, RepoWatchCheckCompletionGeneration,
    RepoWatchCheckCompletionGenerationError, RepoWatchCheckRunObservation,
    RepoWatchCheckSuiteObservation, RepoWatchConvergenceAssessment,
    RepoWatchConvergenceAssessmentError, RepoWatchConvergenceAssessmentInput,
    RepoWatchConvergenceVerdict, RepoWatchDifferError, RepoWatchDifferFailureKind,
    RepoWatchEventContentIdentityV1, RepoWatchEventIdGenerator,
    RepoWatchEventIdentityFrontierEntryV1, RepoWatchEventIdentityFrontierError,
    RepoWatchEventIdentityFrontierV1, RepoWatchEventOccurrenceV1,
    RepoWatchMergedCheckRunBaselineV1, RepoWatchMergedCheckSuiteBaselineV1,
    RepoWatchMergedPullRequestBaselineInputV1, RepoWatchMergedPullRequestBaselineV1,
    RepoWatchObservation, RepoWatchPullRequestLifecycle, RepoWatchPullRequestState,
    RepoWatchPullRequestStateInput, RepoWatchReactionObservation, RepoWatchRepositoryState,
    RepoWatchRepositoryStateError, RepoWatchRepositoryStateInput, RepoWatchReviewDecision,
    RepoWatchReviewObservation, RepoWatchStaleReviewClearanceCandidate,
    RepoWatchStaleReviewClearanceCandidateError, RepoWatchThreadObservation, RepoWatchThreadState,
    RepoWatchWorkflowRunObservation, UuidV7RepoWatchEventIdGenerator, derive_repo_watch_events,
    derive_repo_watch_events_with_merged_baselines,
    repo_watch_events_have_equal_identified_content,
};
pub use review_orchestration::{
    ReviewConcernClaim, ReviewConcernOutcome, ReviewConcernSpec, ReviewConcernSuccess,
    ReviewConcernWork, ReviewDurableSealOutcome, ReviewFanoutBarrierFailure,
    ReviewImportEvidenceFailure, ReviewImportOutcome, ReviewImportedContextEvidence,
    ReviewJudgmentEffectEvidenceFailure, ReviewJudgmentEffectId, ReviewJudgmentEffectOutcome,
    ReviewJudgmentEffectSuccess, ReviewJudgmentEffectWork, ReviewJudgmentPlan,
    ReviewJudgmentPlanFailure, ReviewJudgmentPlanMember, ReviewOrchestrationAttempt,
    ReviewOrchestrationAttemptError, ReviewOrchestrationAttemptId, ReviewOrchestrationAttemptStore,
    ReviewOrchestrationOutcome, ReviewOrchestrationPassRunner, ReviewOrchestrationService,
    ReviewOrchestrationServiceError, ReviewPassIncompleteStatus, ReviewPlannedDisposition,
    ReviewPublicationMemberOutcome, ReviewPublicationSuccess, ReviewPublicationWork,
    ReviewRepairMemberOutcome, ReviewRepairSuccess, ReviewRepairWork, ReviewStageTemplateDigests,
    ReviewTemplateDigest, ReviewTerminalBarrierFailure,
};
pub use review_workflow::{
    ReviewPassCompletionStatus, ReviewWorkflowCommand, ReviewWorkflowCommandOutcome,
    ReviewWorkflowCommandResult, ReviewWorkflowCommandService, ReviewWorkflowOperation,
    ReviewWorkflowOperationKind, ReviewWorkflowReader, ReviewWorkflowTransaction,
};
pub use scheduler::{
    EligibilityNudge, EligibilityNudgeOutcome, EligibilityPass, EligibilitySweep,
    EligibilitySweepBatch, EligibilityWorkSource, GoalAwareEligibilityPass,
    GoalAwareEligibilityPassError, GoalPassDisposition, InProcessEligibilityNudge,
    InProcessEligibilityWorkSource, InvalidReconciliationSweepInterval,
    InvalidSchedulerPassOccupancyBound, ReconciliationSweepInterval, SchedulerLoop,
    SchedulerLoopExit, SchedulerOccupancyObserver, SchedulerOldestInFlightPass,
    SchedulerPassExpiryHandler, SchedulerPassOccupancyBound,
};
pub use search::{
    MAX_SEARCH_HIGHLIGHTS_PER_RESULT, SearchArtifactId, SearchArtifactProjection,
    SearchArtifactProjectionClass, SearchContentClass, SearchCursor, SearchHighlight, SearchPage,
    SearchPageLimit, SearchPageLimitError, SearchProjectionText, SearchProjectionTextError,
    SearchProjectionWriter, SearchQuery, SearchReader, SearchResult, SearchResultSource,
    SearchScope, SearchService, SearchStrategy, SearchText, SearchTextError,
    max_search_highlights_per_result, max_search_page_items, max_search_projection_text_bytes,
    max_search_query_bytes, max_search_snippet_bytes,
};
pub use session_delegation::DelegationMessageDeliveryProjection;
pub use session_live::{
    ReadSessionLiveService, SessionLiveActiveState, SessionLiveActiveTurn, SessionLiveReader,
    SessionLiveReconciliation, SessionLiveRunner, SessionLiveRunnerConnectionHealth,
    SessionLiveRunnerState, SessionLiveSnapshot, max_session_live_queued_turns,
};
pub use session_metadata::{
    ListSessionMetadataService, LoadSessionMetadataService, ReplaceSessionMetadataOutcome,
    ReplaceSessionMetadataRequest, ReplaceSessionMetadataService,
    ReplaceSessionMetadataTransaction, SessionMetadataListItem, SessionMetadataListQuery,
    SessionMetadataListQueryError, SessionMetadataLister, SessionMetadataPageReader,
    SessionMetadataReader,
};
pub use session_timeline::{
    ReadSessionTimelineService, SessionTimelineBounds, SessionTimelineDescriptor,
    SessionTimelineDetail, SessionTimelineDetailBody, SessionTimelineDetailPage,
    SessionTimelineEventKind, SessionTimelineItem, SessionTimelineReader, SessionTimelineSizeFacts,
    SessionTimelineWindow, SessionWorkFacts, TimelineAddress, TimelineBlobReference,
    TimelineBodyContinuation, TimelineBodyField, TimelineContinuation, TimelineDetailContinuation,
    TimelineDetailCursor, TimelineDetailLimitError, TimelineDetailLimits,
    TimelineModelCallDisposition, TimelineModelCallState, TimelineModelUsage, TimelineTextExcerpt,
    TimelineTurnLifecycleKind, TimelineWindowAnchor, TimelineWindowLimitError,
    TimelineWindowLimits, max_timeline_detail_bytes, max_timeline_detail_items,
    max_timeline_window_bytes, max_timeline_window_items, min_timeline_detail_bytes,
    min_timeline_window_bytes, timeline_detail_envelope_bytes,
};
pub use start_eligible_turn::{
    StartEligibleTurnIdGenerator, StartEligibleTurnOutcome, StartEligibleTurnService,
    StartEligibleTurnTransaction, UuidV7StartEligibleTurnIdGenerator,
};
pub use startup_scan::{
    StartupScanError, StartupScanIdGenerator, StartupScanOutcome, StartupScanRepository,
    StartupScanService, StartupScanSessionOutcome, UuidV7StartupScanIdGenerator,
};
pub use submit_input::{
    SubmitInputIdGenerator, SubmitInputOutcome, SubmitInputRequest, SubmitInputRequestError,
    SubmitInputService, SubmitInputTransaction, UuidV7SubmitInputIdGenerator,
};
pub use tool_dispatch_gate::{InProcessToolDispatchGate, InProcessToolDispatchPermit};
#[cfg(feature = "test-support")]
pub use tool_execution_test_support::{
    FixtureToolExecutionTransaction, FixtureTransactionFailures, PreparedAttemptApproval,
    PreparedAttemptIdentities, PreparedAttemptProposal, RecordedEvidence, RecordingToolExecutor,
    prepared_single_attempt_batch,
};
pub use tool_loop::{
    CompiledTool, CompiledToolCatalog, CorrelatedDurableChildWait, CorrelatedDurableToolCompletion,
    CorrelatedToolExecutorEvidence, DecideToolRequestService, DuplicateToolDefinition,
    NoToolCatalog, OverrideDeniedToolRequestService, RetainedToolExecutionState,
    ToolApprovalIdGenerator, ToolArgumentValidator, ToolCatalog, ToolCatalogValidationFailure,
    ToolDefinition, ToolExecutionIdGenerator, ToolExecutionInvocation, ToolExecutionService,
    ToolExecutionServiceError, ToolExecutionServiceOutcome, ToolExecutor, ToolExecutorDisposition,
    ToolExecutorEvidence, ToolInputSchema, ToolInputSchemaError, ToolInputSchemaFailure,
    ToolPreauthorization, UuidV7ToolLoopIdGenerator,
};
pub use tool_loop_ports::{
    DecideToolRequestTransaction, OverrideDeniedToolRequestTransaction,
    PrepareToolContinuationOutcome, ResolvedToolConversationEntry,
    RetainedToolAttemptObservationStatus, ToolAttemptAuthorizationOutcome,
    ToolAttemptAuthorizationStatus, ToolContinuationIdentities, ToolCrashClosureIdentities,
    ToolExecutionTransaction,
};
pub use turn_liveness::{
    AutomaticReconciliationAttempt, AutomaticReconciliationBatch,
    AutomaticReconciliationFailureKind, AutomaticReconciliationOperation,
    AutomaticReconciliationOutcome, ClaimedAutomaticReconciliation, DurableTurnLivenessObservation,
    ExhaustedAutomaticReconciliation, StaleActiveTurnBound, StaleTurnCandidate, StaleTurnOutcome,
    TurnLivenessBoundError, TurnLivenessEvidence, TurnLivenessGuardKind, TurnLivenessLedger,
    TurnLivenessScanInterval,
};
pub use update_session_placement::{
    UpdateSessionPlacementOutcome, UpdateSessionPlacementRequest, UpdateSessionPlacementService,
    UpdateSessionPlacementTransaction,
};
pub use usage::{
    UsageAggregateCompleteness, UsageAggregateGroup, UsageAggregateGroupError, UsageAggregateKey,
    UsageAggregateReport, UsageAggregateReportError, UsageAggregateTokenAxes,
    UsageCacheNormalization, UsageCallCursor, UsageCallEvidence, UsageCallKind, UsageCallOrder,
    UsageCallPage, UsageCallPageContinuation, UsageCallPageError, UsageCallPageLimit,
    UsageCallPageLimitError, UsageCallQuery, UsageCallScope, UsageCredentialProfileLabel,
    UsageCredentialProfileLabelError, UsageInputTokenSemantics, UsageProvenance, UsageQuery,
    UsageReader, UsageSelection, UsageService, UsageTimeFromInclusive, UsageTimeRange,
    UsageTimeRangeError, UsageTimeToExclusive, UsageTimestampError, UsageTimestampMicros,
    UsageTokenAxes, UsageTokenAxis, UsageTokenCoverage, UsageTokenPresence,
    max_usage_aggregate_calls, max_usage_aggregate_groups, max_usage_call_page_items,
    max_usage_credential_profile_utf8_bytes,
};
pub use workspace_instructions::{
    InstructionDiscoveryFinding, InstructionDiscoveryFindingKind, InstructionDiscoveryLimitKind,
    InstructionDiscoveryRoot, InstructionDiscoverySnapshot, discover_workspace_instructions,
};
