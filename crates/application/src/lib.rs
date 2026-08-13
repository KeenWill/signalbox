//! Application orchestration boundary for Signalbox.
//!
//! This crate coordinates domain decisions and external effects while
//! depending inward on `signalbox-domain`.

mod abandon_lost_runner;
mod approval_judge;
mod conversation_import;
mod create_session;
mod create_session_from_imported_frontier;
mod list_conversations;
mod load_session;
mod model_execution;
mod operator_failure;
mod promote_pending_runner;
mod replace_lost_runner_before_pin;
mod replace_session_defaults;
mod repo_watch;
mod review_orchestration;
mod review_workflow;
mod runner_replacement_provisioning;
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
mod update_session_placement;

pub use abandon_lost_runner::{
    AbandonLostRunnerOutcome, AbandonLostRunnerRequest, AbandonLostRunnerService,
    AbandonLostRunnerTransaction,
};
pub use approval_judge::ApprovalJudgeAuthorization;
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
    CommitModelCallObservationTransaction, FailPreparedModelCallTransaction,
    InProcessAttemptDispatchGate, InProcessAttemptDispatchPermit, ModelCallAuthorizationReread,
    ModelCallCapabilityPreparation, ModelCallCredentialReference, ModelCallExecutionError,
    ModelCallExecutionIdGenerator, ModelCallExecutionOutcome, ModelCallExecutionService,
    ModelCallInputTokenCount, ModelCallInputTokenCounter, ModelCallProvider,
    ModelCallTerminalIdentityCandidates, ModelConversationMessage, ModelFrontierRenderingError,
    ModelToolResultContent, PrepareModelCallOutcome, PrepareModelCallTransaction,
    PreparedModelOperation, ResolvedRunnerPlacementConversationEntry,
    RetainedCapabilityFailureStatus, RetainedModelCallExecutionState,
    RetainedModelCallObservationStatus, ScriptedModelCallCapability, ScriptedModelCallError,
    ScriptedModelCallProvider, ScriptedModelCallStep, UuidV7ModelCallExecutionIdGenerator,
};
pub use operator_failure::{ClassifyOperatorFailure, OperatorFailureClass};
pub use promote_pending_runner::{
    PromotePendingRunnerOutcome, PromotePendingRunnerRequest, PromotePendingRunnerService,
    PromotePendingRunnerTransaction,
};
pub use replace_lost_runner_before_pin::{
    ReplaceLostRunnerBeforePinOutcome, ReplaceLostRunnerBeforePinRequest,
    ReplaceLostRunnerBeforePinService, ReplaceLostRunnerBeforePinTransaction,
};
pub use replace_session_defaults::{
    PromptMemberStatement, ReplaceSessionDefaultsOutcome, ReplaceSessionDefaultsRequest,
    ReplaceSessionDefaultsService, ReplaceSessionDefaultsTransaction,
};
pub use repo_watch::{
    RepoWatchBranchHead, RepoWatchCheckCompletionGeneration,
    RepoWatchCheckCompletionGenerationError, RepoWatchCheckRunObservation,
    RepoWatchCheckSuiteObservation, RepoWatchDifferError, RepoWatchDispatchGoal,
    RepoWatchDispatchIdGenerator, RepoWatchDispatchPreparationError, RepoWatchDispatchService,
    RepoWatchDispatchServiceError, RepoWatchDispatchTransaction, RepoWatchEventIdGenerator,
    RepoWatchObservation, RepoWatchPreparedDispatchAction, RepoWatchPullRequestLifecycle,
    RepoWatchPullRequestState, RepoWatchPullRequestStateInput, RepoWatchReactionObservation,
    RepoWatchRepositoryState, RepoWatchRepositoryStateError, RepoWatchRepositoryStateInput,
    RepoWatchResolvedTemplate, RepoWatchReviewObservation, RepoWatchRuleEvaluation,
    RepoWatchRuleEvaluationOutcome, RepoWatchSingletonKey, RepoWatchTemplateResolver,
    RepoWatchThreadObservation, RepoWatchThreadState, RepoWatchWorkflowRunObservation,
    UuidV7RepoWatchDispatchIdGenerator, UuidV7RepoWatchEventIdGenerator, derive_repo_watch_events,
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
pub use runner_replacement_provisioning::{
    RunnerReplacementProvisioningIdGenerator, RunnerReplacementProvisioningOutcome,
    RunnerReplacementProvisioningRequest, RunnerReplacementProvisioningService,
    RunnerReplacementProvisioningStage, RunnerReplacementProvisioningTransaction,
    UuidV7RunnerReplacementProvisioningIdGenerator,
};
pub use scheduler::{
    EligibilityNudge, EligibilityNudgeOutcome, EligibilityPass, EligibilitySweep,
    EligibilitySweepBatch, EligibilityWorkSource, GoalAwareEligibilityPass,
    GoalAwareEligibilityPassError, GoalPassDisposition, InProcessEligibilityNudge,
    InProcessEligibilityWorkSource, InvalidReconciliationSweepInterval,
    ReconciliationSweepInterval, SchedulerLoop, SchedulerLoopExit,
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
    NoToolCatalog, RetainedToolExecutionState, ToolApprovalIdGenerator, ToolArgumentValidator,
    ToolCatalog, ToolCatalogValidationFailure, ToolDefinition, ToolExecutionIdGenerator,
    ToolExecutionInvocation, ToolExecutionService, ToolExecutionServiceError,
    ToolExecutionServiceOutcome, ToolExecutor, ToolExecutorDisposition, ToolExecutorEvidence,
    ToolInputSchema, ToolInputSchemaError, ToolInputSchemaFailure, UuidV7ToolLoopIdGenerator,
};
pub use tool_loop_ports::{
    DecideToolRequestTransaction, PrepareToolContinuationOutcome, ResolvedToolConversationEntry,
    RetainedToolAttemptObservationStatus, ToolAttemptAuthorizationStatus,
    ToolContinuationIdentities, ToolCrashClosureIdentities, ToolExecutionTransaction,
};
pub use update_session_placement::{
    UpdateSessionPlacementOutcome, UpdateSessionPlacementRequest, UpdateSessionPlacementService,
    UpdateSessionPlacementTransaction,
};
