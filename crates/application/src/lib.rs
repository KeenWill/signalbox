//! Application orchestration boundary for Signalbox.
//!
//! This crate coordinates domain decisions and external effects while
//! depending inward on `signalbox-domain`.

mod approval_judge;
mod commissioned_dispatch;
mod conversation_import;
mod create_session;
mod create_session_from_imported_frontier;
mod list_conversations;
mod load_session;
mod model_execution;
mod operator_failure;
mod replace_session_defaults;
mod repo_watch;
mod repo_watch_webhook;
mod review_orchestration;
mod review_workflow;
mod scheduler;
mod session_delegation;
mod session_metadata;
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

pub use approval_judge::{
    ApprovalJudgeAuthorization, ApprovalJudgeBranchAuthority, ApprovalJudgeBranchAuthorityInput,
    ApprovalJudgeCompletionIdentities, ApprovalJudgeDispatchAuthority,
    ApprovalJudgeDispatchProvenance, ApprovalJudgePullRequestAuthority,
    ApprovalJudgePullRequestAuthorityInput,
};
pub use commissioned_dispatch::{
    CommissionDispatchPreparationError, CommissionDispatchRequest, CommissionedDispatchFence,
    CommissionedDispatchIdGenerator, PreparedCommissionedDispatch,
    UuidV7CommissionedDispatchIdGenerator,
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
    AttemptDispatchGate, AuthorizeModelCallOutcome, AuthorizeModelCallTransaction,
    AvailabilitySuccessorOutcome, CommitModelCallObservationTransaction,
    CredentialPoolExhaustedOutcome, FailPreparedModelCallTransaction, InProcessAttemptDispatchGate,
    InProcessAttemptDispatchPermit, ModelCallAuthorizationReread, ModelCallCapabilityPreparation,
    ModelCallCredentialReference, ModelCallExecutionError, ModelCallExecutionIdGenerator,
    ModelCallExecutionOutcome, ModelCallExecutionService, ModelCallInputTokenCount,
    ModelCallInputTokenCounter, ModelCallObservationCommitOutcome, ModelCallProvider,
    ModelCallTerminalIdentityCandidates, ModelConversationMessage, ModelFrontierRenderingError,
    ModelToolResultContent, PrepareModelCallOutcome, PrepareModelCallTransaction,
    PreparedModelOperation, RetainedCapabilityFailureStatus, RetainedModelCallExecutionState,
    RetainedModelCallObservationStatus, ScriptedModelCallCapability, ScriptedModelCallError,
    ScriptedModelCallProvider, ScriptedModelCallStep, UuidV7ModelCallExecutionIdGenerator,
};
pub use operator_failure::{ClassifyOperatorFailure, OperatorFailureClass};
pub use replace_session_defaults::{
    PromptMemberStatement, ReplaceSessionDefaultsOutcome, ReplaceSessionDefaultsRequest,
    ReplaceSessionDefaultsService, ReplaceSessionDefaultsTransaction,
};
pub use repo_watch::{
    RepoWatchBranchHead, RepoWatchCheckCompletionGeneration,
    RepoWatchCheckCompletionGenerationError, RepoWatchCheckRunObservation,
    RepoWatchCheckSuiteObservation, RepoWatchDifferError, RepoWatchDifferFailureKind,
    RepoWatchDispatchIdGenerator, RepoWatchDispatchPreparationError, RepoWatchDispatchService,
    RepoWatchDispatchServiceError, RepoWatchDispatchTransaction, RepoWatchEventContentIdentityV1,
    RepoWatchEventIdGenerator, RepoWatchEventIdentityFrontierEntryV1,
    RepoWatchEventIdentityFrontierError, RepoWatchEventIdentityFrontierV1,
    RepoWatchEventOccurrenceV1, RepoWatchObservation, RepoWatchPreparedDispatchAction,
    RepoWatchPullRequestLifecycle, RepoWatchPullRequestState, RepoWatchPullRequestStateInput,
    RepoWatchReactionObservation, RepoWatchRepositoryState, RepoWatchRepositoryStateError,
    RepoWatchRepositoryStateInput, RepoWatchResolvedTemplate, RepoWatchReviewObservation,
    RepoWatchRuleEvaluation, RepoWatchRuleEvaluationOutcome, RepoWatchSingletonKey,
    RepoWatchTemplateResolver, RepoWatchThreadObservation, RepoWatchThreadState,
    RepoWatchWorkflowRunObservation, UuidV7RepoWatchDispatchIdGenerator,
    UuidV7RepoWatchEventIdGenerator, derive_repo_watch_events,
    repo_watch_events_have_equal_identified_content,
};
pub use repo_watch_webhook::{
    RepoWatchBranchHeadPreviousV1, RepoWatchObservationApplyV1, RepoWatchObservationChangeV1,
    RepoWatchObservationPatchV1, RepoWatchPullRequestHeadGuardV1,
    RepoWatchPullRequestMissingPolicyV1, RepoWatchTargetedRefreshCoalescerV1,
    RepoWatchTargetedRefreshV1, RepoWatchWebhookApplyError, RepoWatchWebhookBodyReferenceV1,
    RepoWatchWebhookDeliveryV1, RepoWatchWebhookDeliveryV1Input, RepoWatchWebhookIgnoredReasonV1,
    RepoWatchWebhookMappedNoChangeV1, RepoWatchWebhookMappingError, RepoWatchWebhookMappingV1,
    RepoWatchWebhookPullRequestContextV1, RepoWatchWebhookPullRequestContextV1Input,
    apply_repo_watch_observation_patch_v1, map_repo_watch_webhook_delivery_v1,
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
    SchedulerPassExpiryHandler, SchedulerPassOccupancyBound, scheduler_pass_admission_cap,
};
pub use session_delegation::DelegationMessageDeliveryProjection;
pub use session_metadata::{
    ListSessionMetadataService, LoadSessionMetadataService, ReplaceSessionMetadataOutcome,
    ReplaceSessionMetadataRequest, ReplaceSessionMetadataService,
    ReplaceSessionMetadataTransaction, SessionMetadataListItem, SessionMetadataListQuery,
    SessionMetadataListQueryError, SessionMetadataLister, SessionMetadataPageReader,
    SessionMetadataReader,
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
    UuidV7ToolLoopIdGenerator,
};
pub use tool_loop_ports::{
    DecideToolRequestTransaction, OverrideDeniedToolRequestTransaction,
    PrepareToolContinuationOutcome, ResolvedToolConversationEntry,
    RetainedToolAttemptObservationStatus, ToolAttemptAuthorizationStatus,
    ToolContinuationIdentities, ToolCrashClosureIdentities, ToolExecutionTransaction,
};
pub use turn_liveness::{
    ClaimedModelCallReconciliation, ExhaustedModelCallReconciliation,
    ModelCallReconciliationAttempt, ModelCallReconciliationBatch,
    ModelCallReconciliationFailureKind, ModelCallReconciliationOutcome, StaleActiveTurnBound,
    StaleTurnCandidate, StaleTurnOutcome, TurnLivenessBoundError, TurnLivenessEvidence,
    TurnLivenessLedger, TurnLivenessScanInterval,
};
pub use update_session_placement::{
    UpdateSessionPlacementOutcome, UpdateSessionPlacementRequest, UpdateSessionPlacementService,
    UpdateSessionPlacementTransaction,
};
