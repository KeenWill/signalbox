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
async fn handle_read_transcript<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let selected_session = SessionId::from_uuid(session_id.into_uuid());
    let spool_result = spool_transcript(
        ProcessReadRepository::new(pool.clone()),
        selected_session,
        version,
        request_id,
        model_configuration,
    )
    .await;
    drop(snapshot_permit);
    let spool = match spool_result {
        Ok(Some(spool)) => spool,
        Ok(None) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::NotFound),
            )
            .await;
        }
        Err(TranscriptSpoolError::Read(error)) => {
            return write_process_read_error(writer, version, request_id, Some(session_id), error)
                .await;
        }
        Err(TranscriptSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    write_spooled_transcript(writer, spool).await.map(|_| ())
}

#[expect(
    clippy::too_many_arguments,
    reason = "the versioned follow stream keeps each protocol and runtime boundary explicit"
)]
async fn handle_follow_session<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
    fanouts: &ProcessFanouts,
    mut shutdown: watch::Receiver<bool>,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let selected_session = SessionId::from_uuid(session_id.into_uuid());
    let mut subscription = fanouts.streaming.subscribe();
    let snapshot_result = run_until_shutdown(
        &mut shutdown,
        spool_transcript(
            ProcessReadRepository::new(pool.clone()),
            selected_session,
            version,
            request_id,
            model_configuration,
        ),
    )
    .await;
    drop(snapshot_permit);
    let Some(snapshot_result) = snapshot_result else {
        return Ok(());
    };
    let spool = match snapshot_result {
        Ok(Some(spool)) => spool,
        Ok(None) => {
            return run_until_shutdown(
                &mut shutdown,
                write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::NotFound),
                ),
            )
            .await
            .unwrap_or(Ok(()));
        }
        Err(TranscriptSpoolError::Read(error)) => {
            return run_until_shutdown(
                &mut shutdown,
                write_process_read_error(writer, version, request_id, Some(session_id), error),
            )
            .await
            .unwrap_or(Ok(()));
        }
        Err(TranscriptSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    let mut updates_queued_at_snapshot = subscription.len();
    let Some(snapshot_write) =
        run_until_shutdown(&mut shutdown, write_spooled_transcript(writer, spool)).await
    else {
        return Ok(());
    };
    let mut observed_cursor = snapshot_write?;

    loop {
        let update = tokio::select! {
            () = wait_for_shutdown(&mut shutdown) => return Ok(()),
            update = subscription.recv() => update,
        };
        let update = match update {
            Ok(update) => update,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                return run_until_shutdown(
                    &mut shutdown,
                    write_error(
                        writer,
                        version,
                        request_id,
                        ProtocolError::without_detail(ErrorCode::ResyncRequired),
                    ),
                )
                .await
                .unwrap_or(Ok(()));
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        };
        let queued_at_snapshot = consume_snapshot_queued_update(&mut updates_queued_at_snapshot);
        match update {
            ProcessUpdate::Durable {
                cursor,
                session,
                event,
            } => {
                if cursor <= observed_cursor {
                    continue;
                }
                observed_cursor = cursor;
                if session != selected_session {
                    continue;
                }
                let message = ServerMessage::SessionEvent {
                    cursor: CanonicalU64::new(cursor),
                    session_id,
                    event: event.wire()?,
                };
                let Some(event_write) = run_until_shutdown(
                    &mut shutdown,
                    write_message(writer, version, request_id, message),
                )
                .await
                else {
                    return Ok(());
                };
                event_write?;
            }
            ProcessUpdate::ProviderTextDelta(delta) => {
                if queued_at_snapshot || delta.session() != selected_session {
                    continue;
                }
                for content in content_fragments(delta.text()) {
                    let message = ServerMessage::ProviderTextDelta {
                        session_id,
                        turn_id: wire_uuid(delta.turn().into_uuid()),
                        model_call_id: wire_uuid(delta.call().into_uuid()),
                        part_index: CanonicalU64::new(u64::from(delta.part_index())),
                        content,
                    };
                    let Some(delta_write) = run_until_shutdown(
                        &mut shutdown,
                        write_message(writer, version, request_id, message),
                    )
                    .await
                    else {
                        return Ok(());
                    };
                    delta_write?;
                }
            }
        }
    }
}

fn consume_snapshot_queued_update(remaining: &mut usize) -> bool {
    if *remaining == 0 {
        false
    } else {
        *remaining -= 1;
        true
    }
}

struct TranscriptSpool {
    file: tokio::fs::File,
    cursor: u64,
}

enum TranscriptSpoolError {
    Read(ProcessReadError),
    Spool(SnapshotSpoolError),
}

async fn spool_transcript(
    repository: ProcessReadRepository,
    session: SessionId,
    version: ProtocolVersion,
    request_id: RequestId,
    model_configuration: &HubModelConfiguration,
) -> Result<Option<TranscriptSpool>, TranscriptSpoolError> {
    let reader = repository.open_transcript(session).await;
    let Some(mut reader) = reader.map_err(TranscriptSpoolError::Read)? else {
        return Ok(None);
    };
    let standard_file = tempfile::tempfile()
        .map_err(SnapshotSpoolError::Io)
        .map_err(TranscriptSpoolError::Spool)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    let session_id = wire_uuid(reader.session().into_uuid());
    let cursor = CanonicalU64::new(reader.cursor());
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::TranscriptSnapshotStart {
            session_id,
            cursor,
            runner: reader
                .runner()
                .map(wire_runner_projection)
                .transpose()
                .map_err(TranscriptSpoolError::Spool)?,
        },
    )
    .await
    .map_err(TranscriptSpoolError::Spool)?;
    let mut model_calls_ended = false;
    let mut model_call_count = 0_u64;
    while let Some(item) = reader
        .next_item()
        .await
        .map_err(TranscriptSpoolError::Read)?
    {
        match item {
            ProcessTranscriptItem::Turn(turn) => {
                write_transcript_turn(&mut file, version, request_id, &turn)
                    .await
                    .map_err(SnapshotSpoolError::from_connection)
                    .map_err(TranscriptSpoolError::Spool)?;
            }
            ProcessTranscriptItem::ModelCallUsage(usage) => {
                write_model_call_usage(
                    &mut file,
                    version,
                    request_id,
                    model_call_count,
                    &usage,
                    model_configuration,
                )
                .await
                .map_err(SnapshotSpoolError::from_connection)
                .map_err(TranscriptSpoolError::Spool)?;
                model_call_count = model_call_count
                    .checked_add(1)
                    .ok_or(SnapshotSpoolError::EncodeInvariant)
                    .map_err(TranscriptSpoolError::Spool)?;
            }
            ProcessTranscriptItem::Entry(entry) => {
                if !model_calls_ended {
                    write_model_calls_end(&mut file, version, request_id, model_call_count)
                        .await
                        .map_err(SnapshotSpoolError::from_connection)
                        .map_err(TranscriptSpoolError::Spool)?;
                    model_calls_ended = true;
                }
                write_transcript_entry(&mut file, version, request_id, &entry)
                    .await
                    .map_err(SnapshotSpoolError::from_connection)
                    .map_err(TranscriptSpoolError::Spool)?;
            }
        }
    }
    let summary = reader
        .summary()
        .ok_or(SnapshotSpoolError::EncodeInvariant)
        .map_err(TranscriptSpoolError::Spool)?;
    if !model_calls_ended {
        write_model_calls_end(&mut file, version, request_id, model_call_count)
            .await
            .map_err(SnapshotSpoolError::from_connection)
            .map_err(TranscriptSpoolError::Spool)?;
    }
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::TranscriptSnapshotEnd {
            session_id,
            cursor,
            turn_count: CanonicalU64::new(summary.turn_count()),
            entry_count: CanonicalU64::new(summary.entry_count()),
        },
    )
    .await
    .map_err(TranscriptSpoolError::Spool)?;
    file.flush()
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(TranscriptSpoolError::Spool)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(TranscriptSpoolError::Spool)?;
    Ok(Some(TranscriptSpool {
        file,
        cursor: summary.cursor(),
    }))
}

fn wire_runner_projection(
    projection: &ProcessRunnerProjection,
) -> Result<WireRunnerProjection, SnapshotSpoolError> {
    let selector = match projection.selector() {
        RunnerSelector::Identity(runner) => WireRunnerProjectionSelector::Runner {
            runner_id: wire_uuid(runner.into_uuid()),
        },
        RunnerSelector::CapabilityClass(capability) => {
            WireRunnerProjectionSelector::CapabilityClass {
                name: WireRunnerCapabilityClass::try_new(capability.as_str().to_owned())
                    .map_err(|_| SnapshotSpoolError::EncodeInvariant)?,
            }
        }
    };
    let sandbox_profile = match projection.sandbox() {
        DomainRunnerSandboxProfile::Ambient => WireRunnerSandboxProfile::Ambient,
        DomainRunnerSandboxProfile::WorkspaceRestricted => {
            WireRunnerSandboxProfile::WorkspaceRestricted
        }
    };
    let state = match projection.state() {
        ProcessRunnerProjectionState::Unpinned => WireRunnerProjectionState::Unpinned,
        ProcessRunnerProjectionState::Pinned => WireRunnerProjectionState::Pinned,
        ProcessRunnerProjectionState::RunnerLostBeforePin => {
            WireRunnerProjectionState::RunnerLostBeforePin
        }
        ProcessRunnerProjectionState::RunnerLost => WireRunnerProjectionState::RunnerLost,
        ProcessRunnerProjectionState::RunnerAbandoned => WireRunnerProjectionState::RunnerAbandoned,
    };
    let connection_health = projection.connection_health().map(|health| match health {
        ProcessRunnerConnectionHealth::Connected => WireRunnerConnectionHealth::Connected,
        ProcessRunnerConnectionHealth::Suspect => WireRunnerConnectionHealth::Suspect,
        ProcessRunnerConnectionHealth::Shutdown => WireRunnerConnectionHealth::Shutdown,
        ProcessRunnerConnectionHealth::Lost => WireRunnerConnectionHealth::Lost,
    });
    WireRunnerProjection::try_new(
        selector,
        projection
            .runner()
            .map(|runner| wire_uuid(runner.into_uuid())),
        WireRunnerPlacementRevision::try_new(projection.placement_revision().get())
            .ok_or(SnapshotSpoolError::EncodeInvariant)?,
        sandbox_profile,
        projection
            .credential_profile()
            .map(|profile| WireRunnerCredentialProfileName::try_new(profile.as_str().to_owned()))
            .transpose()
            .map_err(|_| SnapshotSpoolError::EncodeInvariant)?,
        projection
            .repository()
            .map(|repository| WireRunnerRepositoryKey::try_new(repository.as_str().to_owned()))
            .transpose()
            .map_err(|_| SnapshotSpoolError::EncodeInvariant)?,
        projection
            .working_directory()
            .map(|directory| WireRunnerWorkingDirectory::try_new(directory.as_str().to_owned()))
            .transpose()
            .map_err(|_| SnapshotSpoolError::EncodeInvariant)?,
        connection_health,
        state,
    )
    .map_err(|_| SnapshotSpoolError::EncodeInvariant)
}

async fn write_spooled_transcript<Writer>(
    writer: &mut Writer,
    mut spool: TranscriptSpool,
) -> Result<u64, ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_spooled_file(writer, &mut spool.file).await?;
    Ok(spool.cursor)
}

async fn write_spooled_file<Writer>(
    writer: &mut Writer,
    file: &mut tokio::fs::File,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(ProcessConnectionError::SpoolIo)?;
        if read == 0 {
            return Ok(());
        }
        writer
            .write_all(&buffer[..read])
            .await
            .map_err(ProcessConnectionError::PeerIo)?;
    }
}

async fn write_transcript_turn<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    turn: &ProcessTranscriptTurn,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::TranscriptTurn {
            turn_id: wire_uuid(turn.turn().into_uuid()),
            acceptance_position: CanonicalU64::new(turn.acceptance_position()),
            state: wire_turn_state(turn.state()),
            model_settings: turn.model_settings().map(wire_turn_model_settings),
        },
    )
    .await
}

async fn write_model_call_usage<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    model_call_index: u64,
    evidence: &ProcessTranscriptModelCallUsage,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let usage = evidence.usage();
    let cost = model_configuration
        .derive_model_call_cost(
            evidence.target(),
            evidence.credential_profile(),
            crate::configuration::ModelCallInputUsage::from_persisted(
                usage.input_tokens(),
                evidence.input_token_semantics(),
            ),
            usage.output_tokens(),
            usage.cache_creation_input_tokens(),
            usage.cache_read_input_tokens(),
        )
        .map(|cost| -> Result<_, ProcessConnectionError> {
            Ok(ModelCallDollarCost {
                amount_usd: CanonicalDollarAmount::try_new(
                    cost.amount_usd().normalize().to_string(),
                )
                .map_err(|_| ProcessConnectionError::EncodeInvariant)?,
                rate_version: BillingRateVersion::try_new(cost.rate_version().to_owned())
                    .map_err(|_| ProcessConnectionError::EncodeInvariant)?,
                label: match cost.billing_kind() {
                    crate::BillingKind::ApiMetered => ModelCallCostLabel::Real,
                    crate::BillingKind::Subscription => ModelCallCostLabel::MeteredEquivalent,
                },
            })
        })
        .transpose()?;
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::TranscriptModelCallUsage {
            model_call_index: CanonicalU64::new(model_call_index),
            turn_id: wire_uuid(evidence.turn().into_uuid()),
            model_call_id: wire_uuid(evidence.call().into_uuid()),
            usage_provenance: match evidence.provenance() {
                ProcessModelCallUsageProvenance::Reported => UsageProvenance::Reported,
                ProcessModelCallUsageProvenance::Estimated => UsageProvenance::Estimated,
            },
            usage: ModelCallTokenUsage {
                input_tokens: usage.input_tokens().map(CanonicalU64::new),
                output_tokens: usage.output_tokens().map(CanonicalU64::new),
                cache_creation_input_tokens: usage
                    .cache_creation_input_tokens()
                    .map(CanonicalU64::new),
                cache_read_input_tokens: usage.cache_read_input_tokens().map(CanonicalU64::new),
            },
            cost,
        },
    )
    .await
}

async fn write_model_calls_end<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    model_call_count: u64,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::TranscriptModelCallsEnd {
            model_call_count: CanonicalU64::new(model_call_count),
        },
    )
    .await
}

async fn write_transcript_entry<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    entry: &ProcessTranscriptEntry,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match entry {
        ProcessTranscriptEntry::DelegatedTask {
            entry_index,
            source_session,
            entry,
            spawning_request,
            parent_session,
            parent_turn,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::DelegatedTask {
                        spawning_request_id: wire_uuid(spawning_request.into_uuid()),
                        parent_session_id: wire_uuid(parent_session.into_uuid()),
                        parent_turn_id: wire_uuid(parent_turn.into_uuid()),
                        content: content.clone(),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::DelegationMessage {
            entry_index,
            source_session,
            entry,
            spawning_request,
            message,
            sender,
            recipient,
            ordinal,
            delivery_sequence,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::DelegationMessage {
                        spawning_request_id: wire_uuid(spawning_request.into_uuid()),
                        message_id: wire_uuid(message.into_uuid()),
                        sender_session_id: wire_uuid(sender.into_uuid()),
                        recipient_session_id: wire_uuid(recipient.into_uuid()),
                        ordinal: CanonicalU64::new(*ordinal),
                        delivery_sequence: CanonicalU64::new(*delivery_sequence),
                        content: content.clone(),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::DelegationResult {
            entry_index,
            source_session,
            entry,
            awaiting_request,
            spawning_request,
            child,
            mode,
            delivery_sequence,
            outcome,
            content,
            reason,
            provenance,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::DelegationResult {
                        await_request_id: wire_uuid(awaiting_request.into_uuid()),
                        spawning_request_id: wire_uuid(spawning_request.into_uuid()),
                        child_session_id: wire_uuid(child.into_uuid()),
                        mode: match mode {
                            DispatchedDelegationWaitMode::Foreground => {
                                WireDelegationWaitMode::Foreground
                            }
                            DispatchedDelegationWaitMode::Background => {
                                WireDelegationWaitMode::Background
                            }
                        },
                        delivery_sequence: delivery_sequence.map(CanonicalU64::new),
                        outcome: wire_delegation_outcome(*outcome),
                        content: content.clone(),
                        reason: wire_delegation_reason(*reason),
                        provenance: wire_delegation_provenance(*provenance),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ModelIdentityChanged {
            entry_index,
            source_session,
            entry,
            turn,
            defaults_version,
            selected,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::ModelIdentityChanged {
                        turn_id: wire_uuid(turn.into_uuid()),
                        defaults_version: CanonicalU64::new(*defaults_version),
                        selected_model_id: wire_uuid(selected.into_uuid()),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ContextSummary {
            entry_index,
            source_session,
            entry,
            model_call,
            first,
            through,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptTextEntry::ContextSummary {
                        model_call_id: wire_uuid(model_call.into_uuid()),
                        first_source_session_id: wire_uuid(first.source_session().into_uuid()),
                        first_entry_id: wire_uuid(first.entry().into_uuid()),
                        through_source_session_id: wire_uuid(through.source_session().into_uuid()),
                        through_entry_id: wire_uuid(through.entry().into_uuid()),
                    },
                },
            )
            .await?;
            write_content(writer, version, request_id, *entry_index, content).await
        }
        ProcessTranscriptEntry::User {
            entry_index,
            source_session,
            entry,
            accepted_input,
            turn,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptUserEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    accepted_input_id: wire_uuid(accepted_input.into_uuid()),
                    turn_id: wire_uuid(turn.into_uuid()),
                    content: wire_user_content(content),
                },
            )
            .await
        }
        ProcessTranscriptEntry::Assistant {
            entry_index,
            source_session,
            entry,
            turn,
            model_call,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptTextEntry::Assistant {
                        turn_id: wire_uuid(turn.into_uuid()),
                        model_call_id: wire_uuid(model_call.into_uuid()),
                    },
                },
            )
            .await?;
            write_content(writer, version, request_id, *entry_index, content).await
        }
        ProcessTranscriptEntry::AssistantToolUse {
            entry_index,
            source_session,
            entry,
            turn,
            model_call,
            request,
            name,
            arguments,
            approval,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::AssistantToolUse {
                        turn_id: wire_uuid(turn.into_uuid()),
                        model_call_id: wire_uuid(model_call.into_uuid()),
                        tool_request_id: wire_uuid(request.into_uuid()),
                        tool_name: name.clone(),
                        arguments: arguments.clone(),
                        approval: approval.as_ref().map(|approval| TranscriptToolApproval {
                            decision: match approval.decision() {
                                ToolApprovalDecision::Approve => {
                                    WireToolApprovalEventDecision::Approve {}
                                }
                                ToolApprovalDecision::Deny { reason } => {
                                    WireToolApprovalEventDecision::Deny {
                                        reason: reason
                                            .as_ref()
                                            .map(|reason| reason.as_str().to_owned()),
                                    }
                                }
                            },
                            decider: match approval.decider() {
                                signalbox_domain::ToolApprovalDecider::User { command } => {
                                    WireToolApprovalEventDecider::User {
                                        command_id: wire_uuid(command.into_uuid()),
                                    }
                                }
                                signalbox_domain::ToolApprovalDecider::Delegate { model, call } => {
                                    WireToolApprovalEventDecider::Delegate {
                                        model_selection_id: wire_uuid(model.into_uuid()),
                                        model_call_id: wire_uuid(call.into_uuid()),
                                    }
                                }
                                signalbox_domain::ToolApprovalDecider::UserOverride {
                                    command,
                                    denied_request,
                                } => WireToolApprovalEventDecider::UserOverride {
                                    command_id: wire_uuid(command.into_uuid()),
                                    overridden_tool_request_id: wire_uuid(
                                        denied_request.into_uuid(),
                                    ),
                                },
                            },
                            rationale: approval
                                .rationale()
                                .map(|rationale| rationale.as_str().to_owned()),
                        }),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ToolExecutionResult {
            entry_index,
            source_session,
            entry,
            request,
            attempt,
            disposition: _,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::ToolExecutionResult {
                        tool_request_id: wire_uuid(request.into_uuid()),
                        tool_attempt_id: wire_uuid(attempt.into_uuid()),
                        content: content.clone(),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ToolDenied {
            entry_index,
            source_session,
            entry,
            request,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::ToolDenied {
                        tool_request_id: wire_uuid(request.into_uuid()),
                        content: content.clone(),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ToolClosed {
            entry_index,
            source_session,
            entry,
            request,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::ToolClosed {
                        tool_request_id: wire_uuid(request.into_uuid()),
                        content: content.clone(),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::TurnFailed {
            entry_index,
            source_session,
            entry,
            turn,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::TurnFailed {
                        turn_id: wire_uuid(turn.into_uuid()),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::TurnCompleted {
            entry_index,
            source_session,
            entry,
            turn,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::TurnCompleted {
                        turn_id: wire_uuid(turn.into_uuid()),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::TurnCancelled {
            entry_index,
            source_session,
            entry,
            turn,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::TurnCancelled {
                        turn_id: wire_uuid(turn.into_uuid()),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ImportedText {
            entry_index,
            source_session,
            entry,
            imported_conversation,
            imported_entry,
            source_speaker,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptTextEntry::Imported {
                        imported_conversation_id: wire_uuid(imported_conversation.into_uuid()),
                        imported_entry_id: wire_uuid(imported_entry.into_uuid()),
                        source_speaker: wire_imported_source_speaker(*source_speaker),
                    },
                },
            )
            .await?;
            write_content(writer, version, request_id, *entry_index, content).await
        }
        ProcessTranscriptEntry::Imported {
            entry_index,
            source_session,
            entry,
            imported_conversation,
            imported_entry,
            source_speaker,
            content_kind,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::Imported {
                        imported_conversation_id: wire_uuid(imported_conversation.into_uuid()),
                        imported_entry_id: wire_uuid(imported_entry.into_uuid()),
                        source_speaker: wire_imported_source_speaker(*source_speaker),
                        content_kind: wire_imported_content_kind(*content_kind),
                    },
                },
            )
            .await
        }
    }
}

async fn write_content<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    entry_index: u64,
    content: &str,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut fragments = content_fragments(content).peekable();
    let mut fragment_index = 0_u64;
    while let Some(fragment) = fragments.next() {
        let final_fragment = fragments.peek().is_none();
        write_message(
            writer,
            version,
            request_id,
            ServerMessage::TranscriptContent {
                entry_index: CanonicalU64::new(entry_index),
                fragment_index: CanonicalU64::new(fragment_index),
                final_fragment,
                content_fragment: fragment,
            },
        )
        .await?;
        if !final_fragment {
            fragment_index = fragment_index
                .checked_add(1)
                .ok_or(ProcessConnectionError::EncodeInvariant)?;
        }
    }
    Ok(())
}

fn map_rejection(
    rejected: SubmitInputRejectedResult,
) -> Result<RejectionDetail, ProcessConnectionError> {
    Ok(match rejected {
        SubmitInputRejectedResult::AttachmentBlobNotFound { digest } => {
            RejectionDetail::AttachmentBlobNotFound {
                digest: signalbox_process_protocol::CanonicalBlobDigest::from_digest(digest),
            }
        }
        SubmitInputRejectedResult::AttachmentByteBudgetExceeded { maximum_bytes } => {
            RejectionDetail::AttachmentByteBudgetExceeded {
                maximum_bytes: PositiveCanonicalU64::try_new(maximum_bytes)
                    .map_err(|_| ProcessConnectionError::EncodeInvariant)?,
            }
        }
        SubmitInputRejectedResult::SessionNotFound { session } => {
            RejectionDetail::SessionNotFound {
                session_id: wire_uuid(session.into_uuid()),
            }
        }
        SubmitInputRejectedResult::ActiveTurnPresent {
            session,
            active_turn,
        } => RejectionDetail::ActiveTurnPresent {
            session_id: wire_uuid(session.into_uuid()),
            active_turn_id: wire_uuid(active_turn.into_uuid()),
        },
        SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
            session,
            expected,
            current,
        } => RejectionDetail::DefaultsVersionMismatch {
            session_id: wire_uuid(session.into_uuid()),
            expected: CanonicalU64::new(expected.as_u64()),
            current: CanonicalU64::new(current.as_u64()),
        },
        SubmitInputRejectedResult::UnknownModelAlias { session, alias } => {
            RejectionDetail::UnknownModelAlias {
                session_id: wire_uuid(session.into_uuid()),
                alias_id: wire_uuid(alias.into_uuid()),
            }
        }
        SubmitInputRejectedResult::AcceptancePositionExhausted { session, last } => {
            RejectionDetail::AcceptancePositionExhausted {
                session_id: wire_uuid(session.into_uuid()),
                last: CanonicalU64::new(last.as_u64()),
            }
        }
        SubmitInputRejectedResult::NoActiveTurn {
            session,
            expected_active_turn,
        } => RejectionDetail::NoActiveTurn {
            session_id: wire_uuid(session.into_uuid()),
            expected_active_turn_id: wire_uuid(expected_active_turn.into_uuid()),
        },
        SubmitInputRejectedResult::ActiveTurnMismatch {
            session,
            expected_active_turn,
            actual_active_turn,
        } => RejectionDetail::ActiveTurnMismatch {
            session_id: wire_uuid(session.into_uuid()),
            expected_active_turn_id: wire_uuid(expected_active_turn.into_uuid()),
            active_turn_id: wire_uuid(actual_active_turn.into_uuid()),
        },
        SubmitInputRejectedResult::InterruptAlreadyApplied {
            session,
            active_turn,
            existing_command,
        } => RejectionDetail::InterruptAlreadyApplied {
            session_id: wire_uuid(session.into_uuid()),
            active_turn_id: wire_uuid(active_turn.into_uuid()),
            existing_command_id: wire_uuid(*existing_command.as_uuid()),
        },
        SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
            session,
            active_turn,
        } => RejectionDetail::InterruptUnavailableWhileAwaitingApproval {
            session_id: wire_uuid(session.into_uuid()),
            active_turn_id: wire_uuid(active_turn.into_uuid()),
        },
        SubmitInputRejectedResult::SafePointUnavailableWhileStopping {
            session,
            active_turn,
            existing_command,
        } => RejectionDetail::SafePointUnavailableWhileStopping {
            session_id: wire_uuid(session.into_uuid()),
            active_turn_id: wire_uuid(active_turn.into_uuid()),
            existing_command_id: wire_uuid(*existing_command.as_uuid()),
        },
    })
}

fn domain_model_selection(selection: WireModelSelection) -> ModelSelectionRequest {
    match selection {
        WireModelSelection::Direct { selection_id } => {
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(selection_id.into_uuid()))
        }
        WireModelSelection::Alias { alias_id } => {
            ModelSelectionRequest::Alias(ModelAlias::from_uuid(alias_id.into_uuid()))
        }
    }
}

fn domain_session_placement(placement: WireSessionPlacement) -> Result<DomainSessionPlacement, ()> {
    match placement {
        WireSessionPlacement::Pathless {} => Ok(DomainSessionPlacement::pathless()),
        WireSessionPlacement::Scoped { path } => {
            DomainSessionPlacement::scoped(SessionPlacementPath::try_new(path).map_err(|_| ())?)
                .map_err(|_| ())
        }
        WireSessionPlacement::RootGlobalRead { path, .. } => {
            DomainSessionPlacement::root_global_read(
                SessionPlacementPath::try_new(path).map_err(|_| ())?,
                signalbox_domain::RootPlacementGlobalReadIntent::Acknowledged,
            )
            .map_err(|_| ())
        }
    }
}

fn wire_session_placement(placement: &DomainSessionPlacement) -> WireSessionPlacement {
    match placement.path() {
        None => WireSessionPlacement::Pathless {},
        Some(path) if placement.records_root_global_read_intent() => {
            WireSessionPlacement::RootGlobalRead {
                path: path.as_str().to_owned(),
                intent: signalbox_process_protocol::RootPlacementGlobalReadIntent::Acknowledged,
            }
        }
        Some(path) => WireSessionPlacement::Scoped {
            path: path.as_str().to_owned(),
        },
    }
}

fn wire_model_selection(selection: ProcessModelSelection) -> WireModelSelection {
    match selection {
        ProcessModelSelection::Direct(selection) => WireModelSelection::Direct {
            selection_id: wire_uuid(selection.into_uuid()),
        },
        ProcessModelSelection::Alias(alias) => WireModelSelection::Alias {
            alias_id: wire_uuid(alias.into_uuid()),
        },
    }
}

fn wire_domain_model_selection(selection: ModelSelectionRequest) -> WireModelSelection {
    match selection {
        ModelSelectionRequest::Direct(selection) => WireModelSelection::Direct {
            selection_id: wire_uuid(selection.into_uuid()),
        },
        ModelSelectionRequest::Alias(alias) => WireModelSelection::Alias {
            alias_id: wire_uuid(alias.into_uuid()),
        },
    }
}

fn wire_frozen_model_selection(selection: &FrozenModelSelection) -> WireModelSelection {
    match selection {
        FrozenModelSelection::Direct(selection) => WireModelSelection::Direct {
            selection_id: wire_uuid(selection.into_uuid()),
        },
        FrozenModelSelection::FrozenAlias { alias, .. } => WireModelSelection::Alias {
            alias_id: wire_uuid(alias.into_uuid()),
        },
    }
}

fn domain_model_settings_overlay(value: WireModelSettingsOverlay) -> DomainModelSettingsOverlay {
    DomainModelSettingsOverlay::new(
        domain_setting_overlay(value.reasoning_level, domain_reasoning_level),
        domain_fast_mode_overlay(value.fast_mode),
        domain_setting_overlay(value.service_tier, domain_service_tier),
    )
}

fn validate_session_model_settings(
    configuration: &HubModelConfiguration,
    selection: ModelSelectionRequest,
    value: DomainModelSettingsOverlay,
) -> Result<ValidatedModelSettings, ModelSettingsAdmissionError> {
    configuration
        .validate_session_model_settings(selection, value)
        .ok_or(ModelSettingsAdmissionError::UnknownModel)?
        .map_err(|error| {
            ModelSettingsAdmissionError::Unsupported(wire_unsupported_model_setting(error))
        })
}

fn validate_replacement_model_settings(
    configuration: &HubModelConfiguration,
    selection: ModelSelectionRequest,
    caller: DomainModelSettingsOverlay,
    prior: ValidatedModelSettings,
) -> Result<(ValidatedModelSettings, Box<[DomainModelChangeAdjustment]>), ModelSettingsAdmissionError>
{
    let direct = configuration
        .resolve_direct_selection(selection)
        .ok_or(ModelSettingsAdmissionError::UnknownModel)?;
    let catalog = configuration.model_capability_catalog();
    let capabilities = catalog
        .resolve(direct)
        .ok_or(ModelSettingsAdmissionError::UnknownModel)?;
    let (profile, global_default) = configuration
        .model_settings_lower_layers(direct)
        .ok_or(ModelSettingsAdmissionError::UnknownModel)?;
    let precedence = DomainModelSettingsPrecedence::new(
        DomainModelSettingsOverlay::inherit_all(),
        model_settings_overlay_inheriting_from(caller, prior.precedence().session()),
        profile,
        global_default,
    );
    if prior
        .validated_for()
        .is_some_and(|prior_selection| prior_selection != direct)
    {
        return capabilities
            .validate_model_change(direct, precedence, caller)
            .map(signalbox_domain::AdjustedModelSettings::into_parts)
            .map_err(|error| {
                ModelSettingsAdmissionError::Unsupported(wire_unsupported_model_setting(error))
            });
    }
    capabilities
        .validate_precedence(direct, precedence)
        .map(|settings| {
            (
                settings,
                Vec::<DomainModelChangeAdjustment>::new().into_boxed_slice(),
            )
        })
        .map_err(|error| {
            ModelSettingsAdmissionError::Unsupported(wire_unsupported_model_setting(error))
        })
}

const fn model_settings_overlay_inheriting_from(
    caller: DomainModelSettingsOverlay,
    prior: DomainModelSettingsOverlay,
) -> DomainModelSettingsOverlay {
    DomainModelSettingsOverlay::new(
        match caller.reasoning_level() {
            DomainSettingOverlay::Inherit => prior.reasoning_level(),
            value @ (DomainSettingOverlay::ProviderDefault | DomainSettingOverlay::Value(_)) => {
                value
            }
        },
        match caller.fast_mode() {
            DomainFastModeOverlay::Inherit => prior.fast_mode(),
            value @ DomainFastModeOverlay::Value(_) => value,
        },
        match caller.service_tier() {
            DomainSettingOverlay::Inherit => prior.service_tier(),
            value @ (DomainSettingOverlay::ProviderDefault | DomainSettingOverlay::Value(_)) => {
                value
            }
        },
    )
}

enum ModelSettingsAdmissionError {
    UnknownModel,
    Unsupported(RejectionDetail),
}

fn model_settings_protocol_error(error: ModelSettingsAdmissionError) -> ProtocolError {
    match error {
        ModelSettingsAdmissionError::UnknownModel => {
            ProtocolError::without_detail(ErrorCode::InvalidRequest)
        }
        ModelSettingsAdmissionError::Unsupported(detail) => ProtocolError::rejected(detail),
    }
}

fn wire_unsupported_model_setting(value: UnsupportedModelSetting) -> RejectionDetail {
    match value {
        UnsupportedModelSetting::ReasoningLevel {
            selection,
            requested,
        } => RejectionDetail::UnsupportedReasoningLevel {
            selection_id: wire_uuid(selection.into_uuid()),
            requested: wire_reasoning_level(requested),
        },
        UnsupportedModelSetting::FastMode { selection } => RejectionDetail::UnsupportedFastMode {
            selection_id: wire_uuid(selection.into_uuid()),
        },
        UnsupportedModelSetting::ServiceTier {
            selection,
            requested,
        } => RejectionDetail::UnsupportedServiceTier {
            selection_id: wire_uuid(selection.into_uuid()),
            requested: wire_service_tier(requested),
        },
    }
}

fn domain_setting_overlay<WireT, DomainT>(
    value: WireSettingOverlay<WireT>,
    map: impl FnOnce(WireT) -> DomainT,
) -> DomainSettingOverlay<DomainT> {
    match value {
        WireSettingOverlay::Inherit => DomainSettingOverlay::Inherit,
        WireSettingOverlay::ProviderDefault => DomainSettingOverlay::ProviderDefault,
        WireSettingOverlay::Value(value) => DomainSettingOverlay::Value(map(value)),
    }
}

const fn domain_reasoning_level(value: WireReasoningLevel) -> DomainReasoningLevel {
    match value {
        WireReasoningLevel::None => DomainReasoningLevel::None,
        WireReasoningLevel::Minimal => DomainReasoningLevel::Minimal,
        WireReasoningLevel::Low => DomainReasoningLevel::Low,
        WireReasoningLevel::Medium => DomainReasoningLevel::Medium,
        WireReasoningLevel::High => DomainReasoningLevel::High,
        WireReasoningLevel::XHigh => DomainReasoningLevel::XHigh,
        WireReasoningLevel::Max => DomainReasoningLevel::Max,
        WireReasoningLevel::Ultra => DomainReasoningLevel::Ultra,
    }
}

const fn wire_reasoning_level(value: DomainReasoningLevel) -> WireReasoningLevel {
    match value {
        DomainReasoningLevel::None => WireReasoningLevel::None,
        DomainReasoningLevel::Minimal => WireReasoningLevel::Minimal,
        DomainReasoningLevel::Low => WireReasoningLevel::Low,
        DomainReasoningLevel::Medium => WireReasoningLevel::Medium,
        DomainReasoningLevel::High => WireReasoningLevel::High,
        DomainReasoningLevel::XHigh => WireReasoningLevel::XHigh,
        DomainReasoningLevel::Max => WireReasoningLevel::Max,
        DomainReasoningLevel::Ultra => WireReasoningLevel::Ultra,
    }
}

const fn domain_fast_mode(value: WireFastMode) -> DomainFastMode {
    match value {
        WireFastMode::Disabled => DomainFastMode::Disabled,
        WireFastMode::Enabled => DomainFastMode::Enabled,
    }
}

const fn domain_fast_mode_overlay(value: WireFastModeOverlay) -> DomainFastModeOverlay {
    match value {
        WireFastModeOverlay::Inherit => DomainFastModeOverlay::Inherit,
        WireFastModeOverlay::Value(value) => DomainFastModeOverlay::Value(domain_fast_mode(value)),
    }
}

const fn wire_fast_mode(value: DomainFastMode) -> WireFastMode {
    match value {
        DomainFastMode::Disabled => WireFastMode::Disabled,
        DomainFastMode::Enabled => WireFastMode::Enabled,
    }
}

const fn domain_service_tier(value: WireServiceTier) -> DomainServiceTier {
    match value {
        WireServiceTier::Anthropic(value) => DomainServiceTier::Anthropic(match value {
            signalbox_process_protocol::AnthropicServiceTier::Auto => {
                signalbox_domain::AnthropicServiceTier::Auto
            }
            signalbox_process_protocol::AnthropicServiceTier::StandardOnly => {
                signalbox_domain::AnthropicServiceTier::StandardOnly
            }
        }),
        WireServiceTier::OpenAi(value) => DomainServiceTier::OpenAi(match value {
            signalbox_process_protocol::OpenAiServiceTier::Auto => {
                signalbox_domain::OpenAiServiceTier::Auto
            }
            signalbox_process_protocol::OpenAiServiceTier::Default => {
                signalbox_domain::OpenAiServiceTier::Default
            }
            signalbox_process_protocol::OpenAiServiceTier::Flex => {
                signalbox_domain::OpenAiServiceTier::Flex
            }
            signalbox_process_protocol::OpenAiServiceTier::Scale => {
                signalbox_domain::OpenAiServiceTier::Scale
            }
            signalbox_process_protocol::OpenAiServiceTier::Priority => {
                signalbox_domain::OpenAiServiceTier::Priority
            }
            signalbox_process_protocol::OpenAiServiceTier::Fast => {
                signalbox_domain::OpenAiServiceTier::Fast
            }
        }),
        WireServiceTier::CodexCli(value) => DomainServiceTier::CodexCli(match value {
            signalbox_process_protocol::CodexCliServiceTier::Default => {
                signalbox_domain::CodexCliServiceTier::Default
            }
            signalbox_process_protocol::CodexCliServiceTier::Priority => {
                signalbox_domain::CodexCliServiceTier::Priority
            }
            signalbox_process_protocol::CodexCliServiceTier::Flex => {
                signalbox_domain::CodexCliServiceTier::Flex
            }
        }),
    }
}

const fn wire_service_tier(value: DomainServiceTier) -> WireServiceTier {
    match value {
        DomainServiceTier::Anthropic(value) => WireServiceTier::Anthropic(match value {
            signalbox_domain::AnthropicServiceTier::Auto => {
                signalbox_process_protocol::AnthropicServiceTier::Auto
            }
            signalbox_domain::AnthropicServiceTier::StandardOnly => {
                signalbox_process_protocol::AnthropicServiceTier::StandardOnly
            }
        }),
        DomainServiceTier::OpenAi(value) => WireServiceTier::OpenAi(match value {
            signalbox_domain::OpenAiServiceTier::Auto => {
                signalbox_process_protocol::OpenAiServiceTier::Auto
            }
            signalbox_domain::OpenAiServiceTier::Default => {
                signalbox_process_protocol::OpenAiServiceTier::Default
            }
            signalbox_domain::OpenAiServiceTier::Flex => {
                signalbox_process_protocol::OpenAiServiceTier::Flex
            }
            signalbox_domain::OpenAiServiceTier::Scale => {
                signalbox_process_protocol::OpenAiServiceTier::Scale
            }
            signalbox_domain::OpenAiServiceTier::Priority => {
                signalbox_process_protocol::OpenAiServiceTier::Priority
            }
            signalbox_domain::OpenAiServiceTier::Fast => {
                signalbox_process_protocol::OpenAiServiceTier::Fast
            }
        }),
        DomainServiceTier::CodexCli(value) => WireServiceTier::CodexCli(match value {
            signalbox_domain::CodexCliServiceTier::Default => {
                signalbox_process_protocol::CodexCliServiceTier::Default
            }
            signalbox_domain::CodexCliServiceTier::Priority => {
                signalbox_process_protocol::CodexCliServiceTier::Priority
            }
            signalbox_domain::CodexCliServiceTier::Flex => {
                signalbox_process_protocol::CodexCliServiceTier::Flex
            }
        }),
    }
}

const fn wire_model_change_adjustment(
    value: DomainModelChangeAdjustment,
) -> WireModelChangeAdjustment {
    match value {
        DomainModelChangeAdjustment::ReasoningLevelClamped { from, to } => {
            WireModelChangeAdjustment::ReasoningLevelClamped {
                from: wire_reasoning_level(from),
                to: wire_reasoning_level(to),
            }
        }
        DomainModelChangeAdjustment::ReasoningLevelCleared { from } => {
            WireModelChangeAdjustment::ReasoningLevelCleared {
                from: wire_reasoning_level(from),
            }
        }
        DomainModelChangeAdjustment::FastModeDisabled => {
            WireModelChangeAdjustment::FastModeDisabled {}
        }
        DomainModelChangeAdjustment::ServiceTierCleared { from } => {
            WireModelChangeAdjustment::ServiceTierCleared {
                from: wire_service_tier(from),
            }
        }
    }
}

fn wire_model_settings(settings: ValidatedModelSettings) -> WireModelSettingsSnapshot {
    let precedence = settings.precedence();
    let resolved = settings.resolved();
    let effective = resolved.effective();
    WireModelSettingsSnapshot {
        precedence: WireModelSettingsPrecedence {
            per_call: wire_model_settings_overlay(precedence.per_call()),
            session: wire_model_settings_overlay(precedence.session()),
            profile: wire_model_settings_overlay(precedence.profile()),
            global_default: wire_model_settings_overlay(precedence.global_default()),
        },
        effective: WireEffectiveModelSettings {
            reasoning_level: effective.reasoning_level().map(wire_reasoning_level),
            fast_mode: wire_fast_mode(effective.fast_mode()),
            service_tier: effective.service_tier().map(wire_service_tier),
        },
        reasoning_source: resolved.reasoning_source().map(wire_model_setting_source),
        fast_mode_source: resolved.fast_mode_source().map(wire_model_setting_source),
        service_tier_source: resolved
            .service_tier_source()
            .map(wire_model_setting_source),
        validated_for_selection_id: settings
            .validated_for()
            .map(|selection| wire_uuid(selection.into_uuid())),
    }
}

fn wire_turn_model_settings(
    event: &DomainTurnModelSettingsResolved,
) -> WireTurnModelSettingsSnapshot {
    WireTurnModelSettingsSnapshot {
        turn_id: wire_uuid(event.turn().into_uuid()),
        accepted_input_id: wire_uuid(event.accepted_input().into_uuid()),
        defaults_version: CanonicalU64::new(event.defaults_version().as_u64()),
        requested_model: wire_frozen_model_selection(event.selection()),
        selected_direct_id: wire_uuid(event.selection().selected_direct().into_uuid()),
        per_call_override: wire_model_settings_overlay(event.per_call_override()),
        settings: wire_model_settings(event.settings()),
        adjusted_from_selection_id: event
            .adjusted_from_selection()
            .map(|selection| wire_uuid(selection.into_uuid())),
        adjustments: event
            .adjustments()
            .iter()
            .copied()
            .map(wire_model_change_adjustment)
            .collect(),
    }
}

fn wire_model_settings_overlay(value: DomainModelSettingsOverlay) -> WireModelSettingsOverlay {
    WireModelSettingsOverlay {
        reasoning_level: wire_setting_overlay(value.reasoning_level(), wire_reasoning_level),
        fast_mode: wire_fast_mode_overlay(value.fast_mode()),
        service_tier: wire_setting_overlay(value.service_tier(), wire_service_tier),
    }
}

const fn wire_fast_mode_overlay(value: DomainFastModeOverlay) -> WireFastModeOverlay {
    match value {
        DomainFastModeOverlay::Inherit => WireFastModeOverlay::Inherit,
        DomainFastModeOverlay::Value(value) => WireFastModeOverlay::Value(wire_fast_mode(value)),
    }
}

fn wire_setting_overlay<DomainT, WireT>(
    value: DomainSettingOverlay<DomainT>,
    map: impl FnOnce(DomainT) -> WireT,
) -> WireSettingOverlay<WireT> {
    match value {
        DomainSettingOverlay::Inherit => WireSettingOverlay::Inherit,
        DomainSettingOverlay::ProviderDefault => WireSettingOverlay::ProviderDefault,
        DomainSettingOverlay::Value(value) => WireSettingOverlay::Value(map(value)),
    }
}

const fn wire_model_setting_source(value: DomainModelSettingSource) -> WireModelSettingSource {
    match value {
        DomainModelSettingSource::PerCall => WireModelSettingSource::PerCall,
        DomainModelSettingSource::Session => WireModelSettingSource::Session,
        DomainModelSettingSource::Profile => WireModelSettingSource::Profile,
        DomainModelSettingSource::GlobalDefault => WireModelSettingSource::GlobalDefault,
    }
}

/// Maps the presence-checked wire member into the domain's optional bounded
/// prompt. Frame validation already bounds the text; construction failure is
/// a fail-closed invalid request rather than a panic.
fn domain_system_prompt(
    member: SystemPromptMember,
    max_utf8_bytes: Option<usize>,
) -> Result<Option<signalbox_domain::SessionSystemPrompt>, ()> {
    match member.value() {
        None | Some(None) => Ok(None),
        Some(Some(text)) if max_utf8_bytes.is_some_and(|maximum| text.as_str().len() > maximum) => {
            Err(())
        }
        Some(Some(text)) => {
            signalbox_domain::SessionSystemPrompt::try_new(text.as_str().to_owned())
                .map(Some)
                .map_err(|_| ())
        }
    }
}

/// Maps the domain's optional bounded prompt onto the wire text type.
///
/// The domain admission is strictly at least as strict as the wire's, so a
/// `None` here is fail-closed encode-invariant evidence.
fn wire_system_prompt(
    prompt: Option<&signalbox_domain::SessionSystemPrompt>,
) -> Option<Option<SystemPromptText>> {
    match prompt {
        None => Some(None),
        Some(value) => SystemPromptText::try_new(value.as_str().to_owned())
            .ok()
            .map(Some),
    }
}

fn wire_list_metadata(
    item: &SessionMetadataListItem,
) -> Option<(Option<String>, Vec<String>, Option<MetadataLastWriter>)> {
    let last_writer = item.last_writer().map(wire_metadata_last_writer);
    Some((
        item.title().map(str::to_owned),
        item.tags().map(str::to_owned).collect(),
        last_writer,
    ))
}

fn wire_metadata_snapshot(
    snapshot: &SessionMetadataSnapshot,
) -> Option<(WireSessionMetadata, Option<MetadataLastWriter>)> {
    let content = snapshot.content();
    let metadata = WireSessionMetadata::try_new(
        content.title().map(str::to_owned),
        content.tags().map(str::to_owned).collect(),
        content
            .attributes()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        content.archived(),
    )
    .ok()?;
    let last_writer = snapshot.last_writer().map(wire_metadata_last_writer);
    Some((metadata, last_writer))
}

fn wire_metadata_last_writer(writer: SessionMetadataLastWriter) -> MetadataLastWriter {
    let actor = match writer.actor() {
        Actor::User => MetadataActor::User {},
        Actor::Core => MetadataActor::Core {},
        Actor::Model { turn } => MetadataActor::Model {
            turn_id: wire_uuid(turn.into_uuid()),
        },
        Actor::Recovery => MetadataActor::Recovery {},
        Actor::Tool { request } => MetadataActor::Tool {
            tool_request_id: wire_uuid(request.into_uuid()),
        },
    };
    MetadataLastWriter::new(
        CanonicalU64::new(writer.updated_at().as_unix_micros()),
        actor,
    )
}

const fn wire_imported_source_speaker(
    source: ProcessImportedSourceSpeaker,
) -> ImportedSourceSpeaker {
    match source {
        ProcessImportedSourceSpeaker::NotAttested => ImportedSourceSpeaker::NotAttested {},
        ProcessImportedSourceSpeaker::AttestedAbsent => ImportedSourceSpeaker::AttestedAbsent {},
        ProcessImportedSourceSpeaker::User => ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::User,
        },
        ProcessImportedSourceSpeaker::Assistant => ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::Assistant,
        },
    }
}

const fn wire_imported_content_kind(kind: ProcessImportedContentKind) -> ImportedContentKind {
    match kind {
        ProcessImportedContentKind::SourceEvent => ImportedContentKind::SourceEvent,
        ProcessImportedContentKind::SourceMessageBlock => ImportedContentKind::SourceMessageBlock,
        ProcessImportedContentKind::Text => ImportedContentKind::Text,
        ProcessImportedContentKind::ToolCall => ImportedContentKind::ToolCall,
        ProcessImportedContentKind::ToolResult => ImportedContentKind::ToolResult,
        ProcessImportedContentKind::Thinking => ImportedContentKind::Thinking,
        ProcessImportedContentKind::RedactedThinking => ImportedContentKind::RedactedThinking,
        ProcessImportedContentKind::Document => ImportedContentKind::Document,
        ProcessImportedContentKind::MessageContentAbsent => {
            ImportedContentKind::MessageContentAbsent
        }
    }
}

fn wire_turn_state(state: &ProcessTurnState) -> TurnState {
    match state {
        ProcessTurnState::Queued {
            accepted_input,
            content,
        } => TurnState::Queued {
            accepted_input_id: wire_uuid(accepted_input.into_uuid()),
            content: wire_user_content(content),
        },
        ProcessTurnState::QueuedDelegated {
            spawning_request,
            parent_session,
            parent_turn,
            content,
        } => TurnState::QueuedDelegated {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            parent_session_id: wire_uuid(parent_session.into_uuid()),
            parent_turn_id: wire_uuid(parent_turn.into_uuid()),
            content: InputContent::new(content.clone()),
        },
        ProcessTurnState::QueuedDelegationWake {
            first_delivery_sequence,
            through_delivery_sequence,
        } => TurnState::QueuedDelegationWake {
            first_delivery_sequence: CanonicalU64::new(*first_delivery_sequence),
            through_delivery_sequence: CanonicalU64::new(*through_delivery_sequence),
        },
        ProcessTurnState::DelegationTerminated {
            spawning_request,
            outcome,
            reason,
            provenance,
        } => TurnState::DelegationTerminated {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            outcome: wire_delegation_outcome(*outcome),
            reason: wire_delegation_reason(*reason),
            provenance: wire_delegation_provenance(*provenance),
        },
        ProcessTurnState::ActiveRunning {
            current_attempt,
            current_model_call,
        } => TurnState::ActiveRunning {
            current_attempt_id: wire_uuid(current_attempt.into_uuid()),
            current_model_call: current_model_call.map(|call| {
                CurrentModelCall::new(
                    wire_uuid(call.call().into_uuid()),
                    match call.state() {
                        ProcessCurrentModelCallState::Prepared => {
                            CurrentModelCallState::Prepared {}
                        }
                        ProcessCurrentModelCallState::InFlight => {
                            CurrentModelCallState::InFlight {}
                        }
                        ProcessCurrentModelCallState::CancellationRequested => {
                            CurrentModelCallState::CancellationRequested {}
                        }
                    },
                )
            }),
        },
        ProcessTurnState::ActiveAwaitingModelCallRecovery {
            ended_attempt,
            recovery_call,
            automatic_reconciliation_attempts,
            operator_action_required,
        } => TurnState::ActiveAwaitingModelCallRecovery {
            ended_attempt_id: wire_uuid(ended_attempt.into_uuid()),
            recovery_model_call_id: wire_uuid(recovery_call.into_uuid()),
            automatic_reconciliation_attempts: CanonicalU64::new(u64::from(
                *automatic_reconciliation_attempts,
            )),
            operator_action_required: *operator_action_required,
        },
        ProcessTurnState::ActiveAwaitingToolApproval { request } => {
            TurnState::ActiveAwaitingToolApproval {
                tool_request_id: wire_uuid(request.into_uuid()),
            }
        }
        ProcessTurnState::ActiveAwaitingChild {
            awaiting_request,
            spawning_request,
            child,
        } => TurnState::ActiveAwaitingChild {
            await_request_id: wire_uuid(awaiting_request.into_uuid()),
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            child_session_id: wire_uuid(child.into_uuid()),
        },
        ProcessTurnState::ActiveAwaitingToolRecovery {
            ended_attempt,
            recovery_attempt,
            automatic_reconciliation_attempts,
            operator_action_required,
        } => TurnState::ActiveAwaitingToolRecovery {
            ended_attempt_id: wire_uuid(ended_attempt.into_uuid()),
            recovery_tool_attempt_id: wire_uuid(recovery_attempt.into_uuid()),
            automatic_reconciliation_attempts: CanonicalU64::new(u64::from(
                *automatic_reconciliation_attempts,
            )),
            operator_action_required: *operator_action_required,
        },
        ProcessTurnState::ActiveAwaitingRunnerRecovery {
            runner,
            placement_revision,
            interrupted_tool_attempt,
        } => TurnState::ActiveAwaitingRunnerRecovery {
            runner_id: wire_uuid(runner.into_uuid()),
            placement_revision: PositiveCanonicalU64::from(*placement_revision),
            tool_attempt_id: interrupted_tool_attempt.map(|attempt| wire_uuid(attempt.into_uuid())),
        },
        ProcessTurnState::Failed {
            terminal_frontier,
            terminal_attempt,
            terminal_model_call,
        } => TurnState::Failed {
            terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            terminal_attempt_id: terminal_attempt.map(|attempt| wire_uuid(attempt.into_uuid())),
            terminal_model_call: terminal_model_call.map(|call| {
                let model_call_id = wire_uuid(call.call().into_uuid());
                match call.disposition() {
                    ProcessFailedModelCallDisposition::KnownFailed => {
                        let provider_cause = call.provider_failure_cause();
                        let attachment_cause = call.attachment_preparation_failure_cause();
                        debug_assert!(
                            provider_cause.is_none() || attachment_cause.is_none(),
                            "process-read validation rejects overlapping failure causes"
                        );
                        match (provider_cause, attachment_cause) {
                            (Some(cause), _) => FailedTerminalModelCall::known_failed_with_cause(
                                model_call_id,
                                wire_provider_failure_cause(cause),
                            ),
                            (None, Some(cause)) => {
                                FailedTerminalModelCall::known_failed_with_cause(
                                    model_call_id,
                                    wire_attachment_preparation_failure_cause(cause),
                                )
                            }
                            (None, None) => FailedTerminalModelCall::new(
                                model_call_id,
                                FailedModelCallDisposition::KnownFailed,
                            ),
                        }
                    }
                    ProcessFailedModelCallDisposition::Cancelled => {
                        debug_assert!(
                            call.provider_failure_cause().is_none(),
                            "process-read validation rejects causes on cancelled model calls"
                        );
                        debug_assert!(call.attachment_preparation_failure_cause().is_none());
                        FailedTerminalModelCall::new(
                            model_call_id,
                            FailedModelCallDisposition::Cancelled,
                        )
                    }
                }
            }),
        },
        ProcessTurnState::Completed {
            terminal_frontier,
            terminal_attempt,
            terminal_call,
        } => TurnState::Completed {
            terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
            terminal_model_call_id: wire_uuid(terminal_call.into_uuid()),
        },
        ProcessTurnState::Refused {
            terminal_frontier,
            terminal_attempt,
            terminal_call,
        } => TurnState::Refused {
            terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
            terminal_model_call_id: wire_uuid(terminal_call.into_uuid()),
        },
        ProcessTurnState::Cancelled {
            terminal_frontier,
            terminal_attempt,
            terminal_call,
        } => TurnState::Cancelled {
            terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
            terminal_model_call_id: terminal_call.map(|call| wire_uuid(call.into_uuid())),
        },
        ProcessTurnState::ReconciliationRequired {
            terminal_frontier,
            terminal_attempt,
            operation,
        } => match operation {
            ProcessReconciliationOperation::ModelCall(call) => TurnState::ReconciliationRequired {
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
                terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
                terminal_model_call_id: wire_uuid(call.into_uuid()),
            },
            ProcessReconciliationOperation::ToolAttempt(attempt) => {
                TurnState::ToolReconciliationRequired {
                    terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
                    terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
                    terminal_tool_attempt_id: wire_uuid(attempt.into_uuid()),
                }
            }
        },
    }
}

async fn write_process_read_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: Option<CanonicalUuid>,
    error: ProcessReadError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let response = match error {
        ProcessReadError::Database(_) => ProtocolError::without_detail(ErrorCode::Unavailable),
        ProcessReadError::Corruption(_) => internal_protocol_error(
            session_id.map(CanonicalUuid::into_uuid),
            InternalDiagnostic::ProcessReadCorruption,
        ),
    };
    write_error(writer, version, request_id, response).await
}

/// Closed evidence for one server-side Internal response.
///
/// A variant owns both the operator class and cause code, preventing call sites
/// from pairing independent positional labels. No variant carries payload text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InternalDiagnostic {
    BlobReadIntegrity,
    ReviewWorkflowProjectionCorruption,
    ReviewOrchestrationStoreCorruption,
    ReviewOrchestrationWorkflowCorruption,
    ReviewOrchestrationSessionCorruption,
    ReviewOrchestrationServiceContract,
    ConversationImportAllocationFailure,
    ConversationImportContractDefect,
    ConversationImportWorkerTerminated,
    ImportedSessionDatabase,
    ImportedSessionCommitAmbiguous,
    ImportedSessionCommandKindMismatch,
    ImportedSessionPreparation,
    ImportedSessionIdentityCollision,
    ImportedSessionCorruption,
    ImportedConversationDatabase,
    ImportedConversationIdentityCollision,
    ImportedConversationCorruption,
    SessionDefaultsVersionMissing,
    SessionModelCredentialMissing,
    ContextCompactionRangeCorruption,
    ContextCompactionUnconfiguredTarget,
    ContextCompactionIdentityCollision,
    ContextCompactionRepositoryCorruption,
    ContextCompactionReadCorruption,
    ImportedFrontierRangeCorruption,
    TemplateSessionCreationCorruption,
    CommissionedDispatchCorruption,
    SessionCreationPreparation,
    SessionCreationDatabase,
    SessionCreationCommitAmbiguous,
    SessionCreationCommandKindMismatch,
    SessionCreationCorruption,
    ConversationListingCorruption,
    SessionMetadataDatabase,
    SessionMetadataCommitAmbiguous,
    SessionMetadataCommandKindMismatch,
    SessionMetadataCorruption,
    SessionDefaultsDatabase,
    SessionDefaultsCommitAmbiguous,
    SessionDefaultsCommandKindMismatch,
    SessionDefaultsCorruption,
    SessionDelegationDatabase,
    SessionDelegationCorruption,
    SessionDelegationContract,
    SystemPromptMemberMissing,
    SubmitInputCommandKindMismatch,
    SubmitInputIdentityCollision,
    SubmitInputCorruption,
    SubmitInputModelExecutionCorruption,
    SubmitInputModelExecutionIdentityCollision,
    SubmitInputModelExecutionNoLiveExecution,
    SubmitInputModelExecutionInvalidTransition,
    ToolLoopIdentityCollision,
    ToolLoopCorruption,
    ToolLoopInvalidTransition,
    ProcessReadCorruption,
    OperatorStatusCorruption,
    GoalRepositoryCorruption,
    SessionLifecycleCommandCorruption,
}

impl InternalDiagnostic {
    const fn failure_class(self) -> OperatorFailureClass {
        match self {
            Self::ImportedSessionDatabase
            | Self::ImportedConversationDatabase
            | Self::SessionCreationDatabase
            | Self::SessionMetadataDatabase
            | Self::SessionDefaultsDatabase
            | Self::SessionDelegationDatabase => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::ImportedSessionCommitAmbiguous
            | Self::SessionCreationCommitAmbiguous
            | Self::SessionMetadataCommitAmbiguous
            | Self::SessionDefaultsCommitAmbiguous => OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            },
            Self::ConversationImportAllocationFailure => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::ReviewOrchestrationServiceContract
            | Self::ConversationImportContractDefect
            | Self::ConversationImportWorkerTerminated
            | Self::ImportedSessionCommandKindMismatch
            | Self::ImportedSessionPreparation
            | Self::SessionCreationPreparation
            | Self::SessionCreationCommandKindMismatch
            | Self::SessionMetadataCommandKindMismatch
            | Self::SessionDefaultsCommandKindMismatch
            | Self::SessionDelegationContract
            | Self::ContextCompactionUnconfiguredTarget
            | Self::SystemPromptMemberMissing
            | Self::SubmitInputCommandKindMismatch
            | Self::SubmitInputModelExecutionNoLiveExecution
            | Self::SubmitInputModelExecutionInvalidTransition
            | Self::ToolLoopInvalidTransition => OperatorFailureClass::CallerOrHubBug,
            Self::ImportedSessionIdentityCollision
            | Self::ImportedConversationIdentityCollision
            | Self::ContextCompactionIdentityCollision
            | Self::SubmitInputIdentityCollision
            | Self::SubmitInputModelExecutionIdentityCollision
            | Self::ToolLoopIdentityCollision => OperatorFailureClass::IdentityCollision,
            Self::BlobReadIntegrity
            | Self::ReviewWorkflowProjectionCorruption
            | Self::ReviewOrchestrationStoreCorruption
            | Self::ReviewOrchestrationWorkflowCorruption
            | Self::ReviewOrchestrationSessionCorruption
            | Self::ImportedSessionCorruption
            | Self::ImportedConversationCorruption
            | Self::SessionDefaultsVersionMissing
            | Self::SessionModelCredentialMissing
            | Self::ContextCompactionRangeCorruption
            | Self::ContextCompactionRepositoryCorruption
            | Self::ContextCompactionReadCorruption
            | Self::ImportedFrontierRangeCorruption
            | Self::TemplateSessionCreationCorruption
            | Self::CommissionedDispatchCorruption
            | Self::SessionCreationCorruption
            | Self::ConversationListingCorruption
            | Self::SessionMetadataCorruption
            | Self::SessionDefaultsCorruption
            | Self::SessionDelegationCorruption
            | Self::SubmitInputCorruption
            | Self::SubmitInputModelExecutionCorruption
            | Self::ToolLoopCorruption
            | Self::ProcessReadCorruption
            | Self::OperatorStatusCorruption
            | Self::GoalRepositoryCorruption
            | Self::SessionLifecycleCommandCorruption => OperatorFailureClass::FailClosedCorruption,
        }
    }

    const fn cause_code(self) -> &'static str {
        match self {
            Self::BlobReadIntegrity => "blob_read_integrity",
            Self::ReviewWorkflowProjectionCorruption => "review_workflow_projection_corruption",
            Self::ReviewOrchestrationStoreCorruption => "review_orchestration_store_corruption",
            Self::ReviewOrchestrationWorkflowCorruption => {
                "review_orchestration_workflow_corruption"
            }
            Self::ReviewOrchestrationSessionCorruption => "review_orchestration_session_corruption",
            Self::ReviewOrchestrationServiceContract => "review_orchestration_service_contract",
            Self::ConversationImportAllocationFailure => "conversation_import_allocation_failure",
            Self::ConversationImportContractDefect => "conversation_import_contract_defect",
            Self::ConversationImportWorkerTerminated => "conversation_import_worker_terminated",
            Self::ImportedSessionDatabase => "imported_session_database",
            Self::ImportedSessionCommitAmbiguous => "imported_session_commit_ambiguous",
            Self::ImportedSessionCommandKindMismatch => "imported_session_command_kind_mismatch",
            Self::ImportedSessionPreparation => "imported_session_preparation",
            Self::ImportedSessionIdentityCollision => "imported_session_identity_collision",
            Self::ImportedSessionCorruption => "imported_session_corruption",
            Self::ImportedConversationDatabase => "imported_conversation_database",
            Self::ImportedConversationIdentityCollision => {
                "imported_conversation_identity_collision"
            }
            Self::ImportedConversationCorruption => "imported_conversation_corruption",
            Self::SessionDefaultsVersionMissing => "session_defaults_version_missing",
            Self::SessionModelCredentialMissing => "session_model_credential_missing",
            Self::ContextCompactionRangeCorruption => "context_compaction_range_corruption",
            Self::ContextCompactionUnconfiguredTarget => "context_compaction_unconfigured_target",
            Self::ContextCompactionIdentityCollision => {
                "context_compaction_repository_identity_collision"
            }
            Self::ContextCompactionRepositoryCorruption => {
                "context_compaction_repository_corruption"
            }
            Self::ContextCompactionReadCorruption => "context_compaction_read_corruption",
            Self::ImportedFrontierRangeCorruption => "imported_frontier_range_corruption",
            Self::TemplateSessionCreationCorruption => "template_session_creation_corruption",
            Self::CommissionedDispatchCorruption => "commissioned_dispatch_corruption",
            Self::SessionCreationPreparation => "session_creation_preparation",
            Self::SessionCreationDatabase => "session_creation_database",
            Self::SessionCreationCommitAmbiguous => "session_creation_commit_ambiguous",
            Self::SessionCreationCommandKindMismatch => "session_creation_command_kind_mismatch",
            Self::SessionCreationCorruption => "session_creation_corruption",
            Self::ConversationListingCorruption => "conversation_listing_corruption",
            Self::SessionMetadataDatabase => "session_metadata_database",
            Self::SessionMetadataCommitAmbiguous => "session_metadata_commit_ambiguous",
            Self::SessionMetadataCommandKindMismatch => "session_metadata_command_kind_mismatch",
            Self::SessionMetadataCorruption => "session_metadata_corruption",
            Self::SessionDefaultsDatabase => "session_defaults_database",
            Self::SessionDefaultsCommitAmbiguous => "session_defaults_commit_ambiguous",
            Self::SessionDefaultsCommandKindMismatch => "session_defaults_command_kind_mismatch",
            Self::SessionDefaultsCorruption => "session_defaults_corruption",
            Self::SessionDelegationDatabase => "session_delegation_database",
            Self::SessionDelegationCorruption => "session_delegation_corruption",
            Self::SessionDelegationContract => "session_delegation_contract",
            Self::SystemPromptMemberMissing => "system_prompt_member_missing",
            Self::SubmitInputCommandKindMismatch => "submit_input_command_kind_mismatch",
            Self::SubmitInputIdentityCollision => "submit_input_identity_collision",
            Self::SubmitInputCorruption => "submit_input_corruption",
            Self::SubmitInputModelExecutionCorruption => "submit_input_model_execution_corruption",
            Self::SubmitInputModelExecutionIdentityCollision => {
                "submit_input_model_execution_identity_collision"
            }
            Self::SubmitInputModelExecutionNoLiveExecution => {
                "submit_input_model_execution_no_live_execution"
            }
            Self::SubmitInputModelExecutionInvalidTransition => {
                "submit_input_model_execution_invalid_transition"
            }
            Self::ToolLoopIdentityCollision => "tool_loop_identity_collision",
            Self::ToolLoopCorruption => "tool_loop_corruption",
            Self::ToolLoopInvalidTransition => "tool_loop_invalid_transition",
            Self::ProcessReadCorruption => "process_read_corruption",
            Self::OperatorStatusCorruption => "operator_status_corruption",
            Self::GoalRepositoryCorruption => "goal_repository_corruption",
            Self::SessionLifecycleCommandCorruption => "session_lifecycle_command_corruption",
        }
    }
}

/// Records one typed internal diagnostic without choosing a wire response.
///
/// Present session identities use the same canonical UUID display as surrounding
/// spans; absent identities leave an empty field. Typed evidence contains only
/// closed labels, so request content, credentials, tool arguments, and nested
/// prose stay out.
fn record_internal_diagnostic(session_id: Option<uuid::Uuid>, diagnostic: InternalDiagnostic) {
    let failure_class = diagnostic.failure_class();
    let cause_code = diagnostic.cause_code();
    match session_id {
        Some(session_id) => tracing::error!(
            ?failure_class,
            cause_code,
            session_id = %session_id,
            "request failed an internal integrity check"
        ),
        None => tracing::error!(
            ?failure_class,
            cause_code,
            session_id = tracing::field::Empty,
            "request failed an internal integrity check"
        ),
    }
}

/// Records a fail-closed Internal response before returning its wire shape.
///
/// Every Internal construction routes through this function.
fn internal_protocol_error(
    session_id: Option<uuid::Uuid>,
    diagnostic: InternalDiagnostic,
) -> ProtocolError {
    record_internal_diagnostic(session_id, diagnostic);
    ProtocolError::without_detail(ErrorCode::Internal)
}

fn unavailable_protocol_error(diagnostic: InternalDiagnostic) -> ProtocolError {
    let failure_class = diagnostic.failure_class();
    let cause_code = diagnostic.cause_code();
    tracing::error!(
        ?failure_class,
        cause_code,
        session_id = tracing::field::Empty,
        "requested operation is unavailable"
    );
    ProtocolError::without_detail(ErrorCode::Unavailable)
}

async fn write_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    error: ProtocolError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::Error {
            code: error.code,
            message: error.message.to_owned(),
            detail: error.detail,
        },
    )
    .await
}

async fn write_message<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let frame = ServerFrame::try_new_for_version(version, request_id, message)
        .map_err(FrameEncodeError::Validation)?;
    let encoded = encode_server_line(&frame)?;
    writer.write_all(&encoded).await?;
    Ok(())
}

/// Writes one system-prompt-bearing message through a temporary-file spool.
///
/// A prompt response can approach the frame cap, so the direct
/// `write_message` path would retain the complete encoded frame while a peer
/// that stops reading blocks the write. Spooling first keeps per-connection
/// heap at fixed I/O buffers, and a pre-transmission spool failure stays
/// request-local as the ordinary `unavailable` response — never fatal daemon
/// evidence and never peer I/O — mirroring the snapshot paths
/// (docs/spec/process-protocol.md).
async fn write_message_via_spool<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let spool_result = spool_single_message(version, request_id, message).await;
    let mut file = match spool_result {
        Ok(file) => file,
        Err(error) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    write_spooled_file(writer, &mut file).await
}

/// Writes one committed mutation receipt through a temporary-file spool.
///
/// The receipt's mutation has already durably committed, so a pre-transmission
/// spool failure must answer `commit_ambiguous` — the caller retries the exact
/// command identity to discover the recorded outcome — never `unavailable`,
/// whose contract states no requested mutation may have committed
/// (docs/spec/process-protocol.md).
async fn write_mutation_receipt_via_spool<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let spool_result = spool_single_message(version, request_id, message).await;
    let mut file = match spool_result {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(
                error = %spool_error_display(&error),
                "committed defaults receipt spooling failed before response"
            );
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await;
        }
    };
    write_spooled_file(writer, &mut file).await
}

fn spool_error_display(error: &SnapshotSpoolError) -> String {
    match error {
        SnapshotSpoolError::Io(error) => error.to_string(),
        SnapshotSpoolError::Encode(error) => error.to_string(),
        SnapshotSpoolError::EncodeInvariant => String::from("encode invariant violated"),
    }
}

/// Encodes one message into a rewound temporary-file spool, classifying every
/// failure before the first transmitted byte as a spool failure.
async fn spool_single_message(
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
) -> Result<tokio::fs::File, SnapshotSpoolError> {
    let standard_file = tempfile::tempfile().map_err(SnapshotSpoolError::Io)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(&mut file, version, request_id, message).await?;
    file.flush().await.map_err(SnapshotSpoolError::Io)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)?;
    Ok(file)
}

async fn write_spool_message(
    writer: &mut tokio::fs::File,
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
) -> Result<(), SnapshotSpoolError> {
    write_message(writer, version, request_id, message)
        .await
        .map_err(SnapshotSpoolError::from_connection)
}

enum IncomingLine {
    Complete(Vec<u8>),
    Oversized {
        request_id: RequestId,
        admitted_version: Option<ProtocolVersion>,
    },
}

async fn read_frame_line<Reader>(
    reader: &mut Reader,
) -> Result<Option<IncomingLine>, ProcessConnectionError>
where
    Reader: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(IncomingLine::Complete(line)))
            };
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            let consumed = newline + 1;
            let frame_len = line.len().saturating_add(consumed);
            if frame_len > MAX_FRAME_BYTES {
                let (request_id, admitted_version) = if frame_len == MAX_FRAME_BYTES + 1 {
                    line.extend_from_slice(&available[..newline]);
                    (
                        recover_bounded_client_request_id(&line),
                        recover_bounded_client_protocol_version(&line),
                    )
                } else {
                    (RequestId::uncorrelated(), None)
                };
                reader.consume(consumed);
                return Ok(Some(IncomingLine::Oversized {
                    request_id,
                    admitted_version,
                }));
            }
            line.extend_from_slice(&available[..consumed]);
            reader.consume(consumed);
            return Ok(Some(IncomingLine::Complete(line)));
        }
        if line.len().saturating_add(available.len()) > MAX_FRAME_BYTES {
            let consumed = available.len();
            reader.consume(consumed);
            return Ok(Some(IncomingLine::Oversized {
                request_id: RequestId::uncorrelated(),
                admitted_version: None,
            }));
        }
        line.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }
}

fn shutdown_requested(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow()
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while !shutdown_requested(shutdown) {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

async fn run_until_shutdown<Output, Operation>(
    shutdown: &mut watch::Receiver<bool>,
    operation: Operation,
) -> Option<Output>
where
    Operation: Future<Output = Output>,
{
    tokio::select! {
        () = wait_for_shutdown(shutdown) => None,
        output = operation => Some(output),
    }
}

fn wire_provider_failure_cause(
    cause: ProcessProviderModelCallFailureCause,
) -> FailedModelCallCause {
    match cause {
        ProcessProviderModelCallFailureCause::CredentialRejected => {
            FailedModelCallCause::CredentialRejected
        }
        ProcessProviderModelCallFailureCause::PermissionDenied => {
            FailedModelCallCause::PermissionDenied
        }
        ProcessProviderModelCallFailureCause::InvalidRequest => {
            FailedModelCallCause::InvalidRequest
        }
        ProcessProviderModelCallFailureCause::TargetNotFound => {
            FailedModelCallCause::TargetNotFound
        }
        ProcessProviderModelCallFailureCause::RequestTooLarge => {
            FailedModelCallCause::RequestTooLarge
        }
        ProcessProviderModelCallFailureCause::RateLimited => FailedModelCallCause::RateLimited,
        ProcessProviderModelCallFailureCause::QuotaExhausted => {
            FailedModelCallCause::QuotaExhausted
        }
        ProcessProviderModelCallFailureCause::Overloaded => FailedModelCallCause::Overloaded,
        ProcessProviderModelCallFailureCause::ProviderInternal => {
            FailedModelCallCause::ProviderInternal
        }
        ProcessProviderModelCallFailureCause::Unrecognized => FailedModelCallCause::Unrecognized,
    }
}

fn wire_attachment_preparation_failure_cause(
    cause: signalbox_persistence::process_read::ProcessAttachmentPreparationFailureCause,
) -> FailedModelCallCause {
    use signalbox_persistence::process_read::ProcessAttachmentPreparationFailureCause;

    match cause {
        ProcessAttachmentPreparationFailureCause::TooLarge => {
            FailedModelCallCause::AttachmentTooLarge
        }
        ProcessAttachmentPreparationFailureCause::Missing => {
            FailedModelCallCause::AttachmentMissing
        }
        ProcessAttachmentPreparationFailureCause::Corrupt => {
            FailedModelCallCause::AttachmentCorrupt
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_goal_user_command<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_uuid: uuid::Uuid,
    session_id: CanonicalUuid,
    action: GoalUserAction,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let schedules_turn = match &action {
        GoalUserAction::Attach(_) | GoalUserAction::Resume(_) | GoalUserAction::Supersede(_) => {
            true
        }
        GoalUserAction::Stop { .. } => false,
    };
    let command = GoalUserCommand::new(DurableCommandId::from_uuid(command_uuid), session, action);
    let candidates = schedules_turn.then(|| {
        GoalTurnCandidates::new(
            AcceptedInputId::from_uuid(uuid::Uuid::now_v7()),
            TurnId::from_uuid(uuid::Uuid::now_v7()),
        )
    });
    let outcome = GoalRepository::new(services.pool.clone())
        .handle_user_command(command, candidates, |alias| {
            services.model_configuration.resolve_alias(alias)
        })
        .await;
    match outcome {
        Ok(GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(event))) => {
            if schedules_turn {
                let _ = services.eligibility_nudge.nudge(session);
            }
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::GoalTransitionApplied {
                    session_id,
                    event_ordinal: CanonicalU64::new(event.ordinal().get()),
                    generation: CanonicalU64::new(event.generation().get()),
                },
            )
            .await
        }
        Ok(GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Rejected(reason))) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::rejected(RejectionDetail::GoalCommandRejected {
                    session_id,
                    reason: wire_goal_command_rejection(reason),
                }),
            )
            .await
        }
        Ok(GoalCommandHandlingOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Ok(GoalCommandHandlingOutcome::TargetBusy {
            session: blocking_session,
        }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::rejected(RejectionDetail::CommissionTargetBusy {
                    session_id: CanonicalUuid::from_uuid(blocking_session.into_uuid()),
                }),
            )
            .await
        }
        // A client goal command names no expected lineage head, so it applies
        // to whatever state the session lock reveals. Reaching this answer
        // means the repository decided a question this request never asked.
        Ok(GoalCommandHandlingOutcome::LineageMoved) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Internal),
            )
            .await
        }
        Err(error) => {
            write_goal_repository_error(
                writer,
                version,
                request_id,
                Some(session_id.into_uuid()),
                error,
            )
            .await
        }
    }
}

async fn handle_session_lifecycle_command<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_uuid: uuid::Uuid,
    session_id: CanonicalUuid,
    operation: SessionLifecycleOperation,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let command = SessionLifecycleCommand::new(
        DurableCommandId::from_uuid(command_uuid),
        session,
        operation,
    );
    let outcome = SessionLifecycleCommandRepository::new(services.pool.clone())
        .handle(command.clone(), CommandPrincipal::Operator)
        .await;
    match outcome {
        Ok(SessionLifecycleCommandHandlingOutcome::Recorded(
            SessionLifecycleCommandResult::Applied(application),
        )) => {
            if lifecycle_command_needs_eligibility_nudge(&application, command.operation()) {
                let _ = services.eligibility_nudge.nudge(session);
            }
            if matches!(command.operation(), SessionLifecycleOperation::Adopt { .. })
                && let Some(goal_resumption) = &services.goal_resumption
            {
                goal_resumption.arm_blocked_goal_resumption(session);
            }
            if let SessionLifecycleApplication::ClosurePending {
                live_turn,
                defaults_version,
                ..
            } = application
                && !closure_settled(services, session).await
                && interrupt_for_closure(services, &command, live_turn, defaults_version)
                    .await
                    .is_err()
            {
                // The closure is committed; a retransmission replays it and
                // re-issues the interrupt.
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::mutation_commit_ambiguous(),
                )
                .await;
            }
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionLifecycleCommandApplied {
                    session_id,
                    effect: wire_lifecycle_effect(application),
                },
            )
            .await
        }
        Ok(SessionLifecycleCommandHandlingOutcome::Recorded(
            SessionLifecycleCommandResult::Rejected(reason),
        )) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::rejected(RejectionDetail::SessionLifecycleCommandRejected {
                    session_id,
                    reason: wire_lifecycle_rejection(reason),
                }),
            )
            .await
        }
        Ok(SessionLifecycleCommandHandlingOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(SessionLifecycleCommandRepositoryError::Database(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await
        }
        Err(SessionLifecycleCommandRepositoryError::CommitAmbiguous(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await
        }
        Err(
            SessionLifecycleCommandRepositoryError::Corruption(_)
            | SessionLifecycleCommandRepositoryError::Lifecycle(_),
        ) => {
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    Some(session_id.into_uuid()),
                    InternalDiagnostic::SessionLifecycleCommandCorruption,
                ),
            )
            .await
        }
    }
}

fn lifecycle_command_needs_eligibility_nudge(
    application: &SessionLifecycleApplication,
    operation: &SessionLifecycleOperation,
) -> bool {
    *application == SessionLifecycleApplication::StartReleased
        || matches!(
            operation,
            SessionLifecycleOperation::Release | SessionLifecycleOperation::Resume
        )
}

/// Hands a committed closure's live turn to the committed interrupt
/// machinery (§2) under a fresh core-owned identity.
async fn interrupt_for_closure(
    services: &ConnectionServices,
    command: &SessionLifecycleCommand,
    live_turn: TurnId,
    expected_version: SessionConfigurationDefaultsVersion,
) -> Result<(), ()> {
    let session = command.session();
    let (descendant_scope, cascade_root_kind) = match command.operation() {
        SessionLifecycleOperation::Stop {
            descendant_scope, ..
        } => (*descendant_scope, ParentTerminationKind::Stopped),
        _ => (
            DescendantTerminationScope::ParentAlone,
            ParentTerminationKind::Cancelled,
        ),
    };
    let Ok(content) = UserContent::try_text(String::from("The session was closed.")) else {
        return Err(());
    };
    let selected_model = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT direct_selection_id
           FROM turn_origin_effective_model_configuration($1, $2)",
    )
    .bind(live_turn.into_uuid())
    .bind(session.into_uuid())
    .fetch_optional(&services.pool)
    .await
    .map_err(|error| {
        tracing::warn!(session = %session.into_uuid(), cause = %error,
            "closure interrupt could not load its live turn configuration");
    })?
    .ok_or_else(|| {
        tracing::warn!(session = %session.into_uuid(), turn = %live_turn.into_uuid(),
            "closure interrupt live turn has no effective model configuration");
    })?;
    let request = SubmitInputRequest::try_new_core_interrupt(
        DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
        session,
        content,
        live_turn,
        descendant_scope,
        PerInputConfigurationChoices::new(
            expected_version,
            ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(selected_model),
            )),
        ),
    );
    let Ok(request) = request else {
        return Err(());
    };
    let mut service = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        ConfiguredSubmitInputTransaction {
            repository: SubmitInputRepository::new(services.pool.clone()),
            model_configuration: services.model_configuration.as_ref(),
            principal: CommandPrincipal::Core,
            cascade_root_kind,
        },
        services.eligibility_nudge.clone(),
        services.tool_dispatch_gate.clone(),
    );
    match service.execute(request).await {
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Applied(_))) => Ok(()),
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::InterruptAlreadyApplied {
                session: rejected_session,
                active_turn,
                ..
            },
        ))) if rejected_session == session && active_turn == live_turn => Ok(()),
        Ok(other) => {
            if closure_settled(services, session).await {
                return Ok(());
            }
            tracing::warn!(session = %session.into_uuid(), outcome = ?other,
                "closure interrupt was not applied");
            Err(())
        }
        Err(error) => {
            tracing::warn!(session = %session.into_uuid(), cause = %error,
                "closure interrupt failed");
            Err(())
        }
    }
}

/// A closure whose live turn ended between the command and its interrupt has
/// already settled through the deferred trigger; the interrupt's rejection is
/// then not a failure to report.
async fn closure_settled(services: &ConnectionServices, session: SessionId) -> bool {
    matches!(
        SessionLifecycleRepository::new(services.pool.clone())
            .load(session)
            .await,
        Ok(Some(record)) if record.state().is_terminal()
    )
}

const fn wire_lifecycle_effect(value: SessionLifecycleApplication) -> SessionLifecycleEffect {
    match value {
        SessionLifecycleApplication::StartReleased => SessionLifecycleEffect::StartReleased {},
        SessionLifecycleApplication::Closed { .. } => SessionLifecycleEffect::Closed {},
        SessionLifecycleApplication::ClosurePending { live_turn, .. } => {
            SessionLifecycleEffect::ClosurePending {
                live_turn_id: CanonicalUuid::from_uuid(live_turn.into_uuid()),
            }
        }
        SessionLifecycleApplication::Resumed { .. } => SessionLifecycleEffect::Resumed {},
        SessionLifecycleApplication::OwnershipChanged => {
            SessionLifecycleEffect::OwnershipChanged {}
        }
    }
}

const fn wire_lifecycle_rejection(value: DomainLifecycleRejection) -> WireLifecycleRejection {
    match value {
        DomainLifecycleRejection::SessionNotFound => WireLifecycleRejection::SessionNotFound,
        DomainLifecycleRejection::TransitionNotAdmitted => {
            WireLifecycleRejection::TransitionNotAdmitted
        }
        DomainLifecycleRejection::RequiresParked => WireLifecycleRejection::RequiresParked,
        DomainLifecycleRejection::ReleaseWhileParked => WireLifecycleRejection::ReleaseWhileParked,
        DomainLifecycleRejection::OwnershipUnchanged => WireLifecycleRejection::OwnershipUnchanged,
        DomainLifecycleRejection::FinishConditionAlreadyDeclared => {
            WireLifecycleRejection::FinishConditionAlreadyDeclared
        }
        DomainLifecycleRejection::StandingCauseMismatch => {
            WireLifecycleRejection::StandingCauseMismatch
        }
        DomainLifecycleRejection::SuccessorNotFound => WireLifecycleRejection::SuccessorNotFound,
        DomainLifecycleRejection::SuccessorIsSelf => WireLifecycleRejection::SuccessorIsSelf,
        DomainLifecycleRejection::GoalResumeRequired => WireLifecycleRejection::GoalResumeRequired,
        DomainLifecycleRejection::GoalOutcomeMismatch => {
            WireLifecycleRejection::GoalOutcomeMismatch
        }
        DomainLifecycleRejection::PendingTerminalConflict => {
            WireLifecycleRejection::PendingTerminalConflict
        }
    }
}

const fn domain_session_failure_cause(value: WireSessionFailureCause) -> DomainSessionFailureCause {
    match value {
        WireSessionFailureCause::ProviderTransient => {
            DomainSessionFailureCause::Retryable(SessionRetryableCause::ProviderTransient)
        }
        WireSessionFailureCause::ProviderQuotaExhausted => {
            DomainSessionFailureCause::Retryable(SessionRetryableCause::ProviderQuotaExhausted)
        }
        WireSessionFailureCause::ProviderOverloaded => {
            DomainSessionFailureCause::Retryable(SessionRetryableCause::ProviderOverloaded)
        }
        WireSessionFailureCause::InfrastructureFailure => {
            DomainSessionFailureCause::Retryable(SessionRetryableCause::InfrastructureFailure)
        }
        WireSessionFailureCause::RetryBudgetExhausted => {
            DomainSessionFailureCause::Retryable(SessionRetryableCause::RetryBudgetExhausted)
        }
        WireSessionFailureCause::ContextCompactionWall => {
            DomainSessionFailureCause::Structural(SessionStructuralCause::ContextCompactionWall)
        }
        WireSessionFailureCause::ContextHeadroomExhausted => {
            DomainSessionFailureCause::Structural(SessionStructuralCause::ContextHeadroomExhausted)
        }
        WireSessionFailureCause::BrokenToolchain => {
            DomainSessionFailureCause::Structural(SessionStructuralCause::BrokenToolchain)
        }
        WireSessionFailureCause::ModerationBlock => {
            DomainSessionFailureCause::Structural(SessionStructuralCause::ModerationBlock)
        }
    }
}

async fn handle_read_goal<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let loaded = GoalRepository::new(pool.clone())
        .load_goal(SessionId::from_uuid(session_id.into_uuid()))
        .await;
    let goal = match loaded {
        Ok(Some(goal)) => goal,
        Ok(None) => {
            drop(snapshot_permit);
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::NotFound),
            )
            .await;
        }
        Err(error) => {
            drop(snapshot_permit);
            return write_goal_repository_error(
                writer,
                version,
                request_id,
                Some(session_id.into_uuid()),
                error,
            )
            .await;
        }
    };
    let spool_result = spool_goal_snapshot(&goal, version, request_id, session_id).await;
    drop(goal);
    drop(snapshot_permit);
    let mut spool = match spool_result {
        Ok(spool) => spool,
        Err(error) => return write_snapshot_spool_error(writer, version, request_id, error).await,
    };
    write_spooled_file(writer, &mut spool).await
}

async fn spool_goal_snapshot(
    goal: &Goal,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
) -> Result<tokio::fs::File, SnapshotSpoolError> {
    let standard_file = tempfile::tempfile().map_err(SnapshotSpoolError::Io)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::GoalHistoryStart {
            session_id,
            current_generation: CanonicalU64::new(goal.current().generation().get()),
            current_statement: goal.current().statement().as_str().to_owned(),
        },
    )
    .await?;
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::GoalHistoryState {
            current_state: wire_goal_state(goal.current().state()),
        },
    )
    .await?;
    for event in goal.events() {
        let wire_event = wire_goal_event(event).map_err(SnapshotSpoolError::from_connection)?;
        write_spool_message(
            &mut file,
            version,
            request_id,
            ServerMessage::GoalHistoryItem {
                event_ordinal: CanonicalU64::new(event.ordinal().get()),
                generation: CanonicalU64::new(event.generation().get()),
                event: wire_event,
            },
        )
        .await?;
    }
    let event_count =
        u64::try_from(goal.events().len()).map_err(|_| SnapshotSpoolError::EncodeInvariant)?;
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::GoalHistoryEnd {
            event_count: CanonicalU64::new(event_count),
        },
    )
    .await?;
    file.flush().await.map_err(SnapshotSpoolError::Io)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)?;
    Ok(file)
}

fn wire_goal_state(state: &GoalState) -> GoalLifecycleState {
    match state {
        GoalState::Pursuing => GoalLifecycleState::Pursuing {},
        GoalState::Blocked { reason, need } => GoalLifecycleState::Blocked {
            reason: wire_goal_blocked_reason(*reason),
            need: need.as_str().to_owned(),
        },
        GoalState::Achieved { report } => GoalLifecycleState::Achieved {
            turn_id: wire_uuid(report.turn().into_uuid()),
            tool_request_id: wire_uuid(report.tool_request().into_uuid()),
        },
        GoalState::UserStopped => GoalLifecycleState::UserStopped {},
        GoalState::Superseded { by_generation } => GoalLifecycleState::Superseded {
            by_generation: CanonicalU64::new(by_generation.get()),
        },
        GoalState::SessionClosed { outcome } => GoalLifecycleState::SessionClosed {
            outcome: wire_session_closure_outcome(*outcome),
        },
    }
}

fn wire_session_closure_outcome(
    outcome: signalbox_domain::SessionClosureOutcome,
) -> SessionClosureOutcome {
    match outcome {
        signalbox_domain::SessionClosureOutcome::FailedRetryable => {
            SessionClosureOutcome::FailedRetryable
        }
        signalbox_domain::SessionClosureOutcome::FailedStructural => {
            SessionClosureOutcome::FailedStructural
        }
        signalbox_domain::SessionClosureOutcome::FailedUnknown => {
            SessionClosureOutcome::FailedUnknown
        }
        signalbox_domain::SessionClosureOutcome::Stopped => SessionClosureOutcome::Stopped,
        signalbox_domain::SessionClosureOutcome::Superseded => SessionClosureOutcome::Superseded,
        signalbox_domain::SessionClosureOutcome::Abandoned => SessionClosureOutcome::Abandoned,
        signalbox_domain::SessionClosureOutcome::Retired => SessionClosureOutcome::Retired,
    }
}

fn wire_lifecycle_actor(actor: signalbox_domain::LifecycleActor) -> LifecycleActorClass {
    match actor {
        signalbox_domain::LifecycleActor::Core { .. } => LifecycleActorClass::Core,
        signalbox_domain::LifecycleActor::Operator => LifecycleActorClass::Operator,
        signalbox_domain::LifecycleActor::Module { .. } => LifecycleActorClass::Module,
        signalbox_domain::LifecycleActor::Watchdog => LifecycleActorClass::Watchdog,
    }
}

fn wire_goal_event(event: &GoalEvent) -> Result<GoalHistoryEvent, ProcessConnectionError> {
    match event.kind() {
        GoalEventKind::Commissioned {
            statement,
            provenance,
        } => Ok(GoalHistoryEvent::Commissioned {
            statement: statement.as_str().to_owned(),
            command_id: wire_goal_command_id(provenance.command())?,
        }),
        GoalEventKind::Blocked { block, need } => Ok(GoalHistoryEvent::Blocked {
            reason: wire_goal_blocked_reason(block.reason_kind()),
            need: need.as_str().to_owned(),
            provenance: wire_goal_blocked_provenance(*block),
        }),
        GoalEventKind::Resumed {
            guidance,
            provenance,
        } => Ok(GoalHistoryEvent::Resumed {
            guidance: guidance.as_ref().map(|value| value.as_str().to_owned()),
            command_id: wire_goal_command_id(provenance.command())?,
        }),
        GoalEventKind::Achieved { report, provenance } => Ok(GoalHistoryEvent::Achieved {
            report: report.as_str().to_owned(),
            turn_id: wire_uuid(provenance.turn().into_uuid()),
            tool_request_id: wire_uuid(provenance.tool_request().into_uuid()),
        }),
        GoalEventKind::UserStopped { provenance } => Ok(GoalHistoryEvent::UserStopped {
            command_id: wire_goal_command_id(provenance.command())?,
        }),
        GoalEventKind::Superseded {
            replacement_statement,
            provenance,
        } => Ok(GoalHistoryEvent::Superseded {
            replacement_statement: replacement_statement.as_str().to_owned(),
            command_id: wire_goal_command_id(provenance.command())?,
        }),
        GoalEventKind::SessionClosed {
            outcome,
            provenance,
        } => Ok(GoalHistoryEvent::SessionClosed {
            outcome: wire_session_closure_outcome(*outcome),
            actor: wire_lifecycle_actor(*provenance),
        }),
    }
}

fn wire_goal_blocked_provenance(value: GoalBlockProvenance) -> WireGoalBlockedProvenance {
    match value {
        GoalBlockProvenance::Model { provenance, .. }
        | GoalBlockProvenance::FinishCheck { provenance } => WireGoalBlockedProvenance::Model {
            turn_id: wire_uuid(provenance.turn().into_uuid()),
            tool_request_id: wire_uuid(provenance.tool_request().into_uuid()),
        },
        GoalBlockProvenance::ExecutionFailure { provenance } => {
            WireGoalBlockedProvenance::ExecutionFailure {
                turn_id: wire_uuid(provenance.turn().into_uuid()),
            }
        }
    }
}

const fn wire_goal_blocked_reason(value: GoalBlockedReasonKind) -> WireGoalBlockedReason {
    match value {
        GoalBlockedReasonKind::UserInputRequired => WireGoalBlockedReason::UserInputRequired,
        GoalBlockedReasonKind::ExternalChangeRequired => {
            WireGoalBlockedReason::ExternalChangeRequired
        }
        GoalBlockedReasonKind::AuthorizationRequired => {
            WireGoalBlockedReason::AuthorizationRequired
        }
        GoalBlockedReasonKind::ExecutionFailure => WireGoalBlockedReason::ExecutionFailure,
        GoalBlockedReasonKind::FinishCheckFailed => WireGoalBlockedReason::FinishCheckFailed,
    }
}

const fn wire_goal_command_rejection(
    value: DomainGoalCommandRejection,
) -> WireGoalCommandRejection {
    match value {
        DomainGoalCommandRejection::SessionNotFound => WireGoalCommandRejection::SessionNotFound,
        DomainGoalCommandRejection::SessionClosing => WireGoalCommandRejection::SessionClosing,
        DomainGoalCommandRejection::GoalAlreadyAttached => {
            WireGoalCommandRejection::GoalAlreadyAttached
        }
        DomainGoalCommandRejection::GoalNotAttached => WireGoalCommandRejection::GoalNotAttached,
        DomainGoalCommandRejection::UnknownModelAlias => {
            WireGoalCommandRejection::UnknownModelAlias
        }
        DomainGoalCommandRejection::AcceptancePositionExhausted => {
            WireGoalCommandRejection::AcceptancePositionExhausted
        }
        DomainGoalCommandRejection::RequiresBlocked => WireGoalCommandRejection::RequiresBlocked,
        DomainGoalCommandRejection::RequiresPursuingOrBlocked => {
            WireGoalCommandRejection::RequiresPursuingOrBlocked
        }
        DomainGoalCommandRejection::GenerationExhausted => {
            WireGoalCommandRejection::GenerationExhausted
        }
        DomainGoalCommandRejection::EventOrdinalExhausted => {
            WireGoalCommandRejection::EventOrdinalExhausted
        }
    }
}

fn wire_goal_command_id(
    value: DurableCommandId,
) -> Result<signalbox_process_protocol::CommandId, ProcessConnectionError> {
    signalbox_process_protocol::CommandId::try_from_uuid(value.into_uuid())
        .map_err(|_| ProcessConnectionError::EncodeInvariant)
}

async fn write_goal_repository_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: Option<uuid::Uuid>,
    error: GoalRepositoryError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let protocol_error = match error {
        GoalRepositoryError::Database(_) => ProtocolError::mutation_definitely_unavailable(),
        GoalRepositoryError::CommitAmbiguous(_) => ProtocolError::mutation_commit_ambiguous(),
        GoalRepositoryError::DifferentCommandKind { .. } => {
            ProtocolError::without_detail(ErrorCode::ConflictingReuse)
        }
        GoalRepositoryError::Corruption(_) => {
            internal_protocol_error(session_id, InternalDiagnostic::GoalRepositoryCorruption)
        }
    };
    write_error(writer, version, request_id, protocol_error).await
}

fn wire_uuid(value: uuid::Uuid) -> CanonicalUuid {
    CanonicalUuid::from_uuid(value)
}

struct ProtocolError {
    code: ErrorCode,
    message: &'static str,
    detail: ErrorDetail,
}

impl ProtocolError {
    /// The selected session exists but the named immutable defaults epoch
    /// was never installed; the wire code remains the shared `not_found`.
    const fn defaults_epoch_not_found() -> Self {
        Self {
            code: ErrorCode::NotFound,
            message: "the requested defaults epoch was not found on the selected session",
            detail: ErrorDetail::none(),
        }
    }

    /// No imported conversation has the named identity. The absent read target
    /// is never a session; the wire code remains the shared `not_found`.
    const fn imported_conversation_absent() -> Self {
        Self {
            code: ErrorCode::NotFound,
            message: "the requested imported conversation was not found",
            detail: ErrorDetail::none(),
        }
    }

    const fn blob_not_found() -> Self {
        Self {
            code: ErrorCode::NotFound,
            message: "the requested blob was not found",
            detail: ErrorDetail::none(),
        }
    }

    const fn without_detail(code: ErrorCode) -> Self {
        Self {
            code,
            message: match code {
                ErrorCode::MalformedFrame => "the protocol frame is malformed",
                ErrorCode::UnsupportedVersion => {
                    "the protocol version is unsupported; supported version: 1"
                }
                ErrorCode::InvalidRequest => "the request values are invalid",
                ErrorCode::NotFound => "the requested session was not found",
                ErrorCode::BlobMissing => "all recorded blob replicas are missing",
                ErrorCode::BlobCorrupt => "all usable blob replicas are corrupt",
                ErrorCode::ConflictingReuse => {
                    "the command identity already names different intent"
                }
                ErrorCode::Rejected => "the command was rejected by current durable state",
                ErrorCode::ResyncRequired => {
                    "the follow stream fell behind; reconnect for a fresh snapshot"
                }
                ErrorCode::Unavailable => "the requested operation is unavailable",
                ErrorCode::PublicationAmbiguous => {
                    "the blob publication is ambiguous; retry the exact upload"
                }
                ErrorCode::CommitAmbiguous => {
                    "the mutation commit is ambiguous; retry the exact command"
                }
                ErrorCode::Internal => "the request failed an internal integrity check",
            },
            detail: ErrorDetail::none(),
        }
    }

    const fn invalid_import(detail: RejectionDetail) -> Self {
        Self {
            code: ErrorCode::InvalidRequest,
            message: "conversation import was rejected",
            detail: ErrorDetail::invalid_request(detail),
        }
    }

    const fn invalid_blob_upload(detail: RejectionDetail) -> Self {
        Self {
            code: ErrorCode::InvalidRequest,
            message: "blob upload was rejected",
            detail: ErrorDetail::invalid_request(detail),
        }
    }

    const fn invalid_blob_read(detail: RejectionDetail) -> Self {
        Self {
            code: ErrorCode::InvalidRequest,
            message: "blob read was rejected",
            detail: ErrorDetail::invalid_request(detail),
        }
    }

    const fn invalid_bulk_ingest(detail: RejectionDetail) -> Self {
        Self {
            code: ErrorCode::InvalidRequest,
            message: "bulk ingest was rejected",
            detail: ErrorDetail::invalid_request(detail),
        }
    }

    const fn mutation_definitely_unavailable() -> Self {
        Self::without_detail(ErrorCode::Unavailable)
    }

    const fn mutation_commit_ambiguous() -> Self {
        Self::without_detail(ErrorCode::CommitAmbiguous)
    }

    const fn mutation_unavailable(commit_ambiguous: bool) -> Self {
        if commit_ambiguous {
            Self::mutation_commit_ambiguous()
        } else {
            Self::mutation_definitely_unavailable()
        }
    }

    const fn rejected(detail: RejectionDetail) -> Self {
        Self {
            code: ErrorCode::Rejected,
            message: "the command was rejected by current durable state",
            detail: ErrorDetail::rejected(detail),
        }
    }
}

#[derive(Clone, Debug)]
enum ProcessUpdate {
    Durable {
        cursor: u64,
        session: SessionId,
        event: ProcessUpdateEvent,
    },
    ProviderTextDelta(ProviderTextDelta),
}

impl ProcessUpdate {
    fn from_outbox(event: &DispatchedOutboxEvent) -> Option<Self> {
        Some(Self::Durable {
            cursor: event.sequence(),
            session: event.session()?,
            event: ProcessUpdateEvent::from_outbox(event.kind())?,
        })
    }
}

#[derive(Clone, Debug)]
enum ProcessUpdateEvent {
    SessionCreated,
    SessionModelSettingsChanged(DomainSessionModelSettingsChanged),
    TurnModelSettingsResolved(DomainTurnModelSettingsResolved),
    InputAccepted {
        accepted_input: signalbox_domain::AcceptedInputId,
        turn: signalbox_domain::TurnId,
        acceptance_position: u64,
        content: UserContent,
    },
    GoalTurnRetired {
        turn: signalbox_domain::TurnId,
    },
    TurnActivated {
        turn: signalbox_domain::TurnId,
        current_attempt: signalbox_domain::TurnAttemptId,
    },
    ModelCallTransition {
        turn: signalbox_domain::TurnId,
        call: signalbox_domain::ModelCallId,
        state: DispatchedModelCallState,
    },
    ToolBatchTransition {
        turn: signalbox_domain::TurnId,
        producing_call: signalbox_domain::ModelCallId,
        state: DispatchedToolBatchState,
    },
    ToolApprovalDecided {
        turn: signalbox_domain::TurnId,
        approval: signalbox_domain::ToolApprovalResolution,
        decider: signalbox_domain::ToolApprovalDecider,
    },
    RunnerStateTransition {
        runner: signalbox_domain::RunnerId,
        placement_revision: signalbox_domain::RunnerGeneration,
        sandbox: signalbox_domain::RunnerSandboxProfile,
        working_directory: Option<signalbox_domain::RunnerWorkingDirectory>,
        state: DispatchedRunnerState,
    },
    ContextCompacted {
        compaction: signalbox_domain::ContextCompactionId,
        call: signalbox_domain::ModelCallId,
        through_position: u64,
        summary_entry: signalbox_domain::SemanticTranscriptEntryId,
        result_frontier: signalbox_domain::ContextFrontierId,
    },
    TurnCompleted {
        turn: signalbox_domain::TurnId,
        call: signalbox_domain::ModelCallId,
        completion_entry: signalbox_domain::SemanticTranscriptEntryId,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
    TurnFailed {
        turn: signalbox_domain::TurnId,
        failure_entry: signalbox_domain::SemanticTranscriptEntryId,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
    TurnRefused {
        turn: signalbox_domain::TurnId,
        call: signalbox_domain::ModelCallId,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
    TurnCancelled {
        turn: signalbox_domain::TurnId,
        cancellation_entry: signalbox_domain::SemanticTranscriptEntryId,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
    TurnReconciliationRequired {
        turn: signalbox_domain::TurnId,
        operation: DispatchedReconciliationOperation,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
    DelegationUpdate(DispatchedDelegationUpdate),
}

impl ProcessUpdateEvent {
    fn from_outbox(event: &DispatchedOutboxEventKind) -> Option<Self> {
        Some(match event {
            DispatchedOutboxEventKind::SessionCreated(_) => Self::SessionCreated,
            // The module-facing lifecycle kinds have no wire projection yet.
            DispatchedOutboxEventKind::SessionStateChanged(_)
            | DispatchedOutboxEventKind::SessionTerminal(_)
            | DispatchedOutboxEventKind::GoalChanged(_)
            | DispatchedOutboxEventKind::CommandSettled { .. }
            | DispatchedOutboxEventKind::InjectionSettled { .. }
            | DispatchedOutboxEventKind::SessionOwnershipChanged(_) => return None,
            DispatchedOutboxEventKind::SessionModelSettingsChanged(event) => {
                Self::SessionModelSettingsChanged(event.clone())
            }
            DispatchedOutboxEventKind::TurnModelSettingsResolved(event) => {
                Self::TurnModelSettingsResolved(event.clone())
            }
            DispatchedOutboxEventKind::InputAccepted {
                accepted_input,
                turn,
                acceptance_position,
                content,
            } => Self::InputAccepted {
                accepted_input: *accepted_input,
                turn: *turn,
                acceptance_position: acceptance_position.as_u64(),
                content: content.clone(),
            },
            DispatchedOutboxEventKind::TurnTerminal { turn, disposition } => match disposition {
                DispatchedTurnTerminalDisposition::Completed {
                    call,
                    completion_entry,
                    terminal_frontier,
                } => Self::TurnCompleted {
                    turn: *turn,
                    call: *call,
                    completion_entry: *completion_entry,
                    terminal_frontier: *terminal_frontier,
                },
                DispatchedTurnTerminalDisposition::Refused {
                    call,
                    terminal_frontier,
                } => Self::TurnRefused {
                    turn: *turn,
                    call: *call,
                    terminal_frontier: *terminal_frontier,
                },
                DispatchedTurnTerminalDisposition::Failed {
                    failure_entry,
                    terminal_frontier,
                } => Self::TurnFailed {
                    turn: *turn,
                    failure_entry: *failure_entry,
                    terminal_frontier: *terminal_frontier,
                },
                DispatchedTurnTerminalDisposition::Cancelled {
                    cancellation_entry,
                    terminal_frontier,
                } => Self::TurnCancelled {
                    turn: *turn,
                    cancellation_entry: *cancellation_entry,
                    terminal_frontier: *terminal_frontier,
                },
                DispatchedTurnTerminalDisposition::ReconciliationRequired {
                    operation,
                    terminal_frontier,
                } => Self::TurnReconciliationRequired {
                    turn: *turn,
                    operation: *operation,
                    terminal_frontier: *terminal_frontier,
                },
                DispatchedTurnTerminalDisposition::Retired => Self::GoalTurnRetired { turn: *turn },
            },
            DispatchedOutboxEventKind::TurnActivated {
                turn,
                current_attempt,
            } => Self::TurnActivated {
                turn: *turn,
                current_attempt: *current_attempt,
            },
            DispatchedOutboxEventKind::ModelCallTransition { turn, call, state } => {
                Self::ModelCallTransition {
                    turn: *turn,
                    call: *call,
                    state: *state,
                }
            }
            DispatchedOutboxEventKind::ToolBatchTransition {
                turn,
                producing_call,
                state,
            } => Self::ToolBatchTransition {
                turn: *turn,
                producing_call: *producing_call,
                state: *state,
            },
            DispatchedOutboxEventKind::ToolApprovalDecided {
                turn,
                approval,
                decider,
            } => Self::ToolApprovalDecided {
                turn: *turn,
                approval: approval.clone(),
                decider: *decider,
            },
            DispatchedOutboxEventKind::RunnerStateTransition {
                runner,
                placement_revision,
                sandbox,
                working_directory,
                state,
            } => Self::RunnerStateTransition {
                runner: *runner,
                placement_revision: *placement_revision,
                sandbox: *sandbox,
                working_directory: working_directory.clone(),
                state: *state,
            },
            DispatchedOutboxEventKind::ContextCompacted {
                compaction,
                call,
                through_position,
                summary_entry,
                result_frontier,
            } => Self::ContextCompacted {
                compaction: *compaction,
                call: *call,
                through_position: *through_position,
                summary_entry: *summary_entry,
                result_frontier: *result_frontier,
            },
            DispatchedOutboxEventKind::DelegationUpdate(update) => {
                Self::DelegationUpdate(update.clone())
            }
            DispatchedOutboxEventKind::DelegationWake(_) => return None,
        })
    }

    fn wire(&self) -> Result<SessionEvent, ProcessConnectionError> {
        let event = match self {
            Self::SessionCreated => SessionEvent::SessionCreated {},
            Self::SessionModelSettingsChanged(event) => SessionEvent::SessionModelSettingsChanged {
                command_id: signalbox_process_protocol::CommandId::try_from_uuid(
                    event.command_id().into_uuid(),
                )
                .map_err(|_| ProcessConnectionError::EncodeInvariant)?,
                prior_defaults_version: CanonicalU64::new(event.prior_defaults_version().as_u64()),
                installed_defaults_version: CanonicalU64::new(
                    event.installed_defaults_version().as_u64(),
                ),
                prior_model: wire_domain_model_selection(event.prior_model()),
                installed_model: wire_domain_model_selection(event.installed_model()),
                prior_settings: wire_model_settings(event.prior_settings()),
                installed_settings: wire_model_settings(event.installed_settings()),
                caller_override: wire_model_settings_overlay(event.caller_override()),
                adjustments: event
                    .adjustments()
                    .iter()
                    .copied()
                    .map(wire_model_change_adjustment)
                    .collect(),
            },
            Self::TurnModelSettingsResolved(event) => SessionEvent::TurnModelSettingsResolved {
                accepted_input_id: wire_uuid(event.accepted_input().into_uuid()),
                turn_id: wire_uuid(event.turn().into_uuid()),
                defaults_version: CanonicalU64::new(event.defaults_version().as_u64()),
                requested_model: wire_frozen_model_selection(event.selection()),
                selected_direct_id: wire_uuid(event.selection().selected_direct().into_uuid()),
                per_call_override: wire_model_settings_overlay(event.per_call_override()),
                settings: wire_model_settings(event.settings()),
                adjusted_from_selection_id: event
                    .adjusted_from_selection()
                    .map(|selection| wire_uuid(selection.into_uuid())),
                adjustments: event
                    .adjustments()
                    .iter()
                    .copied()
                    .map(wire_model_change_adjustment)
                    .collect(),
            },
            Self::InputAccepted {
                accepted_input,
                turn,
                acceptance_position,
                content,
            } => SessionEvent::InputAccepted {
                accepted_input_id: wire_uuid(accepted_input.into_uuid()),
                turn_id: wire_uuid(turn.into_uuid()),
                acceptance_position: CanonicalU64::new(*acceptance_position),
                content: wire_user_content(content),
            },
            Self::GoalTurnRetired { turn } => SessionEvent::GoalTurnRetired {
                turn_id: wire_uuid(turn.into_uuid()),
            },
            Self::TurnActivated {
                turn,
                current_attempt,
            } => SessionEvent::TurnActivated {
                turn_id: wire_uuid(turn.into_uuid()),
                current_attempt_id: wire_uuid(current_attempt.into_uuid()),
            },
            Self::ModelCallTransition { turn, call, state } => SessionEvent::ModelCallTransition {
                turn_id: wire_uuid(turn.into_uuid()),
                model_call_id: wire_uuid(call.into_uuid()),
                state: wire_model_call_state(*state),
            },
            Self::ToolBatchTransition {
                turn,
                producing_call,
                state,
            } => SessionEvent::ToolBatchTransition {
                turn_id: wire_uuid(turn.into_uuid()),
                model_call_id: wire_uuid(producing_call.into_uuid()),
                state: match state {
                    DispatchedToolBatchState::Proposed { frontier } => ToolBatchState::Proposed {
                        frontier_id: wire_uuid(frontier.into_uuid()),
                    },
                    DispatchedToolBatchState::ResultsProjected { frontier } => {
                        ToolBatchState::ResultsProjected {
                            frontier_id: wire_uuid(frontier.into_uuid()),
                        }
                    }
                    DispatchedToolBatchState::RecoveryRequired { attempt } => {
                        ToolBatchState::RecoveryRequired {
                            tool_attempt_id: wire_uuid(attempt.into_uuid()),
                        }
                    }
                },
            },
            Self::ToolApprovalDecided {
                turn,
                approval,
                decider,
            } => {
                let decision = match approval.decision() {
                    ToolApprovalDecision::Approve => WireToolApprovalEventDecision::Approve {},
                    ToolApprovalDecision::Deny { reason } => WireToolApprovalEventDecision::Deny {
                        reason: reason.as_ref().map(|value| value.as_str().to_owned()),
                    },
                };
                let decider = match decider {
                    signalbox_domain::ToolApprovalDecider::User { command } => {
                        WireToolApprovalEventDecider::User {
                            command_id: wire_uuid(command.into_uuid()),
                        }
                    }
                    signalbox_domain::ToolApprovalDecider::Delegate { model, call } => {
                        WireToolApprovalEventDecider::Delegate {
                            model_selection_id: wire_uuid(model.into_uuid()),
                            model_call_id: wire_uuid(call.into_uuid()),
                        }
                    }
                    signalbox_domain::ToolApprovalDecider::UserOverride {
                        command,
                        denied_request,
                    } => WireToolApprovalEventDecider::UserOverride {
                        command_id: wire_uuid(command.into_uuid()),
                        overridden_tool_request_id: wire_uuid(denied_request.into_uuid()),
                    },
                };
                SessionEvent::ToolApprovalDecided {
                    turn_id: wire_uuid(turn.into_uuid()),
                    tool_request_id: wire_uuid(approval.request().into_uuid()),
                    decision,
                    decider,
                    rationale: approval.rationale().map(|value| value.as_str().to_owned()),
                }
            }
            Self::RunnerStateTransition {
                runner,
                placement_revision,
                sandbox,
                working_directory,
                state,
            } => SessionEvent::RunnerStateTransition {
                runner_id: wire_uuid(runner.into_uuid()),
                placement_revision: WireRunnerPlacementRevision::try_new(placement_revision.get())
                    .ok_or(ProcessConnectionError::EncodeInvariant)?,
                sandbox_profile: match sandbox {
                    signalbox_domain::RunnerSandboxProfile::Ambient => {
                        WireRunnerSandboxProfile::Ambient
                    }
                    signalbox_domain::RunnerSandboxProfile::WorkspaceRestricted => {
                        WireRunnerSandboxProfile::WorkspaceRestricted
                    }
                },
                working_directory: working_directory
                    .as_ref()
                    .map(|directory| {
                        WireRunnerWorkingDirectory::try_new(directory.as_str().to_owned())
                            .map_err(|_| ProcessConnectionError::EncodeInvariant)
                    })
                    .transpose()?,
                state: match state {
                    DispatchedRunnerState::Pinned => WireRunnerStateTransitionState::Pinned,
                    DispatchedRunnerState::Suspect => WireRunnerStateTransitionState::Suspect,
                    DispatchedRunnerState::Connected => WireRunnerStateTransitionState::Connected,
                    DispatchedRunnerState::RunnerLostBeforePin => {
                        WireRunnerStateTransitionState::RunnerLostBeforePin
                    }
                    DispatchedRunnerState::RunnerLost => WireRunnerStateTransitionState::RunnerLost,
                    DispatchedRunnerState::Replaced => WireRunnerStateTransitionState::Replaced,
                    DispatchedRunnerState::WorkingDirectoryChanged => {
                        WireRunnerStateTransitionState::WorkingDirectoryChanged
                    }
                    DispatchedRunnerState::Abandoned => WireRunnerStateTransitionState::Abandoned,
                },
            },
            Self::ContextCompacted {
                compaction,
                call,
                through_position,
                summary_entry,
                result_frontier,
            } => SessionEvent::ContextCompacted {
                context_compaction_id: wire_uuid(compaction.into_uuid()),
                model_call_id: wire_uuid(call.into_uuid()),
                through_position: CanonicalU64::new(*through_position),
                summary_entry_id: wire_uuid(summary_entry.into_uuid()),
                result_frontier_id: wire_uuid(result_frontier.into_uuid()),
            },
            Self::TurnCompleted {
                turn,
                call,
                completion_entry,
                terminal_frontier,
            } => SessionEvent::TurnCompleted {
                turn_id: wire_uuid(turn.into_uuid()),
                model_call_id: wire_uuid(call.into_uuid()),
                completion_entry_id: wire_uuid(completion_entry.into_uuid()),
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            },
            Self::TurnFailed {
                turn,
                failure_entry,
                terminal_frontier,
            } => SessionEvent::TurnFailed {
                turn_id: wire_uuid(turn.into_uuid()),
                failure_entry_id: wire_uuid(failure_entry.into_uuid()),
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            },
            Self::TurnRefused {
                turn,
                call,
                terminal_frontier,
            } => SessionEvent::TurnRefused {
                turn_id: wire_uuid(turn.into_uuid()),
                model_call_id: wire_uuid(call.into_uuid()),
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            },
            Self::TurnCancelled {
                turn,
                cancellation_entry,
                terminal_frontier,
            } => SessionEvent::TurnCancelled {
                turn_id: wire_uuid(turn.into_uuid()),
                cancellation_entry_id: wire_uuid(cancellation_entry.into_uuid()),
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            },
            Self::TurnReconciliationRequired {
                turn,
                operation,
                terminal_frontier,
            } => match operation {
                DispatchedReconciliationOperation::ModelCall(call) => {
                    SessionEvent::TurnReconciliationRequired {
                        turn_id: wire_uuid(turn.into_uuid()),
                        model_call_id: wire_uuid(call.into_uuid()),
                        terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
                    }
                }
                DispatchedReconciliationOperation::ToolAttempt(attempt) => {
                    SessionEvent::TurnToolReconciliationRequired {
                        turn_id: wire_uuid(turn.into_uuid()),
                        tool_attempt_id: wire_uuid(attempt.into_uuid()),
                        terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
                    }
                }
            },
            Self::DelegationUpdate(update) => wire_delegation_update(update),
        };
        Ok(event)
    }
}

fn wire_delegation_update(update: &DispatchedDelegationUpdate) -> SessionEvent {
    match update {
        DispatchedDelegationUpdate::ChildSpawned {
            spawning_request,
            child,
            policy,
        } => SessionEvent::ChildSpawned {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            child_session_id: wire_uuid(child.into_uuid()),
            relationship: wire_delegation_policy(*policy),
        },
        DispatchedDelegationUpdate::ChildWaiting {
            spawning_request,
            child,
            awaiting_request,
            mode,
        } => SessionEvent::ChildWaiting {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            child_session_id: wire_uuid(child.into_uuid()),
            await_request_id: wire_uuid(awaiting_request.into_uuid()),
            mode: match mode {
                DispatchedDelegationWaitMode::Foreground => WireDelegationWaitMode::Foreground,
                DispatchedDelegationWaitMode::Background => WireDelegationWaitMode::Background,
            },
        },
        DispatchedDelegationUpdate::ChildLifecycleDisposition {
            spawning_request,
            child,
            event_ordinal: _,
            outcome,
            reason,
            provenance,
        } => SessionEvent::ChildLifecycleDisposition {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            child_session_id: wire_uuid(child.into_uuid()),
            outcome: wire_delegation_outcome(*outcome),
            reason: wire_delegation_reason(*reason),
            provenance: wire_delegation_provenance(*provenance),
        },
        DispatchedDelegationUpdate::ChildResult {
            spawning_request,
            child,
            outcome,
            reason,
            provenance,
            content,
        } => SessionEvent::ChildResult {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            child_session_id: wire_uuid(child.into_uuid()),
            outcome: wire_delegation_outcome(*outcome),
            reason: wire_delegation_reason(*reason),
            provenance: wire_delegation_provenance(*provenance),
            content: content.clone(),
        },
        DispatchedDelegationUpdate::SessionMessage {
            spawning_request,
            message,
            sender,
            recipient,
            message_ordinal,
            delivery_sequence,
            content,
        } => SessionEvent::SessionMessage {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            message_id: wire_uuid(message.into_uuid()),
            sender_session_id: wire_uuid(sender.into_uuid()),
            recipient_session_id: wire_uuid(recipient.into_uuid()),
            ordinal: CanonicalU64::new(*message_ordinal),
            delivery_sequence: CanonicalU64::new(*delivery_sequence),
            content: content.clone(),
        },
    }
}

const fn wire_delegation_policy(policy: DispatchedDelegationPolicy) -> WireDelegationPolicy {
    match policy {
        DispatchedDelegationPolicy::Background => WireDelegationPolicy::Background {},
        DispatchedDelegationPolicy::Bound {
            on_parent_stopped,
            on_parent_cancelled,
        } => WireDelegationPolicy::Bound {
            on_parent_stopped: wire_bound_child_action(on_parent_stopped),
            on_parent_cancelled: wire_bound_child_action(on_parent_cancelled),
        },
    }
}

const fn wire_bound_child_action(action: DispatchedBoundChildAction) -> WireBoundChildAction {
    match action {
        DispatchedBoundChildAction::KeepRunning => WireBoundChildAction::KeepRunning,
        DispatchedBoundChildAction::Stop => WireBoundChildAction::Stop,
        DispatchedBoundChildAction::Cancel => WireBoundChildAction::Cancel,
    }
}

const fn wire_delegation_outcome(outcome: DispatchedDelegationOutcome) -> WireDelegationOutcome {
    match outcome {
        DispatchedDelegationOutcome::ResultReturned => WireDelegationOutcome::Returned,
        DispatchedDelegationOutcome::ChildFailed => WireDelegationOutcome::Failed,
        DispatchedDelegationOutcome::ChildStopped => WireDelegationOutcome::Stopped,
        DispatchedDelegationOutcome::ChildCancelled => WireDelegationOutcome::Cancelled,
        DispatchedDelegationOutcome::ContinueRunning => WireDelegationOutcome::ContinueRunning,
        DispatchedDelegationOutcome::AlreadyTerminal => WireDelegationOutcome::AlreadyTerminal,
    }
}

const fn wire_delegation_reason(reason: DispatchedDelegationReason) -> WireDelegationReason {
    match reason {
        DispatchedDelegationReason::ChildCompleted => WireDelegationReason::ChildCompleted,
        DispatchedDelegationReason::ChildExecutionFailed => {
            WireDelegationReason::ChildExecutionFailed
        }
        DispatchedDelegationReason::ChildResultUnavailable => {
            WireDelegationReason::ChildResultUnavailable
        }
        DispatchedDelegationReason::ChildCancelled => WireDelegationReason::ChildCancelled,
        DispatchedDelegationReason::ParentStoppedWithDescendants => {
            WireDelegationReason::ParentStopped
        }
        DispatchedDelegationReason::ParentCancelledWithDescendants => {
            WireDelegationReason::ParentCancelled
        }
    }
}

fn wire_delegation_provenance(
    provenance: DispatchedDelegationProvenance,
) -> WireDelegationProvenance {
    match provenance {
        DispatchedDelegationProvenance::ChildTurn { session, turn } => {
            WireDelegationProvenance::ChildTurn {
                child_session_id: wire_uuid(session.into_uuid()),
                child_turn_id: wire_uuid(turn.into_uuid()),
            }
        }
        DispatchedDelegationProvenance::ParentTurnCommand {
            session,
            turn,
            command,
        } => WireDelegationProvenance::ParentTurnCommand {
            parent_session_id: wire_uuid(session.into_uuid()),
            parent_turn_id: wire_uuid(turn.into_uuid()),
            command_id: wire_uuid(command.into_uuid()),
            descendant_scope: WireDescendantTerminationScope::ParentAndDescendants,
        },
        DispatchedDelegationProvenance::ParentGoalCommand {
            session,
            goal_generation,
            command,
        } => WireDelegationProvenance::ParentGoalCommand {
            parent_session_id: wire_uuid(session.into_uuid()),
            goal_generation: CanonicalU64::new(goal_generation),
            command_id: wire_uuid(command.into_uuid()),
            descendant_scope: WireDescendantTerminationScope::ParentAndDescendants,
        },
        DispatchedDelegationProvenance::ParentLifecycleCommand { session, command } => {
            WireDelegationProvenance::ParentLifecycleCommand {
                parent_session_id: wire_uuid(session.into_uuid()),
                command_id: wire_uuid(command.into_uuid()),
                descendant_scope: WireDescendantTerminationScope::ParentAndDescendants,
            }
        }
    }
}

const fn wire_model_call_state(state: DispatchedModelCallState) -> ModelCallState {
    match state {
        DispatchedModelCallState::Prepared => ModelCallState::Prepared {},
        DispatchedModelCallState::InFlight => ModelCallState::InFlight {},
        DispatchedModelCallState::CancellationRequested => ModelCallState::CancellationRequested {},
        DispatchedModelCallState::Terminal(disposition) => ModelCallState::Terminal {
            disposition: match disposition {
                DispatchedModelCallDisposition::Completed => ModelCallDisposition::Completed,
                DispatchedModelCallDisposition::KnownFailed => ModelCallDisposition::KnownFailed,
                DispatchedModelCallDisposition::Refused => ModelCallDisposition::Refused,
                DispatchedModelCallDisposition::Cancelled => ModelCallDisposition::Cancelled,
                DispatchedModelCallDisposition::Ambiguous => ModelCallDisposition::Ambiguous,
            },
        },
    }
}

#[derive(Debug)]
enum ProcessConnectionError {
    PeerIo(io::Error),
    SpoolIo(io::Error),
    Encode(FrameEncodeError),
    EncodeInvariant,
    InboundFrameBudgetClosed,
    ImportBudgetClosed,
    ReviewCommandBudgetClosed,
    SnapshotReaderBudgetClosed,
}

impl From<io::Error> for ProcessConnectionError {
    fn from(error: io::Error) -> Self {
        Self::PeerIo(error)
    }
}

impl From<FrameEncodeError> for ProcessConnectionError {
    fn from(error: FrameEncodeError) -> Self {
        Self::Encode(error)
    }
}

impl fmt::Display for ProcessConnectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PeerIo(_) => "the local process peer I/O failed",
            Self::SpoolIo(_) => "the local process snapshot spool I/O failed",
            Self::Encode(_) => "the local process connection could not encode a frame",
            Self::EncodeInvariant => {
                "the local process connection could not represent an internal value"
            }
            Self::InboundFrameBudgetClosed => {
                "the local process connection lost its inbound frame budget"
            }
            Self::ImportBudgetClosed => {
                "the local process connection lost its conversation import budget"
            }
            Self::ReviewCommandBudgetClosed => {
                "the local process connection lost its review-command budget"
            }
            Self::SnapshotReaderBudgetClosed => {
                "the local process connection lost its snapshot reader budget"
            }
        })
    }
}

impl Error for ProcessConnectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PeerIo(error) | Self::SpoolIo(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::EncodeInvariant
            | Self::InboundFrameBudgetClosed
            | Self::ImportBudgetClosed
            | Self::ReviewCommandBudgetClosed
            | Self::SnapshotReaderBudgetClosed => None,
        }
    }
}

/// Fatal local-process runtime failure.
#[derive(Debug)]
pub enum ProcessRuntimeError {
    /// The guarded listener could not accept a connection.
    Accept(io::Error),
    /// A completed snapshot spool could not be read for transmission.
    SpoolIo(io::Error),
    /// A server frame could not satisfy the closed wire contract.
    Encode(FrameEncodeError),
    /// Runtime-owned values could not be represented by the closed wire contract.
    EncodeInvariant,
    /// The runtime-owned aggregate inbound frame budget closed unexpectedly.
    InboundFrameBudgetClosed,
    /// The runtime-owned conversation-import budget closed unexpectedly.
    ImportBudgetClosed,
    /// The runtime-owned review-command budget closed unexpectedly.
    ReviewCommandBudgetClosed,
    /// The runtime-owned snapshot-reader budget closed unexpectedly.
    SnapshotReaderBudgetClosed,
    /// The application pool cannot reserve capacity outside snapshot reads.
    InsufficientPoolCapacity,
    /// A connection task panicked or was cancelled unexpectedly.
    ConnectionTask(JoinError),
    /// The durable outbox dispatcher failed.
    Dispatch(OutboxDispatchError),
    /// The single dispatcher produced an impossible retry result.
    UnexpectedDispatcherRetry,
    /// The revalidated socket path could not be cleaned up.
    CleanupSocket(LocalSocketError),
}

impl fmt::Display for ProcessRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Accept(_) => "the local process listener failed",
            Self::SpoolIo(_) => "the local process server could not read a snapshot spool",
            Self::Encode(_) => "the local process server could not encode a frame",
            Self::EncodeInvariant => {
                "the local process server could not represent an internal value"
            }
            Self::InboundFrameBudgetClosed => {
                "the local process server lost its inbound frame budget"
            }
            Self::ImportBudgetClosed => {
                "the local process server lost its conversation import budget"
            }
            Self::ReviewCommandBudgetClosed => {
                "the local process server lost its review-command budget"
            }
            Self::SnapshotReaderBudgetClosed => {
                "the local process server lost its snapshot reader budget"
            }
            Self::InsufficientPoolCapacity => {
                "the local process server cannot reserve database pool capacity"
            }
            Self::ConnectionTask(_) => "a local process connection task failed",
            Self::Dispatch(_) => "the durable process-update dispatcher failed",
            Self::UnexpectedDispatcherRetry => {
                "the process-update dispatcher unexpectedly requested retry"
            }
            Self::CleanupSocket(_) => "the local process socket could not be cleaned up",
        })
    }
}

impl Error for ProcessRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Accept(error) => Some(error),
            Self::SpoolIo(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::ConnectionTask(error) => Some(error),
            Self::Dispatch(error) => Some(error),
            Self::CleanupSocket(error) => Some(error),
            Self::EncodeInvariant
            | Self::InboundFrameBudgetClosed
            | Self::ImportBudgetClosed
            | Self::ReviewCommandBudgetClosed
            | Self::SnapshotReaderBudgetClosed
            | Self::InsufficientPoolCapacity
            | Self::UnexpectedDispatcherRetry => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::{BTreeSet, VecDeque},
        error::Error,
        io::{self, Write},
        sync::{Arc, Mutex, OnceLock, mpsc},
        thread,
    };

    use signalbox_application::{
        EligibilityNudge, EligibilityNudgeOutcome, ImportConversationError,
        ImportedConversationConverter,
    };
    use signalbox_conversation_import_claude_code::ClaudeCodeJsonlConversionFailure;
    use signalbox_conversation_import_codex::CodexRolloutJsonlConversionFailure;
    use signalbox_domain::{
        AcceptedInputId, Actor, ContextFrontierId, DelegationMessageId, DirectModelSelection,
        DurableCommandId, FastModeOverlay, FastModeSupport, FrozenAliasDefinition,
        FrozenModelSelection, Goal, GoalStatement, GoalUserProvenance, ImportedConversation,
        ImportedConversationFormat, ImportedConversationId, ImportedTranscriptEntryId, ModelAlias,
        ModelCallId, ModelCapabilities, ModelChangeAdjustment, ModelSelectionRequest,
        ModelSettingsOverlay, ModelSettingsPrecedence, ReasoningLevel, ReviewPass,
        ReviewPassAcceptedInputEvidence, ReviewPassEvidence, ReviewPassId, ReviewPassKind,
        ReviewPassRef, ReviewPassState, ReviewPassTurnEvidence, ReviewPassTurnOutcome,
        ReviewPolicy, ReviewRun, ReviewRunId, ReviewRunRef, ReviewRunState, ReviewTargetId,
        ReviewWorkflowKind, RunnerGeneration, RunnerId, RunnerWorkingDirectory,
        SemanticTranscriptEntryId, SessionConfigurationDefaultsVersion, SessionId,
        SessionInputPosition, SessionLifecycleApplication, SessionLifecycleOperation,
        SessionLifecycleState, SessionMetadataLastWriter, SessionMetadataUpdatedAt,
        SessionModelSettingsChanged, SettingOverlay, SubmitInputRejectedResult,
        ToolApprovalDecision, ToolAttemptId, ToolRequestId, TurnAttemptId, TurnId,
        TurnModelSettingsResolved, ValidatedModelSettings,
    };
    use signalbox_process_protocol::{
        CanonicalU64, CanonicalUuid, ClientRequest, CommandId, ConversationImportRejectionClass,
        DelegationToolRequestState as WireDelegationToolRequestState, ErrorCode, ErrorDetail,
        FinishCondition as WireFinishCondition, FrameEncodeError, GoalLifecycleState,
        ImportedContentKind, ImportedSourceSpeaker, ImportedSpeaker, MAX_CONTENT_FRAGMENT_BYTES,
        MetadataActor, ProtocolVersion, RejectionDetail, ReviewFindingInput, ReviewSeverity,
        RunnerPlacementRevision as WireRunnerPlacementRevision,
        RunnerSandboxProfile as WireRunnerSandboxProfile,
        RunnerStateTransitionState as WireRunnerStateTransitionState,
        RunnerWorkingDirectory as WireRunnerWorkingDirectory, ServerFrame, ServerMessage,
        SessionEvent, ToolBatchState, ToolDecision, TranscriptEntry, TranscriptTextEntry,
        TurnState, UserInputContent, decode_server_line, encode_server_line,
    };
    use sqlx::postgres::PgPoolOptions;
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, duplex},
        net::UnixStream,
        sync::{Semaphore, watch},
        time::{Duration, Instant, timeout},
    };
    use uuid::Uuid;

    use super::{
        CommittedForegroundDelivery, ContextCompactionRangeLoadError, ConversationImportState,
        ConversionFailureDisposition, DispatchedTurnTerminalDisposition,
        GENERAL_BUFFERED_INBOUND_FRAMES, INBOUND_READ_AHEAD_BYTES, ImportedConversationRepository,
        ImportedConversationRepositoryError, ImportedRawBlobStorageError, InboundFrameBudgets,
        IncomingLine, InternalDiagnostic, MAX_ACTIVE_CONNECTIONS, MAX_BUFFERED_INBOUND_FRAMES,
        MAX_CONCURRENT_BLOB_READS, MAX_CONCURRENT_IMPORTS, MAX_CONCURRENT_REVIEW_COMMANDS,
        MAX_FRAME_BYTES, MAX_IMPORT_ADMISSION_WAITERS, OperationalImportError,
        PendingConversationImport, ProcessConnectionError, ProcessRuntimeError, ProcessUpdateEvent,
        ProtocolError, RESERVED_ACTIVE_IMPORT_INBOUND_FRAMES,
        RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS, RequestId, ReviewCommandAdmission,
        SnapshotReaderAdmission, SnapshotSpoolError, SubmitInputModelExecutionDiagnostic,
        acquire_import_permit, acquire_import_waiter_permit, acquire_inbound_frame_permit,
        acquire_inbound_frame_permit_after_input, acquire_review_command_permit,
        acquire_review_command_permit_while_buffered, acquire_snapshot_reader_permit,
        admit_snapshot_reader, admitted_user_content, blob_upload_begin_preflight,
        bounded_rendered_compaction_boundary, canonical_review_request_digest,
        claude_conversion_failure_disposition, codex_conversion_failure_disposition,
        consume_snapshot_queued_update, context_compaction_failure_disposition,
        domain_finish_condition, execute_import, foreground_peer_activity,
        handle_append_conversation_import, handle_begin_conversation_import,
        handle_commit_conversation_import, import_evidence,
        imported_conversation_internal_diagnostic, inspect_connection_completion,
        internal_protocol_error, lifecycle_command_needs_eligibility_nudge, map_rejection,
        nudge_after_process_await_rejection, nudge_after_process_message_rejection,
        nudge_delegation_issuer, nudge_delegation_wake, observe_outbox_metrics_once,
        operational_import_error, preserve_committed_foreground_wait, process_delegation_rejection,
        process_delegation_rejection_for_recipient, read_frame_line,
        retain_inbound_frame_permit_during_import_admission,
        retry_context_compaction_range_database_reads, run_until_shutdown,
        snapshot_reader_capacity, spool_error_display, spool_goal_snapshot,
        submit_input_model_execution_diagnostic, try_acquire_blob_read_permit,
        unavailable_protocol_error, wait_for_connection_loss, wire_goal_event,
        wire_metadata_last_writer, wire_model_call_state, wire_tool_decision, wire_turn_state,
        wire_uuid, write_content, write_context_compaction_repository_error,
        write_delegation_port_error, write_snapshot_spool_error, write_transcript_entry,
    };

    macro_rules! assert_import_failure_ordinal {
        ($mapping:path, $ordinal:literal, $failure:expr, $class:path) => {{
            let ordinal = $ordinal;
            assert_eq!(
                $mapping(($failure)(ordinal)),
                ConversionFailureDisposition::Rejected(import_evidence($class, Some(ordinal)))
            );
        }};
    }

    macro_rules! assert_simple_import_failures {
        (
            $mapping:path,
            $failure_type:ident;
            $( $ordinal:literal => $failure:ident => $class:path ),+ $(,)?
        ) => {
            $(
                assert_import_failure_ordinal!(
                    $mapping,
                    $ordinal,
                    |line| $failure_type::$failure { line },
                    $class
                );
            )+
        };
    }

    #[test]
    fn a_resumed_session_requests_an_eligibility_pass() {
        assert!(lifecycle_command_needs_eligibility_nudge(
            &SessionLifecycleApplication::Resumed {
                state: SessionLifecycleState::Created,
            },
            &SessionLifecycleOperation::Resume,
        ));
    }

    impl super::ClassifyConversationImportError for io::Error {
        fn disposition(self) -> super::ConversionFailureDisposition {
            super::ConversionFailureDisposition::Rejected(super::import_evidence(
                signalbox_process_protocol::ConversationImportRejectionClass::InvalidJson,
                None,
            ))
        }
    }

    use crate::{FatalExecutionSupervisor, TelemetryMetrics};
    use signalbox_model_provider_runtime::ContextCompactionModelError;
    use signalbox_persistence::{
        context_compaction::{
            ContextCompactionRepositoryError, FailedContextCompactionDisposition,
        },
        conversation_import::{
            ImportedConversationCorruption, ImportedConversationIdentityCollision,
        },
        model_execution::{
            ModelCallCorruption, ModelCallIdentityCollision, ModelCallRepositoryError,
        },
        outbox::{
            DispatchedModelCallDisposition, DispatchedModelCallState, DispatchedOutboxEventKind,
            DispatchedReconciliationOperation, DispatchedRunnerState, DispatchedToolBatchState,
        },
        process_read::{
            ProcessImportedContentKind, ProcessImportedSourceSpeaker, ProcessReadError,
            ProcessReconciliationOperation, ProcessTranscriptEntry, ProcessTurnState,
        },
        session_delegation::{
            DelegationOperationRejection, DelegationRequestExecutionState,
            ProcessDelegationRequestRejection,
        },
    };

    #[derive(Clone, Debug, Default)]
    struct RecordingEligibilityNudge {
        sessions: Arc<Mutex<Vec<SessionId>>>,
    }

    impl EligibilityNudge for RecordingEligibilityNudge {
        fn nudge(&self, session: SessionId) -> EligibilityNudgeOutcome {
            self.sessions
                .lock()
                .expect("recording nudge lock remains available")
                .push(session);
            EligibilityNudgeOutcome::Enqueued
        }
    }

    #[test]
    fn s19_descendant_scope_decode_is_exact() {
        assert_eq!(
            super::decode_descendant_scope(
                signalbox_process_protocol::DescendantTerminationScope::ParentAlone,
            ),
            signalbox_domain::DescendantTerminationScope::ParentAlone
        );
        assert_eq!(
            super::decode_descendant_scope(
                signalbox_process_protocol::DescendantTerminationScope::ParentAndDescendants,
            ),
            signalbox_domain::DescendantTerminationScope::ParentAndDescendants
        );
    }
    use signalbox_process_protocol::{ModelCallDisposition, ModelCallState};

    #[test]
    fn durable_metric_mapping_ignores_content_and_uses_only_closed_labels() {
        let metrics = TelemetryMetrics::new().expect("static metric descriptors are valid");
        let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(1));
        let turn = TurnId::from_uuid(Uuid::from_u128(2));
        let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(3));
        let call = ModelCallId::from_uuid(Uuid::from_u128(4));
        let input = DispatchedOutboxEventKind::InputAccepted {
            accepted_input,
            turn,
            acceptance_position: SessionInputPosition::first(),
            content: signalbox_domain::UserContent::try_text(
                "synthetic prompt with tool arguments".to_owned(),
            )
            .expect("the telemetry fixture content is valid"),
        };
        let activation = DispatchedOutboxEventKind::TurnActivated {
            turn,
            current_attempt: attempt,
        };
        let terminal_call = DispatchedOutboxEventKind::ModelCallTransition {
            turn,
            call,
            state: DispatchedModelCallState::Terminal(DispatchedModelCallDisposition::Ambiguous),
        };
        let mut last_sequence = None;

        observe_outbox_metrics_once(Some(&metrics), &mut last_sequence, 1, &input);
        observe_outbox_metrics_once(Some(&metrics), &mut last_sequence, 2, &activation);
        observe_outbox_metrics_once(Some(&metrics), &mut last_sequence, 2, &activation);
        observe_outbox_metrics_once(Some(&metrics), &mut last_sequence, 3, &terminal_call);
        let rendered = metrics.render().expect("static registry encodes");

        assert!(rendered.contains("signalbox_turns_started_total 1"));
        assert!(rendered.contains("disposition=\"ambiguous\""));
        assert!(!rendered.contains("synthetic prompt with tool arguments"));
        assert!(!rendered.contains(&accepted_input.into_uuid().to_string()));
        assert!(!rendered.contains(&turn.into_uuid().to_string()));
        assert!(!rendered.contains(&attempt.into_uuid().to_string()));
        assert!(!rendered.contains(&call.into_uuid().to_string()));
    }
    struct PendingResponseWriter;

    thread_local! {
        /// Telemetry captured on this thread alone.
        static CAPTURED_TELEMETRY: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    /// Appends every formatted event to the emitting thread's own buffer.
    #[derive(Clone, Copy, Default)]
    struct CapturedTelemetry;

    impl Write for CapturedTelemetry {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            CAPTURED_TELEMETRY.with(|captured| captured.borrow_mut().extend_from_slice(buffer));
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CapturedTelemetry {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            *self
        }
    }

    /// Records the telemetry `record` emits on this thread.
    ///
    /// The subscriber is installed once for the whole test process rather than
    /// scoped to this thread. `tracing` caches each callsite's interest
    /// process-wide, but `set_default` binds a subscriber to one thread, so a
    /// sibling test that reaches a callsite first on another thread registers
    /// it against no subscriber at all -- recording it as uninteresting for
    /// every thread, including the one that installed a capture. The event then
    /// is not merely written late; it is never emitted, and the assertion reads
    /// an empty buffer.
    ///
    /// Writes are routed per thread so concurrent tests never read each other's
    /// events, which keeps assertions on both presence and absence honest.
    fn capture_telemetry(record: impl FnOnce()) -> String {
        static INSTALLED: OnceLock<()> = OnceLock::new();

        INSTALLED.get_or_init(|| {
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(CapturedTelemetry)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other global telemetry subscriber is installed");
        });
        CAPTURED_TELEMETRY.with(|captured| captured.borrow_mut().clear());
        record();
        CAPTURED_TELEMETRY
            .with(|captured| String::from_utf8(captured.borrow().clone()))
            .expect("captured telemetry is UTF-8")
    }

    fn capture_internal_diagnostic(session_id: Uuid, diagnostic: InternalDiagnostic) -> String {
        capture_telemetry(|| {
            let _ = internal_protocol_error(Some(session_id), diagnostic);
        })
    }

    fn capture_submit_input_model_execution_diagnostic(
        session_id: Uuid,
        error: &ModelCallRepositoryError,
    ) -> String {
        let diagnostic = submit_input_model_execution_diagnostic(error);
        capture_telemetry(|| {
            let _ = diagnostic.into_protocol_error(CanonicalUuid::from_uuid(session_id));
        })
    }

    #[test]
    fn internal_diagnostic_uses_canonical_session_and_typed_labels() {
        let session_id = Uuid::from_u128(1);
        let diagnostic = InternalDiagnostic::SessionMetadataCorruption;
        let encoded = capture_internal_diagnostic(session_id, diagnostic);

        assert!(encoded.contains(&format!("session_id={session_id}")));
        assert!(encoded.contains("failure_class=FailClosedCorruption"));
        assert!(encoded.contains(r#"cause_code="session_metadata_corruption""#));
        assert!(!encoded.contains("Some("));
    }

    #[test]
    fn internal_diagnostic_preserves_distinct_integrity_causes() {
        assert_eq!(
            InternalDiagnostic::ContextCompactionIdentityCollision.cause_code(),
            "context_compaction_repository_identity_collision"
        );
        assert_eq!(
            InternalDiagnostic::ContextCompactionRepositoryCorruption.cause_code(),
            "context_compaction_repository_corruption"
        );
        assert_eq!(
            InternalDiagnostic::ContextCompactionUnconfiguredTarget.cause_code(),
            "context_compaction_unconfigured_target"
        );
        assert_eq!(
            InternalDiagnostic::SessionModelCredentialMissing.cause_code(),
            "session_model_credential_missing"
        );
        assert_eq!(
            InternalDiagnostic::ToolLoopIdentityCollision.cause_code(),
            "tool_loop_identity_collision"
        );
        assert_eq!(
            InternalDiagnostic::ToolLoopCorruption.cause_code(),
            "tool_loop_corruption"
        );
        assert_eq!(
            InternalDiagnostic::ToolLoopInvalidTransition.cause_code(),
            "tool_loop_invalid_transition"
        );
        assert_eq!(
            InternalDiagnostic::SubmitInputModelExecutionIdentityCollision.cause_code(),
            "submit_input_model_execution_identity_collision"
        );
        assert_eq!(
            InternalDiagnostic::SubmitInputModelExecutionCorruption.cause_code(),
            "submit_input_model_execution_corruption"
        );
        assert_eq!(
            InternalDiagnostic::SubmitInputModelExecutionNoLiveExecution.cause_code(),
            "submit_input_model_execution_no_live_execution"
        );
        assert_eq!(
            InternalDiagnostic::SubmitInputModelExecutionInvalidTransition.cause_code(),
            "submit_input_model_execution_invalid_transition"
        );
    }

    #[test]
    fn submit_input_model_execution_identity_collision_keeps_its_diagnostic() {
        let error =
            ModelCallRepositoryError::IdentityCollision(ModelCallIdentityCollision::ModelCall);

        assert_eq!(
            submit_input_model_execution_diagnostic(&error),
            SubmitInputModelExecutionDiagnostic::Internal(
                InternalDiagnostic::SubmitInputModelExecutionIdentityCollision
            )
        );
    }

    #[test]
    fn submit_input_model_execution_corruption_keeps_its_diagnostic() {
        let error = ModelCallRepositoryError::Corruption(ModelCallCorruption::Missing(
            "synthetic model-call row",
        ));

        assert_eq!(
            submit_input_model_execution_diagnostic(&error),
            SubmitInputModelExecutionDiagnostic::Internal(
                InternalDiagnostic::SubmitInputModelExecutionCorruption
            )
        );
    }

    #[test]
    fn submit_input_model_execution_no_live_execution_keeps_its_diagnostic() {
        let error = ModelCallRepositoryError::NoLiveExecution;

        assert_eq!(
            submit_input_model_execution_diagnostic(&error),
            SubmitInputModelExecutionDiagnostic::Internal(
                InternalDiagnostic::SubmitInputModelExecutionNoLiveExecution
            )
        );
    }

    #[test]
    fn submit_input_model_execution_invalid_transition_keeps_its_diagnostic() {
        let error = ModelCallRepositoryError::InvalidTransition("synthetic transition");

        assert_eq!(
            submit_input_model_execution_diagnostic(&error),
            SubmitInputModelExecutionDiagnostic::Internal(
                InternalDiagnostic::SubmitInputModelExecutionInvalidTransition
            )
        );
    }

    #[test]
    fn submit_input_model_execution_diagnostic_omits_dynamic_source_detail() {
        let dynamic_detail = "synthetic-credential-prompt-and-provider-prose";
        let session_id = Uuid::from_u128(1);
        let error = ModelCallRepositoryError::InvalidTransition(dynamic_detail);
        let encoded = capture_submit_input_model_execution_diagnostic(session_id, &error);

        assert!(encoded.contains(&format!("session_id={session_id}")));
        assert!(encoded.contains("failure_class=CallerOrHubBug"));
        assert!(
            encoded.contains(r#"cause_code="submit_input_model_execution_invalid_transition""#)
        );
        assert!(!encoded.contains(dynamic_detail));
    }

    #[test]
    fn imported_conversation_identity_collision_keeps_its_diagnostic() {
        let error = ImportedConversationRepositoryError::IdentityCollision(
            ImportedConversationIdentityCollision::Conversation,
        );

        assert_eq!(
            imported_conversation_internal_diagnostic(&error),
            InternalDiagnostic::ImportedConversationIdentityCollision
        );
    }

    #[test]
    fn compaction_terminal_evidence_keeps_its_exact_disposition() {
        assert_eq!(
            context_compaction_failure_disposition(ContextCompactionModelError::Refused),
            FailedContextCompactionDisposition::Refused
        );
        assert_eq!(
            context_compaction_failure_disposition(
                ContextCompactionModelError::CancellationConfirmed
            ),
            FailedContextCompactionDisposition::Cancelled
        );
        assert_eq!(
            context_compaction_failure_disposition(ContextCompactionModelError::ProviderError),
            FailedContextCompactionDisposition::KnownFailed
        );
        assert_eq!(
            context_compaction_failure_disposition(ContextCompactionModelError::ProvenUnsent),
            FailedContextCompactionDisposition::KnownFailed
        );
        assert_eq!(
            context_compaction_failure_disposition(ContextCompactionModelError::BoundaryLoss),
            FailedContextCompactionDisposition::Ambiguous
        );
    }

    #[test]
    fn automatic_compaction_boundary_counts_the_rendered_json_envelope() {
        let first = serde_json::json!({
            "position": 1,
            "type": "user",
            "content": "x".repeat(90),
        });
        let second = serde_json::json!({
            "position": 2,
            "type": "assistant",
            "content": "y".repeat(90),
        });
        let first_bytes = u64::try_from(
            serde_json::to_vec(&first)
                .expect("the fixture JSON is serializable")
                .len(),
        )
        .expect("the fixture length fits u64");
        let second_bytes = u64::try_from(
            serde_json::to_vec(&second)
                .expect("the fixture JSON is serializable")
                .len(),
        )
        .expect("the fixture length fits u64");
        let first_array_bytes = first_bytes + 2;

        assert_eq!(
            bounded_rendered_compaction_boundary(
                &[first_bytes, second_bytes],
                &[(11, true), (12, true)],
                first_array_bytes,
            ),
            Some(11)
        );
    }

    #[test]
    fn automatic_compaction_boundary_never_crosses_the_model_budget_for_a_tool_exchange() {
        assert_eq!(
            bounded_rendered_compaction_boundary(
                &[60, 100, 100],
                &[(21, true), (22, false), (23, true)],
                170,
            ),
            Some(21)
        );
    }

    #[test]
    fn successor_compaction_rejects_an_unreachable_later_safe_boundary() {
        assert!(super::successor_compaction_cannot_advance(
            &[10, 100, 100],
            &[(31, true), (32, false), (33, true)],
            203,
        ));
        assert!(!super::successor_compaction_cannot_advance(
            &[10, 100, 100],
            &[(31, true), (32, false), (33, true)],
            204,
        ));
    }

    #[test]
    fn snapshot_delta_boundary_consumes_only_the_queued_prefix() {
        let mut queued = 2;

        assert!(consume_snapshot_queued_update(&mut queued));
        assert!(consume_snapshot_queued_update(&mut queued));
        assert!(!consume_snapshot_queued_update(&mut queued));
    }

    #[tokio::test]
    async fn automatic_compaction_range_read_retries_transient_database_failure() {
        let expected_range = String::from("rendered compaction range");
        let transient = ProcessReadError::Database(sqlx::Error::Io(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "synthetic transient range read",
        )));
        let mut outcomes = VecDeque::from([
            Err(ContextCompactionRangeLoadError::Read(transient)),
            Ok(expected_range.clone()),
        ]);

        let loaded = retry_context_compaction_range_database_reads(|| {
            std::future::ready(
                outcomes
                    .pop_front()
                    .expect("the fixture supplies one retry and one success"),
            )
        })
        .await
        .expect("a transient database read is retried");

        assert_eq!(loaded, expected_range);
        assert!(outcomes.is_empty());
    }

    impl tokio::io::AsyncWrite for PendingResponseWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
            _buffer: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            std::task::Poll::Pending
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn review_finding_input(identity: u128) -> ReviewFindingInput {
        ReviewFindingInput {
            finding_id: CanonicalUuid::from_uuid(Uuid::from_u128(identity)),
            file_path: String::from("src/lib.rs"),
            line_start: None,
            line_end: None,
            diff_side: None,
            title: String::from("Canonical finding"),
            body: String::from("Finding order does not change command meaning."),
            severity: ReviewSeverity::High,
            is_real_confidence: CanonicalU64::new(9_000),
            severity_label_confidence: CanonicalU64::new(8_500),
            category: String::from("correctness"),
            recommended_fix: None,
        }
    }

    #[test]
    fn review_findings_digest_uses_canonical_identity_order() -> Result<(), Box<dyn Error>> {
        let command_id = CommandId::try_from_uuid(Uuid::from_u128(1))?;
        let run_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let pass_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let output_frontier_id = CanonicalUuid::from_uuid(Uuid::from_u128(5));
        let first = review_finding_input(6);
        let second = review_finding_input(7);
        let mut ordered = ClientRequest::RecordReviewFindings {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            findings: vec![first.clone(), second.clone()],
        };
        let mut reversed = ClientRequest::RecordReviewFindings {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            findings: vec![second, first],
        };

        assert_eq!(
            canonical_review_request_digest(&mut ordered),
            canonical_review_request_digest(&mut reversed)
        );
        assert_eq!(ordered, reversed);
        Ok(())
    }

    /// INV-033: every stop refusal the interrupt treatment records reaches the
    /// wire as its recorded typed rejection, not as an encode invariant that
    /// closes the connection; the racing-target projections are covered by the
    /// reconciliation test below.
    #[test]
    fn inv033_stop_rejections_have_wire_projections() -> Result<(), Box<dyn Error>> {
        let session = SessionId::from_uuid(Uuid::from_u128(1));
        let actual_active_turn = TurnId::from_uuid(Uuid::from_u128(3));
        let existing_command = DurableCommandId::from_uuid(Uuid::from_u128(4));

        assert_eq!(
            map_rejection(SubmitInputRejectedResult::InterruptAlreadyApplied {
                session,
                active_turn: actual_active_turn,
                existing_command,
            })?,
            RejectionDetail::InterruptAlreadyApplied {
                session_id: wire_uuid(session.into_uuid()),
                active_turn_id: wire_uuid(actual_active_turn.into_uuid()),
                existing_command_id: wire_uuid(*existing_command.as_uuid()),
            }
        );
        assert_eq!(
            map_rejection(
                SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                    session,
                    active_turn: actual_active_turn,
                }
            )?,
            RejectionDetail::InterruptUnavailableWhileAwaitingApproval {
                session_id: wire_uuid(session.into_uuid()),
                active_turn_id: wire_uuid(actual_active_turn.into_uuid()),
            }
        );
        assert_eq!(
            map_rejection(
                SubmitInputRejectedResult::SafePointUnavailableWhileStopping {
                    session,
                    active_turn: actual_active_turn,
                    existing_command,
                }
            )?,
            RejectionDetail::SafePointUnavailableWhileStopping {
                session_id: wire_uuid(session.into_uuid()),
                active_turn_id: wire_uuid(actual_active_turn.into_uuid()),
                existing_command_id: wire_uuid(*existing_command.as_uuid()),
            }
        );
        Ok(())
    }

    /// INV-033: the receipt projection is exact — the wire
    /// surface records only reason-bearing denials, so a reason-free denial
    /// fails closed instead of fabricating an empty reason.
    #[test]
    fn inv033_reason_free_denial_has_no_wire_receipt() -> Result<(), Box<dyn Error>> {
        assert_eq!(
            wire_tool_decision(&ToolApprovalDecision::Approve)?,
            ToolDecision::Approve {}
        );
        assert_eq!(
            wire_tool_decision(&ToolApprovalDecision::Deny {
                reason: Some(
                    signalbox_domain::ToolDenialReason::try_new(String::from(
                        "writes outside the workspace"
                    ))
                    .map_err(|error| io::Error::other(format!("{error:?}")))?
                ),
            })?,
            ToolDecision::Deny {
                reason: String::from("writes outside the workspace"),
            }
        );
        assert!(matches!(
            wire_tool_decision(&ToolApprovalDecision::Deny { reason: None }),
            Err(ProcessConnectionError::EncodeInvariant)
        ));
        Ok(())
    }

    /// The post-lock statement time every metadata projection fixture carries.
    /// No projection depends on the value, only on its passing through.
    const METADATA_WRITE_UNIX_MICROS: u64 = 17;

    /// Projects one domain agency and pins both members against the fixture it
    /// came from. A failure names the agency at the call site.
    #[track_caller]
    fn assert_metadata_last_writer_projects(actor: Actor, expected_actor: MetadataActor) {
        let writer = SessionMetadataLastWriter::new(
            SessionMetadataUpdatedAt::from_unix_micros(METADATA_WRITE_UNIX_MICROS),
            actor,
        );
        let projected = wire_metadata_last_writer(writer);
        assert_eq!(projected.actor(), expected_actor);
        assert_eq!(
            projected.updated_at_unix_micros().value(),
            writer.updated_at().as_unix_micros()
        );
    }

    /// INV-033: the metadata last-writer projection is total over the domain
    /// agencies durable metadata records, and each carried reference lands in
    /// its own member. A projection gap here is not a degraded field: both
    /// callers propagate it as an encode invariant, which is fatal to the
    /// daemon and re-fires on every read of the durable row.
    #[test]
    fn inv033_metadata_last_writer_projects_every_domain_agency() {
        let turn = TurnId::from_uuid(Uuid::from_u128(2));
        let request = ToolRequestId::from_uuid(Uuid::from_u128(3));

        assert_metadata_last_writer_projects(Actor::User, MetadataActor::User {});
        assert_metadata_last_writer_projects(Actor::Core, MetadataActor::Core {});
        assert_metadata_last_writer_projects(
            Actor::Model { turn },
            MetadataActor::Model {
                turn_id: wire_uuid(turn.into_uuid()),
            },
        );
        assert_metadata_last_writer_projects(Actor::Recovery, MetadataActor::Recovery {});
        assert_metadata_last_writer_projects(
            Actor::Tool { request },
            MetadataActor::Tool {
                tool_request_id: wire_uuid(request.into_uuid()),
            },
        );
    }

    fn compaction_session() -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(1))
    }

    /// S03 / INV-034: an explicit compaction whose commit outcome cannot be
    /// decided raises the same fatal recovery signal its automatic sibling
    /// raises through the scheduler pass, and still answers the client with the
    /// stable ambiguous code.
    ///
    /// Without the report the connection handler has nowhere left to go: it
    /// holds no `PreparedContextCompaction` to terminalize, replay of the same
    /// command finds it pending, a fresh command finds the nonterminal call,
    /// and the startup scan that does reconcile this state only runs in the
    /// next incarnation.
    #[tokio::test]
    async fn s03_inv034_ambiguous_explicit_compaction_commit_raises_the_fatal_recovery_signal()
    -> Result<(), Box<dyn Error>> {
        let (supervisor, signal) = FatalExecutionSupervisor::new(());
        let reporter = supervisor.recovery_reporter();
        let (mut writer, mut reader) = duplex(1_024);

        write_context_compaction_repository_error(
            &mut writer,
            ProtocolVersion::One,
            RequestId::try_new(11)?,
            compaction_session(),
            Some(&reporter),
            ContextCompactionRepositoryError::CommitAmbiguous(sqlx::Error::PoolClosed),
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;

        assert!(signal.is_triggered());
        assert!(matches!(
            decode_server_line(&encoded)?.message(),
            ServerMessage::Error {
                code: ErrorCode::CommitAmbiguous,
                ..
            }
        ));
        Ok(())
    }

    /// S03 / INV-034: a failure proven to precede the commit boundary is
    /// ordinary unavailability and raises no recovery signal, so the reaction
    /// stays scoped to the one declared class that needs it.
    #[tokio::test]
    async fn s03_inv034_decided_explicit_compaction_failure_raises_no_recovery_signal()
    -> Result<(), Box<dyn Error>> {
        let (supervisor, signal) = FatalExecutionSupervisor::new(());
        let reporter = supervisor.recovery_reporter();
        let (mut writer, mut reader) = duplex(1_024);

        write_context_compaction_repository_error(
            &mut writer,
            ProtocolVersion::One,
            RequestId::try_new(12)?,
            compaction_session(),
            Some(&reporter),
            ContextCompactionRepositoryError::Database(sqlx::Error::PoolClosed),
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;

        assert!(!signal.is_triggered());
        assert!(matches!(
            decode_server_line(&encoded)?.message(),
            ServerMessage::Error {
                code: ErrorCode::Unavailable,
                ..
            }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn delegation_commit_ambiguity_uses_the_mutation_recovery_code()
    -> Result<(), Box<dyn Error>> {
        let (mut writer, mut reader) = duplex(1_024);

        write_delegation_port_error(
            &mut writer,
            ProtocolVersion::One,
            RequestId::try_new(13)?,
            CanonicalUuid::from_uuid(Uuid::from_u128(14)),
            crate::session_delegation::PostgresSessionDelegationPortError::Repository(
                signalbox_persistence::session_delegation::SessionDelegationRepositoryError::CommitAmbiguous(
                    sqlx::Error::PoolClosed,
                ),
            ),
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;

        assert!(matches!(
            decode_server_line(&encoded)?.message(),
            ServerMessage::Error {
                code: ErrorCode::CommitAmbiguous,
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn commit_ambiguity_selects_the_stable_process_error_code() {
        assert_eq!(
            ProtocolError::mutation_definitely_unavailable().code,
            ErrorCode::Unavailable
        );
        assert_eq!(
            ProtocolError::mutation_commit_ambiguous().code,
            ErrorCode::CommitAmbiguous
        );
        assert!(
            ProtocolError::without_detail(ErrorCode::UnsupportedVersion)
                .message
                .contains("supported version: 1")
        );
    }

    /// INV-060: direct blob-read admission exposes one fixed non-waiting
    /// process-wide capacity.
    #[test]
    fn inv060_blob_read_admission_has_fixed_nonwaiting_capacity() -> Result<(), Box<dyn Error>> {
        let budget = Arc::new(Semaphore::new(MAX_CONCURRENT_BLOB_READS));
        let held = Arc::clone(&budget)
            .try_acquire_many_owned(u32::try_from(MAX_CONCURRENT_BLOB_READS)?)
            .map_err(io::Error::other)?;

        assert_eq!(MAX_CONCURRENT_BLOB_READS, 16);
        assert!(try_acquire_blob_read_permit(Arc::clone(&budget)).is_none());
        drop(held);
        assert_eq!(budget.available_permits(), MAX_CONCURRENT_BLOB_READS);
        Ok(())
    }

    #[tokio::test]
    async fn blob_read_disconnect_detection_survives_pipelined_input() -> Result<(), Box<dyn Error>>
    {
        let (mut client, server) = UnixStream::pair()?;
        let (reader, _writer) = server.into_split();
        let mut reader = BufReader::new(reader);
        client.write_all(b"pipelined request").await?;
        assert_eq!(reader.fill_buf().await?, b"pipelined request");
        drop(client);

        timeout(Duration::from_secs(1), wait_for_connection_loss(&reader)).await?;
        assert_eq!(reader.buffer(), b"pipelined request");
        Ok(())
    }

    /// INV-033: a reconciliation decision that lost its race to another
    /// decision reaches the wire as its recorded typed rejection, not as an
    /// encode invariant that closes the connection.
    #[test]
    fn inv033_racing_reconciliation_rejections_have_wire_projections() -> Result<(), Box<dyn Error>>
    {
        let session = SessionId::from_uuid(uuid::Uuid::from_u128(1));
        let expected_active_turn = TurnId::from_uuid(uuid::Uuid::from_u128(2));
        let actual_active_turn = TurnId::from_uuid(uuid::Uuid::from_u128(3));

        assert_eq!(
            map_rejection(SubmitInputRejectedResult::NoActiveTurn {
                session,
                expected_active_turn,
            })?,
            RejectionDetail::NoActiveTurn {
                session_id: wire_uuid(session.into_uuid()),
                expected_active_turn_id: wire_uuid(expected_active_turn.into_uuid()),
            }
        );
        assert_eq!(
            map_rejection(SubmitInputRejectedResult::ActiveTurnMismatch {
                session,
                expected_active_turn,
                actual_active_turn,
            })?,
            RejectionDetail::ActiveTurnMismatch {
                session_id: wire_uuid(session.into_uuid()),
                expected_active_turn_id: wire_uuid(expected_active_turn.into_uuid()),
                active_turn_id: wire_uuid(actual_active_turn.into_uuid()),
            }
        );
        Ok(())
    }

    #[track_caller]
    fn complete_frame(line: Option<IncomingLine>) -> Vec<u8> {
        let Some(IncomingLine::Complete(line)) = line else {
            panic!("fixture expected one complete frame");
        };
        line
    }

    #[track_caller]
    fn oversized_frame_identity(
        line: Option<IncomingLine>,
    ) -> (RequestId, Option<ProtocolVersion>) {
        let Some(IncomingLine::Oversized {
            request_id,
            admitted_version,
        }) = line
        else {
            panic!("fixture expected one oversized frame");
        };
        (request_id, admitted_version)
    }

    #[tokio::test]
    async fn inv033_frame_reader_accepts_the_exact_cap_and_rejects_the_next_byte()
    -> Result<(), Box<dyn Error>> {
        let mut exact = vec![b'x'; MAX_FRAME_BYTES];
        exact[MAX_FRAME_BYTES - 1] = b'\n';
        let mut exact_reader = BufReader::new(exact.as_slice());
        let line = complete_frame(read_frame_line(&mut exact_reader).await?);
        assert_eq!(line.len(), MAX_FRAME_BYTES);

        let mut oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
        oversized[MAX_FRAME_BYTES] = b'\n';
        let mut oversized_reader = BufReader::new(oversized.as_slice());
        let (request_id, admitted_version) =
            oversized_frame_identity(read_frame_line(&mut oversized_reader).await?);
        assert_eq!(request_id.value(), 0);
        assert_eq!(admitted_version, None);

        let correlated_request_id = 9;
        let request_members = format!(r#""request_id":"{correlated_request_id}""#);
        let mut correlated = format!(
            r#"{{"version":1,{request_members},"request":{{"type":"list_sessions","padding":""#
        )
        .into_bytes();
        let suffix = b"\"}}";
        correlated.resize(MAX_FRAME_BYTES - suffix.len(), b'x');
        correlated.extend_from_slice(suffix);
        correlated.push(b'\n');
        let mut correlated_reader = BufReader::new(correlated.as_slice());
        let (request_id, admitted_version) =
            oversized_frame_identity(read_frame_line(&mut correlated_reader).await?);
        assert_eq!(request_id.value(), correlated_request_id);
        assert_eq!(admitted_version, Some(ProtocolVersion::One));
        Ok(())
    }

    #[tokio::test]
    async fn inbound_frame_budget_bounds_raw_accumulation_and_waits_for_shutdown()
    -> Result<(), Box<dyn Error>> {
        assert_eq!(
            MAX_BUFFERED_INBOUND_FRAMES * MAX_FRAME_BYTES,
            64 * 1024 * 1024
        );
        let budget = Arc::new(Semaphore::new(MAX_BUFFERED_INBOUND_FRAMES));
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let mut permits = Vec::new();
        for _ in 0..MAX_BUFFERED_INBOUND_FRAMES {
            permits.push(
                acquire_inbound_frame_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
                    .await?
                    .ok_or_else(|| io::Error::other("the running fixture must acquire a permit"))?,
            );
        }

        assert!(
            timeout(
                Duration::from_millis(20),
                acquire_inbound_frame_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
            )
            .await
            .is_err(),
            "the ninth frame accumulator must wait"
        );

        drop(permits.pop());
        let released = timeout(
            Duration::from_secs(1),
            acquire_inbound_frame_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
        )
        .await??
        .ok_or_else(|| io::Error::other("a released frame slot must be acquired"))?;
        permits.push(released);

        shutdown.send(true)?;
        assert!(
            acquire_inbound_frame_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
                .await?
                .is_none(),
            "a connection waiting for the full budget must stop on shutdown"
        );
        Ok(())
    }

    #[tokio::test]
    async fn idle_reader_does_not_reserve_an_inbound_frame_slot() -> Result<(), Box<dyn Error>> {
        assert_eq!(
            MAX_ACTIVE_CONNECTIONS * INBOUND_READ_AHEAD_BYTES,
            1024 * 1024
        );
        let budget = Arc::new(Semaphore::new(1));
        let (mut client, server) = duplex(8);
        let mut reader = BufReader::new(server);
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let acquire = acquire_inbound_frame_permit_after_input(
            &mut reader,
            Arc::clone(&budget),
            &mut shutdown_receiver,
        );
        tokio::pin!(acquire);

        assert!(
            timeout(Duration::from_millis(20), &mut acquire)
                .await
                .is_err()
        );
        assert_eq!(budget.available_permits(), 1);

        client.write_all(b"{").await?;
        let permit = timeout(Duration::from_secs(1), &mut acquire)
            .await??
            .ok_or_else(|| io::Error::other("ready input must acquire a frame slot"))?;
        assert_eq!(budget.available_permits(), 0);
        drop(permit);
        Ok(())
    }

    /// The orchestration snapshot holds one pooled connection, like every
    /// other review read: its whole reconstruction runs inside a single
    /// `REPEATABLE READ` transaction. A three-connection pool therefore starts,
    /// where the two-connection form of this read needed four.
    #[tokio::test]
    async fn review_orchestration_snapshot_holds_one_pool_connection() -> Result<(), Box<dyn Error>>
    {
        let capacity = 2;
        let budget = Arc::new(Semaphore::new(capacity));
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);

        let permit = admit_snapshot_reader(
            &read_review_orchestration_request(),
            Arc::clone(&budget),
            &mut shutdown_receiver,
        )
        .await?
        .ok_or_else(|| io::Error::other("the running fixture must be admitted"))?
        .ok_or_else(|| io::Error::other("the snapshot read must hold a reader permit"))?;

        assert_eq!(budget.available_permits(), capacity - 1);
        drop(permit);
        assert_eq!(budget.available_permits(), capacity);
        assert_eq!(snapshot_reader_capacity(3, None), Some(1));
        assert!(snapshot_reader_capacity(2, None).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_reader_budget_reserves_two_pool_connections() -> Result<(), Box<dyn Error>> {
        let max_pool_connections = 10;
        let capacity = snapshot_reader_capacity(max_pool_connections, None)
            .ok_or_else(|| io::Error::other("the production pool must admit snapshot readers"))?;
        assert_eq!(
            capacity,
            usize::try_from(max_pool_connections - RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS)?
        );
        assert!(snapshot_reader_capacity(2, None).is_none());

        let budget = Arc::new(Semaphore::new(capacity));
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let permits = Arc::clone(&budget)
            .acquire_many_owned(u32::try_from(capacity)?)
            .await?;
        assert!(
            timeout(
                Duration::from_millis(20),
                acquire_snapshot_reader_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
            )
            .await
            .is_err(),
            "the next snapshot reader must leave two pool slots free"
        );

        shutdown.send(true)?;
        assert!(
            acquire_snapshot_reader_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
                .await?
                .is_none()
        );
        drop(permits);
        Ok(())
    }

    #[test]
    fn enlarged_pool_applies_the_configured_snapshot_reader_limit() {
        let configured_limit = 3;
        let enlarged_pool_connections = u32::try_from(configured_limit)
            .expect("the effective ceiling fits PostgreSQL pool capacity")
            + RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS
            + 1;

        assert_eq!(
            snapshot_reader_capacity(enlarged_pool_connections, Some(configured_limit)),
            Some(configured_limit)
        );
    }

    /// The wire vocabulary as text. The review read verbs are enumerated from
    /// the protocol itself so a later one cannot be admitted by a list here
    /// staying silent about it.
    const WIRE_VOCABULARY: &str = include_str!("../../../../crates/process-protocol/src/lib.rs");

    fn client_request_variant_names(source: &str) -> BTreeSet<String> {
        let declaration = "pub enum ClientRequest {";
        let start = source
            .find(declaration)
            .expect("the wire vocabulary declares the client request enum");
        let body = &source[start + declaration.len()..];
        let end = body
            .find("\n}\n")
            .expect("the client request enum body is closed");
        body[..end]
            .lines()
            .filter_map(|line| {
                let variant = line.strip_prefix("    ")?;
                if !variant.starts_with(|character: char| character.is_ascii_uppercase()) {
                    return None;
                }
                Some(
                    variant
                        .split(|character: char| !character.is_ascii_alphanumeric())
                        .next()?
                        .to_owned(),
                )
            })
            .collect()
    }

    /// The review verbs that read the database, taken from the wire vocabulary.
    fn review_read_verbs_in_vocabulary() -> BTreeSet<String> {
        client_request_variant_names(WIRE_VOCABULARY)
            .into_iter()
            .filter(|name| {
                name.contains("Review") && (name.starts_with("Read") || name.starts_with("List"))
            })
            .collect()
    }

    /// The scraper carries logic no assertion can inspect, so it is pinned on
    /// its own: one name per declaration, single-line and braced forms alike,
    /// with doc comments and field lines excluded.
    #[test]
    fn client_request_variant_names_reads_one_name_per_declaration() {
        let source = concat!(
            "pub enum ClientRequest {\n",
            "    /// Read one target.\n",
            "    ReadReviewTarget { target_id: CanonicalUuid },\n",
            "    ListReviewFindings {\n",
            "        run_id: CanonicalUuid,\n",
            "    },\n",
            "    ListTemplates {},\n",
            "}\n",
        );

        assert_eq!(
            client_request_variant_names(source),
            BTreeSet::from([
                String::from("ListReviewFindings"),
                String::from("ListTemplates"),
                String::from("ReadReviewTarget"),
            ])
        );
    }

    /// One fixture identity. No admission reads an identity's value; the verb
    /// carrying one is the whole input.
    fn fixture_identity(seed: u128) -> CanonicalUuid {
        CanonicalUuid::from_uuid(Uuid::from_u128(seed))
    }

    fn read_review_target_request() -> ClientRequest {
        ClientRequest::ReadReviewTarget {
            target_id: fixture_identity(1),
        }
    }

    fn read_review_run_request() -> ClientRequest {
        ClientRequest::ReadReviewRun {
            run_id: fixture_identity(2),
        }
    }

    fn read_review_finding_request() -> ClientRequest {
        ClientRequest::ReadReviewFinding {
            finding_id: fixture_identity(3),
        }
    }

    fn list_review_findings_request() -> ClientRequest {
        ClientRequest::ListReviewFindings {
            run_id: fixture_identity(4),
        }
    }

    fn read_review_orchestration_request() -> ClientRequest {
        ClientRequest::ReadReviewOrchestration {
            attempt_id: fixture_identity(5),
        }
    }

    /// Every review verb that reads the database reserves snapshot capacity.
    /// The reservation exists so `RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS`
    /// connections stay available to the outbox dispatcher and mutations; a
    /// read verb dispatched without it spends that reserve silently.
    #[test]
    fn every_review_read_verb_reserves_snapshot_capacity() {
        assert_eq!(
            review_read_verbs_in_vocabulary(),
            BTreeSet::from([
                String::from("ListReviewFindings"),
                String::from("ReadReviewFinding"),
                String::from("ReadReviewOrchestration"),
                String::from("ReadReviewRun"),
                String::from("ReadReviewTarget"),
            ]),
            "a review read verb in the wire vocabulary has no admission of its own"
        );

        assert_eq!(
            SnapshotReaderAdmission::for_request(&read_review_target_request()),
            SnapshotReaderAdmission::OneConnection
        );
        assert_eq!(
            SnapshotReaderAdmission::for_request(&read_review_run_request()),
            SnapshotReaderAdmission::OneConnection
        );
        assert_eq!(
            SnapshotReaderAdmission::for_request(&read_review_finding_request()),
            SnapshotReaderAdmission::OneConnection
        );
        assert_eq!(
            SnapshotReaderAdmission::for_request(&list_review_findings_request()),
            SnapshotReaderAdmission::OneConnection
        );
        assert_eq!(
            SnapshotReaderAdmission::for_request(&read_review_orchestration_request()),
            SnapshotReaderAdmission::OneConnection
        );
    }

    /// A metadata point read opens a transaction, sets `REPEATABLE READ ONLY`,
    /// selects, and commits, so it holds a pooled connection across statements
    /// and belongs to the same admission. A defaults read is one statement and
    /// does not.
    #[test]
    fn point_reads_are_admitted_by_how_long_they_hold_a_connection() {
        assert_eq!(
            SnapshotReaderAdmission::for_request(&ClientRequest::ReadSessionMetadata {
                session_id: fixture_identity(6),
            }),
            SnapshotReaderAdmission::OneConnection
        );
        assert_eq!(
            SnapshotReaderAdmission::for_request(&ClientRequest::ReadSessionDefaults {
                session_id: fixture_identity(7),
                defaults_version: None,
            }),
            SnapshotReaderAdmission::NotRequired
        );
    }

    #[tokio::test]
    async fn review_read_admission_draws_on_the_shared_reader_budget() -> Result<(), Box<dyn Error>>
    {
        let capacity = 3;
        let budget = Arc::new(Semaphore::new(capacity));
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);

        let permit = admit_snapshot_reader(
            &read_review_target_request(),
            Arc::clone(&budget),
            &mut shutdown_receiver,
        )
        .await?
        .ok_or_else(|| io::Error::other("the running fixture must be admitted"))?
        .ok_or_else(|| io::Error::other("a review read must hold a reader permit"))?;
        assert_eq!(budget.available_permits(), capacity - 1);
        drop(permit);

        assert!(
            admit_snapshot_reader(
                &ClientRequest::ListModelAliases {},
                Arc::clone(&budget),
                &mut shutdown_receiver,
            )
            .await?
            .ok_or_else(|| io::Error::other("the running fixture must be admitted"))?
            .is_none(),
            "a request that reads no snapshot holds no reader permit"
        );
        assert_eq!(budget.available_permits(), capacity);
        Ok(())
    }

    #[tokio::test]
    async fn queued_review_request_retains_its_inbound_frame_slot() -> Result<(), Box<dyn Error>> {
        let frame_budget = Arc::new(Semaphore::new(1));
        let review_budget = Arc::new(Semaphore::new(1));
        let occupied_review = Arc::clone(&review_budget).acquire_owned().await?;
        let frame_permit = Arc::clone(&frame_budget).acquire_owned().await?;
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let acquire = acquire_review_command_permit_while_buffered(
            ReviewCommandAdmission::Required,
            Some(frame_permit),
            Arc::clone(&review_budget),
            &mut shutdown_receiver,
            None,
        );
        tokio::pin!(acquire);

        assert!(
            timeout(Duration::from_millis(20), &mut acquire)
                .await
                .is_err()
        );
        assert_eq!(frame_budget.available_permits(), 0);
        drop(occupied_review);
        let (held_frame, review_permit) = timeout(Duration::from_secs(1), &mut acquire)
            .await??
            .ok_or_else(|| io::Error::other("the admitted request must retain both permits"))?;
        let held_frame =
            held_frame.ok_or_else(|| io::Error::other("the frame permit must remain"))?;
        assert!(review_permit.is_some());
        assert_eq!(frame_budget.available_permits(), 0);
        drop(held_frame);
        assert_eq!(frame_budget.available_permits(), 1);
        Ok(())
    }

    /// INV-060: an expired active bulk-ingest deadline releases a frame held
    /// while a review mutation waits for its separate admission budget.
    #[tokio::test(start_paused = true)]
    async fn inv060_expired_bulk_ingest_deadline_cancels_review_admission()
    -> Result<(), Box<dyn Error>> {
        let frame_budget = Arc::new(Semaphore::new(1));
        let review_budget = Arc::new(Semaphore::new(1));
        let _occupied_review = Arc::clone(&review_budget).acquire_owned().await?;
        let frame_permit = Arc::clone(&frame_budget).acquire_owned().await?;
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);

        let admission = acquire_review_command_permit_while_buffered(
            ReviewCommandAdmission::Required,
            Some(frame_permit),
            review_budget,
            &mut shutdown_receiver,
            Some(Instant::now()),
        )
        .await?;

        assert!(admission.is_none());
        assert_eq!(frame_budget.available_permits(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn review_command_permit_releases_before_response_write() -> Result<(), Box<dyn Error>> {
        let budget = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&budget).acquire_owned().await?;
        let mut pending = PendingResponseWriter;
        let mut response = super::ReviewResponseWriter::new(&mut pending, Some(permit));

        std::future::poll_fn(|context| {
            let pending = tokio::io::AsyncWrite::poll_write(
                std::pin::Pin::new(&mut response),
                context,
                b"response",
            );
            assert!(pending.is_pending());
            std::task::Poll::Ready(())
        })
        .await;

        let replacement = budget.try_acquire_owned()?;
        drop(replacement);
        Ok(())
    }

    #[test]
    fn terminal_review_state_reconstructs_its_historical_activation() {
        let reference = ReviewRunRef::new(
            ReviewTargetId::from_uuid(Uuid::from_u128(1)),
            ReviewRunId::from_uuid(Uuid::from_u128(2)),
        );
        let pass_reference =
            ReviewPassRef::new(reference, ReviewPassId::from_uuid(Uuid::from_u128(3)));
        let session = SessionId::from_uuid(Uuid::from_u128(4));
        let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(5));
        let origin_turn = TurnId::from_uuid(Uuid::from_u128(6));
        let active_turn = origin_turn;
        let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(7));
        let policy = ReviewPolicy::version_one();
        let mut queued_run = ReviewRun::new(reference, ReviewWorkflowKind::ReadOnlyReview, policy);
        let queued_pass = ReviewPass::try_new(
            pass_reference,
            ReviewPassKind::ReadOnlyReview,
            &mut queued_run,
            session,
            ReviewPassAcceptedInputEvidence::new(accepted_input, session, Some(origin_turn)),
        )
        .expect("the fixture pass owns its accepted input");
        let running_pass = queued_pass
            .transition(
                ReviewPassState::Running { turn: active_turn },
                Some(ReviewPassTurnEvidence::new(
                    active_turn,
                    session,
                    accepted_input,
                    ReviewPassTurnOutcome::Active,
                    None,
                )),
            )
            .expect("the fixture pass activates");
        let running_run = queued_run
            .transition(
                ReviewRunState::Running {
                    active_pass: pass_reference,
                },
                Some(ReviewPassEvidence::from_pass(&running_pass, policy)),
            )
            .expect("the fixture run activates");
        let failed_pass = running_pass
            .clone()
            .transition(
                ReviewPassState::Failed { turn: active_turn },
                Some(ReviewPassTurnEvidence::new(
                    active_turn,
                    session,
                    accepted_input,
                    ReviewPassTurnOutcome::Failed,
                    Some(terminal_frontier),
                )),
            )
            .expect("the fixture pass concludes");
        let failed_run = running_run
            .clone()
            .transition(
                ReviewRunState::Failed {
                    failed_pass: pass_reference,
                },
                Some(ReviewPassEvidence::from_pass(&failed_pass, policy)),
            )
            .expect("the fixture run concludes");

        assert!(super::review_activation_was_applied(
            &failed_run,
            &failed_pass,
            active_turn,
        ));
        let (reconstructed_run, reconstructed_pass) =
            super::historical_review_activation(&failed_run, &failed_pass, active_turn)
                .expect("terminal state retains the historical activation");
        assert_eq!(reconstructed_run, running_run);
        assert_eq!(reconstructed_pass, running_pass);
    }

    #[tokio::test]
    async fn review_command_budget_admits_one_claim_at_a_time() -> Result<(), Box<dyn Error>> {
        assert_eq!(MAX_CONCURRENT_REVIEW_COMMANDS, 1);
        let budget = Arc::new(Semaphore::new(MAX_CONCURRENT_REVIEW_COMMANDS));
        let (_shutdown, shutdown_receiver) = watch::channel(false);
        let first =
            acquire_review_command_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
                .await?
                .ok_or_else(|| {
                    io::Error::other("the first review command must acquire its permit")
                })?;

        assert!(
            timeout(
                Duration::from_millis(20),
                acquire_review_command_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
            )
            .await
            .is_err(),
            "the second review command must wait for the first claim"
        );

        drop(first);
        let second = timeout(
            Duration::from_secs(1),
            acquire_review_command_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
        )
        .await??
        .ok_or_else(|| io::Error::other("the second review command must acquire after release"))?;
        drop(second);
        Ok(())
    }

    #[test]
    fn claude_converter_failures_map_to_typed_content_silent_evidence() {
        use ClaudeCodeJsonlConversionFailure as Failure;
        use ConversationImportRejectionClass as Class;
        use ConversionFailureDisposition::Rejected;

        assert_eq!(
            claude_conversion_failure_disposition(Failure::EmptySource),
            Rejected(import_evidence(Class::EmptySource, None))
        );
        assert_simple_import_failures!(
            claude_conversion_failure_disposition,
            Failure;
            2 => BlankLine => Class::BlankLine,
            3 => InvalidUtf8 => Class::InvalidUtf8,
            4 => InvalidJson => Class::InvalidJson,
            5 => JsonDepthExceeded => Class::JsonDepthExceeded,
            6 => TopLevelNotObject => Class::TopLevelNotObject,
            7 => InvalidRecordType => Class::InvalidRecordType,
            8 => InvalidSourceMetadata => Class::InvalidSourceMetadata,
            9 => InvalidMessageEnvelope => Class::InvalidMessageEnvelope,
            10 => InvalidMessageRole => Class::InvalidMessageRole,
            11 => MessageRoleMismatch => Class::MessageRoleMismatch,
            12 => InvalidMessageContent => Class::InvalidMessageContent,
        );
        assert_import_failure_ordinal!(
            claude_conversion_failure_disposition,
            13,
            |line| Failure::InvalidContentBlock { line, block: 1 },
            Class::InvalidContentBlock
        );
        assert_import_failure_ordinal!(
            claude_conversion_failure_disposition,
            14,
            |line| Failure::InvalidToolResultBlock {
                line,
                block: 1,
                result_block: 2,
            },
            Class::InvalidToolResultBlock
        );
        assert_eq!(
            claude_conversion_failure_disposition(Failure::PositionExhausted),
            ConversionFailureDisposition::Internal
        );
    }

    #[test]
    fn codex_converter_failures_map_to_typed_content_silent_evidence() {
        use CodexRolloutJsonlConversionFailure as Failure;
        use ConversationImportRejectionClass as Class;
        use ConversionFailureDisposition::Rejected;

        assert_eq!(
            codex_conversion_failure_disposition(Failure::EmptySource),
            Rejected(import_evidence(Class::EmptySource, None))
        );
        assert_simple_import_failures!(
            codex_conversion_failure_disposition,
            Failure;
            2 => BlankLine => Class::BlankLine,
            3 => InvalidUtf8 => Class::InvalidUtf8,
            4 => InvalidJson => Class::InvalidJson,
            5 => JsonDepthExceeded => Class::JsonDepthExceeded,
            6 => TopLevelNotObject => Class::TopLevelNotObject,
            7 => InvalidRecordType => Class::InvalidRecordType,
            8 => InvalidResponseItemType => Class::InvalidRecordType,
            9 => InvalidSourceMetadata => Class::InvalidSourceMetadata,
            10 => InvalidResponseItemEnvelope => Class::InvalidMessageEnvelope,
            11 => InvalidMessageRole => Class::InvalidMessageRole,
            12 => InvalidMessageContent => Class::InvalidMessageContent,
            14 => InvalidReasoning => Class::InvalidReasoning,
            16 => InvalidToolCall => Class::InvalidToolCall,
            17 => InvalidToolResult => Class::InvalidToolResult,
        );
        assert_import_failure_ordinal!(
            codex_conversion_failure_disposition,
            13,
            |line| Failure::InvalidMessageBlock { line, block: 1 },
            Class::InvalidContentBlock
        );
        assert_import_failure_ordinal!(
            codex_conversion_failure_disposition,
            15,
            |line| Failure::InvalidReasoningBlock { line, block: 1 },
            Class::InvalidReasoning
        );
        assert_import_failure_ordinal!(
            codex_conversion_failure_disposition,
            18,
            |line| Failure::InvalidToolResultBlock { line, block: 1 },
            Class::InvalidToolResultBlock
        );
        assert_eq!(
            codex_conversion_failure_disposition(Failure::PositionExhausted),
            ConversionFailureDisposition::Internal
        );
    }

    #[test]
    fn oversized_begin_is_rejected_without_reserving_import_capacity() {
        let limit = 8;
        let oversized = ClientRequest::BeginConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes: CanonicalU64::new(u64::try_from(limit + 1).expect("limit fits")),
        };
        let admitted = ClientRequest::BeginConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes: CanonicalU64::new(u64::try_from(limit).expect("limit fits")),
        };
        let zero_blob = ClientRequest::BeginBlobUpload {
            expected_digest: signalbox_process_protocol::CanonicalBlobDigest::from_bytes([0; 32]),
            expected_length_bytes: CanonicalU64::new(0),
        };
        let admitted_blob = ClientRequest::BeginBlobUpload {
            expected_digest: signalbox_process_protocol::CanonicalBlobDigest::from_bytes([0; 32]),
            expected_length_bytes: CanonicalU64::new(u64::try_from(limit).expect("limit fits")),
        };
        let oversized_blob = ClientRequest::BeginBlobUpload {
            expected_digest: signalbox_process_protocol::CanonicalBlobDigest::from_bytes([0; 32]),
            expected_length_bytes: CanonicalU64::new(u64::try_from(limit + 1).expect("limit fits")),
        };

        assert!(!super::conversation_import_request_requires_permit(
            &oversized,
            ConversationImportState::Inactive,
            limit,
            u64::MAX,
        ));
        assert!(super::conversation_import_request_requires_permit(
            &admitted,
            ConversationImportState::Inactive,
            limit,
            u64::MAX,
        ));
        assert!(!super::conversation_import_request_requires_permit(
            &admitted,
            ConversationImportState::Active,
            limit,
            u64::MAX,
        ));
        assert!(!super::conversation_import_request_requires_permit(
            &zero_blob,
            ConversationImportState::Inactive,
            limit,
            u64::try_from(limit).expect("limit fits"),
        ));
        assert!(super::conversation_import_request_requires_permit(
            &admitted_blob,
            ConversationImportState::Inactive,
            limit,
            u64::try_from(limit).expect("limit fits"),
        ));
        assert!(!super::conversation_import_request_requires_permit(
            &oversized_blob,
            ConversationImportState::Inactive,
            limit,
            u64::try_from(limit).expect("limit fits"),
        ));
    }

    /// INV-060: each chunked bulk-ingest kind rejects every lifecycle request
    /// belonging to the other kind while preserving its own lifecycle.
    #[test]
    fn inv060_cross_kind_bulk_ingest_requests_are_classified_before_admission() {
        let append_blob = ClientRequest::AppendBlobUpload {
            chunk: signalbox_process_protocol::BlobChunk::new(vec![1]),
        };
        let append_import = ClientRequest::AppendConversationImport {
            chunk: signalbox_process_protocol::ConversationImportSource::new(vec![1]),
        };

        assert!(super::request_is_cross_kind_bulk_ingest(
            &append_blob,
            signalbox_process_protocol::BulkIngestKind::ConversationImport,
        ));
        assert!(super::request_is_cross_kind_bulk_ingest(
            &append_import,
            signalbox_process_protocol::BulkIngestKind::BlobUpload,
        ));
        assert!(!super::request_is_cross_kind_bulk_ingest(
            &append_blob,
            signalbox_process_protocol::BulkIngestKind::BlobUpload,
        ));
        assert!(!super::request_is_cross_kind_bulk_ingest(
            &append_import,
            signalbox_process_protocol::BulkIngestKind::ConversationImport,
        ));
    }

    /// INV-060: inactivity resets after accepted lifecycle output while the
    /// whole-session deadline stays anchored to permit acquisition.
    #[tokio::test(start_paused = true)]
    async fn inv060_bulk_ingest_deadlines_have_independent_monotonic_anchors()
    -> Result<(), Box<dyn Error>> {
        let started_at = Instant::now();
        let permit = Arc::new(Semaphore::new(1)).acquire_owned().await?;
        let mut pending = Some(PendingConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes: 1,
            actual_size_bytes: 0,
            source: Vec::new(),
            import_permit: permit,
            started_at,
            idle_since: started_at,
        });

        assert_eq!(
            super::pending_bulk_ingest_deadline(&pending, &None, true),
            Some(started_at + super::BULK_INGEST_IDLE_TIMEOUT),
        );
        tokio::time::advance(Duration::from_secs(4 * 60)).await;
        pending
            .as_mut()
            .expect("the fixture import is active")
            .idle_since = Instant::now();
        assert_eq!(
            super::pending_bulk_ingest_deadline(&pending, &None, true),
            Some(started_at + Duration::from_secs(9 * 60)),
        );
        assert_eq!(
            super::pending_bulk_ingest_deadline(&pending, &None, false),
            Some(started_at + super::BULK_INGEST_SESSION_TIMEOUT),
        );
        Ok(())
    }

    /// INV-060: an active upload classifies every second begin as the sole
    /// nonterminal duplicate-begin refusal before inspecting its new length.
    #[test]
    fn inv060_active_blob_upload_precedes_duplicate_begin_length_validation()
    -> Result<(), Box<dyn Error>> {
        let detail = blob_upload_begin_preflight(true, CanonicalU64::new(0), 8)
            .ok_or_else(|| io::Error::other("the active upload must reject a second begin"))?;

        assert_eq!(detail, RejectionDetail::BlobUploadAlreadyInProgress {});
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_begin_refusal_preserves_the_active_import() -> Result<(), Box<dyn Error>> {
        let capacity = 1;
        let budget = Arc::new(Semaphore::new(capacity));
        let permit = Arc::clone(&budget).acquire_owned().await?;
        let format = signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1;
        let source = b"partial".to_vec();
        let expected_source = source.clone();
        let declared_size_bytes = u64::try_from(source.len())?;
        let mut pending = Some(PendingConversationImport {
            format,
            declared_size_bytes,
            actual_size_bytes: declared_size_bytes,
            source,
            import_permit: permit,
            started_at: Instant::now(),
            idle_since: Instant::now(),
        });
        let request_id = RequestId::try_new(1)?;
        let (mut writer, mut reader) = duplex(1_024);

        handle_begin_conversation_import(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            format,
            CanonicalU64::new(declared_size_bytes),
            usize::try_from(declared_size_bytes)?,
            None,
            None,
            &mut pending,
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let observed = decode_server_line(&encoded)?;
        let expected = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request_id,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("conversation import was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::ConversationImportAlreadyInProgress {},
                ),
            },
        )?;
        let active = pending
            .as_ref()
            .expect("the active import remains available");

        assert_eq!(observed, expected);
        assert_eq!(active.source, expected_source);
        assert_eq!(budget.available_permits(), capacity - 1);
        Ok(())
    }

    #[tokio::test]
    async fn waiting_begin_releases_its_inbound_slot_before_import_admission()
    -> Result<(), Box<dyn Error>> {
        let capacity = 1;
        let frame_budgets = InboundFrameBudgets::new();
        let import_budget = Arc::new(Semaphore::new(capacity));
        let occupied_import = Arc::clone(&import_budget).acquire_owned().await?;
        let frame_permit = frame_budgets
            .for_connection(ConversationImportState::Inactive)
            .acquire_owned()
            .await?;
        let begin = ClientRequest::BeginConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes: CanonicalU64::new(u64::try_from(capacity)?),
        };
        let import_requires_permit = super::conversation_import_request_requires_permit(
            &begin,
            ConversationImportState::Inactive,
            capacity,
            u64::MAX,
        );

        let retained = retain_inbound_frame_permit_during_import_admission(
            &begin,
            import_requires_permit,
            frame_permit,
        );

        assert!(retained.is_none());
        assert_eq!(import_budget.available_permits(), 0);
        let general_slots = frame_budgets
            .for_connection(ConversationImportState::Inactive)
            .acquire_many_owned(u32::try_from(GENERAL_BUFFERED_INBOUND_FRAMES)?)
            .await?;
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let active_slot = timeout(
            Duration::from_secs(1),
            acquire_inbound_frame_permit(
                frame_budgets.for_connection(ConversationImportState::Active),
                &mut shutdown_receiver,
            ),
        )
        .await??
        .ok_or_else(|| io::Error::other("the active import must retain frame progress"))?;

        assert_eq!(general_slots.num_permits(), GENERAL_BUFFERED_INBOUND_FRAMES);
        assert_eq!(
            active_slot.num_permits(),
            RESERVED_ACTIVE_IMPORT_INBOUND_FRAMES
        );
        assert_eq!(
            general_slots.num_permits() + active_slot.num_permits(),
            MAX_BUFFERED_INBOUND_FRAMES
        );
        drop(occupied_import);
        Ok(())
    }

    #[tokio::test]
    async fn released_begin_waiters_have_a_separate_bound() -> Result<(), Box<dyn Error>> {
        let capacity = MAX_IMPORT_ADMISSION_WAITERS;
        let budget = Arc::new(Semaphore::new(capacity));
        let occupied = Arc::clone(&budget)
            .acquire_many_owned(u32::try_from(capacity)?)
            .await?;
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let acquire = acquire_import_waiter_permit(Arc::clone(&budget), &mut shutdown_receiver);
        tokio::pin!(acquire);

        assert!(
            timeout(Duration::from_millis(20), &mut acquire)
                .await
                .is_err()
        );
        assert_eq!(occupied.num_permits(), capacity);

        drop(occupied);
        let admitted = timeout(Duration::from_secs(1), &mut acquire)
            .await??
            .ok_or_else(|| io::Error::other("a released waiter place must admit the begin"))?;

        assert_eq!(admitted.num_permits(), 1);
        Ok(())
    }

    #[test]
    fn conversation_import_allocation_exhaustion_is_unavailable() {
        let diagnostic = InternalDiagnostic::ConversationImportAllocationFailure;
        let error = unavailable_protocol_error(diagnostic);

        assert_eq!(error.code, ErrorCode::Unavailable);
        assert_eq!(error.detail, ErrorDetail::none());
        assert_eq!(
            diagnostic.failure_class(),
            signalbox_application::OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            }
        );
    }

    #[test]
    fn conversation_import_capacity_grows_geometrically_within_declared_and_configured_bounds() {
        let chunk_capacity = 4;
        let declared_capacity = chunk_capacity * 4;
        let configured_capacity = declared_capacity * 2;
        let first_capacity = super::conversation_import_capacity_target(
            0,
            chunk_capacity,
            declared_capacity,
            configured_capacity,
        );
        let second_capacity = super::conversation_import_capacity_target(
            first_capacity,
            chunk_capacity * 2,
            declared_capacity,
            configured_capacity,
        );
        let retained_capacity = super::conversation_import_capacity_target(
            second_capacity,
            chunk_capacity * 2 - 1,
            declared_capacity,
            configured_capacity,
        );
        let third_capacity = super::conversation_import_capacity_target(
            retained_capacity,
            chunk_capacity * 2 + 1,
            declared_capacity,
            configured_capacity,
        );
        let declared_bound = super::conversation_import_capacity_target(
            third_capacity,
            declared_capacity,
            declared_capacity,
            configured_capacity,
        );
        let configured_bound = super::conversation_import_capacity_target(
            declared_bound,
            declared_capacity + 1,
            declared_capacity,
            configured_capacity,
        );

        assert_eq!(first_capacity, chunk_capacity);
        assert_eq!(second_capacity, chunk_capacity * 2);
        assert_eq!(retained_capacity, second_capacity);
        assert_eq!(third_capacity, declared_capacity);
        assert_eq!(declared_bound, declared_capacity);
        assert_eq!(configured_bound, configured_capacity);
    }

    #[tokio::test]
    async fn chunk_appends_assemble_exact_source_order() -> Result<(), Box<dyn Error>> {
        let budget = Arc::new(Semaphore::new(1));
        let permit = budget.clone().acquire_owned().await?;
        let first = b"first".to_vec();
        let second = b"second".to_vec();
        let expected_source = [first.as_slice(), second.as_slice()].concat();
        let expected_size = u64::try_from(expected_source.len())?;
        let limit = 32;
        let mut pending = Some(PendingConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes: expected_size,
            actual_size_bytes: 0,
            source: Vec::new(),
            import_permit: permit,
            started_at: Instant::now(),
            idle_since: Instant::now(),
        });
        let (mut writer, _reader) = duplex(1_024);

        handle_append_conversation_import(
            &mut writer,
            ProtocolVersion::One,
            RequestId::try_new(1)?,
            first,
            limit,
            &mut pending,
        )
        .await?;
        handle_append_conversation_import(
            &mut writer,
            ProtocolVersion::One,
            RequestId::try_new(2)?,
            second,
            limit,
            &mut pending,
        )
        .await?;

        let assembled = pending.as_ref().expect("the import remains pending");
        assert_eq!(assembled.source, expected_source);
        assert_eq!(assembled.actual_size_bytes, expected_size);
        assert!(assembled.source.capacity() <= limit);
        Ok(())
    }

    #[tokio::test]
    async fn begin_rejects_a_declared_size_above_the_configured_bound() -> Result<(), Box<dyn Error>>
    {
        let capacity = 1;
        let budget = Arc::new(Semaphore::new(capacity));
        let permit = budget.clone().acquire_owned().await?;
        let request_id = RequestId::try_new(1)?;
        let limit = 8;
        let declared_size_bytes = CanonicalU64::new(9);
        let mut pending = None;
        let (mut writer, mut reader) = duplex(1_024);

        handle_begin_conversation_import(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes,
            limit,
            Some(permit),
            None,
            &mut pending,
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let observed = decode_server_line(&encoded)?;
        let expected = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request_id,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("conversation import was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::ConversationImportSourceTooLarge {
                        limit_bytes: CanonicalU64::new(u64::try_from(limit)?),
                        declared_size_bytes,
                        actual_size_bytes: None,
                    },
                ),
            },
        )?;

        assert_eq!(observed, expected);
        assert!(pending.is_none());
        assert_eq!(budget.available_permits(), capacity);
        Ok(())
    }

    #[tokio::test]
    async fn append_rejects_observed_size_above_the_configured_bound() -> Result<(), Box<dyn Error>>
    {
        let capacity = 1;
        let budget = Arc::new(Semaphore::new(capacity));
        let permit = budget.clone().acquire_owned().await?;
        let request_id = RequestId::try_new(1)?;
        let limit = 8;
        let declared_size_bytes = u64::try_from(limit)?;
        let prior_size_bytes = u64::try_from(limit - 1)?;
        let chunk = vec![b'x'; 2];
        let observed_size_bytes = prior_size_bytes + u64::try_from(chunk.len())?;
        let mut pending = Some(PendingConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes,
            actual_size_bytes: prior_size_bytes,
            source: vec![b'x'; usize::try_from(prior_size_bytes)?],
            import_permit: permit,
            started_at: Instant::now(),
            idle_since: Instant::now(),
        });
        let (mut writer, mut reader) = duplex(1_024);

        handle_append_conversation_import(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            chunk,
            limit,
            &mut pending,
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let observed = decode_server_line(&encoded)?;
        let expected = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request_id,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("conversation import was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::ConversationImportSourceTooLarge {
                        limit_bytes: CanonicalU64::new(u64::try_from(limit)?),
                        declared_size_bytes: CanonicalU64::new(declared_size_bytes),
                        actual_size_bytes: Some(CanonicalU64::new(observed_size_bytes)),
                    },
                ),
            },
        )?;

        assert_eq!(budget.available_permits(), capacity);
        assert_eq!(observed, expected);
        assert!(pending.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn commit_rejects_declared_and_actual_size_mismatch() -> Result<(), Box<dyn Error>> {
        let capacity = 1;
        let budget = Arc::new(Semaphore::new(capacity));
        let permit = budget.clone().acquire_owned().await?;
        let source = vec![b'x'];
        let actual_size_bytes = u64::try_from(source.len())?;
        let declared_size_bytes = actual_size_bytes + 1;
        let request_id = RequestId::try_new(1)?;
        let limit = 8;
        let mut pending = Some(PendingConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes,
            actual_size_bytes,
            source,
            import_permit: permit,
            started_at: Instant::now(),
            idle_since: Instant::now(),
        });
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://signalbox:fixture@127.0.0.1/signalbox")?;
        let repository = ImportedConversationRepository::new(pool);
        let (mut writer, mut reader) = duplex(1);
        let write = tokio::spawn(async move {
            let result = handle_commit_conversation_import(
                &mut writer,
                ProtocolVersion::One,
                request_id,
                limit,
                repository,
                &mut pending,
            )
            .await;
            (result, pending)
        });

        let reacquired = timeout(Duration::from_secs(1), budget.acquire_owned()).await??;
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let (write_result, pending) = write.await?;
        write_result?;
        let observed = decode_server_line(&encoded)?;
        let expected = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request_id,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("conversation import was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::ConversationImportSourceSizeMismatch {
                        declared_size_bytes: CanonicalU64::new(declared_size_bytes),
                        actual_size_bytes: CanonicalU64::new(actual_size_bytes),
                    },
                ),
            },
        )?;

        assert_eq!(reacquired.num_permits(), capacity);
        assert_eq!(observed, expected);
        assert!(pending.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn commit_rechecks_the_configured_total_bound() -> Result<(), Box<dyn Error>> {
        let budget = Arc::new(Semaphore::new(1));
        let permit = budget.clone().acquire_owned().await?;
        let request_id = RequestId::try_new(1)?;
        let declared_size_bytes = 7;
        let actual_size_bytes = 9;
        let limit = 8;
        let mut pending = Some(PendingConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes,
            actual_size_bytes,
            source: Vec::new(),
            import_permit: permit,
            started_at: Instant::now(),
            idle_since: Instant::now(),
        });
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://signalbox:fixture@127.0.0.1/signalbox")?;
        let repository = ImportedConversationRepository::new(pool);
        let (mut writer, mut reader) = duplex(1_024);

        handle_commit_conversation_import(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            limit,
            repository,
            &mut pending,
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let observed = decode_server_line(&encoded)?;
        let expected = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request_id,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("conversation import was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::ConversationImportSourceTooLarge {
                        limit_bytes: CanonicalU64::new(u64::try_from(limit)?),
                        declared_size_bytes: CanonicalU64::new(declared_size_bytes),
                        actual_size_bytes: Some(CanonicalU64::new(actual_size_bytes)),
                    },
                ),
            },
        )?;

        assert_eq!(observed, expected);
        assert!(pending.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn disconnect_drop_discards_partial_import_and_releases_its_permit()
    -> Result<(), Box<dyn Error>> {
        let capacity = 1;
        let budget = Arc::new(Semaphore::new(capacity));
        let permit = budget.clone().acquire_owned().await?;
        let pending = PendingConversationImport {
            format: signalbox_process_protocol::ConversationImportFormat::CodexRolloutJsonlV1,
            declared_size_bytes: 4,
            actual_size_bytes: 2,
            source: b"pa".to_vec(),
            import_permit: permit,
            started_at: Instant::now(),
            idle_since: Instant::now(),
        };

        drop(pending);
        let reacquired = timeout(Duration::from_secs(1), budget.acquire_owned()).await??;

        assert_eq!(reacquired.num_permits(), capacity);
        Ok(())
    }

    #[tokio::test]
    async fn import_budget_admits_one_retained_aggregate_at_a_time() -> Result<(), Box<dyn Error>> {
        assert_eq!(MAX_CONCURRENT_IMPORTS, 1);
        let budget = Arc::new(Semaphore::new(MAX_CONCURRENT_IMPORTS));
        let (_shutdown, shutdown_receiver) = watch::channel(false);
        let first = acquire_import_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
            .await?
            .ok_or_else(|| io::Error::other("the first import must acquire its permit"))?;

        assert!(
            timeout(
                Duration::from_millis(20),
                acquire_import_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
            )
            .await
            .is_err(),
            "a second retained import aggregate must wait"
        );

        drop(first);
        let second = timeout(
            Duration::from_secs(1),
            acquire_import_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
        )
        .await??
        .ok_or_else(|| io::Error::other("the released import permit must be acquired"))?;
        drop(second);
        Ok(())
    }

    #[tokio::test]
    async fn import_conversion_runs_off_the_async_worker() -> Result<(), Box<dyn Error>> {
        let async_worker = thread::current().id();
        let (thread_sender, thread_receiver) = mpsc::sync_channel(1);
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://signalbox:fixture@127.0.0.1/signalbox")?;
        let repository = ImportedConversationRepository::new(pool);

        let outcome = execute_import(
            ThreadReportingRejectConverter(thread_sender),
            Vec::new(),
            repository,
        )
        .await;
        let conversion_worker = thread_receiver.recv_timeout(Duration::from_secs(1))?;

        assert_eq!(
            outcome,
            Err(OperationalImportError::InvalidSource(
                super::import_evidence(
                    signalbox_process_protocol::ConversationImportRejectionClass::InvalidJson,
                    None,
                )
            ))
        );
        assert_ne!(conversion_worker, async_worker);
        Ok(())
    }

    #[tokio::test]
    async fn import_worker_termination_remains_distinct_from_repository_corruption()
    -> Result<(), Box<dyn Error>> {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://signalbox:fixture@127.0.0.1/signalbox")?;
        let repository = ImportedConversationRepository::new(pool);

        let outcome = execute_import(PanickingConverter, Vec::new(), repository).await;

        assert_eq!(
            outcome,
            Err(OperationalImportError::Internal(
                InternalDiagnostic::ConversationImportWorkerTerminated,
            )),
        );
        Ok(())
    }

    #[test]
    fn import_worker_termination_has_exact_operator_diagnostic() {
        let diagnostic = InternalDiagnostic::ConversationImportWorkerTerminated;

        assert_eq!(
            diagnostic.failure_class(),
            signalbox_application::OperatorFailureClass::CallerOrHubBug
        );
        assert_eq!(
            diagnostic.cause_code(),
            "conversation_import_worker_terminated"
        );
    }

    #[test]
    fn import_converter_contract_defect_has_exact_operator_diagnostic() {
        let error = ImportConversationError::<io::Error, ImportedConversationRepositoryError>::
            ConverterEntryIdentitySequenceMismatch;

        assert_eq!(
            operational_import_error(error),
            OperationalImportError::Internal(InternalDiagnostic::ConversationImportContractDefect,),
        );
        assert_eq!(
            InternalDiagnostic::ConversationImportContractDefect.failure_class(),
            signalbox_application::OperatorFailureClass::CallerOrHubBug,
        );
        assert_eq!(
            InternalDiagnostic::ConversationImportContractDefect.cause_code(),
            "conversation_import_contract_defect",
        );
    }

    #[test]
    fn import_repository_identity_collision_keeps_its_operator_class() {
        let error =
            ImportConversationError::<io::Error, ImportedConversationRepositoryError>::Store(
                ImportedConversationRepositoryError::IdentityCollision(
                    ImportedConversationIdentityCollision::Conversation,
                ),
            );

        assert_eq!(
            operational_import_error(error),
            OperationalImportError::Internal(
                InternalDiagnostic::ImportedConversationIdentityCollision,
            ),
        );
        assert_eq!(
            InternalDiagnostic::ImportedConversationIdentityCollision.failure_class(),
            signalbox_application::OperatorFailureClass::IdentityCollision,
        );
    }

    #[test]
    fn import_repository_corruption_keeps_its_operator_class() {
        let error =
            ImportConversationError::<io::Error, ImportedConversationRepositoryError>::Store(
                ImportedConversationRepositoryError::Corruption(
                    ImportedConversationCorruption::Missing("fixture required field"),
                ),
            );

        assert_eq!(
            operational_import_error(error),
            OperationalImportError::Internal(InternalDiagnostic::ImportedConversationCorruption,),
        );
        assert_eq!(
            InternalDiagnostic::ImportedConversationCorruption.failure_class(),
            signalbox_application::OperatorFailureClass::FailClosedCorruption,
        );
    }

    #[test]
    fn import_blob_unavailability_is_retryable_without_ambiguity() {
        let error =
            ImportConversationError::<io::Error, ImportedConversationRepositoryError>::Store(
                ImportedConversationRepositoryError::BlobStorage(
                    ImportedRawBlobStorageError::Unavailable,
                ),
            );

        assert_eq!(
            operational_import_error(error),
            OperationalImportError::Unavailable,
        );
    }

    #[test]
    fn import_catalog_database_failure_remains_retryable() {
        let error =
            ImportConversationError::<io::Error, ImportedConversationRepositoryError>::Store(
                ImportedConversationRepositoryError::BlobCatalog(
                    signalbox_persistence::blob::BlobCatalogRepositoryError::Database(
                        sqlx::Error::PoolTimedOut,
                    ),
                ),
            );

        assert_eq!(
            operational_import_error(error),
            OperationalImportError::Database,
        );
    }

    #[test]
    fn import_catalog_ambiguous_commit_remains_retryable() {
        let error =
            ImportConversationError::<io::Error, ImportedConversationRepositoryError>::Store(
                ImportedConversationRepositoryError::BlobCatalog(
                    signalbox_persistence::blob::BlobCatalogRepositoryError::CommitAmbiguous(
                        sqlx::Error::PoolTimedOut,
                    ),
                ),
            );

        assert_eq!(
            operational_import_error(error),
            OperationalImportError::Unavailable,
        );
    }

    #[test]
    fn import_blob_integrity_failure_is_fail_closed() {
        let error =
            ImportConversationError::<io::Error, ImportedConversationRepositoryError>::Store(
                ImportedConversationRepositoryError::BlobStorage(
                    ImportedRawBlobStorageError::Integrity,
                ),
            );

        assert_eq!(
            operational_import_error(error),
            OperationalImportError::Internal(InternalDiagnostic::ImportedConversationCorruption),
        );
    }

    struct PanickingConverter;

    impl ImportedConversationConverter for PanickingConverter {
        type Error = io::Error;

        fn format(&self) -> ImportedConversationFormat {
            ImportedConversationFormat::CodexRolloutJsonlV1
        }

        fn convert<NextEntryId>(
            &mut self,
            _conversation: ImportedConversationId,
            _source: &[u8],
            _next_entry_id: NextEntryId,
        ) -> Result<ImportedConversation, Self::Error>
        where
            NextEntryId: FnMut() -> ImportedTranscriptEntryId,
        {
            panic!("synthetic import worker panic")
        }
    }

    struct ThreadReportingRejectConverter(mpsc::SyncSender<thread::ThreadId>);

    impl ImportedConversationConverter for ThreadReportingRejectConverter {
        type Error = io::Error;

        fn format(&self) -> ImportedConversationFormat {
            ImportedConversationFormat::CodexRolloutJsonlV1
        }

        fn convert<NextEntryId>(
            &mut self,
            _conversation: ImportedConversationId,
            _source: &[u8],
            _next_entry_id: NextEntryId,
        ) -> Result<ImportedConversation, Self::Error>
        where
            NextEntryId: FnMut() -> ImportedTranscriptEntryId,
        {
            self.0
                .send(thread::current().id())
                .map_err(|_| io::Error::other("the test thread receiver closed"))?;
            Err(io::Error::other("fixture conversion rejection"))
        }
    }

    #[test]
    fn process_submission_admits_the_exact_content_bound() {
        let exact =
            UserInputContent::text("\u{1}".repeat(signalbox_domain::UserContent::MAX_TEXT_BYTES));
        assert!(admitted_user_content(exact).is_ok());
    }

    #[test]
    fn process_submission_rejects_content_over_the_bound() {
        assert!(
            admitted_user_content(UserInputContent::text(
                "x".repeat(signalbox_domain::UserContent::MAX_TEXT_BYTES + 1),
            ))
            .is_err()
        );
    }

    #[test]
    fn accepted_input_bound_keeps_snapshot_projection_representable() -> Result<(), Box<dyn Error>>
    {
        let frame = ServerFrame::try_new(
            RequestId::try_new(u64::MAX)?,
            ServerMessage::TranscriptTurn {
                turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX)),
                acceptance_position: CanonicalU64::new(u64::MAX),
                model_settings: None,
                state: TurnState::Queued {
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX - 1)),
                    content: UserInputContent::text(
                        "\u{1}".repeat(signalbox_domain::UserContent::MAX_TEXT_BYTES),
                    ),
                },
            },
        )?;

        assert!(encode_server_line(&frame)?.len() <= MAX_FRAME_BYTES);
        Ok(())
    }

    #[test]
    fn accepted_input_bound_keeps_update_projection_representable() -> Result<(), Box<dyn Error>> {
        let frame = ServerFrame::try_new(
            RequestId::try_new(u64::MAX)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(u64::MAX),
                session_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX)),
                event: SessionEvent::InputAccepted {
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX - 1)),
                    turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX - 2)),
                    acceptance_position: CanonicalU64::new(u64::MAX),
                    content: UserInputContent::text(
                        "\u{1}".repeat(signalbox_domain::UserContent::MAX_TEXT_BYTES),
                    ),
                },
            },
        )?;

        assert!(encode_server_line(&frame)?.len() <= MAX_FRAME_BYTES);
        Ok(())
    }

    #[test]
    fn oversized_connection_frame_does_not_fail_the_runtime() {
        assert!(
            inspect_connection_completion(Some(Ok(Err(ProcessConnectionError::Encode(
                FrameEncodeError::OversizedFrame
            )))))
            .is_ok()
        );
    }

    #[test]
    fn peer_io_failure_does_not_fail_the_runtime() {
        assert!(
            inspect_connection_completion(Some(Ok(Err(ProcessConnectionError::PeerIo(
                io::Error::new(io::ErrorKind::BrokenPipe, "fixture peer closed")
            )))))
            .is_ok()
        );
    }

    #[test]
    fn spool_read_failure_is_fatal_runtime_evidence() {
        let result = inspect_connection_completion(Some(Ok(Err(ProcessConnectionError::SpoolIo(
            io::Error::other("fixture spool read"),
        )))));

        assert!(matches!(result, Err(ProcessRuntimeError::SpoolIo(_))));
    }

    #[tokio::test]
    async fn pre_response_spool_io_is_reported_as_unavailable() -> Result<(), Box<dyn Error>> {
        let request_id = RequestId::try_new(9)?;
        let (mut writer, mut reader) = duplex(1_024);

        write_snapshot_spool_error(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            SnapshotSpoolError::Io(io::Error::other("fixture spool write")),
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;

        let frame = decode_server_line(&encoded)?;
        assert!(matches!(
            frame.message(),
            ServerMessage::Error {
                code: ErrorCode::Unavailable,
                ..
            }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn goal_history_is_completed_in_spool_before_socket_write() -> Result<(), Box<dyn Error>>
    {
        let session = SessionId::from_uuid(Uuid::from_u128(40));
        let session_id = wire_uuid(session.into_uuid());
        let command = DurableCommandId::from_uuid(Uuid::from_u128(41));
        let statement = GoalStatement::try_new(String::from("finish the fixture task"))?;
        let goal = Goal::commission(session, statement.clone(), GoalUserProvenance::new(command));
        let request_id = RequestId::try_new(42)?;
        let mut spool = spool_goal_snapshot(&goal, ProtocolVersion::One, request_id, session_id)
            .await
            .map_err(|error| {
                io::Error::other(format!(
                    "goal spool fixture failed: {}",
                    spool_error_display(&error)
                ))
            })?;
        let mut encoded = Vec::new();
        spool.read_to_end(&mut encoded).await?;

        let mut expected = encode_server_line(&ServerFrame::try_new(
            request_id,
            ServerMessage::GoalHistoryStart {
                session_id,
                current_generation: CanonicalU64::new(goal.current().generation().get()),
                current_statement: statement.as_str().to_owned(),
            },
        )?)?;
        expected.extend(encode_server_line(&ServerFrame::try_new(
            request_id,
            ServerMessage::GoalHistoryState {
                current_state: GoalLifecycleState::Pursuing {},
            },
        )?)?);
        expected.extend(encode_server_line(&ServerFrame::try_new(
            request_id,
            ServerMessage::GoalHistoryItem {
                event_ordinal: CanonicalU64::new(goal.events()[0].ordinal().get()),
                generation: CanonicalU64::new(goal.events()[0].generation().get()),
                event: wire_goal_event(&goal.events()[0])?,
            },
        )?)?);
        expected.extend(encode_server_line(&ServerFrame::try_new(
            request_id,
            ServerMessage::GoalHistoryEnd {
                event_count: CanonicalU64::new(
                    u64::try_from(goal.events().len()).expect("fixture event count fits u64"),
                ),
            },
        )?)?);

        assert_eq!(encoded, expected);
        Ok(())
    }

    #[tokio::test]
    async fn blocked_follow_write_is_cancelled_by_shutdown() -> Result<(), Box<dyn Error>> {
        let (mut writer, _reader) = duplex(1);
        writer.write_all(b"x").await?;
        let (shutdown, mut shutdown_receiver) = watch::channel(false);
        let blocked_write = tokio::spawn(async move {
            run_until_shutdown(
                &mut shutdown_receiver,
                writer.write_all(b"blocked follow output"),
            )
            .await
        });
        tokio::task::yield_now().await;

        shutdown.send(true)?;

        let outcome = timeout(Duration::from_secs(1), blocked_write).await??;
        assert!(outcome.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn runtime_content_writer_preserves_empty_and_multibyte_text()
    -> Result<(), Box<dyn Error>> {
        let request_id = RequestId::try_new(7)?;
        let text = format!(
            "{}\u{1f980}tail",
            "a".repeat(MAX_CONTENT_FRAGMENT_BYTES - 1)
        );
        let (mut writer, mut reader) = duplex(MAX_FRAME_BYTES * 2);
        write_content(&mut writer, ProtocolVersion::One, request_id, 3, &text).await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;

        let mut reconstructed = String::new();
        let mut expected_fragment = 0_u64;
        let lines = encoded.split_inclusive(|byte| *byte == b'\n');
        for line in lines {
            let frame = decode_server_line(line)?;
            match frame.message() {
                ServerMessage::TranscriptContent {
                    entry_index,
                    fragment_index,
                    final_fragment,
                    content_fragment,
                } => {
                    assert_eq!(entry_index.value(), 3);
                    assert_eq!(fragment_index.value(), expected_fragment);
                    reconstructed.push_str(content_fragment.as_str());
                    expected_fragment += 1;
                    assert_eq!(*final_fragment, expected_fragment == 2);
                }
                message => {
                    return Err(io::Error::other(format!("unexpected message: {message:?}")).into());
                }
            }
        }
        assert_eq!(expected_fragment, 2);
        assert_eq!(reconstructed, text);

        let (mut writer, mut reader) = duplex(1_024);
        write_content(&mut writer, ProtocolVersion::One, request_id, 0, "").await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let frame = decode_server_line(&encoded)?;
        assert!(matches!(
            frame.message(),
            ServerMessage::TranscriptContent {
                fragment_index,
                final_fragment: true,
                content_fragment,
                ..
            } if fragment_index.value() == 0 && content_fragment.as_str().is_empty()
        ));
        Ok(())
    }

    #[tokio::test]
    async fn s28_imported_entries_map_only_to_conservative_shapes() -> Result<(), Box<dyn Error>> {
        let request_id = RequestId::try_new(11)?;
        let source_session = SessionId::from_uuid(Uuid::from_u128(1));
        let conversation = ImportedConversationId::from_uuid(Uuid::from_u128(2));
        let imported_entry = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(3));
        let semantic_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(4));
        let (mut writer, mut reader) = duplex(4_096);

        let source_attested = "source-attested";
        write_transcript_entry(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            &ProcessTranscriptEntry::ImportedText {
                entry_index: 0,
                source_session,
                entry: semantic_entry,
                imported_conversation: conversation,
                imported_entry,
                source_speaker: ProcessImportedSourceSpeaker::User,
                content: String::from(source_attested),
            },
        )
        .await?;
        write_transcript_entry(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            &ProcessTranscriptEntry::Imported {
                entry_index: 1,
                source_session,
                entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(5)),
                imported_conversation: conversation,
                imported_entry: ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(6)),
                source_speaker: ProcessImportedSourceSpeaker::NotAttested,
                content_kind: ProcessImportedContentKind::ToolResult,
            },
        )
        .await?;
        drop(writer);

        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let mut lines = encoded.split_inclusive(|byte| *byte == b'\n');
        let text = decode_server_line(
            lines
                .next()
                .ok_or_else(|| io::Error::other("missing imported text metadata"))?,
        )?;
        let ServerMessage::TranscriptTextEntry {
            entry:
                TranscriptTextEntry::Imported {
                    imported_conversation_id,
                    imported_entry_id,
                    source_speaker:
                        ImportedSourceSpeaker::Attested {
                            speaker: ImportedSpeaker::User,
                        },
                },
            ..
        } = text.message()
        else {
            panic!(
                "fixture expected imported text metadata, got {:?}",
                text.message()
            );
        };
        assert_eq!(
            imported_conversation_id.into_uuid(),
            conversation.into_uuid()
        );
        assert_eq!(imported_entry_id.into_uuid(), imported_entry.into_uuid());
        let content = decode_server_line(
            lines
                .next()
                .ok_or_else(|| io::Error::other("missing imported text content"))?,
        )?;
        let ServerMessage::TranscriptContent {
            final_fragment: true,
            content_fragment,
            ..
        } = content.message()
        else {
            panic!(
                "fixture expected imported text content, got {:?}",
                content.message()
            );
        };
        assert_eq!(content_fragment.as_str(), source_attested);
        assert!(matches!(
            decode_server_line(
                lines
                    .next()
                    .ok_or_else(|| io::Error::other("missing conservative imported entry"))?
            )?
            .message(),
            ServerMessage::TranscriptEntry {
                entry: TranscriptEntry::Imported {
                    source_speaker: ImportedSourceSpeaker::NotAttested {},
                    content_kind: ImportedContentKind::ToolResult,
                    ..
                },
                ..
            }
        ));
        assert!(lines.next().is_none());
        Ok(())
    }

    #[test]
    fn every_persistence_terminal_call_disposition_has_a_wire_projection() {
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::CancellationRequested),
            ModelCallState::CancellationRequested {}
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::Completed,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::Completed
            }
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::KnownFailed,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::KnownFailed
            }
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::Refused,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::Refused
            }
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::Cancelled,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::Cancelled
            }
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::Ambiguous,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::Ambiguous
            }
        );
    }

    #[test]
    fn goal_turn_retirement_projects_to_the_exact_wire_identity() {
        let turn = TurnId::from_uuid(Uuid::from_u128(7));
        let update = ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::TurnTerminal {
            turn,
            disposition: DispatchedTurnTerminalDisposition::Retired,
        })
        .expect("a client-visible event projects to one update");

        assert_eq!(
            update.wire().expect("the fixture event is representable"),
            SessionEvent::GoalTurnRetired {
                turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
            }
        );
    }

    /// INV-033: a recorded session settings change reaches the wire as its
    /// typed projection without losing either settings snapshot.
    #[test]
    fn inv033_session_model_settings_change_projects_to_the_closed_wire_shape() {
        let session = SessionId::from_uuid(Uuid::from_u128(1));
        let command = DurableCommandId::from_uuid(Uuid::from_u128(2));
        let prior_selection = DirectModelSelection::from_uuid(Uuid::from_u128(3));
        let installed_selection = DirectModelSelection::from_uuid(Uuid::from_u128(4));
        let prior_version = SessionConfigurationDefaultsVersion::first();
        let installed_version = prior_version
            .checked_next()
            .expect("the initial defaults version has a successor");
        let prior_settings = ValidatedModelSettings::provider_defaults();
        let inherited = ModelSettingsOverlay::inherit_all();
        let installed_precedence = ModelSettingsPrecedence::new(
            inherited,
            ModelSettingsOverlay::new(
                SettingOverlay::Value(ReasoningLevel::Low),
                FastModeOverlay::Inherit,
                SettingOverlay::Inherit,
            ),
            inherited,
            inherited,
        );
        let installed_settings = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::Low]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        )
        .validate_precedence(installed_selection, installed_precedence)
        .expect("the fixture capability admits low reasoning");
        let caller_override = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::Low),
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        );
        let changed = SessionModelSettingsChanged::try_new(
            session,
            command,
            prior_version,
            installed_version,
            ModelSelectionRequest::Direct(prior_selection),
            ModelSelectionRequest::Direct(installed_selection),
            prior_settings,
            installed_settings,
            caller_override,
            Vec::new(),
        )
        .expect("the fixture changes direct model selection");
        let changed_update = ProcessUpdateEvent::from_outbox(
            &DispatchedOutboxEventKind::SessionModelSettingsChanged(changed),
        )
        .expect("the fixture event projects onto an update");

        assert_eq!(
            changed_update
                .wire()
                .expect("the fixture event is representable"),
            SessionEvent::SessionModelSettingsChanged {
                command_id: signalbox_process_protocol::CommandId::try_from_uuid(
                    command.into_uuid(),
                )
                .expect("fixture command identity is admitted"),
                prior_defaults_version: CanonicalU64::new(prior_version.as_u64()),
                installed_defaults_version: CanonicalU64::new(installed_version.as_u64()),
                prior_model: signalbox_process_protocol::ModelSelection::Direct {
                    selection_id: CanonicalUuid::from_uuid(prior_selection.into_uuid()),
                },
                installed_model: signalbox_process_protocol::ModelSelection::Direct {
                    selection_id: CanonicalUuid::from_uuid(installed_selection.into_uuid()),
                },
                prior_settings: signalbox_process_protocol::ModelSettingsSnapshot {
                    precedence: signalbox_process_protocol::ModelSettingsPrecedence {
                        per_call: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                        session: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                        profile: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                        global_default:
                            signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                    },
                    effective: signalbox_process_protocol::EffectiveModelSettings {
                        reasoning_level: None,
                        fast_mode: signalbox_process_protocol::FastMode::Disabled,
                        service_tier: None,
                    },
                    reasoning_source: None,
                    fast_mode_source: None,
                    service_tier_source: None,
                    validated_for_selection_id: None,
                },
                installed_settings: signalbox_process_protocol::ModelSettingsSnapshot {
                    precedence: signalbox_process_protocol::ModelSettingsPrecedence {
                        per_call: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                        session: signalbox_process_protocol::ModelSettingsOverlay {
                            reasoning_level: signalbox_process_protocol::SettingOverlay::Value(
                                signalbox_process_protocol::ReasoningLevel::Low,
                            ),
                            fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
                            service_tier: signalbox_process_protocol::SettingOverlay::Inherit,
                        },
                        profile: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                        global_default:
                            signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                    },
                    effective: signalbox_process_protocol::EffectiveModelSettings {
                        reasoning_level: Some(signalbox_process_protocol::ReasoningLevel::Low),
                        fast_mode: signalbox_process_protocol::FastMode::Disabled,
                        service_tier: None,
                    },
                    reasoning_source: Some(signalbox_process_protocol::ModelSettingSource::Session,),
                    fast_mode_source: None,
                    service_tier_source: None,
                    validated_for_selection_id: Some(CanonicalUuid::from_uuid(
                        installed_selection.into_uuid(),
                    )),
                },
                caller_override: signalbox_process_protocol::ModelSettingsOverlay {
                    reasoning_level: signalbox_process_protocol::SettingOverlay::Value(
                        signalbox_process_protocol::ReasoningLevel::Low,
                    ),
                    fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
                    service_tier: signalbox_process_protocol::SettingOverlay::Inherit,
                },
                adjustments: Vec::new(),
            }
        );
    }

    /// INV-033: a recorded per-turn settings resolution reaches the wire with
    /// its requested alias and exact resolved-settings evidence.
    #[test]
    fn inv033_turn_model_settings_resolution_projects_to_the_closed_wire_shape() {
        let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(5));
        let turn = TurnId::from_uuid(Uuid::from_u128(6));
        let requested_alias = ModelAlias::from_uuid(Uuid::from_u128(3));
        let prior_selection = DirectModelSelection::from_uuid(Uuid::from_u128(2));
        let installed_selection = DirectModelSelection::from_uuid(Uuid::from_u128(4));
        let installed_version = SessionConfigurationDefaultsVersion::first();
        let caller_override = ModelSettingsOverlay::inherit_all();
        let session_settings = ModelSettingsOverlay::new(
            SettingOverlay::ProviderDefault,
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        );
        let precedence = ModelSettingsPrecedence::new(
            caller_override,
            session_settings,
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        );
        let settings = ModelCapabilities::new(
            BTreeSet::new(),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        )
        .validate_precedence(installed_selection, precedence)
        .expect("the explicit provider default is supported");
        let resolved = TurnModelSettingsResolved::try_new(
            accepted_input,
            turn,
            installed_version,
            FrozenModelSelection::FrozenAlias {
                alias: requested_alias,
                definition: FrozenAliasDefinition::selecting(installed_selection),
            },
            caller_override,
            settings,
            Some(prior_selection),
            vec![ModelChangeAdjustment::ReasoningLevelCleared {
                from: ReasoningLevel::High,
            }],
        )
        .expect("provider-default settings are valid for the fixture selection");
        let resolved_update = ProcessUpdateEvent::from_outbox(
            &DispatchedOutboxEventKind::TurnModelSettingsResolved(resolved),
        )
        .expect("the fixture event projects onto an update");

        assert_eq!(
            resolved_update
                .wire()
                .expect("the fixture event is representable"),
            SessionEvent::TurnModelSettingsResolved {
                accepted_input_id: CanonicalUuid::from_uuid(accepted_input.into_uuid()),
                turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
                defaults_version: CanonicalU64::new(installed_version.as_u64()),
                requested_model: signalbox_process_protocol::ModelSelection::Alias {
                    alias_id: CanonicalUuid::from_uuid(requested_alias.into_uuid()),
                },
                selected_direct_id: CanonicalUuid::from_uuid(installed_selection.into_uuid()),
                per_call_override: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                settings: signalbox_process_protocol::ModelSettingsSnapshot {
                    precedence: signalbox_process_protocol::ModelSettingsPrecedence {
                        per_call: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                        session: signalbox_process_protocol::ModelSettingsOverlay {
                            reasoning_level:
                                signalbox_process_protocol::SettingOverlay::ProviderDefault,
                            fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
                            service_tier: signalbox_process_protocol::SettingOverlay::Inherit,
                        },
                        profile: signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                        global_default:
                            signalbox_process_protocol::ModelSettingsOverlay::inherit_all(),
                    },
                    effective: signalbox_process_protocol::EffectiveModelSettings {
                        reasoning_level: None,
                        fast_mode: signalbox_process_protocol::FastMode::Disabled,
                        service_tier: None,
                    },
                    reasoning_source: Some(signalbox_process_protocol::ModelSettingSource::Session,),
                    fast_mode_source: None,
                    service_tier_source: None,
                    validated_for_selection_id: Some(CanonicalUuid::from_uuid(
                        installed_selection.into_uuid(),
                    )),
                },
                adjusted_from_selection_id: Some(CanonicalUuid::from_uuid(
                    prior_selection.into_uuid(),
                )),
                adjustments: vec![
                    signalbox_process_protocol::ModelChangeAdjustment::ReasoningLevelCleared {
                        from: signalbox_process_protocol::ReasoningLevel::High,
                    },
                ],
            }
        );
    }

    #[test]
    fn committed_process_foreground_wait_retries_follow_up_read_failure() {
        let disposition = preserve_committed_foreground_wait::<u8, _>(Err("database"));

        assert_eq!(disposition, CommittedForegroundDelivery::Retry("database"));
    }

    #[test]
    fn delegation_updates_project_but_internal_wakes_do_not_follow() {
        let spawning_request = signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(8));
        let child = SessionId::from_uuid(Uuid::from_u128(9));
        let update = ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::DelegationUpdate(
            signalbox_persistence::outbox::DispatchedDelegationUpdate::ChildSpawned {
                spawning_request,
                child,
                policy: signalbox_persistence::outbox::DispatchedDelegationPolicy::Background,
            },
        ))
        .expect("a delegation update is client-visible");

        assert_eq!(
            update.wire().expect("the fixture event is representable"),
            SessionEvent::ChildSpawned {
                spawning_request_id: CanonicalUuid::from_uuid(spawning_request.into_uuid()),
                child_session_id: CanonicalUuid::from_uuid(child.into_uuid()),
                relationship: signalbox_process_protocol::DelegationPolicy::Background {},
            }
        );
        assert!(
            ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::DelegationWake(
                signalbox_persistence::outbox::DispatchedDelegationWake::Result {
                    spawning_request,
                    awaiting_request: None,
                },
            ))
            .is_none()
        );
    }

    /// S17 / INV-032: committing an internal delivery wake makes the exact
    /// recipient eligible without projecting the wake onto follow streams.
    #[test]
    fn s17_inv032_internal_delegation_wake_nudges_exact_recipient() {
        let recipient = SessionId::from_uuid(Uuid::from_u128(10));
        let spawning_request = ToolRequestId::from_uuid(Uuid::from_u128(11));
        let nudge = RecordingEligibilityNudge::default();
        let recorded = Arc::clone(&nudge.sessions);

        nudge_delegation_wake(
            &nudge,
            recipient,
            &DispatchedOutboxEventKind::DelegationWake(
                signalbox_persistence::outbox::DispatchedDelegationWake::Result {
                    spawning_request,
                    awaiting_request: None,
                },
            ),
        );

        assert_eq!(
            recorded
                .lock()
                .expect("recorded nudge lock remains available")
                .as_slice(),
            &[recipient]
        );
    }

    #[test]
    fn completed_process_delegation_nudges_exact_issuer() {
        let issuer = SessionId::from_uuid(Uuid::from_u128(12));
        let nudge = RecordingEligibilityNudge::default();
        let recorded = Arc::clone(&nudge.sessions);

        nudge_delegation_issuer(&nudge, issuer);

        assert_eq!(
            recorded
                .lock()
                .expect("recorded nudge lock remains available")
                .as_slice(),
            &[issuer]
        );
    }

    #[test]
    fn definitive_process_message_rejection_nudges_exact_issuer() {
        let issuer = SessionId::from_uuid(Uuid::from_u128(17));
        let nudge = RecordingEligibilityNudge::default();
        let recorded = Arc::clone(&nudge.sessions);

        nudge_after_process_message_rejection(
            &nudge,
            issuer,
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::RelationshipNotFound,
            ),
        );

        assert_eq!(
            recorded
                .lock()
                .expect("recorded nudge lock remains available")
                .as_slice(),
            &[issuer]
        );
    }

    #[test]
    fn definitive_process_await_rejection_nudges_exact_issuer() {
        let issuer = SessionId::from_uuid(Uuid::from_u128(19));
        let nudge = RecordingEligibilityNudge::default();
        let recorded = Arc::clone(&nudge.sessions);

        nudge_after_process_await_rejection(
            &nudge,
            issuer,
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::DeliverySequenceExhausted,
            ),
        );

        assert_eq!(
            recorded
                .lock()
                .expect("recorded nudge lock remains available")
                .as_slice(),
            &[issuer]
        );
    }

    #[test]
    fn stale_process_await_rejection_does_not_nudge_issuer() {
        let issuer = SessionId::from_uuid(Uuid::from_u128(20));
        let nudge = RecordingEligibilityNudge::default();
        let recorded = Arc::clone(&nudge.sessions);

        nudge_after_process_await_rejection(
            &nudge,
            issuer,
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::AttemptEnded,
                },
            ),
        );

        assert_eq!(
            recorded
                .lock()
                .expect("recorded nudge lock remains available")
                .as_slice(),
            &[]
        );
    }

    #[test]
    fn stale_process_message_rejection_does_not_nudge_issuer() {
        let issuer = SessionId::from_uuid(Uuid::from_u128(18));
        let nudge = RecordingEligibilityNudge::default();
        let recorded = Arc::clone(&nudge.sessions);

        nudge_after_process_message_rejection(
            &nudge,
            issuer,
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::AttemptEnded,
                },
            ),
        );

        assert_eq!(
            recorded
                .lock()
                .expect("recorded nudge lock remains available")
                .as_slice(),
            &[]
        );
    }

    #[tokio::test]
    async fn foreground_delegation_peer_disconnect_abandons_socket_wait() {
        let (peer, daemon) = duplex(64);
        let mut reader = BufReader::new(daemon);
        drop(peer);

        let error = foreground_peer_activity(&mut reader)
            .await
            .expect_err("a disconnected foreground peer ends its socket wait");
        let source = error
            .source()
            .expect("peer failure retains its I/O source")
            .downcast_ref::<io::Error>()
            .expect("peer failure source is I/O");

        assert_eq!(source.kind(), io::ErrorKind::ConnectionAborted);
    }

    #[test]
    fn delegated_process_rejections_use_typed_wire_details() {
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(13));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(14));
        let request_id = CanonicalUuid::from_uuid(Uuid::from_u128(15));
        let peer_id = CanonicalUuid::from_uuid(Uuid::from_u128(16));
        let relationship = process_delegation_rejection(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::RelationshipNotFound,
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let session_not_found = process_delegation_rejection(
            ProcessDelegationRequestRejection::SessionNotFound,
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let await_conflict = process_delegation_rejection(
            ProcessDelegationRequestRejection::AwaitConflict,
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let message_conflict = process_delegation_rejection(
            ProcessDelegationRequestRejection::MessageConflict,
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let awaiting_approval = process_delegation_rejection(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::AwaitingApproval,
                },
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let denied = process_delegation_rejection(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::Denied,
                },
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let approved = process_delegation_rejection(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::Approved,
                },
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let prepared = process_delegation_rejection(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::Prepared,
                },
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let closed = process_delegation_rejection(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::Closed,
                },
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
        );
        let attempt_ended = process_delegation_rejection(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::StaleDispatch {
                    state: DelegationRequestExecutionState::AttemptEnded,
                },
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
        );

        assert_eq!(
            session_not_found.detail,
            ErrorDetail::rejected(RejectionDetail::SessionNotFound { session_id })
        );
        assert_eq!(
            relationship.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationRelationNotFound {
                session_id,
                peer_session_id: peer_id,
            })
        );
        assert_eq!(
            await_conflict.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationAwaitConflict {
                tool_request_id: request_id,
            })
        );
        assert_eq!(
            message_conflict.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationMessageConflict {
                tool_request_id: request_id,
            })
        );
        assert_eq!(
            awaiting_approval.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: request_id,
                state: WireDelegationToolRequestState::AwaitingApproval,
            })
        );
        assert_eq!(
            denied.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: request_id,
                state: WireDelegationToolRequestState::Denied,
            })
        );
        assert_eq!(
            approved.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: request_id,
                state: WireDelegationToolRequestState::Approved,
            })
        );
        assert_eq!(
            prepared.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: request_id,
                state: WireDelegationToolRequestState::Prepared,
            })
        );
        assert_eq!(
            closed.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: request_id,
                state: WireDelegationToolRequestState::Closed,
            })
        );
        assert_eq!(
            attempt_ended.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: request_id,
                state: WireDelegationToolRequestState::AttemptEnded,
            })
        );
    }

    #[test]
    fn delivery_sequence_exhaustion_names_the_operation_recipient() {
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(13));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(14));
        let request_id = CanonicalUuid::from_uuid(Uuid::from_u128(15));
        let peer_id = CanonicalUuid::from_uuid(Uuid::from_u128(16));
        let recipient_id = CanonicalUuid::from_uuid(Uuid::from_u128(17));
        let error = process_delegation_rejection_for_recipient(
            ProcessDelegationRequestRejection::Operation(
                DelegationOperationRejection::DeliverySequenceExhausted,
            ),
            session_id,
            turn_id,
            request_id,
            peer_id,
            recipient_id,
        );

        assert_eq!(
            error.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationDeliverySequenceExhausted {
                recipient_session_id: recipient_id,
                last: CanonicalU64::new(u64::MAX),
            })
        );
    }

    #[test]
    fn message_identity_collision_preserves_the_minted_identity() {
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(13));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(14));
        let request_id = CanonicalUuid::from_uuid(Uuid::from_u128(15));
        let peer_id = CanonicalUuid::from_uuid(Uuid::from_u128(16));
        let message = DelegationMessageId::from_uuid(Uuid::from_u128(17));
        let error = process_delegation_rejection(
            ProcessDelegationRequestRejection::MessageIdentityCollision { message },
            session_id,
            turn_id,
            request_id,
            peer_id,
        );

        assert_eq!(
            error.detail,
            ErrorDetail::rejected(RejectionDetail::DelegationMessageIdentityCollision {
                message_id: wire_uuid(message.into_uuid()),
            })
        );
    }

    #[test]
    fn cancellation_and_reconciliation_project_to_exact_wire_shapes() {
        let turn = TurnId::from_uuid(Uuid::from_u128(1));
        let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(2));
        let call = ModelCallId::from_uuid(Uuid::from_u128(3));
        let entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(4));
        let frontier = ContextFrontierId::from_uuid(Uuid::from_u128(5));

        assert_eq!(
            wire_turn_state(&ProcessTurnState::Cancelled {
                terminal_frontier: frontier,
                terminal_attempt: attempt,
                terminal_call: Some(call),
            }),
            TurnState::Cancelled {
                terminal_frontier_id: CanonicalUuid::from_uuid(frontier.into_uuid()),
                terminal_attempt_id: CanonicalUuid::from_uuid(attempt.into_uuid()),
                terminal_model_call_id: Some(CanonicalUuid::from_uuid(call.into_uuid())),
            }
        );
        assert_eq!(
            wire_turn_state(&ProcessTurnState::ReconciliationRequired {
                terminal_frontier: frontier,
                terminal_attempt: attempt,
                operation: ProcessReconciliationOperation::ModelCall(call),
            }),
            TurnState::ReconciliationRequired {
                terminal_frontier_id: CanonicalUuid::from_uuid(frontier.into_uuid()),
                terminal_attempt_id: CanonicalUuid::from_uuid(attempt.into_uuid()),
                terminal_model_call_id: CanonicalUuid::from_uuid(call.into_uuid()),
            }
        );

        let cancelled = ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::TurnTerminal {
            turn,
            disposition: DispatchedTurnTerminalDisposition::Cancelled {
                cancellation_entry: entry,
                terminal_frontier: frontier,
            },
        })
        .expect("a client-visible event projects to one update");
        assert_eq!(
            cancelled
                .wire()
                .expect("the fixture event is representable"),
            SessionEvent::TurnCancelled {
                turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
                cancellation_entry_id: CanonicalUuid::from_uuid(entry.into_uuid()),
                terminal_frontier_id: CanonicalUuid::from_uuid(frontier.into_uuid()),
            }
        );
        let reconciliation =
            ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::TurnTerminal {
                turn,
                disposition: DispatchedTurnTerminalDisposition::ReconciliationRequired {
                    operation: DispatchedReconciliationOperation::ModelCall(call),
                    terminal_frontier: frontier,
                },
            })
            .expect("a client-visible event projects to one update");
        assert_eq!(
            reconciliation
                .wire()
                .expect("the fixture event is representable"),
            SessionEvent::TurnReconciliationRequired {
                turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
                model_call_id: CanonicalUuid::from_uuid(call.into_uuid()),
                terminal_frontier_id: CanonicalUuid::from_uuid(frontier.into_uuid()),
            }
        );
        let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(6));
        let recovery =
            ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::ToolBatchTransition {
                turn,
                producing_call: call,
                state: DispatchedToolBatchState::RecoveryRequired {
                    attempt: tool_attempt,
                },
            })
            .expect("a client-visible event projects to one update");
        assert_eq!(
            recovery.wire().expect("the fixture event is representable"),
            SessionEvent::ToolBatchTransition {
                turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
                model_call_id: CanonicalUuid::from_uuid(call.into_uuid()),
                state: ToolBatchState::RecoveryRequired {
                    tool_attempt_id: CanonicalUuid::from_uuid(tool_attempt.into_uuid()),
                },
            }
        );
    }

    /// INV-032 / INV-044: the daemon preserves every bounded runner-placement
    /// fact while projecting one dispatched outbox transition to the wire.
    #[test]
    fn inv032_inv044_runner_state_transition_projects_to_the_closed_wire_shape() {
        let runner = RunnerId::from_uuid(Uuid::from_u128(7));
        let placement_revision =
            RunnerGeneration::try_from_u64(9).expect("the fixture revision is positive");
        let working_directory = RunnerWorkingDirectory::try_new("workspace/project".to_owned())
            .expect("the fixture directory is bounded exact text");
        let expected_working_directory = working_directory.as_str().to_owned();
        let update =
            ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::RunnerStateTransition {
                runner,
                placement_revision,
                sandbox: signalbox_domain::RunnerSandboxProfile::WorkspaceRestricted,
                working_directory: Some(working_directory),
                state: DispatchedRunnerState::WorkingDirectoryChanged,
            })
            .expect("a client-visible runner event projects to one update");

        assert_eq!(
            update.wire().expect("the fixture event is representable"),
            SessionEvent::RunnerStateTransition {
                runner_id: CanonicalUuid::from_uuid(runner.into_uuid()),
                placement_revision: WireRunnerPlacementRevision::try_new(placement_revision.get(),)
                    .expect("the fixture placement revision is positive"),
                sandbox_profile: WireRunnerSandboxProfile::WorkspaceRestricted,
                working_directory: Some(
                    WireRunnerWorkingDirectory::try_new(expected_working_directory)
                        .expect("the fixture wire directory is valid"),
                ),
                state: WireRunnerStateTransitionState::WorkingDirectoryChanged,
            }
        );
    }

    #[test]
    fn finish_condition_wire_union_admits_both_domain_variants() {
        let statement = signalbox_domain::FinishConditionStatement::try_new(String::from(
            "the branch is green",
        ))
        .expect("the fixture statement is admitted");
        assert_eq!(
            domain_finish_condition(WireFinishCondition::ExternalGate)
                .expect("the external gate is admitted"),
            signalbox_domain::FinishCondition::ExternalGate
        );
        assert_eq!(
            domain_finish_condition(WireFinishCondition::Declared {
                statement: statement.as_str().to_owned(),
            })
            .expect("the declared condition is admitted"),
            signalbox_domain::FinishCondition::Declared(statement)
        );
    }
}
