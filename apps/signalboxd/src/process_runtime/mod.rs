//! Local process-protocol serving and durable outbox fan-out.

use std::{
    error::Error,
    fmt,
    future::Future,
    io::{self, SeekFrom},
    num::NonZeroU64,
    sync::Arc,
    time::Duration,
};

use sha2::{Digest, Sha256};
use signalbox_application::{
    ClassifyOperatorFailure, CommissionDispatchRequest,
    CommissionedDispatchFence as ApplicationCommissionedDispatchFence, ConversationListCursor,
    ConversationListItem, ConversationListQuery, ConversationOriginFilter, ConversationPageReader,
    CreateSessionError, CreateSessionFromImportedFrontierOutcome,
    CreateSessionFromImportedFrontierRequest, CreateSessionFromImportedFrontierService,
    CreateSessionOutcome, CreateSessionRequest, CreateSessionService, DecideToolRequestService,
    EligibilityNudge, ImportConversationError, ImportConversationOutcome,
    ImportConversationService, ImportedConversationConverter, InProcessEligibilityNudge,
    InProcessToolDispatchGate, ListConversationsService, ListSessionMetadataService,
    LoadSessionMetadataService, OperatorFailureClass, OverrideDeniedToolRequestService,
    PromptMemberStatement, ReplaceSessionDefaultsOutcome, ReplaceSessionDefaultsRequest,
    ReplaceSessionDefaultsService, ReplaceSessionMetadataOutcome, ReplaceSessionMetadataRequest,
    ReplaceSessionMetadataService, ReviewPassCompletionStatus, ReviewWorkflowCommand,
    ReviewWorkflowCommandOutcome, ReviewWorkflowCommandResult, ReviewWorkflowCommandService,
    ReviewWorkflowOperation, ReviewWorkflowOperationKind, SessionMetadataListItem,
    SessionMetadataListQuery, SessionTimelineEventKind, SubmitInputOutcome, SubmitInputRequest,
    SubmitInputService, SubmitInputTransaction, UpdateSessionPlacementOutcome,
    UpdateSessionPlacementRequest, UpdateSessionPlacementService,
    UuidV7CommissionedDispatchIdGenerator, UuidV7CreateSessionFromImportedFrontierIdGenerator,
    UuidV7ImportedConversationIdGenerator, UuidV7SessionIdGenerator, UuidV7SubmitInputIdGenerator,
    UuidV7ToolLoopIdGenerator, render_model_user_content,
};
use signalbox_blob_store::ExpectedBlob;
use signalbox_conversation_import_claude_code::{
    ClaudeCodeJsonlConversionError, ClaudeCodeJsonlConversionFailure, ClaudeCodeJsonlConverter,
};
use signalbox_conversation_import_codex::{
    CodexRolloutJsonlConversionError, CodexRolloutJsonlConversionFailure,
    CodexRolloutJsonlConverter,
};
use signalbox_domain::{
    AcceptedInputId, Actor, BranchName, CancelledModelCallTurnIdentities, CommandPrincipal,
    CommitSha, ContextCompactionId, ContextCompactionTokenUsage, ContextFrontierId,
    DangerousToolAutoApproval, DecideToolRequest, DecideToolRequestRejectedResult,
    DecideToolRequestResult, DelegationMessageDirection as DomainDelegationMessageDirection,
    DelegationOutcomeKind as DomainDelegationOutcomeKind,
    DelegationOutcomeReason as DomainDelegationOutcomeReason,
    DelegationProvenance as DomainDelegationProvenance, DelegationWait,
    DelegationWaitMode as DomainDelegationWaitMode, DeliveryRequest, DescendantTerminationScope,
    DirectModelSelection, DurableCommandId, FastMode as DomainFastMode,
    FastModeOverlay as DomainFastModeOverlay, FinishCondition, FinishConditionStatement,
    FrozenModelSelection, Goal, GoalBlockProvenance, GoalBlockedReasonKind,
    GoalCommandRejection as DomainGoalCommandRejection, GoalCommandResult, GoalEvent,
    GoalEventKind, GoalGuidance, GoalState, GoalStatement, GoalUserAction, GoalUserCommand,
    ImportedConversation, ImportedConversationFormat, ImportedConversationId,
    ImportedSessionRelationship as DomainImportedSessionRelationship, ImportedSourceAttestation,
    ImportedSpeaker as DomainImportedSpeaker, ImportedTranscriptContent,
    ImportedTranscriptEntryInput, ImportedTranscriptPosition, ModelAlias, ModelCallId,
    ModelChangeAdjustment as DomainModelChangeAdjustment, ModelSelectionOverride,
    ModelSelectionRequest, ModelSettingSource as DomainModelSettingSource,
    ModelSettingsOverlay as DomainModelSettingsOverlay,
    ModelSettingsPrecedence as DomainModelSettingsPrecedence, OverrideDeniedToolRequest,
    OverrideDeniedToolRequestRejectedResult, OverrideDeniedToolRequestResult,
    ParentTerminationCommandSource, ParentTerminationKind, PerInputConfigurationChoices,
    PullRequestNumber, ReasoningLevel as DomainReasoningLevel, ReconstitutedSessionCreation,
    ReplaceSessionDefaults as DomainReplaceSessionDefaults, ReplaceSessionDefaultsRejectedResult,
    ReplaceSessionDefaultsResult, ReplaceSessionMetadataRejectedResult,
    ReplaceSessionMetadataResult, RepositorySlug, ReviewChangeRequestNumber, ReviewConfidence,
    ReviewEventOrdinal, ReviewExternalLink, ReviewExternalLinkAssociation,
    ReviewExternalLinkAttachment, ReviewExternalLinkAttachmentResult, ReviewExternalLinkId,
    ReviewExternalObjectKind, ReviewFinding, ReviewFindingConfidenceAxes, ReviewFindingContent,
    ReviewFindingDiffSide, ReviewFindingEvent, ReviewFindingEventKind, ReviewFindingEventResult,
    ReviewFindingEventResultKind, ReviewFindingId, ReviewFindingLocation,
    ReviewFindingPendingExternalLinkRef, ReviewFindingProposal, ReviewFindingRef,
    ReviewFindingSeverity, ReviewKey, ReviewLineRange, ReviewPass, ReviewPassAcceptedInputEvidence,
    ReviewPassEvidence, ReviewPassId, ReviewPassKind, ReviewPassRef, ReviewPassResult,
    ReviewPassState, ReviewPassTurnEvidence, ReviewPassTurnOutcome, ReviewPolicy,
    ReviewProducedFindings, ReviewReferencedFindingEvidence, ReviewRun, ReviewRunId, ReviewRunRef,
    ReviewRunState, ReviewTarget, ReviewTargetId, ReviewTargetSubject, ReviewText,
    ReviewWorkflowKind, RunnerSandboxProfile as DomainRunnerSandboxProfile, RunnerSelector,
    SemanticTranscriptEntryId, ServiceTier as DomainServiceTier, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionFailureCause as DomainSessionFailureCause,
    SessionId, SessionLifecycleApplication, SessionLifecycleCommand,
    SessionLifecycleCommandRejection as DomainLifecycleRejection, SessionLifecycleCommandResult,
    SessionLifecycleOperation, SessionMetadataContent, SessionMetadataLastWriter,
    SessionMetadataSnapshot, SessionModelSettingsChanged as DomainSessionModelSettingsChanged,
    SessionOwnership as DomainSessionOwnership, SessionPlacement as DomainSessionPlacement,
    SessionPlacementPath, SessionPlacementVersion, SessionRetryableCause, SessionStructuralCause,
    SessionTemplateName, SessionTemplateProvenance, SettingOverlay as DomainSettingOverlay,
    StartGate as DomainStartGate, StopStickiness, SubmitInput, SubmitInputAppliedResult,
    SubmitInputRejectedResult, SubmitInputResult, ToolApprovalDecision, ToolDenialReason,
    ToolRequestId, TurnId, TurnModelSettingsResolved as DomainTurnModelSettingsResolved,
    UnsupportedModelSetting, UpdateSessionPlacementRejectionKind, UpdateSessionPlacementResult,
    UserContent, ValidatedModelSettings,
};
use signalbox_model_provider_runtime::{
    ContextCompactionModel, ContextCompactionModelError, ContextCompactionModelRequest,
    ProviderTextDelta, ProviderTextDeltaSink,
};
use signalbox_persistence::{
    blob::BlobCatalogRepository,
    commissioned_dispatch::{
        CommissionDispatchOutcome, CommissionedDispatchRepositoryError,
        PostgresCommissionedDispatchStore,
    },
    context_compaction::{
        AppliedContextCompaction, AutomaticContextCompactionPreviewMember,
        ContextCompactionCommandLookup, ContextCompactionRepository,
        ContextCompactionRepositoryError, FailedContextCompactionDisposition,
        PrepareContextCompactionOutcome, PrepareContextCompactionRequest,
        PreparedContextCompaction,
    },
    conversation_import::{
        ImportedConversationRepository, ImportedConversationRepositoryError,
        ImportedRawBlobStorageError,
    },
    conversation_listing::{ConversationListingRepository, ConversationListingRepositoryError},
    create_session::{CreateSessionRepository, CreateSessionRepositoryError},
    create_session_from_imported_frontier::{
        ImportedSessionRepository, ImportedSessionRepositoryError,
    },
    goal::{GoalCommandHandlingOutcome, GoalRepository, GoalRepositoryError},
    goal_turn::GoalTurnCandidates,
    lifecycle_metrics::LifecycleNonTerminalState,
    model_execution::{ModelCallRepositoryError, PostgresModelCallRepository},
    operator_status::{
        ProcessOperatorStatusConvergenceSeal, ProcessOperatorStatusConvergenceVerdict,
        ProcessOperatorStatusError, ProcessOperatorStatusHeldSlotBlocker,
        ProcessOperatorStatusHeldSlotOrigin, ProcessOperatorStatusItem,
        ProcessOperatorStatusMergeableState, ProcessOperatorStatusRepository,
        ProcessOperatorStatusReviewDecision, ProcessOperatorStatusSingletonScope,
    },
    outbox::{
        DispatchedBoundChildAction, DispatchedDelegationOutcome, DispatchedDelegationPolicy,
        DispatchedDelegationProvenance, DispatchedDelegationReason, DispatchedDelegationUpdate,
        DispatchedDelegationWaitMode, DispatchedModelCallDisposition, DispatchedModelCallState,
        DispatchedOutboxEvent, DispatchedOutboxEventKind, DispatchedReconciliationOperation,
        DispatchedRunnerState, DispatchedToolBatchState, DispatchedTurnTerminalDisposition,
        OutboxDeliveryDecision, OutboxDispatchError, OutboxDispatchOutcome, OutboxDispatcher,
    },
    process_read::{
        ProcessCurrentModelCallState, ProcessFailedModelCallDisposition,
        ProcessImportedContentKind, ProcessImportedSourceSpeaker,
        ProcessModelCallRecoveryPrecondition, ProcessModelCallUsageProvenance,
        ProcessModelSelection, ProcessProviderModelCallFailureCause, ProcessReadError,
        ProcessReadRepository, ProcessReconciliationOperation, ProcessRunnerConnectionHealth,
        ProcessRunnerProjection, ProcessRunnerProjectionState, ProcessSessionDefaultsRead,
        ProcessTranscriptEntry, ProcessTranscriptItem, ProcessTranscriptModelCallUsage,
        ProcessTranscriptTurn, ProcessTurnState,
    },
    replace_session_defaults::{
        ReplaceSessionDefaultsHandlingOutcome, ReplaceSessionDefaultsRejectionOnlyOutcome,
        ReplaceSessionDefaultsRepository, ReplaceSessionDefaultsRepositoryError,
    },
    review_workflow::{ReviewTurnLifecycleState, ReviewWorkflowStore, ReviewWorkflowStoreError},
    session_delegation::{
        DelegationOperationRejection, DelegationRequestExecutionState, ProcessDelegationOutcome,
        ProcessDelegationRequestRejection,
    },
    session_lifecycle::SessionLifecycleRepository,
    session_lifecycle_command::{
        SessionLifecycleCommandHandlingOutcome, SessionLifecycleCommandRepository,
        SessionLifecycleCommandRepositoryError,
    },
    session_metadata::{SessionMetadataRepository, SessionMetadataRepositoryError},
    session_placement::{SessionPlacementRepository, SessionPlacementRepositoryError},
    submit_input::{SubmitInputHandlingOutcome, SubmitInputRepository, SubmitInputRepositoryError},
    tool_loop::{PostgresToolLoopRepository, ToolLoopRepositoryError},
};
use signalbox_process_protocol::{
    BillingRateVersion, BlobChunk, BoundChildAction as WireBoundChildAction, BulkIngestKind,
    CanonicalBlobDigest, CanonicalDollarAmount, CanonicalU64, CanonicalUuid, ClientRequest,
    CommissionedSessionFence as WireCommissionedSessionFence,
    ConversationCursor as WireConversationCursor, ConversationImportFormat,
    ConversationImportRejectionClass, ConversationOrigin as WireConversationOrigin,
    ConversationOriginFilter as WireConversationOriginFilter,
    ConversationSummary as WireConversationSummary, CurrentModelCall, CurrentModelCallState,
    DelegationOutcome as WireDelegationOutcome, DelegationPolicy as WireDelegationPolicy,
    DelegationProvenance as WireDelegationProvenance, DelegationReason as WireDelegationReason,
    DelegationToolRequestState as WireDelegationToolRequestState,
    DelegationWaitMode as WireDelegationWaitMode,
    DescendantTerminationScope as WireDescendantTerminationScope,
    EffectiveModelSettings as WireEffectiveModelSettings, ErrorCode, ErrorDetail,
    FailedModelCallCause, FailedModelCallDisposition, FailedTerminalModelCall,
    FastMode as WireFastMode, FastModeOverlay as WireFastModeOverlay,
    FinishCondition as WireFinishCondition, FrameDecodeErrorKind, FrameEncodeError,
    GoalBlockedProvenance as WireGoalBlockedProvenance, GoalBlockedReason as WireGoalBlockedReason,
    GoalCommandRejection as WireGoalCommandRejection, GoalHistoryEvent, GoalLifecycleState,
    ImportedContentKind, ImportedConversationSourceFormat as WireImportedConversationSourceFormat,
    ImportedSessionRelationship as WireImportedSessionRelationship, ImportedSourceSpeaker,
    ImportedSpeaker, ImportedTextPreview, InputContent, InputDelivery, LifecycleActorClass,
    MAX_BLOB_READ_BYTES, MAX_FRAME_BYTES, MetadataActor, MetadataLastWriter, ModelCallCostLabel,
    ModelCallDisposition, ModelCallDollarCost, ModelCallState, ModelCallTokenUsage,
    ModelCapabilities as WireModelCapabilities, ModelChangeAdjustment as WireModelChangeAdjustment,
    ModelSelection as WireModelSelection, ModelSettingSource as WireModelSettingSource,
    ModelSettingsOverlay as WireModelSettingsOverlay,
    ModelSettingsPrecedence as WireModelSettingsPrecedence,
    ModelSettingsSnapshot as WireModelSettingsSnapshot,
    OperatorStatusConvergenceSeal as WireOperatorStatusConvergenceSeal,
    OperatorStatusConvergenceVerdict as WireOperatorStatusConvergenceVerdict,
    OperatorStatusEndMessage, OperatorStatusHeldSlotBlocker as WireOperatorStatusHeldSlotBlocker,
    OperatorStatusHeldSlotMessage, OperatorStatusHeldSlotOrigin,
    OperatorStatusLifecycleDeadlineViolationMessage, OperatorStatusLifecycleState,
    OperatorStatusLifecycleWeekMessage,
    OperatorStatusMergeableState as WireOperatorStatusMergeableState, OperatorStatusMessage,
    OperatorStatusPendingStaleReviewClearanceMessage, OperatorStatusPullRequestConvergenceMessage,
    OperatorStatusQueuedObligationMessage,
    OperatorStatusReviewDecision as WireOperatorStatusReviewDecision,
    OperatorStatusSingletonScope as WireOperatorStatusSingletonScope, PositiveCanonicalU64,
    ProtocolVersion, ReasoningLevel as WireReasoningLevel, RejectionDetail, RequestId,
    ReviewDiffSide as WireReviewDiffSide, ReviewExternalObjectKind as WireReviewExternalObjectKind,
    ReviewFindingEvent as WireReviewFindingEvent, ReviewFindingInput, ReviewFindingSnapshot,
    ReviewFindingStatus as WireReviewFindingStatus, ReviewPassLifecycle, ReviewPassSnapshot,
    ReviewPassTerminalOutcome, ReviewRunLifecycle, ReviewRunSnapshot,
    ReviewSeverity as WireReviewSeverity, ReviewTargetSnapshot,
    ReviewTargetSubject as WireReviewTargetSubject, ReviewWorkflow as WireReviewWorkflow,
    RunnerCapabilityClass as WireRunnerCapabilityClass,
    RunnerConnectionHealth as WireRunnerConnectionHealth,
    RunnerCredentialProfileName as WireRunnerCredentialProfileName,
    RunnerPlacementRevision as WireRunnerPlacementRevision,
    RunnerProjection as WireRunnerProjection,
    RunnerProjectionSelector as WireRunnerProjectionSelector,
    RunnerProjectionState as WireRunnerProjectionState,
    RunnerRepositoryKey as WireRunnerRepositoryKey,
    RunnerSandboxProfile as WireRunnerSandboxProfile,
    RunnerStateTransitionState as WireRunnerStateTransitionState,
    RunnerWorkingDirectory as WireRunnerWorkingDirectory, ServerFrame, ServerMessage,
    ServiceTier as WireServiceTier, SessionClosureOutcome, SessionEvent,
    SessionFailureCause as WireSessionFailureCause,
    SessionLifecycleCommandRejection as WireLifecycleRejection, SessionLifecycleEffect,
    SessionLifecycleMembers, SessionMetadata as WireSessionMetadata,
    SessionOwnership as WireSessionOwnership, SessionPlacement as WireSessionPlacement,
    SettingOverlay as WireSettingOverlay, StartGate as WireStartGate, SystemPromptMember,
    SystemPromptText, ToolApprovalEventDecider as WireToolApprovalEventDecider,
    ToolApprovalEventDecision as WireToolApprovalEventDecision, ToolBatchState, ToolDecision,
    TranscriptEntry, TranscriptTextEntry, TranscriptToolApproval,
    TurnModelSettingsSnapshot as WireTurnModelSettingsSnapshot, TurnState, UsageProvenance,
    UserInputContent, content_fragments, decode_client_line, encode_server_line,
    recover_bounded_client_protocol_version, recover_bounded_client_request_id,
};
use signalbox_tools_sessions::{AwaitSessionPortOutcome, DeliveredChildResult};
use sqlx::PgPool;
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt,
        BufReader, Interest,
    },
    net::{UnixStream, unix::OwnedReadHalf},
    sync::{OwnedSemaphorePermit, Semaphore, broadcast, watch},
    task::{JoinError, JoinSet},
    time::{Instant, sleep, sleep_until},
};

use crate::goal_mode::PostgresGoalPassDisposition;
use crate::telemetry::{ModelMetricDisposition, TelemetryMetrics, TurnMetricOutcome};
use crate::{
    BlobStoreRegistry, FatalRecoveryReporter, HubModelConfiguration, LocalProcessListener,
    LocalSocketError, SessionTemplateConfiguration,
    blob_read_runtime::{
        BLOB_READ_TIMEOUT, BlobReadError, read_blob_chunk, read_blob_entry, read_blob_metadata,
    },
    blob_upload_runtime::{
        BeginBlobUploadOutcome, BlobUploadError, PendingBlobUpload, begin_blob_upload,
    },
    review_orchestration_runtime::{
        ReviewOrchestrationInternalCause, ReviewOrchestrationRuntimeError,
        execute_review_orchestration_request, read_review_orchestration_request,
    },
    session_delegation::{PostgresSessionDelegationPort, PostgresSessionDelegationPortError},
    usage_limits::context_compaction_usage_exceeds_configured_limits,
};

const OUTBOX_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const CONTEXT_COMPACTION_PERSISTENCE_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const DELEGATION_DELIVERY_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const PROCESS_UPDATE_CAPACITY: usize = 64;
const MAX_ACTIVE_CONNECTIONS: usize = 128;
const MAX_BUFFERED_INBOUND_FRAMES: usize = 8;
const MAX_CONCURRENT_IMPORTS: usize = 1;
const RESERVED_ACTIVE_IMPORT_INBOUND_FRAMES: usize = MAX_CONCURRENT_IMPORTS;
const GENERAL_BUFFERED_INBOUND_FRAMES: usize =
    MAX_BUFFERED_INBOUND_FRAMES - RESERVED_ACTIVE_IMPORT_INBOUND_FRAMES;
const MAX_IMPORT_ADMISSION_WAITERS: usize = GENERAL_BUFFERED_INBOUND_FRAMES;
const MAX_CONCURRENT_REVIEW_COMMANDS: usize = 1;
/// Hard safety ceiling limiting aggregate range-buffer and spool memory.
const MAX_CONCURRENT_BLOB_READS: usize = crate::blob_storage_runtime::MAX_CONCURRENT_BLOB_READS;
const BULK_INGEST_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const BULK_INGEST_SESSION_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const INBOUND_READ_AHEAD_BYTES: usize = 8 * 1024;
const RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS: u32 = 2;

#[derive(Debug)]
struct UnavailableContextCompactionModel;

impl ContextCompactionModel for UnavailableContextCompactionModel {
    fn execute<'a>(
        &'a self,
        _request: ContextCompactionModelRequest,
    ) -> std::pin::Pin<
        Box<
            dyn Future<
                    Output = Result<
                        signalbox_model_provider_runtime::ContextCompactionModelResult,
                        ContextCompactionModelError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async { Err(ContextCompactionModelError::UnconfiguredTarget) })
    }
}

#[derive(Clone, Debug)]
struct ConnectionServices {
    recovery_reporter: Option<FatalRecoveryReporter>,
    pool: PgPool,
    eligibility_nudge: InProcessEligibilityNudge,
    tool_dispatch_gate: InProcessToolDispatchGate,
    goal_resumption: Option<PostgresGoalPassDisposition>,
    model_configuration: Arc<HubModelConfiguration>,
    context_compaction_model: Arc<dyn ContextCompactionModel>,
    template_configuration: Arc<SessionTemplateConfiguration>,
    fanouts: ProcessFanouts,
    inbound_frame_budgets: InboundFrameBudgets,
    import_budget: Arc<Semaphore>,
    import_waiter_budget: Arc<Semaphore>,
    blob_read_budget: Arc<Semaphore>,
    review_command_budget: Arc<Semaphore>,
    snapshot_reader_budget: Arc<Semaphore>,
    blob_store_registry: Option<Arc<BlobStoreRegistry>>,
    imported_conversations: ImportedConversationRepository,
}

#[derive(Clone, Debug)]
struct InboundFrameBudgets {
    general: Arc<Semaphore>,
    active_import: Arc<Semaphore>,
}

#[derive(Clone, Copy, Debug)]
enum ConversationImportState {
    Inactive,
    Active,
}

impl InboundFrameBudgets {
    fn new() -> Self {
        Self {
            general: Arc::new(Semaphore::new(GENERAL_BUFFERED_INBOUND_FRAMES)),
            active_import: Arc::new(Semaphore::new(RESERVED_ACTIVE_IMPORT_INBOUND_FRAMES)),
        }
    }

    fn for_connection(&self, import_state: ConversationImportState) -> Arc<Semaphore> {
        match import_state {
            ConversationImportState::Inactive => Arc::clone(&self.general),
            ConversationImportState::Active => Arc::clone(&self.active_import),
        }
    }
}

mod runtime;

use runtime::{ProcessFanouts, nudge_delegation_issuer};
pub use runtime::{
    ProcessMonitor, ProcessMonitorReceiveError, ProcessMonitorSubscription, ProcessMonitorUpdate,
    ProcessProviderTextDeltaSink, ProcessRuntime,
};
#[cfg(test)]
use runtime::{nudge_delegation_wake, observe_outbox_metrics_once};
mod connection;
pub use connection::shared_snapshot_reader_budget;
use connection::*;
mod request;
use request::handle_request;
mod delegation;
use delegation::*;
mod review;
use review::*;
mod ingest;
use ingest::*;
mod compaction;
use compaction::*;
pub(crate) use compaction::{AutomaticContextCompactionError, compact_automatically};
mod sessions;
use sessions::*;
mod turns;
pub(crate) use turns::wire_user_content;
use turns::*;
mod transcript;
use transcript::*;
mod protocol;
pub use protocol::ProcessRuntimeError;
use protocol::*;
#[cfg(test)]
include!("tests.rs");
