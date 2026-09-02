//! Core domain boundary for Signalbox.
//!
//! Domain identities are distinct from storage, protocol, and framework types.
//! Lifecycle behavior is introduced only in slices authorized by accepted
//! decisions; aggregate orchestration and product behavior remain deferred.
//! [`AcceptedInputLifecycle`] is the canonical public boundary for validated
//! disposition transitions that preserve an accepted input's identity.

mod accepted_input;
mod actor;
mod applied_interrupt;
mod blob;
mod configuration;
mod context_compaction;
mod context_frontier;
mod delivery_request;
mod fatal_mismatch;
mod git_remote;
mod goal;
mod goal_command;
mod imported_conversation;
mod imported_session;
mod model_call;
mod model_execution;
mod model_settings;
mod program_journal;
mod provider_evidence;
mod queue_order;
mod replace_session_defaults;
mod repo_watch;
mod review_workflow;
mod runner;
mod semantic_entry;
mod session;
mod session_delegation;
mod session_lifecycle;
mod session_lifecycle_command;
mod session_metadata;
mod session_placement;
mod session_template;
mod submit_input;
mod tool;
mod tool_attempt;
mod tool_execution;
mod turn_attempt;
mod turn_eligibility;
mod turn_lifecycle;
mod user_content;
mod workspace;
mod workspace_instruction;

pub use accepted_input::{
    AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputLifecycleTransitionError,
    SteeringBinding, SteeringReclassificationReason,
};
pub use actor::Actor;
pub use applied_interrupt::{AppliedInterruptCommandResult, AppliedInterruptProof};
pub use blob::{
    BlobDerivation, BlobDerivationError, BlobDerivationProducer, BlobDigest, BlobDigestParseError,
    BlobDigestParseFailure, BlobTransformation, BlobTransformationError, BlobTransformationName,
    DeterministicBlobDerivationKey,
};
pub use configuration::{
    ConfigurationRequest, DirectModelSelection, EffectiveConfiguration, FrozenAliasDefinition,
    FrozenModelSelection, KnownProviderFailureRetry, ModelAlias, ModelFallback, ModelParameters,
    ModelSelectionOverride, ModelSelectionRequest, OriginConfiguration,
    OriginConfigurationReconstitutionInput, OriginModelSettingsError, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionDefaultsVersionMismatch, SessionSystemPrompt,
    SessionSystemPromptError, SessionSystemPromptFailure, TurnConfigurationProvenance,
    UnknownModelAlias, VersionCheckedConfigurationRequest, VersionedSessionConfigurationDefaults,
};
pub use context_compaction::{
    ContextCompaction, ContextCompactionId, ContextCompactionModelCall,
    ContextCompactionModelCallReconstitutionFailure, ContextCompactionModelCallReconstitutionInput,
    ContextCompactionModelCallState, ContextCompactionRange,
    ContextCompactionReconstitutionFailure, ContextCompactionReconstitutionInput,
    ContextCompactionTokenUsage, ContextFrontierProjection, ContextFrontierProjectionFailure,
};
pub use context_frontier::{
    ContextFrontier, ContextFrontierId, ResolvedContextFrontierReconstitutionInput,
    ResolvedContextFrontierSnapshot, SemanticTranscriptEntryId, SemanticTranscriptEntryRef,
};
pub use delivery_request::{DeliveryRequest, PerInputConfigurationChoices};
pub use git_remote::{
    ConfiguredGitRemoteRecord, GitRemoteName, GitRemoteTextError, GitRemoteUrl,
    max_git_remote_name_bytes, max_git_remote_url_bytes,
};
pub use goal::{
    FinishConditionStatement, Goal, GoalBlockProvenance, GoalBlockedReasonKind, GoalEvent,
    GoalEventKind, GoalEventOrdinal, GoalGeneration, GoalGenerationSnapshot, GoalGuidance,
    GoalModelBlockedReasonKind, GoalModelProvenance, GoalNeed, GoalReconstitutionError,
    GoalReconstitutionFailure, GoalReconstitutionInput, GoalReport, GoalReportRef,
    GoalSchedulerProvenance, GoalState, GoalStatement, GoalTextError, GoalTransitionError,
    GoalTransitionFailure, GoalTurnSource, GoalUserProvenance,
};
pub use goal_command::{
    GoalCommandRejection, GoalCommandResult, GoalUserAction, GoalUserCommand,
    ReconstitutedGoalCommand,
};
pub use imported_conversation::{
    ImportedConversation, ImportedConversationDisplayTitle, ImportedConversationDisplayTitleError,
    ImportedConversationFormat, ImportedConversationReconstitutionError,
    ImportedConversationReconstitutionFailure, ImportedConversationReconstitutionInput,
    ImportedConversationSourceDigest, ImportedJsonNumber, ImportedJsonNumberError,
    ImportedMediaSource, ImportedMessageContentAbsence, ImportedRawRecordConversionDigest,
    ImportedRawRecordHash, ImportedRawRecordPosition, ImportedRawSourceRecord,
    ImportedRawSourceRecordReconstitutionInput, ImportedRecordEntryPosition,
    ImportedSourceAttestation, ImportedSourceMetadata, ImportedSpeaker,
    ImportedStructuredFieldError, ImportedStructuredObjectMember, ImportedStructuredValue,
    ImportedText, ImportedToolResultBlock, ImportedToolResultValue, ImportedTranscriptContent,
    ImportedTranscriptEntry, ImportedTranscriptEntryInput, ImportedTranscriptFrontier,
    ImportedTranscriptPosition, imported_bool_attestation, imported_string_structured_attestation,
    imported_structured_attestation, imported_text_attestation, unique_imported_structured_field,
};
pub use imported_session::{
    BoundedImportedSessionReconstitutionError, BoundedImportedSessionReconstitutionFailure,
    BoundedImportedSessionReconstitutionInput, CreateSessionFromImportedFrontierAppliedResult,
    CreateSessionFromImportedFrontierPreparationError,
    CreateSessionFromImportedFrontierPreparationFailure,
    CreateSessionFromImportedFrontierReconstitutionError,
    CreateSessionFromImportedFrontierReconstitutionFailure,
    CreateSessionFromImportedFrontierReconstitutionInput,
    ImportedSessionNormalizedReconstitutionError, ImportedSessionNormalizedReconstitutionInput,
    ImportedSessionReconstitutionError, ImportedSessionReconstitutionFailure,
    ImportedSessionReconstitutionInput, ImportedSessionSeedHeaderReconstitutionInput,
    ImportedSessionSeedReconstitutionFailure, ImportedSessionSeedReconstitutionInput,
    PreparedCreateSessionFromImportedFrontier, ReconstitutedImportedSession,
    ReconstitutedSessionCreationFromImportedFrontier,
};
pub use model_call::{
    CurrentModelCall, CurrentModelCallState, EndedModelCall, ModelCallDisposition,
    ModelCallReconstitutionFailure, ModelCallReconstitutionInput, ModelCallReconstitutionState,
    PinnedProviderTarget, PinnedProviderTargetReconstitutionInput, ProviderModelIdentity,
    ReconstitutedModelCall, ResolvedProviderTarget,
};
pub use model_execution::{
    AmbiguousModelCallTurn, AmbiguousModelCallTurnIdentities, AuthorizedModelCall,
    AvailabilitySuccessorModelCallTurn, CancelledModelCallTurn, CancelledModelCallTurnIdentities,
    CancelledToolRoundModelCallTurn, CompletedModelCallIdentities, CompletedModelCallTurn,
    ContextHeadroomExhaustedModelCallTurn, CorrelatedModelCallTerminalObservation,
    CredentialPoolExhaustedModelCallTurn, FailedModelCallTurn, FailedModelCallTurnIdentities,
    IssuedModelCallCorrelation, ModelCallAuthorizationError, ModelCallAuthorizationFailure,
    ModelCallClosureError, ModelCallExecution, ModelCallExecutionReconstitutionError,
    ModelCallExecutionReconstitutionFailure, ModelCallExecutionReconstitutionInput,
    ModelCallInterruptOutcome, ModelCallOriginContent, ModelCallPreparationError,
    ModelCallPreparationFailure, ModelCallResumeFailure, ModelCallTerminalIdentities,
    ModelCallTerminalObservation, ModelCallTerminalOutcome, ModelTargetCatalog,
    ModelTargetCatalogError, ModelTargetDefinition, ModelTargetResolutionError,
    PendingSteeringReclassificationIdentity, PhysicalCancellationModelCallTurnIdentities,
    PreparedInitialModelCall, PreparedModelCallRequest, PreparedSteeringConsumption,
    ProviderModelCallFailureCause, ProviderReportedTokenUsage, ReclassifiedPendingSteeringTurn,
    ReconciliationRequiredModelCallTurn, ReconciliationRequiredToolTurn, RefusedModelCallTurn,
    RefusedModelCallTurnIdentities, ResolvedModelSelection, StopRequestedModelCallTurn,
    StoppedToolResponsePartIdentity, StoppedToolRoundModelCallIdentities, ToolResponsePartIdentity,
    ToolResultAttemptCorrelation, ToolRoundModelCallIdentities, ToolRoundModelCallTurn,
};
pub use model_settings::{
    AdjustedModelSettings, AnthropicServiceTier, CodexCliServiceTier, CompatibleModelSettings,
    EffectiveModelSettings, FastMode, FastModeOverlay, FastModeSupport, ModelCapabilities,
    ModelCapabilityCatalog, ModelCapabilityCatalogError, ModelCapabilityDefinition,
    ModelChangeAdjustment, ModelSettingSource, ModelSettingsOverlay, ModelSettingsPrecedence,
    OpenAiServiceTier, ReasoningLevel, ResolvedModelSettings, ServiceTier,
    SessionModelSettingsChanged, SettingOverlay, TurnModelSettingsResolved,
    UnsupportedModelSetting, ValidatedModelSettings,
};
pub use program_journal::{
    DeliveryFrame, DeliveryKind, DeliveryOrdinal, EffectRequest, FaultCause, FaultEvidenceRef,
    InlineFramePayload, JournalEntry, JournalFrame, JournalPosition, NondeterminismError,
    ProgramCapability, ProgramFault, ProgramJournal, ProgramJournalError, RejectReason,
    ReplayCursor, ReplayInstruction, ReplayedRequest, RequestFrame, RequestKind, RequestOrdinal,
    ScopeOperation, ScopeOrdinal, ScopeRequest,
};
pub use provider_evidence::{
    ProviderTargetEvidence, ProviderTargetEvidenceLog, ProviderTargetMismatchInvalidation,
    ProviderTargetMismatchInvalidationLog, ProviderTargetObservation,
};
pub use queue_order::{
    AcceptedInputQueueOrder, AcceptedInputQueueOrderError, AcceptedInputQueuePriority,
    AcceptedInputQueueWork, SessionInputPosition, derive_accepted_input_total_order,
};
pub use replace_session_defaults::{
    PreparedReplaceSessionDefaults, ReconstitutedReplaceSessionDefaults, ReplaceSessionDefaults,
    ReplaceSessionDefaultsAppliedResult, ReplaceSessionDefaultsCurrentVersionMismatch,
    ReplaceSessionDefaultsPreparationError, ReplaceSessionDefaultsReconstitutionError,
    ReplaceSessionDefaultsReconstitutionFailure, ReplaceSessionDefaultsReconstitutionInput,
    ReplaceSessionDefaultsRejectedResult, ReplaceSessionDefaultsResult,
    ReplaceSessionDefaultsSessionNotFound, ReplaceSessionDefaultsVersionExhausted,
};
pub use repo_watch::{
    BranchContext, BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha,
    DispatchSessionAction, DispatchSessionParameters, GitHubObjectId, LabelName, MergeableState,
    PullRequestBody, PullRequestContext, PullRequestEventContext, PullRequestEventContextInput,
    PullRequestNumber, PullRequestTitle, ReactionChange, ReactionContent, ReactionSubject,
    RepoWatchActionV1, RepoWatchAuthorLogin, RepoWatchDispatchContextError,
    RepoWatchDispatchContextShape, RepoWatchEvent, RepoWatchEventConstructionError,
    RepoWatchEventKindNameV1, RepoWatchEventKindV1, RepoWatchEventTarget, RepoWatchLabelMatcher,
    RepoWatchLabelMatcherInput, RepoWatchMatcherV1, RepoWatchMatcherV1Input, RepoWatchPattern,
    RepoWatchRule, RepoWatchRuleActionV1, RepoWatchRuleContentDigest, RepoWatchRuleId,
    RepoWatchRuleIdentityField, RepoWatchRuleIdentityFieldDigest, RepoWatchRuleValidationError,
    RepoWatchRuleVersion, RepoWatchSingletonScope, RepoWatchTemplateContextDeclaration,
    RepoWatchTemplateContextDeclarationError, RepoWatchTextError, RepoWatchWorkflowRunAttempt,
    RepositorySlug, ReviewState, ReviewThreadId, WorkflowName,
};
pub use review_workflow::{
    ReviewChangeRequestNumber, ReviewConfidence, ReviewConfidenceError, ReviewEventOrdinal,
    ReviewExternalLink, ReviewExternalLinkAssociation, ReviewExternalLinkAttachment,
    ReviewExternalLinkAttachmentResult, ReviewExternalLinkClaim, ReviewExternalLinkNoChangeResult,
    ReviewExternalLinkObservation, ReviewExternalLinkObservationResult,
    ReviewExternalLinkPublicationBlockedResult, ReviewExternalLinkTransitionError,
    ReviewExternalLinkTransitionFailure, ReviewExternalObjectClaim, ReviewExternalObjectClaimError,
    ReviewExternalObjectKind, ReviewExternalObjectState, ReviewFinding,
    ReviewFindingConfidenceAxes, ReviewFindingContent, ReviewFindingDiffSide, ReviewFindingEvent,
    ReviewFindingEventKind, ReviewFindingEventResult, ReviewFindingEventResultKind,
    ReviewFindingEventType, ReviewFindingExternalLinkError, ReviewFindingExternalLinkFailure,
    ReviewFindingExternalLinkRef, ReviewFindingLocation, ReviewFindingPendingExternalLinkRef,
    ReviewFindingProposal, ReviewFindingRef, ReviewFindingReferenceGraphError,
    ReviewFindingSeverity, ReviewFindingStatus, ReviewFindingTransitionError,
    ReviewFindingTransitionFailure, ReviewKey, ReviewLineRange, ReviewLineRangeError, ReviewPass,
    ReviewPassAcceptedInputEvidence, ReviewPassConstructionError, ReviewPassConstructionFailure,
    ReviewPassEvidence, ReviewPassKind, ReviewPassReconstitutionError,
    ReviewPassReconstitutionFailure, ReviewPassReconstitutionInput, ReviewPassRef,
    ReviewPassResult, ReviewPassState, ReviewPassTransitionError, ReviewPassTransitionFailure,
    ReviewPassTurnEvidence, ReviewPassTurnOutcome, ReviewPolicy, ReviewPolicyError,
    ReviewPolicyVersion, ReviewPositiveNumberError, ReviewProducedFindings,
    ReviewProducedFindingsError, ReviewReferencedFindingEvidence, ReviewRun, ReviewRunEvidence,
    ReviewRunEvidenceFailure, ReviewRunReconstitutionError, ReviewRunReconstitutionInput,
    ReviewRunRef, ReviewRunState, ReviewRunTransitionError, ReviewRunTransitionFailure,
    ReviewTarget, ReviewTargetError, ReviewTargetParentRef, ReviewTargetSubject, ReviewText,
    ReviewValueError, ReviewValueFailure, ReviewWorkflowKind,
    validate_complete_review_finding_reference_graph,
};
pub use runner::{
    AbandonedRunnerPlacement, CanonicalCloneUrlDigest, CredentialDispatchAuthorization,
    CredentialProfileChange, CredentialProfileGrant, CredentialProfileGrantReconstitutionInput,
    CredentialProfileGrantReplacement, CredentialProfileGrantState, CredentialProfileName,
    CredentialProfilePlacementReplacement, CredentialProfilePolicy, CredentialToolApproval,
    LostPinnedRunnerPlacement, PinnedRunnerPlacement, PreparedRunnerRegistration,
    ProvisionedWorkspace, RunnerAdvertisement, RunnerCapabilityClass, RunnerCatalog,
    RunnerClaimedAttemptReplacement, RunnerCredentialGrantChange, RunnerCredentialGrantLineage,
    RunnerDomainError, RunnerEnrollment, RunnerEnrollmentReconstitutionInput,
    RunnerEnrollmentState, RunnerGeneration, RunnerLease, RunnerLeaseCorrelation, RunnerLeaseLoss,
    RunnerLeaseNoExecutionProof, RunnerLeaseOfferRequest, RunnerLeaseReconstitutionInput,
    RunnerLeaseRetryAuthority, RunnerLeaseRetryPreparation, RunnerLeaseState, RunnerLostBeforePin,
    RunnerPlacementChange, RunnerPlacementLossSource, RunnerPlacementReconstitutionHistory,
    RunnerPlacementReplacement, RunnerPrePinReplacement, RunnerPrePinReplacementHistory,
    RunnerRepositoryEntry, RunnerSandboxProfile, RunnerSelector, RunnerToolAttemptAuthorization,
    RunnerToolDeclaration, RunnerToolEffectClass, RunnerToolModelDefinition,
    RunnerToolPermissionOverride, RunnerToolPermissionOverrides,
    RunnerUnclaimedAttemptReauthorization, RunnerWorkingDirectory, SessionRunnerPin,
    SessionRunnerPlacement, SessionRunnerPlacementReconstitutionInput,
    SessionRunnerPlacementRequest, SessionRunnerPlacementState, ToolAdmissibleLoci,
    ValidatedRunnerRegistration, ValidatedRunnerRegistrationReconstitutionInput,
    WorkingDirectorySelection, WorkspaceBranchName, WorkspaceCapability, WorkspaceRecovery,
    WorkspaceRelativePath, WorkspaceRepositoryKey, WorkspaceRequirement, WorkspaceRevision,
};
pub(crate) use semantic_entry::InitialSemanticTranscriptEntryPayload;
pub use semantic_entry::{
    AssistantText, SemanticTranscriptEntry, SemanticTranscriptEntryPayload,
    SemanticTranscriptEntryReconstitutionInput,
};
pub use session::{
    CreateSession, CreateSessionAppliedResult, CreateSessionFromImportedFrontier,
    CreateSessionPreparationError, CreateSessionPreparationFailure,
    CreateSessionReconstitutionError, CreateSessionReconstitutionFailure,
    CreateSessionReconstitutionInput, ImportedSessionRelationship, ImportedSessionSeed,
    InitialSession, PreparedCreateSession, ReconstitutedSessionCreation, Session,
    SessionCreationCause, SessionCreationProvenance, SessionPlacementReconstitutionFacts,
    SessionReconstitutionError, SessionReconstitutionFailure, SessionReconstitutionInput,
    TranscriptAncestry, TranscriptFrontier,
};
pub use session_delegation::{
    BoundChildAction, ChildRelationshipPolicy, ChildWait, DelegatedSpawnRequest,
    DelegationAwaitRequest, DelegationContent, DelegationContentError, DelegationContentFailure,
    DelegationEvent, DelegationEventOrdinal, DelegationLifecycle, DelegationMessage,
    DelegationMessageDirection, DelegationMessageEndpoints, DelegationMessageRequest,
    DelegationOutcome, DelegationOutcomeKind, DelegationOutcomeReason, DelegationProvenance,
    DelegationProvenanceProjection, DelegationProvenanceReconstitutionInput,
    DelegationRequestError, DelegationRequestFailure, DelegationTransitionError,
    DelegationTransitionFailure, DelegationWait, DelegationWaitMode, DescendantTerminationScope,
    ParentTerminationAuthority, ParentTerminationCommandSource, ParentTerminationKind,
    RejectedDelegationTransition, SessionDelegation, SessionDelegationReconstitutionError,
    SessionDelegationReconstitutionFailure, SessionDelegationReconstitutionInput,
    TerminalChildTurn, await_session_tool_name, send_session_message_tool_name,
    spawn_session_tool_name,
};
pub use session_lifecycle::{
    CoreAgency, DispatchingModule, LifecycleActor, ModuleDispatch, SessionClosureOutcome,
    SessionDeadlineExpiry, SessionDeadlineKind, SessionFailureCause, SessionLifecycleState,
    SessionLifecycleTransitionError, SessionOwnership, SessionOwnershipTransition,
    SessionParkCause, SessionParkResponder, SessionRecoveryOperation, SessionRetirementCause,
    SessionRetryableCause, SessionStructuralCause, SessionTerminalOutcome, SessionWait,
    SessionWaitKind, SessionWaker, StopStickiness,
};
pub use session_lifecycle_command::{
    CommandPrincipal, FinishCheckVerdict, FinishCondition, SessionLifecycleApplication,
    SessionLifecycleCommand, SessionLifecycleCommandRejection, SessionLifecycleCommandResult,
    SessionLifecycleOperation, StartGate,
};
pub use session_metadata::{
    PreparedReplaceSessionMetadata, ReconstitutedReplaceSessionMetadata, ReplaceSessionMetadata,
    ReplaceSessionMetadataAppliedResult, ReplaceSessionMetadataReconstitutionError,
    ReplaceSessionMetadataReconstitutionFailure, ReplaceSessionMetadataReconstitutionInput,
    ReplaceSessionMetadataRejectedResult, ReplaceSessionMetadataResult,
    ReplaceSessionMetadataSessionNotFound, SessionMetadataContent, SessionMetadataContentError,
    SessionMetadataLastWriter, SessionMetadataSnapshot, SessionMetadataUpdatedAt,
};
pub use session_placement::{
    RootPlacementGlobalReadIntent, SessionPlacement, SessionPlacementDirectory,
    SessionPlacementError, SessionPlacementEvent, SessionPlacementEventKind, SessionPlacementPath,
    SessionPlacementPathError, SessionPlacementVersion, SessionReadRefusalReason,
    SessionReadScopeDecision, SessionReadScopeRefusal, UpdateSessionPlacement,
    UpdateSessionPlacementApplied, UpdateSessionPlacementRejection,
    UpdateSessionPlacementRejectionKind, UpdateSessionPlacementResult, VersionedSessionPlacement,
};
pub use session_template::{
    SessionTemplateContentDigest, SessionTemplateName, SessionTemplateNameError,
    SessionTemplateNameFailure, SessionTemplateProvenance, SessionTemplateVersion,
};
pub use submit_input::{
    GoalTurnOriginConstructionInput, NonAcceptedTurnPredecessorReconstitutionInput,
    PreparedSubmitInput, ReconstitutedSubmitInput, SubmitInput,
    SubmitInputAppliedPendingSteeringReconstitutionInput, SubmitInputAppliedResult,
    SubmitInputAppliedTurnOriginReconstitutionInput,
    SubmitInputAutomaticReconciliationConstructionInput,
    SubmitInputDirectTurnOriginConstructionInput,
    SubmitInputInterruptedModelCallReconciliationConstructionInput,
    SubmitInputInterruptedToolReconciliationConstructionInput,
    SubmitInputPendingSteeringAppliedResult, SubmitInputPreparationError,
    SubmitInputPreparationFailure, SubmitInputReclassifiedTurnOriginConstructionInput,
    SubmitInputReconstitutionError, SubmitInputReconstitutionFailure,
    SubmitInputReconstitutionInput,
    SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput,
    SubmitInputRejectedActiveTurnMismatchReconstitutionInput,
    SubmitInputRejectedActiveTurnPresentReconstitutionInput,
    SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput,
    SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput,
    SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput,
    SubmitInputRejectedInterruptAlreadyAppliedReconstitutionInput,
    SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput,
    SubmitInputRejectedNoActiveTurnReconstitutionInput, SubmitInputRejectedResult,
    SubmitInputRejectedSafePointUnavailableWhileStoppingReconstitutionInput,
    SubmitInputRejectedSessionNotFoundReconstitutionInput,
    SubmitInputRejectedUnknownModelAliasReconstitutionInput, SubmitInputResult,
    SubmitInputTerminalSourceConstructionInput, SubmitInputTerminalSourceReconstitutionInput,
    SubmitInputTurnOriginAppliedResult, SubmitInputTurnOriginReconstitutionInput,
};
pub use tool::{
    AssistantResponsePart, DangerousToolAutoApproval, DecideToolRequest,
    DecideToolRequestAppliedResult, DecideToolRequestConstructionError,
    DecideToolRequestPreparationError, DecideToolRequestRejectedResult, DecideToolRequestResult,
    DelegateApprovalRecommendation, DelegateToolApproval, DelegateToolApprovalError,
    InitialToolApproval, NormalizedToolArguments, OverrideDeniedToolRequest,
    OverrideDeniedToolRequestAppliedResult, OverrideDeniedToolRequestConstructionError,
    OverrideDeniedToolRequestPreparationError, OverrideDeniedToolRequestRejectedResult,
    OverrideDeniedToolRequestResult, PreparedDecideToolRequest, PreparedOverrideDeniedToolRequest,
    RecordedUserOverride, ToolApprovalDecider, ToolApprovalDecision, ToolApprovalPosture,
    ToolApprovalResolution, ToolApprovalResolutionReconstitutionError,
    ToolApprovalResolutionReconstitutionInput, ToolArgumentsError, ToolArgumentsFailure,
    ToolArgumentsKind, ToolCallProposal, ToolDecisionRationale, ToolDecisionRationaleError,
    ToolDecisionSource, ToolDenialReason, ToolDenialReasonError, ToolDenialReasonFailure,
    ToolEffectClass, ToolName, ToolNameError, ToolNameFailure, ToolPermissionDefault, ToolRequest,
    ToolRequestOrdinal, ToolRequestReconstitutionInput, ToolRequestResolution, ToolResultContent,
    ToolResultText, ToolResultTextError, ToolResultTextFailure, ToolUsingAssistantResponse,
    ToolUsingAssistantResponseError,
};
pub use tool_attempt::{
    ApprovedToolRequest, ApprovedToolRequestError, AuthorizedToolAttempt,
    CorrelatedToolAttemptObservation, CurrentToolAttempt, CurrentToolAttemptState,
    EndedToolAttempt, IssuedExecutorFence, ReconstitutedToolAttempt, ToolAttemptCrashOutcome,
    ToolAttemptDispatchCorrelation, ToolAttemptDispatchCorrelationReconstitutionInput,
    ToolAttemptDisposition, ToolAttemptEnd, ToolAttemptObservation, ToolAttemptReconstitutionError,
    ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState, ToolAttemptTransitionError,
    ToolAttemptTransitionFailure, ToolDispatchAuthority, ToolDispatchGeneration,
    ToolExecutionError, ToolExecutionErrorDetail, ToolExecutionErrorDetailError,
    ToolExecutionErrorDetailFailure, ToolExecutionErrorKind,
};
pub use tool_execution::{
    AwaitingToolApproval, AwaitingToolRecovery, DelegateToolApprovalTransitionError,
    DelegateToolApprovalTransitionFailure, PreparedDelegateToolApproval, PreparedToolAttempt,
    PreparedToolBatchDecision, PreparedToolResultProjection, ToolBatch, ToolBatchDecisionError,
    ToolBatchDecisionFailure, ToolBatchExecutionError, ToolBatchExecutionFailure, ToolBatchPhase,
    ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionError,
    ToolBatchReconstitutionFailure, ToolBatchReconstitutionInput, ToolResultProjectionError,
    ToolResultProjectionFailure,
};
pub use turn_attempt::{
    AppliedInterruptState, AttemptEnd, CancellationStopDisposition, CurrentTurnAttempt,
    CurrentTurnAttemptState, EndedTurnAttempt, FatalMismatchStopCauses,
    FatalMismatchStopDisposition, ProviderTargetMismatchFailureKind,
    ProviderTargetMismatchFailureRef, TurnAttemptStopCauseUnionError, TurnAttemptStopCauses,
    UnstoppedAttemptDisposition,
};
pub use turn_eligibility::{
    AcceptedInputEligibilityError, AcceptedInputEligibilityFailure,
    AcceptedInputSchedulingProjection, AcceptedInputSchedulingReconstitutionError,
    AcceptedInputSchedulingReconstitutionFailure, AcceptedInputSchedulingReconstitutionInput,
    AcceptedInputTurnActivationIdentities, AcceptedInputTurnFailureError,
    AcceptedInputTurnFailureFailure, AcceptedInputTurnFailureIdentities,
    AcceptedInputTurnSchedulingProjection, AcceptedInputTurnSchedulingRecord,
    AcceptedInputTurnSchedulingRecordState, AcceptedInputTurnSchedulingStatus,
    ActivatedAcceptedInputTurn, ActivatedDelegatedTurn, ActivatedTurn,
    ActiveTurnSchedulingReconstitutionInput, AutomaticReconciliationAuthority,
    CancelledTurnExecutionReconstitutionInput, ConsumedSteeringInput,
    ConsumedSteeringReconstitutionInput, ContinuationRoundReconstitutionInput,
    DelegatedModelCallRecoveryReconstitutionInput, DelegatedTurnActivationInput,
    DelegatedTurnSchedulingFact, DelegatedTurnSchedulingState, DelegatedWakeTurnActivationInput,
    FailedAcceptedInputTurn, FailedTurnExecutionReconstitutionInput, PendingSteeringInput,
    PreparedAcceptedInputTurnActivation, PreparedAcceptedInputTurnFailure,
    PreparedDelegatedTurnActivation, PreparedTurnActivation,
    SessionAcceptanceTailEntryReconstitutionInput, SessionAcceptanceTailReconstitutionInput,
    SteeringContinuationRoundReconstitutionInput, TerminalAttemptEndReconstitutionInput,
};
pub use turn_lifecycle::{
    AcceptedInputStartingLineage, AcceptedInputTurnStart, ActiveTurnPhase,
    AppliedStopForReconciliationProof, IssuedOperationRef, NonEmptyIssuedOperationRefs,
    NonEmptyIssuedOperationRefsError, ReconciliationMarker, ReconciliationReason, TurnDisposition,
    TurnTerminalCause,
};
pub use user_content::{
    AttachmentBlobFact, AttachmentDisplayFilename, AttachmentDisplayFilenameError,
    AttachmentDisplayFilenameFailure, AttachmentKind, DeclaredMediaType, DeclaredMediaTypeError,
    DeclaredMediaTypeFailure, NonEmptyUnicodeText, NonEmptyUnicodeTextError,
    NonEmptyUnicodeTextFailure, UserContent, UserContentError, UserContentFailure, UserContentPart,
};
pub use workspace::{WorkspaceOrigin, WorkspaceRecord, WorkspaceRootPath, WorkspaceRootPathError};
pub use workspace_instruction::{
    EmptyTurnInstructionManifestEvidence, InstructionBundleId, InstructionBundleKind,
    InstructionBundleRegistration, InstructionBundleRegistrationInput, InstructionDigest,
    InstructionDiscoveryId, InstructionDiscoveryRootKind, InstructionPath, InstructionPathError,
    InstructionSkillMetadata, InstructionSkillMetadataError, InstructionSkillMetadataInput,
    InstructionSourcePath, InstructionSourcePathInterner, InstructionSourcePathPrefix,
    TurnInstructionManifest, TurnInstructionManifestId,
};

macro_rules! define_identity {
    ($(#[$documentation:meta])* $name:ident) => {
        $(#[$documentation])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(uuid::Uuid);

        impl $name {
            /// Creates this domain identity from its UUID value.
            pub const fn from_uuid(value: uuid::Uuid) -> Self {
                Self(value)
            }

            /// Borrows the UUID value.
            pub const fn as_uuid(&self) -> &uuid::Uuid {
                &self.0
            }

            /// Returns the UUID value.
            pub const fn into_uuid(self) -> uuid::Uuid {
                self.0
            }
        }
    };
}

pub(crate) use define_identity;

define_identity!(
    /// Identifies one user-global, durably handled command submission.
    ///
    /// This identity does not prove that the command was applied.
    DurableCommandId
);

define_identity!(
    /// Identifies one immutable blob-to-blob derivation fact.
    BlobDerivationId
);

define_identity!(
    /// Identifies one durable, independently browsable conversation.
    SessionId
);

define_identity!(
    /// Identifies one immutable message within a delegated-session relation.
    DelegationMessageId
);

define_identity!(
    /// Identifies one immutable externally sourced conversation record.
    ImportedConversationId
);

define_identity!(
    /// Identifies one entry in an imported conversation record.
    ImportedTranscriptEntryId
);

define_identity!(
    /// Identifies one user submission durably accepted with a delivery treatment.
    AcceptedInputId
);

define_identity!(
    /// Identifies one logical request for a conversational outcome.
    TurnId
);

define_identity!(
    /// Identifies one physical orchestration tenure for a turn.
    TurnAttemptId
);

define_identity!(
    /// Identifies one hub authorization to attempt a provider interaction.
    ModelCallId
);

define_identity!(
    /// Identifies one trusted observation of a provider's reported model.
    ProviderTargetEvidenceId
);

define_identity!(
    /// Identifies one logical request for a normalized tool operation.
    ToolRequestId
);

define_identity!(
    /// Identifies one physical effort to execute a tool request.
    ToolAttemptId
);

define_identity!(
    /// Identifies one logical runner enrollment.
    RunnerEnrollmentId
);

define_identity!(
    /// Identifies one enrollment-issued logical runner.
    RunnerId
);

define_identity!(
    /// Identifies one daemon-owned runner authentication reference.
    RunnerAuthenticationId
);

define_identity!(
    /// Identifies one runner-dispatch lease across fenced generations.
    RunnerLeaseId
);

define_identity!(
    /// Identifies one stable runner-owned workspace manifest across lifecycle changes.
    WorkspaceManifestId
);

define_identity!(
    /// Identifies one durable execution of a registered program.
    ProgramRunId
);

define_identity!(
    /// Identifies one immutable review-target snapshot.
    ReviewTargetId
);

define_identity!(
    /// Identifies one review workflow execution.
    ReviewRunId
);

define_identity!(
    /// Identifies one session-backed pass inside a review run.
    ReviewPassId
);

define_identity!(
    /// Identifies one structured review finding.
    ReviewFindingId
);

define_identity!(
    /// Identifies one durable external-object reservation.
    ReviewExternalLinkId
);

define_identity!(
    /// Identifies one durable repository-watch event.
    RepoWatchEventId
);

define_identity!(
    /// Identifies one durable repository-watch dispatch audit record.
    RepoWatchDispatchId
);

define_identity!(
    /// Identifies one durable operator-commissioned dispatch audit record.
    CommissionedDispatchId
);

define_identity!(
    /// Identifies one durable workspace an authority grant may be scoped to.
    WorkspaceId
);

define_identity!(
    /// Identifies one durable operator-minted Git push destination.
    GitRemoteMintId
);

define_identity!(
    /// Identifies one durable withdrawal of a minted Git push destination.
    GitRemoteWithdrawalId
);

#[cfg(test)]
pub(crate) mod test_support {
    //! Behavior-irrelevant test plumbing shared across unit-test modules.
    //!
    //! Every unit test needs deterministic identity values, and each identity
    //! is a distinct UUID-backed newtype built by [`define_identity`]. These
    //! constructors keep the `from_uuid(Uuid::from_u128(..))` pattern in one
    //! place instead of repeating it in each module's test helpers. Snapshot
    //! tables for the expect tests described in `docs/agents/testing-style.md` come
    //! from the `signalbox-expect-table` dev-dependency.

    macro_rules! identity_constructors {
        ($($constructor:ident -> $identity:ty),+ $(,)?) => {
            $(
                pub(crate) fn $constructor(value: u128) -> $identity {
                    <$identity>::from_uuid(uuid::Uuid::from_u128(value))
                }
            )+
        };
    }

    identity_constructors! {
        command_id -> crate::DurableCommandId,
        turn_id -> crate::TurnId,
        turn_attempt_id -> crate::TurnAttemptId,
        session_id -> crate::SessionId,
        imported_conversation_id -> crate::ImportedConversationId,
        imported_transcript_entry_id -> crate::ImportedTranscriptEntryId,
        accepted_input_id -> crate::AcceptedInputId,
        model_call_id -> crate::ModelCallId,
        provider_target_evidence_id -> crate::ProviderTargetEvidenceId,
        provider_model_identity -> crate::ProviderModelIdentity,
        context_frontier_id -> crate::ContextFrontierId,
        semantic_transcript_entry_id -> crate::SemanticTranscriptEntryId,
        tool_request_id -> crate::ToolRequestId,
        delegation_message_id -> crate::DelegationMessageId,
        tool_attempt_id -> crate::ToolAttemptId,
        runner_enrollment_id -> crate::RunnerEnrollmentId,
        runner_id -> crate::RunnerId,
        runner_authentication_id -> crate::RunnerAuthenticationId,
        runner_lease_id -> crate::RunnerLeaseId,
        direct -> crate::DirectModelSelection,
        alias -> crate::ModelAlias,
    }

    // An opaque source transcript boundary for ancestry-bearing session
    // fixtures; the producing slice for real boundaries does not exist yet.
    pub(crate) use crate::session::test_frontier as transcript_frontier;
}

#[cfg(test)]
mod tests {
    use super::{
        AcceptedInputId, CommissionedDispatchId, ContextFrontierId, DurableCommandId,
        GitRemoteMintId, GitRemoteWithdrawalId, ImportedConversationId, ImportedTranscriptEntryId,
        ModelCallId, ProviderTargetEvidenceId, RepoWatchDispatchId, RepoWatchEventId,
        RunnerAuthenticationId, RunnerEnrollmentId, RunnerId, RunnerLeaseId,
        SemanticTranscriptEntryId, SessionId, ToolAttemptId, ToolRequestId, TurnAttemptId, TurnId,
        WorkspaceId, WorkspaceManifestId,
    };
    use uuid::Uuid;

    macro_rules! assert_uuid_contract {
        ($identity:ty) => {{
            let first_uuid = Uuid::from_u128(1);
            let second_uuid = Uuid::from_u128(2);
            let first_id = <$identity>::from_uuid(first_uuid);
            let equal_id = <$identity>::from_uuid(first_uuid);
            let different_id = <$identity>::from_uuid(second_uuid);

            assert_eq!(first_id, equal_id);
            assert_ne!(first_id, different_id);
            assert_eq!(first_id.as_uuid(), &first_uuid);
            assert_eq!(first_id.into_uuid(), first_uuid);
        }};
    }

    #[test]
    fn identity_uuid_representation_contract() {
        assert_uuid_contract!(DurableCommandId);
        assert_uuid_contract!(SessionId);
        assert_uuid_contract!(ImportedConversationId);
        assert_uuid_contract!(ImportedTranscriptEntryId);
        assert_uuid_contract!(AcceptedInputId);
        assert_uuid_contract!(TurnId);
        assert_uuid_contract!(TurnAttemptId);
        assert_uuid_contract!(ModelCallId);
        assert_uuid_contract!(ProviderTargetEvidenceId);
        assert_uuid_contract!(ContextFrontierId);
        assert_uuid_contract!(SemanticTranscriptEntryId);
        assert_uuid_contract!(ToolRequestId);
        assert_uuid_contract!(ToolAttemptId);
        assert_uuid_contract!(RunnerEnrollmentId);
        assert_uuid_contract!(RunnerId);
        assert_uuid_contract!(RunnerAuthenticationId);
        assert_uuid_contract!(RunnerLeaseId);
        assert_uuid_contract!(WorkspaceManifestId);
        assert_uuid_contract!(RepoWatchEventId);
        assert_uuid_contract!(RepoWatchDispatchId);
        assert_uuid_contract!(CommissionedDispatchId);
        assert_uuid_contract!(WorkspaceId);
        assert_uuid_contract!(GitRemoteMintId);
        assert_uuid_contract!(GitRemoteWithdrawalId);
    }
}
