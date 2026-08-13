//! Local process-protocol serving and durable outbox fan-out.

use std::{
    error::Error,
    fmt,
    future::Future,
    io::{self, SeekFrom},
    sync::Arc,
    time::Duration,
};

use sha2::{Digest, Sha256};
use signalbox_application::{
    ClassifyOperatorFailure, ConversationListCursor, ConversationListItem, ConversationListQuery,
    ConversationOriginFilter, ConversationPageReader, CreateSessionError,
    CreateSessionFromImportedFrontierOutcome, CreateSessionFromImportedFrontierRequest,
    CreateSessionFromImportedFrontierService, CreateSessionOutcome, CreateSessionRequest,
    CreateSessionService, DecideToolRequestService, EligibilityNudge, ImportConversationError,
    ImportConversationOutcome, ImportConversationService, ImportedConversationConverter,
    InProcessEligibilityNudge, InProcessToolDispatchGate, ListConversationsService,
    ListSessionMetadataService, LoadSessionMetadataService, OperatorFailureClass,
    PromptMemberStatement, ReplaceSessionDefaultsOutcome, ReplaceSessionDefaultsRequest,
    ReplaceSessionDefaultsService, ReplaceSessionMetadataOutcome, ReplaceSessionMetadataRequest,
    ReplaceSessionMetadataService, ReviewPassCompletionStatus, ReviewWorkflowCommand,
    ReviewWorkflowCommandOutcome, ReviewWorkflowCommandResult, ReviewWorkflowCommandService,
    ReviewWorkflowOperation, ReviewWorkflowOperationKind, SessionMetadataListItem,
    SessionMetadataListQuery, SubmitInputOutcome, SubmitInputRequest, SubmitInputService,
    SubmitInputTransaction, UpdateSessionPlacementOutcome, UpdateSessionPlacementRequest,
    UpdateSessionPlacementService, UuidV7CreateSessionFromImportedFrontierIdGenerator,
    UuidV7ImportedConversationIdGenerator, UuidV7SessionIdGenerator, UuidV7SubmitInputIdGenerator,
    UuidV7ToolLoopIdGenerator,
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
    AcceptedInputId, Actor, CancelledModelCallTurnIdentities, ContextCompactionId,
    ContextCompactionTokenUsage, ContextFrontierId, DangerousToolAutoApproval, DecideToolRequest,
    DecideToolRequestRejectedResult, DecideToolRequestResult,
    DelegationMessageDirection as DomainDelegationMessageDirection,
    DelegationOutcomeKind as DomainDelegationOutcomeKind,
    DelegationOutcomeReason as DomainDelegationOutcomeReason,
    DelegationProvenance as DomainDelegationProvenance, DelegationWait,
    DelegationWaitMode as DomainDelegationWaitMode, DeliveryRequest, DescendantTerminationScope,
    DirectModelSelection, DurableCommandId, FastMode as DomainFastMode,
    FastModeOverlay as DomainFastModeOverlay, FrozenModelSelection, Goal, GoalBlockProvenance,
    GoalBlockedReasonKind, GoalCommandRejection as DomainGoalCommandRejection, GoalCommandResult,
    GoalEvent, GoalEventKind, GoalGuidance, GoalState, GoalStatement, GoalUserAction,
    GoalUserCommand, ImportedConversation, ImportedConversationFormat, ImportedConversationId,
    ImportedSessionRelationship as DomainImportedSessionRelationship, ImportedSourceAttestation,
    ImportedSpeaker as DomainImportedSpeaker, ImportedTranscriptContent,
    ImportedTranscriptPosition, ModelAlias, ModelCallId,
    ModelChangeAdjustment as DomainModelChangeAdjustment, ModelSelectionOverride,
    ModelSelectionRequest, ModelSettingSource as DomainModelSettingSource,
    ModelSettingsOverlay as DomainModelSettingsOverlay,
    ModelSettingsPrecedence as DomainModelSettingsPrecedence, ParentTerminationCommandSource,
    PerInputConfigurationChoices, ReasoningLevel as DomainReasoningLevel,
    ReplaceSessionDefaults as DomainReplaceSessionDefaults, ReplaceSessionDefaultsRejectedResult,
    ReplaceSessionDefaultsResult, ReplaceSessionMetadataRejectedResult,
    ReplaceSessionMetadataResult, ReviewChangeRequestNumber, ReviewConfidence, ReviewEventOrdinal,
    ReviewExternalLink, ReviewExternalLinkAssociation, ReviewExternalLinkAttachment,
    ReviewExternalLinkAttachmentResult, ReviewExternalLinkId, ReviewExternalObjectKind,
    ReviewFinding, ReviewFindingConfidenceAxes, ReviewFindingContent, ReviewFindingDiffSide,
    ReviewFindingEvent, ReviewFindingEventKind, ReviewFindingEventResult,
    ReviewFindingEventResultKind, ReviewFindingId, ReviewFindingLocation,
    ReviewFindingPendingExternalLinkRef, ReviewFindingProposal, ReviewFindingRef,
    ReviewFindingSeverity, ReviewKey, ReviewLineRange, ReviewPass, ReviewPassAcceptedInputEvidence,
    ReviewPassEvidence, ReviewPassId, ReviewPassKind, ReviewPassRef, ReviewPassResult,
    ReviewPassState, ReviewPassTurnEvidence, ReviewPassTurnOutcome, ReviewPolicy,
    ReviewProducedFindings, ReviewReferencedFindingEvidence, ReviewRun, ReviewRunId, ReviewRunRef,
    ReviewRunState, ReviewTarget, ReviewTargetId, ReviewTargetSubject, ReviewText,
    ReviewWorkflowKind, RunnerSandboxProfile as DomainRunnerSandboxProfile, RunnerSelector,
    SemanticTranscriptEntryId, ServiceTier as DomainServiceTier, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionId, SessionMetadataContent,
    SessionMetadataLastWriter, SessionMetadataSnapshot,
    SessionModelSettingsChanged as DomainSessionModelSettingsChanged,
    SessionPlacement as DomainSessionPlacement, SessionPlacementPath, SessionPlacementVersion,
    SessionTemplateName, SessionTemplateProvenance, SettingOverlay as DomainSettingOverlay,
    SubmitInput, SubmitInputAppliedResult, SubmitInputRejectedResult, SubmitInputResult,
    ToolApprovalDecision, ToolDenialReason, ToolRequestId, TurnId,
    TurnModelSettingsResolved as DomainTurnModelSettingsResolved, UnsupportedModelSetting,
    UpdateSessionPlacementRejectionKind, UpdateSessionPlacementResult, UserContent,
    ValidatedModelSettings,
};
use signalbox_model_provider_runtime::{
    ContextCompactionModel, ContextCompactionModelError, ContextCompactionModelRequest,
    ProviderTextDelta, ProviderTextDeltaSink,
};
use signalbox_persistence::{
    blob::BlobCatalogRepository,
    context_compaction::{
        AppliedContextCompaction, ContextCompactionCommandLookup, ContextCompactionRepository,
        ContextCompactionRepositoryError, FailedContextCompactionDisposition,
        PrepareContextCompactionOutcome, PrepareContextCompactionRequest,
        PreparedContextCompaction,
    },
    conversation_import::{ImportedConversationRepository, ImportedConversationRepositoryError},
    conversation_listing::{ConversationListingRepository, ConversationListingRepositoryError},
    create_session::{CreateSessionRepository, CreateSessionRepositoryError},
    create_session_from_imported_frontier::{
        ImportedSessionRepository, ImportedSessionRepositoryError,
    },
    goal::{GoalCommandHandlingOutcome, GoalRepository, GoalRepositoryError},
    goal_turn::GoalTurnCandidates,
    model_execution::{ModelCallRepositoryError, PostgresModelCallRepository},
    outbox::{
        DispatchedBoundChildAction, DispatchedDelegationOutcome, DispatchedDelegationPolicy,
        DispatchedDelegationProvenance, DispatchedDelegationReason, DispatchedDelegationUpdate,
        DispatchedDelegationWaitMode, DispatchedModelCallDisposition, DispatchedModelCallState,
        DispatchedOutboxEvent, DispatchedOutboxEventKind, DispatchedReconciliationOperation,
        DispatchedRunnerState, DispatchedToolBatchState, OutboxDeliveryDecision,
        OutboxDispatchError, OutboxDispatchOutcome, OutboxDispatcher,
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
    review_workflow::{ReviewWorkflowStore, ReviewWorkflowStoreError},
    session_delegation::{
        DelegationOperationRejection, DelegationRequestExecutionState, ProcessDelegationOutcome,
        ProcessDelegationRequestRejection,
    },
    session_metadata::{SessionMetadataRepository, SessionMetadataRepositoryError},
    session_placement::{SessionPlacementRepository, SessionPlacementRepositoryError},
    submit_input::{SubmitInputHandlingOutcome, SubmitInputRepository, SubmitInputRepositoryError},
    tool_loop::{PostgresToolLoopRepository, ToolLoopRepositoryError},
};
use signalbox_process_protocol::{
    BillingRateVersion, BoundChildAction as WireBoundChildAction, BulkIngestKind,
    CanonicalBlobDigest, CanonicalDollarAmount, CanonicalU64, CanonicalUuid, ClientRequest,
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
    FastMode as WireFastMode, FastModeOverlay as WireFastModeOverlay, FrameDecodeErrorKind,
    FrameEncodeError, GoalBlockedProvenance as WireGoalBlockedProvenance,
    GoalBlockedReason as WireGoalBlockedReason, GoalCommandRejection as WireGoalCommandRejection,
    GoalHistoryEvent, GoalLifecycleState, ImportedContentKind,
    ImportedConversationSourceFormat as WireImportedConversationSourceFormat,
    ImportedSessionRelationship as WireImportedSessionRelationship, ImportedSourceSpeaker,
    ImportedSpeaker, ImportedTextPreview, InputContent, InputDelivery, MAX_FRAME_BYTES,
    MetadataActor, MetadataLastWriter, ModelCallCostLabel, ModelCallDisposition,
    ModelCallDollarCost, ModelCallState, ModelCallTokenUsage,
    ModelCapabilities as WireModelCapabilities, ModelChangeAdjustment as WireModelChangeAdjustment,
    ModelSelection as WireModelSelection, ModelSettingSource as WireModelSettingSource,
    ModelSettingsOverlay as WireModelSettingsOverlay,
    ModelSettingsPrecedence as WireModelSettingsPrecedence,
    ModelSettingsSnapshot as WireModelSettingsSnapshot, PositiveCanonicalU64, ProtocolVersion,
    ReasoningLevel as WireReasoningLevel, RejectionDetail, RequestId,
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
    ServiceTier as WireServiceTier, SessionEvent, SessionMetadata as WireSessionMetadata,
    SessionPlacement as WireSessionPlacement, SettingOverlay as WireSettingOverlay,
    SystemPromptMember, SystemPromptText, ToolApprovalEventDecider as WireToolApprovalEventDecider,
    ToolApprovalEventDecision as WireToolApprovalEventDecision, ToolBatchState, ToolDecision,
    TranscriptEntry, TranscriptTextEntry, TranscriptToolApproval,
    TurnModelSettingsSnapshot as WireTurnModelSettingsSnapshot, TurnState, UsageProvenance,
    content_fragments, decode_client_line, encode_server_line,
    recover_bounded_client_protocol_version, recover_bounded_client_request_id,
};
use signalbox_tools_sessions::{AwaitSessionPortOutcome, DeliveredChildResult};
use sqlx::{PgPool, Row};
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt,
        BufReader,
    },
    net::UnixStream,
    sync::{OwnedSemaphorePermit, Semaphore, broadcast, watch},
    task::{JoinError, JoinSet},
    time::{Instant, sleep, sleep_until},
};

use crate::telemetry::{ModelMetricDisposition, TelemetryMetrics, TurnMetricOutcome};
use crate::{
    BlobStoreRegistry, FatalRecoveryReporter, HubModelConfiguration, LocalProcessListener,
    LocalSocketError, SessionTemplateConfiguration,
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
const BULK_INGEST_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const BULK_INGEST_SESSION_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const INBOUND_READ_AHEAD_BYTES: usize = 8 * 1024;
const MAX_SUBMITTED_INPUT_BYTES: usize = 1024 * 1024;
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
    model_configuration: Arc<HubModelConfiguration>,
    context_compaction_model: Arc<dyn ContextCompactionModel>,
    template_configuration: Arc<SessionTemplateConfiguration>,
    fanouts: ProcessFanouts,
    inbound_frame_budgets: InboundFrameBudgets,
    import_budget: Arc<Semaphore>,
    import_waiter_budget: Arc<Semaphore>,
    review_command_budget: Arc<Semaphore>,
    snapshot_reader_budget: Arc<Semaphore>,
    blob_store_registry: Option<Arc<BlobStoreRegistry>>,
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

/// The hub-owned local protocol runtime: one outbox dispatcher, one bounded
/// durable and streaming fan-outs, and one guarded Unix listener.
#[derive(Debug)]
pub struct ProcessRuntime {
    recovery_reporter: Option<FatalRecoveryReporter>,
    listener: LocalProcessListener,
    pool: PgPool,
    eligibility_nudge: InProcessEligibilityNudge,
    tool_dispatch_gate: InProcessToolDispatchGate,
    model_configuration: HubModelConfiguration,
    context_compaction_model: Arc<dyn ContextCompactionModel>,
    template_configuration: SessionTemplateConfiguration,
    fanouts: ProcessFanouts,
    metrics: Option<TelemetryMetrics>,
    blob_store_registry: Option<Arc<BlobStoreRegistry>>,
}

#[derive(Clone, Debug)]
struct ProcessFanouts {
    durable: broadcast::Sender<ProcessUpdate>,
    streaming: broadcast::Sender<ProcessUpdate>,
}

impl ProcessRuntime {
    /// Composes the guarded listener, fenced database, nudge, and static models.
    pub fn new(
        listener: LocalProcessListener,
        pool: PgPool,
        eligibility_nudge: InProcessEligibilityNudge,
        tool_dispatch_gate: InProcessToolDispatchGate,
        model_configuration: HubModelConfiguration,
    ) -> Self {
        Self::new_with_templates(
            listener,
            pool,
            eligibility_nudge,
            tool_dispatch_gate,
            model_configuration,
            SessionTemplateConfiguration::default(),
        )
    }

    /// Composes the guarded runtime with startup-resolved session templates.
    pub fn new_with_templates(
        listener: LocalProcessListener,
        pool: PgPool,
        eligibility_nudge: InProcessEligibilityNudge,
        tool_dispatch_gate: InProcessToolDispatchGate,
        model_configuration: HubModelConfiguration,
        template_configuration: SessionTemplateConfiguration,
    ) -> Self {
        let (durable_updates, _) = broadcast::channel(PROCESS_UPDATE_CAPACITY);
        let (streaming_updates, _) = broadcast::channel(PROCESS_UPDATE_CAPACITY);
        Self {
            recovery_reporter: None,
            listener,
            pool,
            eligibility_nudge,
            tool_dispatch_gate,
            model_configuration,
            context_compaction_model: Arc::new(UnavailableContextCompactionModel),
            template_configuration,
            metrics: None,
            blob_store_registry: None,
            fanouts: ProcessFanouts {
                durable: durable_updates,
                streaming: streaming_updates,
            },
        }
    }

    /// Returns the nonblocking sink that places already-redacted provider text
    /// on this runtime incarnation's ordered follow fan-out.
    /// Installs the dedicated summary-call adapter used by explicit and automatic compaction.
    pub fn with_context_compaction_model(
        mut self,
        model: impl ContextCompactionModel + 'static,
    ) -> Self {
        self.context_compaction_model = Arc::new(model);
        self
    }

    /// Installs the handle raising the daemon's fatal recovery signal.
    ///
    /// A connection handler has no execution role, so without this a durable
    /// outcome it cannot decide would end at the client response and nothing
    /// would stop the process for the next incarnation's startup scan.
    #[must_use]
    pub fn with_recovery_reporter(mut self, reporter: FatalRecoveryReporter) -> Self {
        self.recovery_reporter = Some(reporter);
        self
    }

    /// Installs the private Prometheus counters fed by durable outbox events.
    #[must_use]
    pub fn with_metrics(mut self, metrics: TelemetryMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Installs the startup-authenticated immutable-blob registry.
    #[must_use]
    pub fn with_blob_store_registry(mut self, registry: Arc<BlobStoreRegistry>) -> Self {
        self.blob_store_registry = Some(registry);
        self
    }

    pub fn provider_text_delta_sink(&self) -> ProcessProviderTextDeltaSink {
        ProcessProviderTextDeltaSink {
            updates: self.fanouts.streaming.clone(),
        }
    }

    /// Serves requests and dispatches durable updates until `shutdown` changes
    /// to true or its sender closes.
    pub async fn run(self, shutdown: watch::Receiver<bool>) -> Result<(), ProcessRuntimeError> {
        let fanouts = self.fanouts;
        let connection_dependencies = ConnectionDependencies {
            recovery_reporter: self.recovery_reporter,
            pool: self.pool.clone(),
            eligibility_nudge: self.eligibility_nudge.clone(),
            tool_dispatch_gate: self.tool_dispatch_gate,
            model_configuration: self.model_configuration,
            context_compaction_model: self.context_compaction_model,
            template_configuration: self.template_configuration,
            fanouts: fanouts.clone(),
            blob_store_registry: self.blob_store_registry,
        };
        let server = serve_connections(&self.listener, connection_dependencies, shutdown.clone());
        let dispatcher = dispatch_updates(
            self.pool,
            self.eligibility_nudge,
            fanouts,
            self.metrics,
            shutdown,
        );
        let result = tokio::try_join!(server, dispatcher);
        let cleanup = self.listener.cleanup();

        result?;
        cleanup.map_err(ProcessRuntimeError::CleanupSocket)
    }
}

/// Daemon-owned nonblocking bridge from provider observations to follow streams.
#[derive(Clone, Debug)]
pub struct ProcessProviderTextDeltaSink {
    updates: broadcast::Sender<ProcessUpdate>,
}

impl ProviderTextDeltaSink for ProcessProviderTextDeltaSink {
    fn publish(&self, delta: ProviderTextDelta) {
        let _ = self.updates.send(ProcessUpdate::ProviderTextDelta(delta));
    }
}

async fn dispatch_updates(
    pool: PgPool,
    eligibility_nudge: InProcessEligibilityNudge,
    fanouts: ProcessFanouts,
    metrics: Option<TelemetryMetrics>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessRuntimeError> {
    let dispatcher = OutboxDispatcher::new(pool);
    let mut last_metric_sequence = None;
    loop {
        if shutdown_requested(&shutdown) {
            return Ok(());
        }
        let outcome = dispatcher
            .dispatch_next(|event| {
                observe_outbox_metrics_once(
                    metrics.as_ref(),
                    &mut last_metric_sequence,
                    event.sequence(),
                    event.kind(),
                );
                nudge_delegation_wake(&eligibility_nudge, event.session(), event.kind());
                if let Some(update) = ProcessUpdate::from_outbox(event) {
                    let _ = fanouts.durable.send(update.clone());
                    let _ = fanouts.streaming.send(update);
                }
                OutboxDeliveryDecision::Delivered
            })
            .await
            .map_err(ProcessRuntimeError::Dispatch)?;
        match outcome {
            OutboxDispatchOutcome::Delivered { .. } => {}
            OutboxDispatchOutcome::Idle => {
                tokio::select! {
                    () = wait_for_shutdown(&mut shutdown) => return Ok(()),
                    () = sleep(OUTBOX_IDLE_POLL_INTERVAL) => {}
                }
            }
            OutboxDispatchOutcome::Retry { .. } => {
                return Err(ProcessRuntimeError::UnexpectedDispatcherRetry);
            }
        }
    }
}

fn nudge_delegation_wake(
    eligibility_nudge: &impl EligibilityNudge,
    session: SessionId,
    event: &DispatchedOutboxEventKind,
) {
    if matches!(event, DispatchedOutboxEventKind::DelegationWake(_)) {
        let _ = eligibility_nudge.nudge(session);
    }
}

fn nudge_delegation_issuer(eligibility_nudge: &impl EligibilityNudge, session: SessionId) {
    let _ = eligibility_nudge.nudge(session);
}

fn observe_outbox_metrics_once(
    metrics: Option<&TelemetryMetrics>,
    last_sequence: &mut Option<u64>,
    sequence: u64,
    event: &DispatchedOutboxEventKind,
) {
    if *last_sequence == Some(sequence) {
        return;
    }
    *last_sequence = Some(sequence);
    observe_outbox_metrics(metrics, event);
}

fn observe_outbox_metrics(metrics: Option<&TelemetryMetrics>, event: &DispatchedOutboxEventKind) {
    let Some(metrics) = metrics else {
        return;
    };
    match event {
        DispatchedOutboxEventKind::TurnActivated { .. } => metrics.observe_turn_started(),
        DispatchedOutboxEventKind::TurnCompleted { .. } => {
            metrics.observe_turn_terminal(TurnMetricOutcome::Completed);
        }
        DispatchedOutboxEventKind::TurnFailed { .. } => {
            metrics.observe_turn_terminal(TurnMetricOutcome::Failed);
        }
        DispatchedOutboxEventKind::TurnRefused { .. } => {
            metrics.observe_turn_terminal(TurnMetricOutcome::Refused);
        }
        DispatchedOutboxEventKind::TurnCancelled { .. } => {
            metrics.observe_turn_terminal(TurnMetricOutcome::Cancelled);
        }
        DispatchedOutboxEventKind::TurnReconciliationRequired { .. } => {
            metrics.observe_turn_terminal(TurnMetricOutcome::ReconciliationRequired);
        }
        DispatchedOutboxEventKind::ModelCallTransition { state, .. } => {
            observe_model_call_metrics(metrics, *state);
        }
        DispatchedOutboxEventKind::SessionCreated
        | DispatchedOutboxEventKind::SessionModelSettingsChanged(_)
        | DispatchedOutboxEventKind::TurnModelSettingsResolved(_)
        | DispatchedOutboxEventKind::InputAccepted { .. }
        | DispatchedOutboxEventKind::GoalTurnRetired { .. }
        | DispatchedOutboxEventKind::ToolBatchTransition { .. }
        | DispatchedOutboxEventKind::RunnerStateTransition { .. }
        | DispatchedOutboxEventKind::ContextCompacted { .. }
        | DispatchedOutboxEventKind::DelegationUpdate(_)
        | DispatchedOutboxEventKind::ToolApprovalDecided { .. }
        | DispatchedOutboxEventKind::DelegationWake(_) => {}
    }
}

fn observe_model_call_metrics(metrics: &TelemetryMetrics, state: DispatchedModelCallState) {
    let disposition = match state {
        DispatchedModelCallState::Terminal(disposition) => disposition,
        DispatchedModelCallState::Prepared
        | DispatchedModelCallState::InFlight
        | DispatchedModelCallState::CancellationRequested => return,
    };
    let disposition = match disposition {
        DispatchedModelCallDisposition::Completed => ModelMetricDisposition::Completed,
        DispatchedModelCallDisposition::KnownFailed => ModelMetricDisposition::KnownFailed,
        DispatchedModelCallDisposition::Refused => ModelMetricDisposition::Refused,
        DispatchedModelCallDisposition::Cancelled => ModelMetricDisposition::Cancelled,
        DispatchedModelCallDisposition::Ambiguous => ModelMetricDisposition::Ambiguous,
    };
    metrics.observe_model_terminal(disposition);
}

struct ConnectionDependencies {
    recovery_reporter: Option<FatalRecoveryReporter>,
    pool: PgPool,
    eligibility_nudge: InProcessEligibilityNudge,
    tool_dispatch_gate: InProcessToolDispatchGate,
    model_configuration: HubModelConfiguration,
    context_compaction_model: Arc<dyn ContextCompactionModel>,
    template_configuration: SessionTemplateConfiguration,
    fanouts: ProcessFanouts,
    blob_store_registry: Option<Arc<BlobStoreRegistry>>,
}

async fn serve_connections(
    listener: &LocalProcessListener,
    dependencies: ConnectionDependencies,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessRuntimeError> {
    let snapshot_reader_capacity =
        snapshot_reader_capacity(dependencies.pool.options().get_max_connections())
            .ok_or(ProcessRuntimeError::InsufficientPoolCapacity)?;
    let services = ConnectionServices {
        recovery_reporter: dependencies.recovery_reporter,
        pool: dependencies.pool,
        eligibility_nudge: dependencies.eligibility_nudge,
        tool_dispatch_gate: dependencies.tool_dispatch_gate,
        model_configuration: Arc::new(dependencies.model_configuration),
        context_compaction_model: dependencies.context_compaction_model,
        template_configuration: Arc::new(dependencies.template_configuration),
        fanouts: dependencies.fanouts,
        inbound_frame_budgets: InboundFrameBudgets::new(),
        import_budget: Arc::new(Semaphore::new(MAX_CONCURRENT_IMPORTS)),
        import_waiter_budget: Arc::new(Semaphore::new(MAX_IMPORT_ADMISSION_WAITERS)),
        review_command_budget: Arc::new(Semaphore::new(MAX_CONCURRENT_REVIEW_COMMANDS)),
        snapshot_reader_budget: Arc::new(Semaphore::new(snapshot_reader_capacity)),
        blob_store_registry: dependencies.blob_store_registry,
    };
    let mut connections = JoinSet::new();
    loop {
        if shutdown_requested(&shutdown) {
            break;
        }
        tokio::select! {
            () = wait_for_shutdown(&mut shutdown) => break,
            accepted = listener.accept(), if connections.len() < MAX_ACTIVE_CONNECTIONS => {
                let (stream, _) = accepted.map_err(ProcessRuntimeError::Accept)?;
                connections.spawn(serve_connection(
                    stream,
                    services.clone(),
                    shutdown.clone(),
                ));
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                inspect_connection_completion(completed)?;
            }
        }
    }

    while let Some(completed) = connections.join_next().await {
        inspect_connection_completion(Some(completed))?;
    }
    Ok(())
}

fn inspect_connection_completion(
    completed: Option<Result<Result<(), ProcessConnectionError>, JoinError>>,
) -> Result<(), ProcessRuntimeError> {
    match completed {
        None | Some(Ok(Ok(()))) => Ok(()),
        Some(Ok(Err(ProcessConnectionError::PeerIo(error)))) => {
            drop(error);
            Ok(())
        }
        Some(Ok(Err(ProcessConnectionError::SpoolIo(error)))) => {
            Err(ProcessRuntimeError::SpoolIo(error))
        }
        Some(Ok(Err(ProcessConnectionError::Encode(FrameEncodeError::OversizedFrame)))) => Ok(()),
        Some(Ok(Err(ProcessConnectionError::Encode(error)))) => {
            Err(ProcessRuntimeError::Encode(error))
        }
        Some(Ok(Err(ProcessConnectionError::EncodeInvariant))) => {
            Err(ProcessRuntimeError::EncodeInvariant)
        }
        Some(Ok(Err(ProcessConnectionError::InboundFrameBudgetClosed))) => {
            Err(ProcessRuntimeError::InboundFrameBudgetClosed)
        }
        Some(Ok(Err(ProcessConnectionError::SnapshotReaderBudgetClosed))) => {
            Err(ProcessRuntimeError::SnapshotReaderBudgetClosed)
        }
        Some(Ok(Err(ProcessConnectionError::ImportBudgetClosed))) => {
            Err(ProcessRuntimeError::ImportBudgetClosed)
        }
        Some(Ok(Err(ProcessConnectionError::ReviewCommandBudgetClosed))) => {
            Err(ProcessRuntimeError::ReviewCommandBudgetClosed)
        }
        Some(Err(error)) => Err(ProcessRuntimeError::ConnectionTask(error)),
    }
}

async fn serve_connection(
    stream: UnixStream,
    services: ConnectionServices,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::with_capacity(INBOUND_READ_AHEAD_BYTES, reader);
    let mut pending_import = None;
    let mut pending_blob_upload = None;

    loop {
        if shutdown_requested(&shutdown) {
            return Ok(());
        }
        let awaiting_bulk_ingest_deadline =
            pending_bulk_ingest_deadline(&pending_import, &pending_blob_upload, true);
        let import_state = if pending_import.is_some() || pending_blob_upload.is_some() {
            ConversationImportState::Active
        } else {
            ConversationImportState::Inactive
        };
        let inbound_frame_budget = services.inbound_frame_budgets.for_connection(import_state);
        let frame_buffer_permit = tokio::select! {
            biased;
            () = wait_for_deadline(awaiting_bulk_ingest_deadline) => return Ok(()),
            permit = acquire_inbound_frame_permit_after_input(
                &mut reader,
                inbound_frame_budget,
                &mut shutdown,
            ) => permit?,
        };
        let Some(frame_buffer_permit) = frame_buffer_permit else {
            return Ok(());
        };
        let line = tokio::select! {
            biased;
            () = wait_for_shutdown(&mut shutdown) => return Ok(()),
            () = wait_for_deadline(awaiting_bulk_ingest_deadline) => return Ok(()),
            line = read_frame_line(&mut reader) => line?,
        };
        let Some(line) = line else {
            return Ok(());
        };
        let frame = match line {
            IncomingLine::Complete(line) => match decode_client_line(&line) {
                Ok(frame) => frame,
                Err(error) => {
                    let admitted_version = line
                        .strip_suffix(b"\n")
                        .and_then(recover_bounded_client_protocol_version);
                    let code = match error.kind() {
                        FrameDecodeErrorKind::UnsupportedVersion => ErrorCode::UnsupportedVersion,
                        FrameDecodeErrorKind::OversizedFrame
                        | FrameDecodeErrorKind::MalformedFrame => ErrorCode::MalformedFrame,
                    };
                    drop(line);
                    drop(pending_import.take());
                    drop(pending_blob_upload.take());
                    drop(frame_buffer_permit);
                    write_error(
                        &mut writer,
                        admitted_version.unwrap_or(ProtocolVersion::One),
                        error.request_id(),
                        ProtocolError::without_detail(code),
                    )
                    .await?;
                    return Ok(());
                }
            },
            IncomingLine::Oversized {
                request_id,
                admitted_version,
            } => {
                drop(pending_import.take());
                drop(pending_blob_upload.take());
                drop(frame_buffer_permit);
                write_error(
                    &mut writer,
                    admitted_version.unwrap_or(ProtocolVersion::One),
                    request_id,
                    ProtocolError::without_detail(ErrorCode::MalformedFrame),
                )
                .await?;
                return Ok(());
            }
        };
        let (version, request_id, request) = frame.into_parts();
        let follows = matches!(request, ClientRequest::FollowSession { .. });
        let import_limit = services
            .model_configuration
            .conversation_import_max_source_bytes();
        let import_requires_permit = conversation_import_request_requires_permit(
            &request,
            import_state,
            import_limit,
            services
                .blob_store_registry
                .as_deref()
                .map_or(0, BlobStoreRegistry::max_blob_bytes),
        );
        let import_waiter_permit = if import_requires_permit
            && matches!(
                &request,
                ClientRequest::BeginConversationImport { .. }
                    | ClientRequest::BeginBlobUpload { .. }
            ) {
            let Some(permit) = acquire_import_waiter_permit(
                Arc::clone(&services.import_waiter_budget),
                &mut shutdown,
            )
            .await?
            else {
                return Ok(());
            };
            Some(permit)
        } else {
            None
        };
        let frame_buffer_permit = retain_inbound_frame_permit_during_import_admission(
            &request,
            import_requires_permit,
            frame_buffer_permit,
        );
        let import_permit = if import_requires_permit {
            let Some(permit) =
                acquire_import_permit(Arc::clone(&services.import_budget), &mut shutdown).await?
            else {
                return Ok(());
            };
            Some(permit)
        } else {
            None
        };
        let acquired_bulk_ingest_at = import_permit.as_ref().map(|_| Instant::now());
        drop(import_waiter_permit);
        let review_admission_deadline =
            pending_bulk_ingest_deadline(&pending_import, &pending_blob_upload, true);
        let Some((frame_buffer_permit, review_command_permit)) =
            acquire_review_command_permit_while_buffered(
                ReviewCommandAdmission::for_request(&request),
                frame_buffer_permit,
                Arc::clone(&services.review_command_budget),
                &mut shutdown,
                review_admission_deadline,
            )
            .await?
        else {
            return Ok(());
        };
        drop(frame_buffer_permit);
        let active_lifecycle_request =
            active_bulk_ingest_kind(&pending_import, &pending_blob_upload)
                .is_some_and(|kind| request_is_lifecycle_for_kind(&request, kind));
        let operation_deadline = pending_bulk_ingest_deadline(
            &pending_import,
            &pending_blob_upload,
            !active_lifecycle_request,
        )
        .or_else(|| acquired_bulk_ingest_at.map(|started| started + BULK_INGEST_SESSION_TIMEOUT));
        let request_result = handle_request(
            &mut reader,
            &mut writer,
            version,
            request_id,
            request,
            ConnectionRequestResources {
                import_permit,
                acquired_bulk_ingest_at,
                review_command_permit,
                pending_import: &mut pending_import,
                pending_blob_upload: &mut pending_blob_upload,
            },
            &services,
            shutdown.clone(),
        );
        tokio::select! {
            biased;
            () = wait_for_deadline(operation_deadline) => return Ok(()),
            result = request_result => result?,
        }
        if follows {
            return Ok(());
        }
    }
}

fn active_bulk_ingest_kind(
    pending_import: &Option<PendingConversationImport>,
    pending_blob_upload: &Option<PendingBlobUpload>,
) -> Option<BulkIngestKind> {
    if pending_import.is_some() {
        Some(BulkIngestKind::ConversationImport)
    } else if pending_blob_upload.is_some() {
        Some(BulkIngestKind::BlobUpload)
    } else {
        None
    }
}

fn request_is_lifecycle_for_kind(request: &ClientRequest, kind: BulkIngestKind) -> bool {
    match kind {
        BulkIngestKind::ConversationImport => matches!(
            request,
            ClientRequest::AppendConversationImport { .. }
                | ClientRequest::CommitConversationImport {}
                | ClientRequest::AbortConversationImport {}
        ),
        BulkIngestKind::BlobUpload => matches!(
            request,
            ClientRequest::AppendBlobUpload { .. }
                | ClientRequest::CommitBlobUpload {}
                | ClientRequest::AbortBlobUpload {}
        ),
    }
}

fn request_is_cross_kind_bulk_ingest(request: &ClientRequest, active_kind: BulkIngestKind) -> bool {
    match active_kind {
        BulkIngestKind::ConversationImport => matches!(
            request,
            ClientRequest::BeginBlobUpload { .. }
                | ClientRequest::AppendBlobUpload { .. }
                | ClientRequest::CommitBlobUpload {}
                | ClientRequest::AbortBlobUpload {}
        ),
        BulkIngestKind::BlobUpload => matches!(
            request,
            ClientRequest::ImportConversation { .. }
                | ClientRequest::BeginConversationImport { .. }
                | ClientRequest::AppendConversationImport { .. }
                | ClientRequest::CommitConversationImport {}
                | ClientRequest::AbortConversationImport {}
        ),
    }
}

fn pending_bulk_ingest_deadline(
    pending_import: &Option<PendingConversationImport>,
    pending_blob_upload: &Option<PendingBlobUpload>,
    include_idle: bool,
) -> Option<Instant> {
    let (started_at, idle_since) = if let Some(import) = pending_import {
        (import.started_at, import.idle_since)
    } else if let Some(upload) = pending_blob_upload {
        (upload.started_at(), upload.idle_since())
    } else {
        return None;
    };
    let session_deadline = started_at + BULK_INGEST_SESSION_TIMEOUT;
    if include_idle {
        Some(session_deadline.min(idle_since + BULK_INGEST_IDLE_TIMEOUT))
    } else {
        Some(session_deadline)
    }
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn acquire_inbound_frame_permit_after_input<Reader>(
    reader: &mut Reader,
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError>
where
    Reader: AsyncBufRead + Unpin,
{
    let input_ready = tokio::select! {
        () = wait_for_shutdown(shutdown) => false,
        available = reader.fill_buf() => !available?.is_empty(),
    };
    if !input_ready {
        return Ok(None);
    }
    acquire_inbound_frame_permit(budget, shutdown).await
}

async fn acquire_inbound_frame_permit(
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError> {
    tokio::select! {
        () = wait_for_shutdown(shutdown) => Ok(None),
        permit = budget.acquire_owned() => permit
            .map(Some)
            .map_err(|_| ProcessConnectionError::InboundFrameBudgetClosed),
    }
}

async fn acquire_snapshot_reader_permit(
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError> {
    tokio::select! {
        () = wait_for_shutdown(shutdown) => Ok(None),
        permit = budget.acquire_owned() => permit
            .map(Some)
            .map_err(|_| ProcessConnectionError::SnapshotReaderBudgetClosed),
    }
}

fn conversation_import_request_requires_permit(
    request: &ClientRequest,
    import_state: ConversationImportState,
    limit: usize,
    max_blob_bytes: u64,
) -> bool {
    match import_state {
        ConversationImportState::Inactive => {}
        ConversationImportState::Active => return false,
    }
    match request {
        ClientRequest::ImportConversation { source, .. } => source.as_bytes().len() <= limit,
        ClientRequest::BeginConversationImport {
            declared_size_bytes,
            ..
        } => usize::try_from(declared_size_bytes.value()).is_ok_and(|size| size <= limit),
        ClientRequest::BeginBlobUpload {
            expected_length_bytes,
            ..
        } => (1..=max_blob_bytes).contains(&expected_length_bytes.value()),
        ClientRequest::CreateSession { .. }
        | ClientRequest::CreateSessionFromTemplate { .. }
        | ClientRequest::ListTemplates {}
        | ClientRequest::ListSessions {}
        | ClientRequest::UpdateSessionPlacement { .. }
        | ClientRequest::AttachGoal { .. }
        | ClientRequest::ReadGoal { .. }
        | ClientRequest::ResumeGoal { .. }
        | ClientRequest::StopGoal { .. }
        | ClientRequest::SupersedeGoal { .. }
        | ClientRequest::SubmitInput { .. }
        | ClientRequest::CompactSession { .. }
        | ClientRequest::ReadTranscript { .. }
        | ClientRequest::FollowSession { .. }
        | ClientRequest::SpawnSession { .. }
        | ClientRequest::AwaitSession { .. }
        | ClientRequest::SendSessionMessage { .. }
        | ClientRequest::ListSessionMetadata { .. }
        | ClientRequest::ListConversations { .. }
        | ClientRequest::ListModelAliases {}
        | ClientRequest::ListModelCapabilities {}
        | ClientRequest::ReadSessionMetadata { .. }
        | ClientRequest::ReplaceSessionMetadata { .. }
        | ClientRequest::ReplaceSessionDefaults { .. }
        | ClientRequest::ReadSessionDefaults { .. }
        | ClientRequest::AppendConversationImport { .. }
        | ClientRequest::CommitConversationImport {}
        | ClientRequest::AbortConversationImport {}
        | ClientRequest::AppendBlobUpload { .. }
        | ClientRequest::CommitBlobUpload {}
        | ClientRequest::AbortBlobUpload {}
        | ClientRequest::ReadImportedConversation { .. }
        | ClientRequest::CreateSessionFromImportedFrontier { .. }
        | ClientRequest::ReconcileTurn { .. }
        | ClientRequest::CreateReviewTarget { .. }
        | ClientRequest::StartReviewRun { .. }
        | ClientRequest::ActivateReviewPass { .. }
        | ClientRequest::CompleteReviewPass { .. }
        | ClientRequest::RecordReviewFindings { .. }
        | ClientRequest::RecordReviewFindingEvent { .. }
        | ClientRequest::ReserveReviewExternalLink { .. }
        | ClientRequest::AttachReviewExternalLink { .. }
        | ClientRequest::ReadReviewTarget { .. }
        | ClientRequest::ReadReviewRun { .. }
        | ClientRequest::ReadReviewFinding { .. }
        | ClientRequest::ListReviewFindings { .. }
        | ClientRequest::StartReviewOrchestration { .. }
        | ClientRequest::RecordReviewImportOutcome { .. }
        | ClientRequest::RecordReviewConcernOutcome { .. }
        | ClientRequest::RecordReviewJudgmentPlan { .. }
        | ClientRequest::RecordReviewJudgmentEffect { .. }
        | ClientRequest::RecordReviewRepairOutcomes { .. }
        | ClientRequest::RecordReviewPublicationOutcomes { .. }
        | ClientRequest::ReadReviewOrchestration { .. }
        | ClientRequest::StopTurn { .. }
        | ClientRequest::DecideToolRequest { .. } => false,
    }
}
fn retain_inbound_frame_permit_during_import_admission(
    request: &ClientRequest,
    import_requires_permit: bool,
    permit: OwnedSemaphorePermit,
) -> Option<OwnedSemaphorePermit> {
    if import_requires_permit
        && matches!(
            request,
            ClientRequest::BeginConversationImport { .. } | ClientRequest::BeginBlobUpload { .. }
        )
    {
        None
    } else {
        Some(permit)
    }
}

async fn acquire_import_permit(
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError> {
    tokio::select! {
        () = wait_for_shutdown(shutdown) => Ok(None),
        permit = budget.acquire_owned() => permit
            .map(Some)
            .map_err(|_| ProcessConnectionError::ImportBudgetClosed),
    }
}

async fn acquire_import_waiter_permit(
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError> {
    tokio::select! {
        () = wait_for_shutdown(shutdown) => Ok(None),
        permit = budget.acquire_owned() => permit
            .map(Some)
            .map_err(|_| ProcessConnectionError::ImportBudgetClosed),
    }
}

async fn acquire_review_command_permit(
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError> {
    tokio::select! {
        () = wait_for_shutdown(shutdown) => Ok(None),
        permit = budget.acquire_owned() => permit
            .map(Some)
            .map_err(|_| ProcessConnectionError::ReviewCommandBudgetClosed),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewCommandAdmission {
    Required,
    NotRequired,
}

impl ReviewCommandAdmission {
    const fn for_request(request: &ClientRequest) -> Self {
        if is_review_mutation(request) {
            Self::Required
        } else {
            Self::NotRequired
        }
    }
}

async fn acquire_review_command_permit_while_buffered(
    review_admission: ReviewCommandAdmission,
    frame_buffer_permit: Option<OwnedSemaphorePermit>,
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
    deadline: Option<Instant>,
) -> Result<
    Option<(Option<OwnedSemaphorePermit>, Option<OwnedSemaphorePermit>)>,
    ProcessConnectionError,
> {
    let review_command_permit = match review_admission {
        ReviewCommandAdmission::Required => {
            let permit = tokio::select! {
                biased;
                () = wait_for_deadline(deadline) => return Ok(None),
                permit = acquire_review_command_permit(budget, shutdown) => permit?,
            };
            let Some(permit) = permit else {
                return Ok(None);
            };
            Some(permit)
        }
        ReviewCommandAdmission::NotRequired => None,
    };
    Ok(Some((frame_buffer_permit, review_command_permit)))
}

/// One closed snapshot-reader admission class, decided for every request before
/// dispatch.
///
/// The decision lives here rather than in each dispatch arm because a verb that
/// forgets to reserve capacity does not fail: it quietly spends the connections
/// [`RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS`] holds back for the outbox
/// dispatcher and mutations. An exhaustive match makes a later verb state its
/// class instead of inheriting one by omission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotReaderAdmission {
    /// The request holds no pooled connection across statements: it either
    /// touches no database or completes in one statement on a pooled
    /// connection it returns immediately.
    NotRequired,
    /// The request holds one pooled connection across its database phase — a
    /// multi-statement read, a `REPEATABLE READ` transaction, or a spool.
    OneConnection,
}

impl SnapshotReaderAdmission {
    const fn for_request(request: &ClientRequest) -> Self {
        match request {
            ClientRequest::ListSessions {}
            | ClientRequest::ReadGoal { .. }
            | ClientRequest::ReadTranscript { .. }
            | ClientRequest::FollowSession { .. }
            | ClientRequest::ListSessionMetadata { .. }
            | ClientRequest::ListConversations { .. }
            | ClientRequest::ReadImportedConversation { .. }
            // The metadata point read is not one statement: it opens a
            // transaction, sets `REPEATABLE READ ONLY`, selects, and commits.
            | ClientRequest::ReadSessionMetadata { .. }
            // Each review read opens its own `REPEATABLE READ` transaction, and
            // the findings listing opens two and then walks the finding graph.
            | ClientRequest::ReadReviewTarget { .. }
            | ClientRequest::ReadReviewRun { .. }
            | ClientRequest::ReadReviewFinding { .. }
            | ClientRequest::ListReviewFindings { .. }
            // The coherent orchestration snapshot reads every adapter fact
            // inside one `REPEATABLE READ` transaction on a single connection.
            | ClientRequest::ReadReviewOrchestration { .. } => Self::OneConnection,
            ClientRequest::CreateSession { .. }
            | ClientRequest::CreateSessionFromTemplate { .. }
            | ClientRequest::ListTemplates {}
            | ClientRequest::UpdateSessionPlacement { .. }
            | ClientRequest::AttachGoal { .. }
            | ClientRequest::ResumeGoal { .. }
            | ClientRequest::StopGoal { .. }
            | ClientRequest::SupersedeGoal { .. }
            | ClientRequest::SubmitInput { .. }
            | ClientRequest::CompactSession { .. }
            | ClientRequest::SpawnSession { .. }
            | ClientRequest::AwaitSession { .. }
            | ClientRequest::SendSessionMessage { .. }
            | ClientRequest::ListModelAliases {}
            | ClientRequest::ListModelCapabilities {}
            | ClientRequest::ReplaceSessionMetadata { .. }
            | ClientRequest::ReplaceSessionDefaults { .. }
            | ClientRequest::ReadSessionDefaults { .. }
            | ClientRequest::ImportConversation { .. }
            | ClientRequest::BeginConversationImport { .. }
            | ClientRequest::AppendConversationImport { .. }
            | ClientRequest::CommitConversationImport {}
            | ClientRequest::AbortConversationImport {}
            | ClientRequest::BeginBlobUpload { .. }
            | ClientRequest::AppendBlobUpload { .. }
            | ClientRequest::CommitBlobUpload {}
            | ClientRequest::AbortBlobUpload {}
            | ClientRequest::CreateSessionFromImportedFrontier { .. }
            | ClientRequest::ReconcileTurn { .. }
            | ClientRequest::CreateReviewTarget { .. }
            | ClientRequest::StartReviewRun { .. }
            | ClientRequest::ActivateReviewPass { .. }
            | ClientRequest::CompleteReviewPass { .. }
            | ClientRequest::RecordReviewFindings { .. }
            | ClientRequest::RecordReviewFindingEvent { .. }
            | ClientRequest::ReserveReviewExternalLink { .. }
            | ClientRequest::AttachReviewExternalLink { .. }
            | ClientRequest::StartReviewOrchestration { .. }
            | ClientRequest::RecordReviewImportOutcome { .. }
            | ClientRequest::RecordReviewConcernOutcome { .. }
            | ClientRequest::RecordReviewJudgmentPlan { .. }
            | ClientRequest::RecordReviewJudgmentEffect { .. }
            | ClientRequest::RecordReviewRepairOutcomes { .. }
            | ClientRequest::RecordReviewPublicationOutcomes { .. }
            | ClientRequest::StopTurn { .. }
            | ClientRequest::DecideToolRequest { .. } => Self::NotRequired,
        }
    }
}

/// The snapshot-reader capacity one request holds, or `None` when shutdown
/// cancelled the wait and the request goes unanswered.
async fn admit_snapshot_reader(
    request: &ClientRequest,
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<Option<OwnedSemaphorePermit>>, ProcessConnectionError> {
    match SnapshotReaderAdmission::for_request(request) {
        SnapshotReaderAdmission::NotRequired => Ok(Some(None)),
        SnapshotReaderAdmission::OneConnection => {
            Ok(acquire_snapshot_reader_permit(budget, shutdown)
                .await?
                .map(Some))
        }
    }
}

fn snapshot_reader_capacity(max_pool_connections: u32) -> Option<usize> {
    let available =
        max_pool_connections.checked_sub(RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS)?;
    if available == 0 {
        return None;
    }
    usize::try_from(available).ok()
}

const fn is_review_mutation(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::CreateReviewTarget { .. }
            | ClientRequest::StartReviewRun { .. }
            | ClientRequest::ActivateReviewPass { .. }
            | ClientRequest::CompleteReviewPass { .. }
            | ClientRequest::RecordReviewFindings { .. }
            | ClientRequest::RecordReviewFindingEvent { .. }
            | ClientRequest::ReserveReviewExternalLink { .. }
            | ClientRequest::AttachReviewExternalLink { .. }
            | ClientRequest::StartReviewOrchestration { .. }
            | ClientRequest::RecordReviewImportOutcome { .. }
            | ClientRequest::RecordReviewConcernOutcome { .. }
            | ClientRequest::RecordReviewJudgmentPlan { .. }
            | ClientRequest::RecordReviewJudgmentEffect { .. }
            | ClientRequest::RecordReviewRepairOutcomes { .. }
            | ClientRequest::RecordReviewPublicationOutcomes { .. }
    )
}

fn canonical_review_request_digest(request: &mut ClientRequest) -> Option<[u8; 32]> {
    if let ClientRequest::RecordReviewFindings { findings, .. } = request {
        findings.sort_unstable_by_key(|finding| finding.finding_id.into_uuid());
    }
    serde_json::to_vec(request).ok().map(|bytes| {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        digest
    })
}

struct ReviewResponseWriter<'a, Writer> {
    writer: &'a mut Writer,
    command_permit: Option<OwnedSemaphorePermit>,
}

impl<'a, Writer> ReviewResponseWriter<'a, Writer> {
    const fn new(writer: &'a mut Writer, command_permit: Option<OwnedSemaphorePermit>) -> Self {
        Self {
            writer,
            command_permit,
        }
    }

    fn release_command_permit(&mut self) {
        self.command_permit.take();
    }
}

impl<Writer> AsyncWrite for ReviewResponseWriter<'_, Writer>
where
    Writer: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let this = self.as_mut().get_mut();
        this.release_command_permit();
        std::pin::Pin::new(&mut *this.writer).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        this.release_command_permit();
        std::pin::Pin::new(&mut *this.writer).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        this.release_command_permit();
        std::pin::Pin::new(&mut *this.writer).poll_shutdown(context)
    }
}

struct ConnectionRequestResources<'connection> {
    import_permit: Option<OwnedSemaphorePermit>,
    acquired_bulk_ingest_at: Option<Instant>,
    review_command_permit: Option<OwnedSemaphorePermit>,
    pending_import: &'connection mut Option<PendingConversationImport>,
    pending_blob_upload: &'connection mut Option<PendingBlobUpload>,
}

struct PendingConversationImport {
    format: ConversationImportFormat,
    declared_size_bytes: u64,
    actual_size_bytes: u64,
    source: Vec<u8>,
    import_permit: OwnedSemaphorePermit,
    started_at: Instant,
    idle_since: Instant,
}

#[allow(
    clippy::too_many_arguments,
    reason = "request execution keeps connection I/O and durable correlation explicit"
)]
async fn handle_request<Reader, Writer>(
    reader: &mut Reader,
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    mut request: ClientRequest,
    resources: ConnectionRequestResources<'_>,
    services: &ConnectionServices,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError>
where
    Reader: AsyncBufRead + Unpin,
    Writer: AsyncWrite + Unpin,
{
    let review_request = is_review_mutation(&request);
    let ConnectionRequestResources {
        import_permit,
        acquired_bulk_ingest_at,
        mut review_command_permit,
        pending_import,
        pending_blob_upload,
    } = resources;
    debug_assert_eq!(review_request, review_command_permit.is_some());
    let review_digest = if review_request {
        canonical_review_request_digest(&mut request)
    } else {
        None
    };
    let Some(snapshot_permit) = admit_snapshot_reader(
        &request,
        Arc::clone(&services.snapshot_reader_budget),
        &mut shutdown,
    )
    .await?
    else {
        return Ok(());
    };
    if let Some(active_kind) = active_bulk_ingest_kind(pending_import, pending_blob_upload)
        && request_is_cross_kind_bulk_ingest(&request, active_kind)
    {
        return write_bulk_ingest_rejection(writer, version, request_id, active_kind).await;
    }
    match request {
        ClientRequest::CreateSession {
            command_id,
            initial_model_selection,
            model_settings,
            system_prompt,
            placement,
        } => {
            handle_create_session(
                writer,
                version,
                request_id,
                WireCreateSessionRequest {
                    command_uuid: command_id.into_uuid(),
                    initial_model_selection,
                    model_settings,
                    system_prompt,
                    placement,
                },
                services,
            )
            .await
        }
        ClientRequest::CreateSessionFromTemplate {
            command_id,
            template_name,
            placement,
        } => {
            handle_create_session_from_template(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                template_name,
                placement,
                services,
            )
            .await
        }
        ClientRequest::CreateSessionFromImportedFrontier {
            command_id,
            imported_conversation_id,
            through_position,
            relationship,
            initial_model_selection,
            model_settings,
        } => {
            handle_create_session_from_imported_frontier(
                writer,
                version,
                request_id,
                WireImportedContinuationRequest {
                    command_uuid: command_id.into_uuid(),
                    conversation: imported_conversation_id,
                    through_position,
                    relationship,
                    initial_model_selection,
                    model_settings,
                },
                &services.pool,
                services.model_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::CompactSession {
            command_id,
            session_id,
            through_position,
        } => {
            handle_compact_session(
                writer,
                version,
                request_id,
                command_id,
                session_id,
                through_position,
                services,
            )
            .await
        }
        ClientRequest::ReadImportedConversation {
            imported_conversation_id,
        } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_read_imported_conversation(
                writer,
                version,
                request_id,
                imported_conversation_id,
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ListSessions {} => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_list_sessions(writer, version, request_id, &services.pool, snapshot_permit).await
        }
        ClientRequest::UpdateSessionPlacement {
            command_id,
            session_id,
            expected_placement_version,
            replacement,
        } => {
            handle_update_session_placement(
                writer,
                version,
                request_id,
                WireSessionPlacementUpdateRequest {
                    command_id,
                    session_id,
                    expected_version: expected_placement_version,
                    replacement,
                },
                &services.pool,
            )
            .await
        }
        ClientRequest::AttachGoal {
            command_id,
            session_id,
            statement,
        } => {
            let Ok(statement) = GoalStatement::try_new(statement) else {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::InvalidRequest),
                )
                .await;
            };
            handle_goal_user_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                GoalUserAction::Attach(statement),
                services,
            )
            .await
        }
        ClientRequest::ReadGoal { session_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_read_goal(
                writer,
                version,
                request_id,
                session_id,
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ResumeGoal {
            command_id,
            session_id,
            guidance,
        } => {
            let Ok(guidance) = guidance.map(GoalGuidance::try_new).transpose() else {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::InvalidRequest),
                )
                .await;
            };
            handle_goal_user_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                GoalUserAction::Resume(guidance),
                services,
            )
            .await
        }
        ClientRequest::StopGoal {
            command_id,
            session_id,
            descendant_scope,
        } => {
            handle_goal_user_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                GoalUserAction::Stop {
                    descendant_scope: decode_descendant_scope(descendant_scope),
                },
                services,
            )
            .await
        }
        ClientRequest::SupersedeGoal {
            command_id,
            session_id,
            statement,
        } => {
            let Ok(statement) = GoalStatement::try_new(statement) else {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::InvalidRequest),
                )
                .await;
            };
            handle_goal_user_command(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                GoalUserAction::Supersede(statement),
                services,
            )
            .await
        }
        ClientRequest::ListTemplates {} => {
            handle_list_templates(
                writer,
                version,
                request_id,
                services.template_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::SubmitInput {
            command_id,
            session_id,
            content,
            expected_defaults_version,
            model_settings,
            delivery,
        } => {
            handle_submit_input(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                content,
                expected_defaults_version,
                model_settings,
                delivery,
                &services.pool,
                &services.eligibility_nudge,
                &services.tool_dispatch_gate,
                services.model_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::ReconcileTurn {
            command_id,
            session_id,
            expected_active_turn_id,
            content,
            expected_defaults_version,
            model_settings,
        } => {
            handle_reconcile_turn(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                expected_active_turn_id,
                content,
                expected_defaults_version,
                model_settings,
                &services.pool,
                &services.eligibility_nudge,
                &services.tool_dispatch_gate,
                services.model_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::ReadTranscript { session_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_read_transcript(
                writer,
                version,
                request_id,
                session_id,
                &services.pool,
                &services.model_configuration,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::FollowSession { session_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_follow_session(
                writer,
                version,
                request_id,
                session_id,
                &services.pool,
                &services.model_configuration,
                &services.fanouts,
                shutdown,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ListSessionMetadata {
            required_tags,
            title_contains,
            include_archived,
            page_size,
            after_session_id,
        } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_list_session_metadata(
                writer,
                version,
                request_id,
                WireMetadataPageRequest {
                    required_tags,
                    title_contains,
                    include_archived,
                    page_size,
                    after_session_id,
                },
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ListConversations {
            title_contains,
            origin,
            include_archived,
            page_size,
            after,
        } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_list_conversations(
                writer,
                version,
                request_id,
                WireConversationPageRequest {
                    title_contains,
                    origin,
                    include_archived,
                    page_size,
                    after,
                },
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ListModelAliases {} => {
            handle_list_model_aliases(
                writer,
                version,
                request_id,
                services.model_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::ListModelCapabilities {} => {
            handle_list_model_capabilities(
                writer,
                version,
                request_id,
                services.model_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::ReadSessionMetadata { session_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_read_session_metadata(
                writer,
                version,
                request_id,
                session_id,
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ReplaceSessionMetadata {
            command_id,
            session_id,
            metadata,
        } => {
            handle_replace_session_metadata(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                metadata,
                &services.pool,
            )
            .await
        }
        ClientRequest::ReplaceSessionDefaults {
            command_id,
            session_id,
            expected_defaults_version,
            model_selection,
            model_settings,
            dangerous_tool_auto_approval,
            system_prompt,
        } => {
            handle_replace_session_defaults(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                expected_defaults_version,
                model_selection,
                model_settings,
                dangerous_tool_auto_approval,
                system_prompt,
                &services.pool,
                services.model_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::ReadSessionDefaults {
            session_id,
            defaults_version,
        } => {
            handle_read_session_defaults(
                writer,
                version,
                request_id,
                session_id,
                defaults_version,
                &services.pool,
            )
            .await
        }
        ClientRequest::ImportConversation { format, source } => {
            if pending_import.is_some() {
                drop(source);
                return write_import_rejection(
                    writer,
                    version,
                    request_id,
                    RejectionDetail::ConversationImportAlreadyInProgress {},
                )
                .await;
            }
            let source = source.into_bytes();
            let source_size =
                u64::try_from(source.len()).map_err(|_| ProcessConnectionError::EncodeInvariant)?;
            let limit = services
                .model_configuration
                .conversation_import_max_source_bytes();
            if source.len() > limit {
                let detail = RejectionDetail::ConversationImportSourceTooLarge {
                    limit_bytes: wire_size(limit)?,
                    declared_size_bytes: CanonicalU64::new(source_size),
                    actual_size_bytes: Some(CanonicalU64::new(source_size)),
                };
                drop(source);
                return write_import_rejection(writer, version, request_id, detail).await;
            }
            let import_permit = import_permit.ok_or(ProcessConnectionError::ImportBudgetClosed)?;
            handle_import_conversation(
                writer,
                version,
                request_id,
                format,
                source,
                &services.pool,
                import_permit,
            )
            .await
        }
        ClientRequest::BeginConversationImport {
            format,
            declared_size_bytes,
        } => {
            handle_begin_conversation_import(
                writer,
                version,
                request_id,
                format,
                declared_size_bytes,
                services
                    .model_configuration
                    .conversation_import_max_source_bytes(),
                import_permit,
                acquired_bulk_ingest_at,
                pending_import,
            )
            .await
        }
        ClientRequest::AppendConversationImport { chunk } => {
            handle_append_conversation_import(
                writer,
                version,
                request_id,
                chunk.into_bytes(),
                services
                    .model_configuration
                    .conversation_import_max_source_bytes(),
                pending_import,
            )
            .await
        }
        ClientRequest::CommitConversationImport {} => {
            handle_commit_conversation_import(
                writer,
                version,
                request_id,
                services
                    .model_configuration
                    .conversation_import_max_source_bytes(),
                &services.pool,
                pending_import,
            )
            .await
        }
        ClientRequest::AbortConversationImport {} => {
            handle_abort_conversation_import(writer, version, request_id, pending_import).await
        }
        ClientRequest::BeginBlobUpload {
            expected_digest,
            expected_length_bytes,
        } => {
            handle_begin_blob_upload(
                writer,
                version,
                request_id,
                expected_digest,
                expected_length_bytes,
                import_permit,
                acquired_bulk_ingest_at,
                services,
                pending_blob_upload,
            )
            .await
        }
        ClientRequest::AppendBlobUpload { chunk } => {
            handle_append_blob_upload(
                writer,
                version,
                request_id,
                chunk.into_bytes(),
                pending_blob_upload,
            )
            .await
        }
        ClientRequest::CommitBlobUpload {} => {
            handle_commit_blob_upload(writer, version, request_id, services, pending_blob_upload)
                .await
        }
        ClientRequest::AbortBlobUpload {} => {
            handle_abort_blob_upload(writer, version, request_id, pending_blob_upload).await
        }
        ClientRequest::CreateReviewTarget {
            command_id,
            target_id,
            provider,
            repository,
            subject,
            head_revision,
            base_revision,
            stack_parent_target_id,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_create_review_target(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                target_id,
                provider,
                repository,
                subject,
                head_revision,
                base_revision,
                stack_parent_target_id,
                &services.pool,
            )
            .await
        }
        ClientRequest::StartReviewRun {
            command_id,
            target_id,
            run_id,
            pass_id,
            workflow,
            session_id,
            accepted_input_id,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_start_review_run(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                target_id,
                run_id,
                pass_id,
                workflow,
                session_id,
                accepted_input_id,
                &services.pool,
            )
            .await
        }
        ClientRequest::ActivateReviewPass {
            command_id,
            run_id,
            pass_id,
            turn_id,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_activate_review_pass(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                run_id,
                pass_id,
                turn_id,
                &services.pool,
            )
            .await
        }
        ClientRequest::CompleteReviewPass {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            outcome,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_complete_review_pass(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                run_id,
                pass_id,
                turn_id,
                output_frontier_id,
                outcome,
                &services.pool,
            )
            .await
        }
        ClientRequest::RecordReviewFindings {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            findings,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_record_review_findings(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                run_id,
                pass_id,
                turn_id,
                output_frontier_id,
                findings,
                &services.pool,
            )
            .await
        }
        ClientRequest::RecordReviewFindingEvent {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            finding_id,
            event_ordinal,
            event,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_record_review_disposition(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                run_id,
                pass_id,
                turn_id,
                output_frontier_id,
                finding_id,
                event_ordinal,
                event,
                &services.pool,
            )
            .await
        }
        ClientRequest::ReserveReviewExternalLink {
            command_id,
            external_link_id,
            finding_id,
            provider,
            object_kind,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_reserve_review_external_link(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                external_link_id,
                finding_id,
                provider,
                object_kind,
                &services.pool,
            )
            .await
        }
        ClientRequest::AttachReviewExternalLink {
            command_id,
            external_link_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            external_object,
            event_ordinal,
        } => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_attach_review_external_link(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                command_id,
                external_link_id,
                run_id,
                pass_id,
                turn_id,
                output_frontier_id,
                external_object,
                event_ordinal,
                &services.pool,
            )
            .await
        }
        request @ (ClientRequest::StartReviewOrchestration { .. }
        | ClientRequest::RecordReviewImportOutcome { .. }
        | ClientRequest::RecordReviewConcernOutcome { .. }
        | ClientRequest::RecordReviewJudgmentPlan { .. }
        | ClientRequest::RecordReviewJudgmentEffect { .. }
        | ClientRequest::RecordReviewRepairOutcomes { .. }
        | ClientRequest::RecordReviewPublicationOutcomes { .. }) => {
            let mut response_writer =
                ReviewResponseWriter::new(writer, review_command_permit.take());
            handle_review_orchestration_mutation(
                &mut response_writer,
                version,
                request_id,
                required_review_digest(review_digest)?,
                request,
                &services.pool,
                services.template_configuration.as_ref(),
            )
            .await
        }
        request @ ClientRequest::ReadReviewOrchestration { .. } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            run_until_shutdown(
                &mut shutdown,
                handle_read_review_orchestration(
                    writer,
                    version,
                    request_id,
                    request,
                    &services.pool,
                    services.template_configuration.as_ref(),
                    snapshot_permit,
                ),
            )
            .await
            .unwrap_or(Ok(()))
        }
        ClientRequest::ReadReviewTarget { target_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_read_review_target(
                writer,
                version,
                request_id,
                target_id,
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ReadReviewRun { run_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_read_review_run(
                writer,
                version,
                request_id,
                run_id,
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ReadReviewFinding { finding_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_read_review_finding(
                writer,
                version,
                request_id,
                finding_id,
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ListReviewFindings { run_id } => {
            let Some(snapshot_permit) = snapshot_permit else {
                return Ok(());
            };
            handle_list_review_findings(
                writer,
                version,
                request_id,
                run_id,
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::StopTurn {
            command_id,
            session_id,
            expected_active_turn_id,
            content,
            expected_defaults_version,
            descendant_scope,
            model_settings,
        } => {
            handle_stop_turn(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                expected_active_turn_id,
                content,
                expected_defaults_version,
                decode_descendant_scope(descendant_scope),
                model_settings,
                &services.pool,
                &services.eligibility_nudge,
                &services.tool_dispatch_gate,
                services.model_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::SpawnSession { .. } => {
            reject_uncomposed_spawn(writer, version, request_id).await
        }
        ClientRequest::AwaitSession {
            session_id,
            turn_id,
            tool_request_id,
            child_session_id,
            mode,
        } => {
            handle_await_session(
                reader,
                writer,
                version,
                request_id,
                session_id,
                turn_id,
                tool_request_id,
                child_session_id,
                mode,
                services,
                shutdown,
            )
            .await
        }
        ClientRequest::SendSessionMessage {
            session_id,
            turn_id,
            tool_request_id,
            peer_session_id,
            content,
        } => {
            handle_send_session_message(
                writer,
                version,
                request_id,
                session_id,
                turn_id,
                tool_request_id,
                peer_session_id,
                content,
                &services.pool,
                &services.eligibility_nudge,
            )
            .await
        }
        ClientRequest::DecideToolRequest {
            command_id,
            session_id,
            tool_request_id,
            decision,
        } => {
            handle_decide_tool_request(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                tool_request_id,
                decision,
                &services.pool,
                &services.eligibility_nudge,
            )
            .await
        }
    }
}

async fn reject_uncomposed_spawn<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::without_detail(ErrorCode::InvalidRequest),
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "the closed await request keeps every durable correlation explicit"
)]
async fn handle_await_session<Reader, Writer>(
    reader: &mut Reader,
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    child_session_id: CanonicalUuid,
    mode: WireDelegationWaitMode,
    services: &ConnectionServices,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError>
where
    Reader: AsyncBufRead + Unpin,
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let turn = TurnId::from_uuid(turn_id.into_uuid());
    let request = ToolRequestId::from_uuid(tool_request_id.into_uuid());
    let child = SessionId::from_uuid(child_session_id.into_uuid());
    let mode = match mode {
        WireDelegationWaitMode::Foreground => DomainDelegationWaitMode::Foreground,
        WireDelegationWaitMode::Background => DomainDelegationWaitMode::Background,
    };
    let mut subscription = services.fanouts.durable.subscribe();
    let port = PostgresSessionDelegationPort::new(services.pool.clone());
    let Some(outcome) = run_until_shutdown(
        &mut shutdown,
        port.await_process_session(session, turn, request, child, mode),
    )
    .await
    else {
        return Ok(());
    };
    match outcome {
        Ok(ProcessDelegationOutcome::Applied(AwaitSessionPortOutcome::BackgroundRegistered(
            receipt,
        ))) => {
            nudge_delegation_issuer(&services.eligibility_nudge, session);
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionAwaitRegistered {
                    tool_request_id: wire_uuid(receipt.tool_request().into_uuid()),
                    child_session_id: wire_uuid(receipt.child().into_uuid()),
                    mode: WireDelegationWaitMode::Background,
                },
            )
            .await
        }
        Ok(ProcessDelegationOutcome::Applied(AwaitSessionPortOutcome::Delivered(result))) => {
            write_message(writer, version, request_id, wire_child_result(&result)?).await
        }
        Ok(ProcessDelegationOutcome::Applied(AwaitSessionPortOutcome::ForegroundPending(wait))) => {
            wait_for_foreground_child_result(
                reader,
                writer,
                version,
                request_id,
                &port,
                wait,
                turn,
                &mut subscription,
                shutdown,
            )
            .await
        }
        Ok(ProcessDelegationOutcome::InvalidRequest) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::InvalidRequest),
            )
            .await
        }
        Ok(ProcessDelegationOutcome::Rejected(rejection)) => {
            nudge_after_process_await_rejection(&services.eligibility_nudge, session, rejection);
            write_error(
                writer,
                version,
                request_id,
                process_delegation_rejection_for_recipient(
                    rejection,
                    session_id,
                    turn_id,
                    tool_request_id,
                    child_session_id,
                    session_id,
                ),
            )
            .await
        }
        Ok(ProcessDelegationOutcome::Applied(
            AwaitSessionPortOutcome::Rejected | AwaitSessionPortOutcome::DurablyRejected,
        )) => {
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    Some(session_id.into_uuid()),
                    InternalDiagnostic::SessionDelegationContract,
                ),
            )
            .await
        }
        Err(error) => {
            write_delegation_port_error(writer, version, request_id, session_id, error).await
        }
    }
}

fn nudge_after_process_await_rejection(
    eligibility_nudge: &impl EligibilityNudge,
    issuer: SessionId,
    rejection: ProcessDelegationRequestRejection,
) {
    let attempt_ended = match rejection {
        ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::RelationshipNotFound
            | DelegationOperationRejection::DeliverySequenceExhausted
            | DelegationOperationRejection::Transition { .. },
        ) => true,
        ProcessDelegationRequestRejection::SessionNotFound
        | ProcessDelegationRequestRejection::ToolRequestNotFound
        | ProcessDelegationRequestRejection::ToolRequestNotInSession
        | ProcessDelegationRequestRejection::RequestNotInTurn
        | ProcessDelegationRequestRejection::AwaitConflict
        | ProcessDelegationRequestRejection::MessageConflict
        | ProcessDelegationRequestRejection::MessageIdentityCollision { .. }
        | ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::StaleDispatch { .. }
            | DelegationOperationRejection::MessageIdentityCollision,
        ) => false,
    };
    if attempt_ended {
        nudge_delegation_issuer(eligibility_nudge, issuer);
    }
}

fn nudge_after_process_message_rejection(
    eligibility_nudge: &impl EligibilityNudge,
    issuer: SessionId,
    rejection: ProcessDelegationRequestRejection,
) {
    let attempt_ended = match rejection {
        ProcessDelegationRequestRejection::MessageIdentityCollision { .. }
        | ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::RelationshipNotFound
            | DelegationOperationRejection::MessageIdentityCollision
            | DelegationOperationRejection::DeliverySequenceExhausted
            | DelegationOperationRejection::Transition { .. },
        ) => true,
        ProcessDelegationRequestRejection::SessionNotFound
        | ProcessDelegationRequestRejection::ToolRequestNotFound
        | ProcessDelegationRequestRejection::ToolRequestNotInSession
        | ProcessDelegationRequestRejection::RequestNotInTurn
        | ProcessDelegationRequestRejection::AwaitConflict
        | ProcessDelegationRequestRejection::MessageConflict
        | ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::StaleDispatch { .. },
        ) => false,
    };
    if attempt_ended {
        nudge_delegation_issuer(eligibility_nudge, issuer);
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "foreground delivery keeps socket cancellation and durable correlation explicit"
)]
async fn wait_for_foreground_child_result<Reader, Writer>(
    reader: &mut Reader,
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    port: &PostgresSessionDelegationPort,
    wait: DelegationWait,
    turn: TurnId,
    subscription: &mut broadcast::Receiver<ProcessUpdate>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError>
where
    Reader: AsyncBufRead + Unpin,
    Writer: AsyncWrite + Unpin,
{
    loop {
        let Some(delivery) =
            run_until_shutdown(&mut shutdown, port.load_foreground_delivery(wait)).await
        else {
            return Ok(());
        };
        match preserve_committed_foreground_wait(delivery) {
            CommittedForegroundDelivery::Delivered(result) => {
                return write_message(writer, version, request_id, wire_child_result(&result)?)
                    .await;
            }
            CommittedForegroundDelivery::Pending => {}
            CommittedForegroundDelivery::Retry(error) => {
                tracing::error!(
                    diagnostic = "delegation_foreground_delivery_reread_failed",
                    cause_code = error.operator_failure_cause_code(),
                    session_id = %wait.parent().as_uuid(),
                    turn_id = %turn.as_uuid(),
                    "foreground process delivery reread failed after wait commit"
                );
                tokio::select! {
                    () = wait_for_shutdown(&mut shutdown) => return Ok(()),
                    peer = foreground_peer_activity(reader) => return peer,
                    () = sleep(DELEGATION_DELIVERY_RETRY_INTERVAL) => continue,
                }
            }
        }
        loop {
            let update = tokio::select! {
                () = wait_for_shutdown(&mut shutdown) => return Ok(()),
                peer = foreground_peer_activity(reader) => return peer,
                update = subscription.recv() => update,
            };
            match update {
                Ok(update) if update_signals_child_result(&update, wait) => break,
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => break,
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum CommittedForegroundDelivery<T, E> {
    Delivered(T),
    Pending,
    Retry(E),
}

fn preserve_committed_foreground_wait<T, E>(
    delivery: Result<Option<T>, E>,
) -> CommittedForegroundDelivery<T, E> {
    match delivery {
        Ok(Some(delivered)) => CommittedForegroundDelivery::Delivered(delivered),
        Ok(None) => CommittedForegroundDelivery::Pending,
        Err(error) => CommittedForegroundDelivery::Retry(error),
    }
}

async fn foreground_peer_activity<Reader>(reader: &mut Reader) -> Result<(), ProcessConnectionError>
where
    Reader: AsyncBufRead + Unpin,
{
    reader
        .fill_buf()
        .await
        .map_err(ProcessConnectionError::PeerIo)?;
    Err(ProcessConnectionError::PeerIo(io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "foreground delegation peer ended or sent another request",
    )))
}

fn update_signals_child_result(update: &ProcessUpdate, wait: DelegationWait) -> bool {
    match update {
        ProcessUpdate::Durable { session, event, .. } => match event {
            ProcessUpdateEvent::DelegationUpdate(delegation) => match delegation {
                DispatchedDelegationUpdate::ChildResult {
                    spawning_request,
                    child,
                    ..
                } => {
                    *session == wait.parent()
                        && *spawning_request == wait.spawning_request()
                        && *child == wait.child()
                }
                DispatchedDelegationUpdate::ChildSpawned { .. }
                | DispatchedDelegationUpdate::ChildWaiting { .. }
                | DispatchedDelegationUpdate::ChildLifecycleDisposition { .. }
                | DispatchedDelegationUpdate::SessionMessage { .. } => false,
            },
            ProcessUpdateEvent::SessionCreated
            | ProcessUpdateEvent::SessionModelSettingsChanged(_)
            | ProcessUpdateEvent::TurnModelSettingsResolved(_)
            | ProcessUpdateEvent::InputAccepted { .. }
            | ProcessUpdateEvent::GoalTurnRetired { .. }
            | ProcessUpdateEvent::TurnActivated { .. }
            | ProcessUpdateEvent::ModelCallTransition { .. }
            | ProcessUpdateEvent::ToolBatchTransition { .. }
            | ProcessUpdateEvent::ToolApprovalDecided { .. }
            | ProcessUpdateEvent::RunnerStateTransition { .. }
            | ProcessUpdateEvent::ContextCompacted { .. }
            | ProcessUpdateEvent::TurnCompleted { .. }
            | ProcessUpdateEvent::TurnFailed { .. }
            | ProcessUpdateEvent::TurnRefused { .. }
            | ProcessUpdateEvent::TurnCancelled { .. }
            | ProcessUpdateEvent::TurnReconciliationRequired { .. } => false,
        },
        ProcessUpdate::ProviderTextDelta(_) => false,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the closed message request keeps every durable correlation explicit"
)]
async fn handle_send_session_message<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    peer_session_id: CanonicalUuid,
    content: String,
    pool: &PgPool,
    eligibility_nudge: &InProcessEligibilityNudge,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let port = PostgresSessionDelegationPort::new(pool.clone());
    let result = port
        .send_process_message(
            SessionId::from_uuid(session_id.into_uuid()),
            TurnId::from_uuid(turn_id.into_uuid()),
            ToolRequestId::from_uuid(tool_request_id.into_uuid()),
            SessionId::from_uuid(peer_session_id.into_uuid()),
            content,
        )
        .await;
    match result {
        Ok(ProcessDelegationOutcome::Applied(receipt)) => {
            nudge_delegation_issuer(
                eligibility_nudge,
                SessionId::from_uuid(session_id.into_uuid()),
            );
            let direction = match receipt.direction() {
                DomainDelegationMessageDirection::ParentToChild => {
                    signalbox_process_protocol::DelegationMessageDirection::ParentToChild
                }
                DomainDelegationMessageDirection::ChildToParent => {
                    signalbox_process_protocol::DelegationMessageDirection::ChildToParent
                }
            };
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionMessageSent {
                    tool_request_id: wire_uuid(receipt.tool_request().into_uuid()),
                    message_id: wire_uuid(receipt.message().into_uuid()),
                    direction,
                    ordinal: CanonicalU64::new(receipt.ordinal().get()),
                    delivery_sequence: CanonicalU64::new(receipt.delivery_sequence().get()),
                },
            )
            .await
        }
        Ok(ProcessDelegationOutcome::InvalidRequest) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::InvalidRequest),
            )
            .await
        }
        Ok(ProcessDelegationOutcome::Rejected(rejection)) => {
            nudge_after_process_message_rejection(
                eligibility_nudge,
                SessionId::from_uuid(session_id.into_uuid()),
                rejection,
            );
            write_error(
                writer,
                version,
                request_id,
                process_delegation_rejection(
                    rejection,
                    session_id,
                    turn_id,
                    tool_request_id,
                    peer_session_id,
                ),
            )
            .await
        }
        Err(error) => {
            write_delegation_port_error(writer, version, request_id, session_id, error).await
        }
    }
}

fn process_delegation_rejection(
    rejection: ProcessDelegationRequestRejection,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    peer_session_id: CanonicalUuid,
) -> ProtocolError {
    process_delegation_rejection_for_recipient(
        rejection,
        session_id,
        turn_id,
        tool_request_id,
        peer_session_id,
        peer_session_id,
    )
}

fn process_delegation_rejection_for_recipient(
    rejection: ProcessDelegationRequestRejection,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    peer_session_id: CanonicalUuid,
    delivery_recipient_id: CanonicalUuid,
) -> ProtocolError {
    let detail = match rejection {
        ProcessDelegationRequestRejection::SessionNotFound => {
            RejectionDetail::SessionNotFound { session_id }
        }
        ProcessDelegationRequestRejection::ToolRequestNotFound => {
            RejectionDetail::ToolRequestNotFound { tool_request_id }
        }
        ProcessDelegationRequestRejection::ToolRequestNotInSession => {
            RejectionDetail::ToolRequestNotInSession {
                session_id,
                tool_request_id,
            }
        }
        ProcessDelegationRequestRejection::RequestNotInTurn => {
            RejectionDetail::DelegationRequestNotInTurn {
                session_id,
                turn_id,
                tool_request_id,
            }
        }
        ProcessDelegationRequestRejection::AwaitConflict => {
            RejectionDetail::DelegationAwaitConflict { tool_request_id }
        }
        ProcessDelegationRequestRejection::MessageConflict
        | ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::Transition {
                failure: signalbox_domain::DelegationTransitionFailure::ConflictingMessageReplay,
                ..
            },
        ) => RejectionDetail::DelegationMessageConflict { tool_request_id },
        ProcessDelegationRequestRejection::MessageIdentityCollision { message } => {
            RejectionDetail::DelegationMessageIdentityCollision {
                message_id: wire_uuid(message.into_uuid()),
            }
        }
        ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::RelationshipNotFound,
        ) => RejectionDetail::DelegationRelationNotFound {
            session_id,
            peer_session_id,
        },
        ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::StaleDispatch { state },
        ) => RejectionDetail::DelegationToolRequestNotExecutable {
            tool_request_id,
            state: match state {
                DelegationRequestExecutionState::AwaitingApproval => {
                    WireDelegationToolRequestState::AwaitingApproval
                }
                DelegationRequestExecutionState::Denied => WireDelegationToolRequestState::Denied,
                DelegationRequestExecutionState::Approved => {
                    WireDelegationToolRequestState::Approved
                }
                DelegationRequestExecutionState::Prepared => {
                    WireDelegationToolRequestState::Prepared
                }
                DelegationRequestExecutionState::Closed => WireDelegationToolRequestState::Closed,
                DelegationRequestExecutionState::AttemptEnded => {
                    WireDelegationToolRequestState::AttemptEnded
                }
            },
        },
        ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::Transition {
                spawning_request,
                failure: signalbox_domain::DelegationTransitionFailure::EventOrdinalExhausted,
            },
        ) => RejectionDetail::DelegationEventOrdinalExhausted {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            last: CanonicalU64::new(u64::MAX),
        },
        ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::DeliverySequenceExhausted,
        ) => RejectionDetail::DelegationDeliverySequenceExhausted {
            recipient_session_id: delivery_recipient_id,
            last: CanonicalU64::new(u64::MAX),
        },
        ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::MessageIdentityCollision
            | DelegationOperationRejection::Transition { .. },
        ) => {
            return internal_protocol_error(
                Some(session_id.into_uuid()),
                InternalDiagnostic::SessionDelegationContract,
            );
        }
    };
    ProtocolError::rejected(detail)
}

fn wire_child_result(
    result: &DeliveredChildResult,
) -> Result<ServerMessage, ProcessConnectionError> {
    let wait = result.wait();
    let outcome = match result.kind() {
        DomainDelegationOutcomeKind::ResultReturned => WireDelegationOutcome::Returned,
        DomainDelegationOutcomeKind::ChildFailed => WireDelegationOutcome::Failed,
        DomainDelegationOutcomeKind::ChildStopped => WireDelegationOutcome::Stopped,
        DomainDelegationOutcomeKind::ChildCancelled => WireDelegationOutcome::Cancelled,
        DomainDelegationOutcomeKind::AlreadyTerminal
        | DomainDelegationOutcomeKind::ContinueRunning => {
            return Err(ProcessConnectionError::EncodeInvariant);
        }
    };
    let reason = match result.reason() {
        DomainDelegationOutcomeReason::ChildCompleted => WireDelegationReason::ChildCompleted,
        DomainDelegationOutcomeReason::ChildExecutionFailed => {
            WireDelegationReason::ChildExecutionFailed
        }
        DomainDelegationOutcomeReason::ChildResultUnavailable => {
            WireDelegationReason::ChildResultUnavailable
        }
        DomainDelegationOutcomeReason::ChildCancelled => WireDelegationReason::ChildCancelled,
        DomainDelegationOutcomeReason::ParentStopped { .. } => WireDelegationReason::ParentStopped,
        DomainDelegationOutcomeReason::ParentCancelled { .. } => {
            WireDelegationReason::ParentCancelled
        }
    };
    Ok(ServerMessage::ChildResult {
        await_request_id: wire_uuid(wait.awaiting_request().into_uuid()),
        spawning_request_id: wire_uuid(wait.spawning_request().into_uuid()),
        child_session_id: wire_uuid(wait.child().into_uuid()),
        outcome,
        content: result.content().map(|content| content.as_str().to_owned()),
        reason,
        provenance: wire_domain_delegation_provenance(result.provenance())?,
    })
}

fn wire_domain_delegation_provenance(
    provenance: DomainDelegationProvenance,
) -> Result<WireDelegationProvenance, ProcessConnectionError> {
    let authority = match provenance.projection() {
        signalbox_domain::DelegationProvenanceProjection::ChildTurn { terminal } => {
            return Ok(WireDelegationProvenance::ChildTurn {
                child_session_id: wire_uuid(terminal.session().into_uuid()),
                child_turn_id: wire_uuid(terminal.turn().into_uuid()),
            });
        }
        signalbox_domain::DelegationProvenanceProjection::ParentCommand { authority } => authority,
        signalbox_domain::DelegationProvenanceProjection::ToolRequest { .. } => {
            return Err(ProcessConnectionError::EncodeInvariant);
        }
    };
    let descendant_scope = match authority.scope() {
        DescendantTerminationScope::ParentAlone => WireDescendantTerminationScope::ParentAlone,
        DescendantTerminationScope::ParentAndDescendants => {
            WireDescendantTerminationScope::ParentAndDescendants
        }
    };
    match authority.source() {
        ParentTerminationCommandSource::Turn { turn } => {
            Ok(WireDelegationProvenance::ParentTurnCommand {
                parent_session_id: wire_uuid(authority.parent().into_uuid()),
                parent_turn_id: wire_uuid(turn.into_uuid()),
                command_id: wire_uuid(authority.command().into_uuid()),
                descendant_scope,
            })
        }
        ParentTerminationCommandSource::Goal { generation } => {
            Ok(WireDelegationProvenance::ParentGoalCommand {
                parent_session_id: wire_uuid(authority.parent().into_uuid()),
                goal_generation: CanonicalU64::new(generation.get()),
                command_id: wire_uuid(authority.command().into_uuid()),
                descendant_scope,
            })
        }
    }
}

async fn write_delegation_port_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    error: PostgresSessionDelegationPortError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let protocol_error = match error {
        PostgresSessionDelegationPortError::Repository(
            signalbox_persistence::session_delegation::SessionDelegationRepositoryError::Database(
                _,
            ),
        ) => unavailable_protocol_error(InternalDiagnostic::SessionDelegationDatabase),
        PostgresSessionDelegationPortError::Repository(
            signalbox_persistence::session_delegation::SessionDelegationRepositoryError::CommitAmbiguous(
                _,
            ),
        ) => ProtocolError::mutation_commit_ambiguous(),
        PostgresSessionDelegationPortError::Repository(
            signalbox_persistence::session_delegation::SessionDelegationRepositoryError::ToolLoop(
                error,
            ),
        ) => {
            return write_tool_loop_error(writer, version, request_id, session_id, error).await;
        }
        PostgresSessionDelegationPortError::Repository(
            signalbox_persistence::session_delegation::SessionDelegationRepositoryError::Corruption(
                _,
            ),
        ) => internal_protocol_error(
            Some(session_id.into_uuid()),
            InternalDiagnostic::SessionDelegationCorruption,
        ),
        PostgresSessionDelegationPortError::Repository(
            signalbox_persistence::session_delegation::SessionDelegationRepositoryError::InvalidTransition(
                _,
            ),
        )
        | PostgresSessionDelegationPortError::Contract => internal_protocol_error(
            Some(session_id.into_uuid()),
            InternalDiagnostic::SessionDelegationContract,
        ),
    };
    write_error(writer, version, request_id, protocol_error).await
}

async fn handle_review_orchestration_mutation<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    request: ClientRequest,
    pool: &PgPool,
    templates: &SessionTemplateConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match execute_review_orchestration_request(request, digest, pool.clone(), templates).await {
        Ok(message) => write_message(writer, version, request_id, message).await,
        Err(error) => write_review_orchestration_error(writer, version, request_id, error).await,
    }
}

async fn handle_read_review_orchestration<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: ClientRequest,
    pool: &PgPool,
    templates: &SessionTemplateConfiguration,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let result = read_review_orchestration_request(request, [0; 32], pool.clone(), templates).await;
    drop(snapshot_permit);
    match result {
        Ok(message) => write_message(writer, version, request_id, message).await,
        Err(error) => write_review_orchestration_error(writer, version, request_id, error).await,
    }
}

async fn write_review_orchestration_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    error: ReviewOrchestrationRuntimeError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let protocol_error = match error {
        ReviewOrchestrationRuntimeError::InvalidRequest
        | ReviewOrchestrationRuntimeError::Rejected => {
            ProtocolError::without_detail(ErrorCode::InvalidRequest)
        }
        ReviewOrchestrationRuntimeError::NotFound => {
            ProtocolError::without_detail(ErrorCode::NotFound)
        }
        ReviewOrchestrationRuntimeError::ConflictingReuse => {
            ProtocolError::without_detail(ErrorCode::ConflictingReuse)
        }
        ReviewOrchestrationRuntimeError::Unavailable { commit_ambiguous } => {
            review_orchestration_unavailable_error(commit_ambiguous)
        }
        ReviewOrchestrationRuntimeError::Internal { session_id, cause } => {
            let diagnostic = match cause {
                ReviewOrchestrationInternalCause::StoreCorruption => {
                    InternalDiagnostic::ReviewOrchestrationStoreCorruption
                }
                ReviewOrchestrationInternalCause::WorkflowCorruption => {
                    InternalDiagnostic::ReviewOrchestrationWorkflowCorruption
                }
                ReviewOrchestrationInternalCause::SessionCorruption => {
                    InternalDiagnostic::ReviewOrchestrationSessionCorruption
                }
                ReviewOrchestrationInternalCause::ServiceContract => {
                    InternalDiagnostic::ReviewOrchestrationServiceContract
                }
            };
            internal_protocol_error(session_id, diagnostic)
        }
    };
    write_error(writer, version, request_id, protocol_error).await
}

fn review_orchestration_unavailable_error(commit_ambiguous: bool) -> ProtocolError {
    let failure_class = OperatorFailureClass::Infrastructure { commit_ambiguous };
    let cause_code = if commit_ambiguous {
        "review_orchestration_commit_ambiguous"
    } else {
        "review_orchestration_database_unavailable"
    };
    tracing::error!(
        ?failure_class,
        cause_code,
        session_id = tracing::field::Empty,
        "review orchestration request failed"
    );
    ProtocolError::mutation_unavailable(commit_ambiguous)
}

fn required_review_digest(digest: Option<[u8; 32]>) -> Result<[u8; 32], ProcessConnectionError> {
    digest.ok_or(ProcessConnectionError::EncodeInvariant)
}

#[allow(clippy::too_many_arguments)]
async fn handle_create_review_target<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    target_id: CanonicalUuid,
    provider: String,
    repository: String,
    subject: WireReviewTargetSubject,
    head_revision: String,
    base_revision: Option<String>,
    stack_parent_target_id: Option<CanonicalUuid>,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::CreateTarget,
    )
    .await?
    {
        return Ok(());
    }
    let store = ReviewWorkflowStore::new(pool.clone());
    let parent = match stack_parent_target_id {
        Some(parent) => match store
            .load_target(ReviewTargetId::from_uuid(parent.into_uuid()))
            .await
        {
            Ok(Some(parent)) => Some(parent),
            Ok(None) => return write_review_invalid(writer, version, request_id).await,
            Err(error) => {
                return write_review_store_error(writer, version, request_id, error).await;
            }
        },
        None => None,
    };
    let subject = match subject {
        WireReviewTargetSubject::ChangeRequest { number } => {
            let Ok(number) = ReviewChangeRequestNumber::try_new(number.value()) else {
                return write_review_invalid(writer, version, request_id).await;
            };
            ReviewTargetSubject::ChangeRequest(number)
        }
        WireReviewTargetSubject::Commit {} => ReviewTargetSubject::Commit,
    };
    let values = (
        ReviewKey::try_new(provider),
        ReviewKey::try_new(repository),
        ReviewKey::try_new(head_revision),
        base_revision.map(ReviewKey::try_new).transpose(),
    );
    let (Ok(provider), Ok(repository), Ok(head), Ok(base)) = values else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let Ok(target) = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(target_id.into_uuid()),
        provider,
        repository,
        subject,
        head,
        base,
        parent.as_ref(),
    ) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::CreateTarget(target),
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

#[allow(clippy::too_many_arguments)]
async fn handle_start_review_run<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    target_id: CanonicalUuid,
    run_id: CanonicalUuid,
    pass_id: CanonicalUuid,
    workflow: WireReviewWorkflow,
    session_id: CanonicalUuid,
    accepted_input_id: CanonicalUuid,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::StartRun,
    )
    .await?
    {
        return Ok(());
    }
    let store = ReviewWorkflowStore::new(pool.clone());
    match store
        .load_target(ReviewTargetId::from_uuid(target_id.into_uuid()))
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    }
    let row = match sqlx::query(
        "SELECT session_id, origin_turn_id FROM accepted_input WHERE accepted_input_id = $1",
    )
    .bind(accepted_input_id.into_uuid())
    .fetch_optional(pool)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(_) => return write_review_unavailable(writer, version, request_id, false).await,
    };
    let canonical_session: uuid::Uuid = match row.try_get("session_id") {
        Ok(value) => value,
        Err(_) => return write_review_internal(writer, version, request_id).await,
    };
    let origin_turn: Option<uuid::Uuid> = match row.try_get("origin_turn_id") {
        Ok(value) => value,
        Err(_) => return write_review_internal(writer, version, request_id).await,
    };
    if canonical_session != session_id.into_uuid() || origin_turn.is_none() {
        return write_review_invalid(writer, version, request_id).await;
    }
    let reference = ReviewRunRef::new(
        ReviewTargetId::from_uuid(target_id.into_uuid()),
        ReviewRunId::from_uuid(run_id.into_uuid()),
    );
    let (workflow_kind, pass_kind) = review_workflow_kind(workflow);
    let mut run = ReviewRun::new(reference, workflow_kind, ReviewPolicy::version_one());
    let pass = ReviewPass::try_new(
        ReviewPassRef::new(reference, ReviewPassId::from_uuid(pass_id.into_uuid())),
        pass_kind,
        &mut run,
        SessionId::from_uuid(session_id.into_uuid()),
        ReviewPassAcceptedInputEvidence::new(
            AcceptedInputId::from_uuid(accepted_input_id.into_uuid()),
            SessionId::from_uuid(canonical_session),
            origin_turn.map(TurnId::from_uuid),
        ),
    );
    let Ok(pass) = pass else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::StartRun { run, pass },
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

const fn review_workflow_kind(
    workflow: WireReviewWorkflow,
) -> (ReviewWorkflowKind, ReviewPassKind) {
    match workflow {
        WireReviewWorkflow::ImportExternalContext => (
            ReviewWorkflowKind::ImportExternalContext,
            ReviewPassKind::ImportExternalContext,
        ),
        WireReviewWorkflow::ReadOnlyReview => (
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPassKind::ReadOnlyReview,
        ),
        WireReviewWorkflow::JudgeFindings => {
            (ReviewWorkflowKind::JudgeFindings, ReviewPassKind::Judge)
        }
        WireReviewWorkflow::DedupeFindings => {
            (ReviewWorkflowKind::DedupeFindings, ReviewPassKind::Dedupe)
        }
        WireReviewWorkflow::PublishReview => {
            (ReviewWorkflowKind::PublishReview, ReviewPassKind::Publish)
        }
        WireReviewWorkflow::FixFindings => (ReviewWorkflowKind::FixFindings, ReviewPassKind::Fix),
        WireReviewWorkflow::PropagateStack => (
            ReviewWorkflowKind::PropagateStack,
            ReviewPassKind::PropagateStack,
        ),
    }
}

async fn replay_review_command<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    pool: &PgPool,
    command_id: signalbox_process_protocol::CommandId,
    digest: [u8; 32],
    operation_kind: ReviewWorkflowOperationKind,
) -> Result<bool, ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let store = ReviewWorkflowStore::new(pool.clone());
    match store
        .load_command_outcome(
            DurableCommandId::from_uuid(command_id.into_uuid()),
            digest,
            operation_kind,
        )
        .await
    {
        Ok(Some(outcome)) => {
            write_review_command_outcome(writer, version, request_id, outcome).await?;
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(error) => {
            write_review_store_error(writer, version, request_id, error).await?;
            Ok(true)
        }
    }
}

async fn execute_review_command<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    pool: &PgPool,
    command: ReviewWorkflowCommand,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut service = ReviewWorkflowCommandService::new(ReviewWorkflowStore::new(pool.clone()));
    match service.execute(command).await {
        Ok(outcome) => write_review_command_outcome(writer, version, request_id, outcome).await,
        Err(error) => write_review_store_error(writer, version, request_id, error).await,
    }
}

async fn write_review_command_outcome<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    outcome: ReviewWorkflowCommandOutcome,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match outcome {
        ReviewWorkflowCommandOutcome::Recorded(result) => {
            write_review_command_result(writer, version, request_id, result).await
        }
        ReviewWorkflowCommandOutcome::ConflictingReuse { .. } => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
    }
}

async fn write_review_command_result<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    result: ReviewWorkflowCommandResult,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let message = match result {
        ReviewWorkflowCommandResult::TargetCreated { target } => {
            ServerMessage::ReviewTargetCreated {
                target_id: wire_uuid(target.into_uuid()),
            }
        }
        ReviewWorkflowCommandResult::RunStarted { run, pass } => ServerMessage::ReviewRunStarted {
            run_id: wire_uuid(run.into_uuid()),
            pass_id: wire_uuid(pass.into_uuid()),
        },
        ReviewWorkflowCommandResult::PassActivated { run, pass } => {
            ServerMessage::ReviewPassActivated {
                run_id: wire_uuid(run.into_uuid()),
                pass_id: wire_uuid(pass.into_uuid()),
            }
        }
        ReviewWorkflowCommandResult::PassCompleted { run, pass, status } => {
            let state = match status {
                ReviewPassCompletionStatus::Succeeded => ReviewPassLifecycle::Succeeded,
                ReviewPassCompletionStatus::Failed => ReviewPassLifecycle::Failed,
                ReviewPassCompletionStatus::Blocked => ReviewPassLifecycle::Blocked,
                ReviewPassCompletionStatus::Cancelled => ReviewPassLifecycle::Cancelled,
            };
            ServerMessage::ReviewPassCompleted {
                run_id: wire_uuid(run.into_uuid()),
                pass_id: wire_uuid(pass.into_uuid()),
                state,
            }
        }
        ReviewWorkflowCommandResult::FindingsRecorded {
            run,
            pass,
            finding_count,
        } => {
            let count = u64::try_from(finding_count)
                .map_err(|_| ProcessConnectionError::EncodeInvariant)?;
            ServerMessage::ReviewFindingsRecorded {
                run_id: wire_uuid(run.into_uuid()),
                pass_id: wire_uuid(pass.into_uuid()),
                finding_count: CanonicalU64::new(count),
            }
        }
        ReviewWorkflowCommandResult::FindingEventRecorded { finding, status } => {
            ServerMessage::ReviewFindingEventRecorded {
                finding_id: wire_uuid(finding.into_uuid()),
                status: wire_review_finding_status(status),
            }
        }
        ReviewWorkflowCommandResult::ExternalLinkReserved { link } => {
            ServerMessage::ReviewExternalLinkReserved {
                external_link_id: wire_uuid(link.into_uuid()),
            }
        }
        ReviewWorkflowCommandResult::ExternalLinkAttached {
            link,
            external_object,
        } => ServerMessage::ReviewExternalLinkAttached {
            external_link_id: wire_uuid(link.into_uuid()),
            external_object: external_object.into_string(),
        },
    };
    write_message(writer, version, request_id, message).await
}

fn review_activation_was_applied(run: &ReviewRun, pass: &ReviewPass, turn: TurnId) -> bool {
    let reference = pass.reference();
    let run_retains_pass = match run.state() {
        ReviewRunState::Running { active_pass }
        | ReviewRunState::Succeeded {
            concluding_pass: active_pass,
        }
        | ReviewRunState::Failed {
            failed_pass: active_pass,
        }
        | ReviewRunState::Blocked {
            blocking_pass: active_pass,
        }
        | ReviewRunState::Cancelled {
            last_pass: Some(active_pass),
        } => active_pass == reference,
        ReviewRunState::Queued | ReviewRunState::Cancelled { last_pass: None } => false,
    };
    let pass_retains_turn = match pass.state() {
        ReviewPassState::Running { turn: retained }
        | ReviewPassState::Succeeded { turn: retained, .. }
        | ReviewPassState::Failed { turn: retained }
        | ReviewPassState::Blocked { turn: retained, .. }
        | ReviewPassState::Cancelled {
            turn: Some(retained),
        } => *retained == turn,
        ReviewPassState::Queued | ReviewPassState::Cancelled { turn: None } => false,
    };
    run_retains_pass && pass_retains_turn
}

fn historical_review_activation(
    current_run: &ReviewRun,
    current_pass: &ReviewPass,
    turn: TurnId,
) -> Option<(ReviewRun, ReviewPass)> {
    let mut run = ReviewRun::new(
        current_run.reference(),
        current_run.workflow(),
        current_run.policy(),
    );
    let pass = ReviewPass::try_new(
        current_pass.reference(),
        current_pass.kind(),
        &mut run,
        current_pass.session(),
        ReviewPassAcceptedInputEvidence::new(
            current_pass.accepted_input(),
            current_pass.session(),
            Some(current_pass.origin_turn()),
        ),
    )
    .ok()?;
    let pass = pass
        .transition(
            ReviewPassState::Running { turn },
            Some(ReviewPassTurnEvidence::new(
                turn,
                current_pass.session(),
                current_pass.accepted_input(),
                ReviewPassTurnOutcome::Active,
                None,
            )),
        )
        .ok()?;
    let pass_evidence = ReviewPassEvidence::from_pass(&pass, current_run.policy());
    let run = run
        .transition(
            ReviewRunState::Running {
                active_pass: current_pass.reference(),
            },
            Some(pass_evidence),
        )
        .ok()?;
    Some((run, pass))
}

#[allow(clippy::too_many_arguments)]
async fn handle_activate_review_pass<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    run_id: CanonicalUuid,
    pass_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::ActivatePass,
    )
    .await?
    {
        return Ok(());
    }
    let run_id = ReviewRunId::from_uuid(run_id.into_uuid());
    let pass_id = ReviewPassId::from_uuid(pass_id.into_uuid());
    let turn_id = TurnId::from_uuid(turn_id.into_uuid());
    let store = ReviewWorkflowStore::new(pool.clone());
    let current_run = match store.load_run(run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let current_pass = match store.load_pass(pass_id).await {
        Ok(Some(pass)) => pass,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    if current_pass.reference().run().run() != run_id {
        return write_review_invalid(writer, version, request_id).await;
    }
    let row = match sqlx::query(
        "SELECT session_id, origin_accepted_input_id, state_kind
           FROM turn_lifecycle
          WHERE turn_id = $1",
    )
    .bind(turn_id.into_uuid())
    .fetch_optional(pool)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(_) => return write_review_unavailable(writer, version, request_id, false).await,
    };
    let canonical_session: uuid::Uuid = match row.try_get("session_id") {
        Ok(value) => value,
        Err(_) => return write_review_internal(writer, version, request_id).await,
    };
    let canonical_input: uuid::Uuid = match row.try_get("origin_accepted_input_id") {
        Ok(value) => value,
        Err(_) => return write_review_internal(writer, version, request_id).await,
    };
    let state: String = match row.try_get("state_kind") {
        Ok(value) => value,
        Err(_) => return write_review_internal(writer, version, request_id).await,
    };
    if canonical_session != current_pass.session().into_uuid()
        || canonical_input != current_pass.accepted_input().into_uuid()
        || (state != "active" && state != "terminal")
    {
        return write_review_invalid(writer, version, request_id).await;
    }
    let evidence = ReviewPassTurnEvidence::new(
        turn_id,
        current_pass.session(),
        current_pass.accepted_input(),
        ReviewPassTurnOutcome::Active,
        None,
    );
    let policy = current_run.policy();
    let active_pass_state = ReviewPassState::Running { turn: turn_id };
    let active_run_state = ReviewRunState::Running {
        active_pass: current_pass.reference(),
    };
    let (run, pass) = if state == "active"
        && current_run.state() == ReviewRunState::Queued
        && current_pass.state() == &ReviewPassState::Queued
    {
        let Ok(pass) = current_pass.transition(active_pass_state, Some(evidence)) else {
            return write_review_invalid(writer, version, request_id).await;
        };
        let pass_evidence = ReviewPassEvidence::from_pass(&pass, policy);
        let Ok(run) = current_run.transition(active_run_state, Some(pass_evidence)) else {
            return write_review_invalid(writer, version, request_id).await;
        };
        (run, pass)
    } else if review_activation_was_applied(&current_run, &current_pass, turn_id) {
        let Some(activation) = historical_review_activation(&current_run, &current_pass, turn_id)
        else {
            return write_review_internal(writer, version, request_id).await;
        };
        activation
    } else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::ActivatePass { run, pass },
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

#[allow(clippy::too_many_arguments)]
async fn handle_complete_review_pass<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    run_id: CanonicalUuid,
    pass_id: CanonicalUuid,
    turn_id: Option<CanonicalUuid>,
    output_frontier_id: Option<CanonicalUuid>,
    outcome: ReviewPassTerminalOutcome,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::CompletePass,
    )
    .await?
    {
        return Ok(());
    }
    let run_id = ReviewRunId::from_uuid(run_id.into_uuid());
    let pass_id = ReviewPassId::from_uuid(pass_id.into_uuid());
    let store = ReviewWorkflowStore::new(pool.clone());
    let current_run = match store.load_run(run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let current_pass = match store.load_pass(pass_id).await {
        Ok(Some(pass)) => pass,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    if current_pass.reference().run().run() != run_id {
        return write_review_invalid(writer, version, request_id).await;
    }
    if matches!(outcome, ReviewPassTerminalOutcome::Succeeded)
        && current_pass.kind() == ReviewPassKind::ReadOnlyReview
    {
        return write_review_invalid(writer, version, request_id).await;
    }
    let completed = match (outcome, turn_id, output_frontier_id) {
        (ReviewPassTerminalOutcome::Cancelled, None, None) => {
            complete_queued_review_pass(current_run, current_pass)
        }
        (ReviewPassTerminalOutcome::Succeeded, Some(turn), Some(frontier)) => {
            let Some(completed) = complete_review_pass(
                writer,
                version,
                request_id,
                pool,
                current_run,
                current_pass,
                ReviewPassState::Succeeded {
                    turn: TurnId::from_uuid(turn.into_uuid()),
                    output_frontier: ContextFrontierId::from_uuid(frontier.into_uuid()),
                    result: None,
                },
            )
            .await?
            else {
                return Ok(());
            };
            Some(completed)
        }
        (ReviewPassTerminalOutcome::Failed, Some(turn), None) => {
            let Some(completed) = complete_review_pass(
                writer,
                version,
                request_id,
                pool,
                current_run,
                current_pass,
                ReviewPassState::Failed {
                    turn: TurnId::from_uuid(turn.into_uuid()),
                },
            )
            .await?
            else {
                return Ok(());
            };
            Some(completed)
        }
        (ReviewPassTerminalOutcome::Blocked, Some(turn), None) => {
            let Some(completed) = complete_review_pass(
                writer,
                version,
                request_id,
                pool,
                current_run,
                current_pass,
                ReviewPassState::Blocked {
                    turn: TurnId::from_uuid(turn.into_uuid()),
                    result: None,
                },
            )
            .await?
            else {
                return Ok(());
            };
            Some(completed)
        }
        (ReviewPassTerminalOutcome::Cancelled, Some(turn), None) => {
            let Some(completed) = complete_review_pass(
                writer,
                version,
                request_id,
                pool,
                current_run,
                current_pass,
                ReviewPassState::Cancelled {
                    turn: Some(TurnId::from_uuid(turn.into_uuid())),
                },
            )
            .await?
            else {
                return Ok(());
            };
            Some(completed)
        }
        _ => return write_review_invalid(writer, version, request_id).await,
    };
    let Some((run, pass)) = completed.map(|(pass, run)| (run, pass)) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::CompletePass { run, pass },
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

fn complete_queued_review_pass(
    current_run: ReviewRun,
    current_pass: ReviewPass,
) -> Option<(ReviewPass, ReviewRun)> {
    let next_pass = ReviewPassState::Cancelled { turn: None };
    let pass = if current_pass.state() == &next_pass {
        current_pass
    } else {
        current_pass.transition(next_pass, None).ok()?
    };
    let pass_evidence = ReviewPassEvidence::from_pass(&pass, current_run.policy());
    let next_run = ReviewRunState::Cancelled {
        last_pass: Some(pass.reference()),
    };
    let run = if current_run.state() == next_run {
        current_run
    } else {
        current_run.transition(next_run, Some(pass_evidence)).ok()?
    };
    Some((pass, run))
}

#[allow(clippy::too_many_arguments)]
async fn handle_record_review_findings<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    run_id: CanonicalUuid,
    pass_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    output_frontier_id: CanonicalUuid,
    inputs: Vec<ReviewFindingInput>,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::RecordFindings,
    )
    .await?
    {
        return Ok(());
    }
    let run_id = ReviewRunId::from_uuid(run_id.into_uuid());
    let pass_id = ReviewPassId::from_uuid(pass_id.into_uuid());
    let store = ReviewWorkflowStore::new(pool.clone());
    let current_run = match store.load_run(run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let current_pass = match store.load_pass(pass_id).await {
        Ok(Some(pass)) => pass,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    if current_pass.reference().run().run() != run_id
        || current_pass.kind() != ReviewPassKind::ReadOnlyReview
    {
        return write_review_invalid(writer, version, request_id).await;
    }
    let references = inputs
        .iter()
        .map(|input| {
            ReviewFindingRef::new(
                current_pass.reference(),
                ReviewFindingId::from_uuid(input.finding_id.into_uuid()),
            )
        })
        .collect::<Vec<_>>();
    let Ok(inventory) = ReviewProducedFindings::try_new(references) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let next = ReviewPassState::Succeeded {
        turn: TurnId::from_uuid(turn_id.into_uuid()),
        output_frontier: ContextFrontierId::from_uuid(output_frontier_id.into_uuid()),
        result: Some(ReviewPassResult::ProducedFindings(inventory)),
    };
    let Some((completed_pass, completed_run)) = complete_review_pass(
        writer,
        version,
        request_id,
        pool,
        current_run,
        current_pass,
        next,
    )
    .await?
    else {
        return Ok(());
    };
    let pass_evidence = ReviewPassEvidence::from_pass(&completed_pass, completed_run.policy());
    let run_evidence = completed_run.evidence();
    let target = match store.load_target(pass_evidence.reference().target()).await {
        Ok(Some(target)) => target,
        Ok(None) => return write_review_internal(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let mut findings = Vec::with_capacity(inputs.len());
    for input in inputs {
        let Some((finding_id, content)) = review_finding_content(input) else {
            return write_review_invalid(writer, version, request_id).await;
        };
        let reference = ReviewFindingRef::new(
            pass_evidence.reference(),
            ReviewFindingId::from_uuid(finding_id.into_uuid()),
        );
        let Ok(proposal) = ReviewFindingProposal::try_new(
            reference,
            pass_evidence.clone(),
            run_evidence,
            &target,
            content,
        ) else {
            return write_review_invalid(writer, version, request_id).await;
        };
        findings.push(ReviewFinding::new(proposal));
    }
    findings.sort_unstable_by_key(|finding| finding.proposal().reference());
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::RecordFindings {
            pass: pass_evidence,
            findings,
        },
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

#[allow(clippy::too_many_arguments)]
async fn handle_record_review_disposition<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    run_id: CanonicalUuid,
    pass_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    output_frontier_id: Option<CanonicalUuid>,
    finding_id: CanonicalUuid,
    event_ordinal: CanonicalU64,
    event: WireReviewFindingEvent,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::RecordFindingEvent,
    )
    .await?
    {
        return Ok(());
    }
    let run_id = ReviewRunId::from_uuid(run_id.into_uuid());
    let pass_id = ReviewPassId::from_uuid(pass_id.into_uuid());
    let finding_id = ReviewFindingId::from_uuid(finding_id.into_uuid());
    let store = ReviewWorkflowStore::new(pool.clone());
    let current_run = match store.load_run(run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let current_pass = match store.load_pass(pass_id).await {
        Ok(Some(pass)) => pass,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let current_finding = match store.load_finding(finding_id).await {
        Ok(Some(finding)) => finding,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    if current_pass.reference().run().run() != run_id
        || current_finding.proposal().reference().target() != current_pass.reference().target()
    {
        return write_review_invalid(writer, version, request_id).await;
    }
    let Ok(ordinal_value) = u32::try_from(event_ordinal.value()) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let Ok(ordinal) = ReviewEventOrdinal::try_new(ordinal_value) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let (result_kind, event_kind, blocked) = match event {
        WireReviewFindingEvent::Accepted {} => (
            ReviewFindingEventResultKind::Accepted,
            ReviewFindingEventKind::Accepted,
            false,
        ),
        WireReviewFindingEvent::Rejected { reason } => {
            let Ok(reason) = ReviewText::try_new(reason) else {
                return write_review_invalid(writer, version, request_id).await;
            };
            (
                ReviewFindingEventResultKind::Rejected {
                    reason: reason.clone(),
                },
                ReviewFindingEventKind::Rejected { reason },
                false,
            )
        }
        WireReviewFindingEvent::Duplicate {
            canonical_finding_id,
        } => {
            let referenced = match store
                .load_finding(ReviewFindingId::from_uuid(canonical_finding_id.into_uuid()))
                .await
            {
                Ok(Some(finding)) => finding,
                Ok(None) => return write_review_invalid(writer, version, request_id).await,
                Err(error) => {
                    return write_review_store_error(writer, version, request_id, error).await;
                }
            };
            let Some(canonical) = ReviewReferencedFindingEvidence::try_from_finding(&referenced)
            else {
                return write_review_invalid(writer, version, request_id).await;
            };
            (
                ReviewFindingEventResultKind::Duplicate { canonical },
                ReviewFindingEventKind::Duplicate { canonical },
                false,
            )
        }
        WireReviewFindingEvent::Superseded {
            successor_finding_id,
        } => {
            let referenced = match store
                .load_finding(ReviewFindingId::from_uuid(successor_finding_id.into_uuid()))
                .await
            {
                Ok(Some(finding)) => finding,
                Ok(None) => return write_review_invalid(writer, version, request_id).await,
                Err(error) => {
                    return write_review_store_error(writer, version, request_id, error).await;
                }
            };
            let Some(successor) = ReviewReferencedFindingEvidence::try_from_finding(&referenced)
            else {
                return write_review_invalid(writer, version, request_id).await;
            };
            (
                ReviewFindingEventResultKind::Superseded { successor },
                ReviewFindingEventKind::Superseded { successor },
                false,
            )
        }
        WireReviewFindingEvent::Stale {} => (
            ReviewFindingEventResultKind::Stale,
            ReviewFindingEventKind::Stale,
            false,
        ),
        WireReviewFindingEvent::Fixed {} => (
            ReviewFindingEventResultKind::Fixed,
            ReviewFindingEventKind::Fixed,
            false,
        ),
        WireReviewFindingEvent::BlockedWithReason {
            reason,
            external_link_id,
        } => {
            let Ok(reason) = ReviewText::try_new(reason) else {
                return write_review_invalid(writer, version, request_id).await;
            };
            let link = match external_link_id {
                Some(link_id) => {
                    let link_id = ReviewExternalLinkId::from_uuid(link_id.into_uuid());
                    let link = match store.load_external_link(link_id).await {
                        Ok(Some(link)) => link,
                        Ok(None) => {
                            return write_review_invalid(writer, version, request_id).await;
                        }
                        Err(error) => {
                            return write_review_store_error(writer, version, request_id, error)
                                .await;
                        }
                    };
                    let Ok(reference) = ReviewFindingPendingExternalLinkRef::try_new(
                        current_finding.proposal().reference(),
                        &link,
                    ) else {
                        return write_review_invalid(writer, version, request_id).await;
                    };
                    Some(reference)
                }
                None => None,
            };
            (
                ReviewFindingEventResultKind::BlockedWithReason {
                    reason: reason.clone(),
                    link: link.as_ref().map(ReviewFindingPendingExternalLinkRef::link),
                },
                ReviewFindingEventKind::BlockedWithReason {
                    reason,
                    link: link.map(Box::new),
                },
                true,
            )
        }
    };
    let finding_reference = current_finding.proposal().reference();
    let result = ReviewFindingEventResult::new(finding_reference, ordinal, result_kind);
    let next = if blocked {
        if output_frontier_id.is_some() {
            return write_review_invalid(writer, version, request_id).await;
        }
        ReviewPassState::Blocked {
            turn: TurnId::from_uuid(turn_id.into_uuid()),
            result: Some(ReviewPassResult::FindingEvent(result)),
        }
    } else {
        let Some(output_frontier_id) = output_frontier_id else {
            return write_review_invalid(writer, version, request_id).await;
        };
        ReviewPassState::Succeeded {
            turn: TurnId::from_uuid(turn_id.into_uuid()),
            output_frontier: ContextFrontierId::from_uuid(output_frontier_id.into_uuid()),
            result: Some(ReviewPassResult::FindingEvent(result)),
        }
    };
    let Some((completed_pass, completed_run)) = complete_review_pass(
        writer,
        version,
        request_id,
        pool,
        current_run,
        current_pass,
        next,
    )
    .await?
    else {
        return Ok(());
    };
    let pass_evidence = ReviewPassEvidence::from_pass(&completed_pass, completed_run.policy());
    let run_evidence = completed_run.evidence();
    let event = ReviewFindingEvent::new(
        finding_reference,
        ordinal,
        pass_evidence.reference(),
        pass_evidence.clone(),
        run_evidence,
        event_kind,
    );
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::RecordFindingEvent {
            pass: pass_evidence,
            event,
        },
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

#[allow(clippy::too_many_arguments)]
async fn handle_reserve_review_external_link<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    external_link_id: CanonicalUuid,
    finding_id: CanonicalUuid,
    provider: String,
    object_kind: WireReviewExternalObjectKind,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::ReserveExternalLink,
    )
    .await?
    {
        return Ok(());
    }
    let store = ReviewWorkflowStore::new(pool.clone());
    let finding = match store
        .load_finding(ReviewFindingId::from_uuid(finding_id.into_uuid()))
        .await
    {
        Ok(Some(finding)) => finding,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let association = ReviewExternalLinkAssociation::Finding(finding.proposal().reference());
    let target = match store.load_target(association.target()).await {
        Ok(Some(target)) => target,
        Ok(None) => return write_review_internal(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let object_kind = match object_kind {
        WireReviewExternalObjectKind::Review => ReviewExternalObjectKind::Review,
        WireReviewExternalObjectKind::ReviewThread => ReviewExternalObjectKind::ReviewThread,
        WireReviewExternalObjectKind::ReviewComment => ReviewExternalObjectKind::ReviewComment,
        WireReviewExternalObjectKind::ChangeRequestComment => {
            ReviewExternalObjectKind::ChangeRequestComment
        }
    };
    let Ok(provider) = ReviewKey::try_new(provider) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let link_id = ReviewExternalLinkId::from_uuid(external_link_id.into_uuid());
    let Ok(link) =
        ReviewExternalLink::try_reserve(link_id, association, provider, object_kind, &target)
    else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::ReserveExternalLink(link),
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

#[allow(clippy::too_many_arguments)]
async fn handle_attach_review_external_link<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    digest: [u8; 32],
    command_id: signalbox_process_protocol::CommandId,
    external_link_id: CanonicalUuid,
    run_id: CanonicalUuid,
    pass_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    output_frontier_id: CanonicalUuid,
    external_object: String,
    event_ordinal: CanonicalU64,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if replay_review_command(
        writer,
        version,
        request_id,
        pool,
        command_id,
        digest,
        ReviewWorkflowOperationKind::AttachExternalLink,
    )
    .await?
    {
        return Ok(());
    }
    let link_id = ReviewExternalLinkId::from_uuid(external_link_id.into_uuid());
    let run_id = ReviewRunId::from_uuid(run_id.into_uuid());
    let pass_id = ReviewPassId::from_uuid(pass_id.into_uuid());
    let store = ReviewWorkflowStore::new(pool.clone());
    let link = match store.load_external_link(link_id).await {
        Ok(Some(link)) => link,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let ReviewExternalLinkAssociation::Finding(finding_reference) = link.association() else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let current_run = match store.load_run(run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let current_pass = match store.load_pass(pass_id).await {
        Ok(Some(pass)) => pass,
        Ok(None) => return write_review_invalid(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    if current_pass.reference().run().run() != run_id
        || current_pass.reference().target() != finding_reference.target()
        || current_pass.kind() != ReviewPassKind::Publish
    {
        return write_review_invalid(writer, version, request_id).await;
    }
    let Ok(ordinal_value) = u32::try_from(event_ordinal.value()) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let Ok(ordinal) = ReviewEventOrdinal::try_new(ordinal_value) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let Ok(external_object) = ReviewKey::try_new(external_object) else {
        return write_review_invalid(writer, version, request_id).await;
    };
    let result = ReviewExternalLinkAttachmentResult::new(
        link_id,
        external_object.clone(),
        Some(ReviewFindingEventResult::new(
            finding_reference,
            ordinal,
            ReviewFindingEventResultKind::Posted { link: link_id },
        )),
    );
    let next = ReviewPassState::Succeeded {
        turn: TurnId::from_uuid(turn_id.into_uuid()),
        output_frontier: ContextFrontierId::from_uuid(output_frontier_id.into_uuid()),
        result: Some(ReviewPassResult::ExternalLinkAttachment(result)),
    };
    let Some((completed_pass, completed_run)) = complete_review_pass(
        writer,
        version,
        request_id,
        pool,
        current_run,
        current_pass,
        next,
    )
    .await?
    else {
        return Ok(());
    };
    let pass_evidence = ReviewPassEvidence::from_pass(&completed_pass, completed_run.policy());
    let run_evidence = completed_run.evidence();
    let attachment = ReviewExternalLinkAttachment::new(
        link_id,
        pass_evidence.reference(),
        pass_evidence,
        run_evidence,
        external_object.clone(),
    );
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        digest,
        ReviewWorkflowOperation::AttachExternalLink {
            link: link_id,
            attachment,
        },
    );
    execute_review_command(writer, version, request_id, pool, command).await
}

async fn handle_read_review_target<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    target_id: CanonicalUuid,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let store = ReviewWorkflowStore::new(pool.clone());
    let loaded = store
        .load_target(ReviewTargetId::from_uuid(target_id.into_uuid()))
        .await;
    drop(snapshot_permit);
    match loaded {
        Ok(Some(target)) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::ReviewTarget {
                    target: review_target_snapshot(&target),
                },
            )
            .await
        }
        Ok(None) => write_review_not_found(writer, version, request_id).await,
        Err(error) => write_review_store_error(writer, version, request_id, error).await,
    }
}

async fn handle_read_review_run<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    run_id: CanonicalUuid,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let store = ReviewWorkflowStore::new(pool.clone());
    let loaded = store
        .load_run_with_pass(ReviewRunId::from_uuid(run_id.into_uuid()))
        .await;
    drop(snapshot_permit);
    let (run, pass) = match loaded {
        Ok(Some(aggregate)) => aggregate,
        Ok(None) => return write_review_not_found(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    let pass = pass.as_ref().map(review_pass_snapshot);
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ReviewRun {
            run: review_run_snapshot(&run),
            pass,
        },
    )
    .await
}

async fn handle_read_review_finding<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    finding_id: CanonicalUuid,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let store = ReviewWorkflowStore::new(pool.clone());
    let loaded = store
        .load_finding(ReviewFindingId::from_uuid(finding_id.into_uuid()))
        .await;
    drop(snapshot_permit);
    match loaded {
        Ok(Some(finding)) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::ReviewFinding {
                    finding: review_finding_snapshot(&finding),
                },
            )
            .await
        }
        Ok(None) => write_review_not_found(writer, version, request_id).await,
        Err(error) => write_review_store_error(writer, version, request_id, error).await,
    }
}

/// Loads one run's complete finding page, or `None` when the run is absent.
///
/// The existence check and the graph walk are separate database phases, so they
/// belong to the same reader admission: splitting them would let a listing hold
/// pool capacity it never reserved.
async fn load_review_findings_page(
    store: &ReviewWorkflowStore,
    run_id: ReviewRunId,
) -> Result<Option<Vec<ReviewFinding>>, ReviewWorkflowStoreError> {
    if store.load_run(run_id).await?.is_none() {
        return Ok(None);
    }
    store.list_findings(run_id).await.map(Some)
}

async fn handle_list_review_findings<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    run_id: CanonicalUuid,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let run_id = ReviewRunId::from_uuid(run_id.into_uuid());
    let store = ReviewWorkflowStore::new(pool.clone());
    let loaded = load_review_findings_page(&store, run_id).await;
    drop(snapshot_permit);
    let findings = match loaded {
        Ok(Some(findings)) => findings,
        Ok(None) => return write_review_not_found(writer, version, request_id).await,
        Err(error) => return write_review_store_error(writer, version, request_id, error).await,
    };
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ReviewFindingsStart {
            run_id: wire_uuid(run_id.into_uuid()),
        },
    )
    .await?;
    for finding in &findings {
        write_message(
            writer,
            version,
            request_id,
            ServerMessage::ReviewFindingItem {
                finding: review_finding_snapshot(finding),
            },
        )
        .await?;
    }
    let Ok(finding_count) = u64::try_from(findings.len()) else {
        return Err(ProcessConnectionError::EncodeInvariant);
    };
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ReviewFindingsEnd {
            finding_count: CanonicalU64::new(finding_count),
        },
    )
    .await
}

fn review_target_snapshot(target: &ReviewTarget) -> ReviewTargetSnapshot {
    let subject = match target.subject() {
        ReviewTargetSubject::ChangeRequest(number) => WireReviewTargetSubject::ChangeRequest {
            number: CanonicalU64::new(number.get()),
        },
        ReviewTargetSubject::Commit => WireReviewTargetSubject::Commit {},
    };
    ReviewTargetSnapshot {
        target_id: wire_uuid(target.id().into_uuid()),
        provider: target.provider().as_str().to_owned(),
        repository: target.repository().as_str().to_owned(),
        subject,
        head_revision: target.head_revision().as_str().to_owned(),
        base_revision: target
            .base_revision()
            .map(|revision| revision.as_str().to_owned()),
        stack_parent_target_id: target
            .stack_parent()
            .map(|parent| wire_uuid(parent.target().into_uuid())),
    }
}

const fn wire_review_workflow(workflow: ReviewWorkflowKind) -> WireReviewWorkflow {
    match workflow {
        ReviewWorkflowKind::ImportExternalContext => WireReviewWorkflow::ImportExternalContext,
        ReviewWorkflowKind::ReadOnlyReview => WireReviewWorkflow::ReadOnlyReview,
        ReviewWorkflowKind::JudgeFindings => WireReviewWorkflow::JudgeFindings,
        ReviewWorkflowKind::DedupeFindings => WireReviewWorkflow::DedupeFindings,
        ReviewWorkflowKind::PublishReview => WireReviewWorkflow::PublishReview,
        ReviewWorkflowKind::FixFindings => WireReviewWorkflow::FixFindings,
        ReviewWorkflowKind::PropagateStack => WireReviewWorkflow::PropagateStack,
    }
}

const fn wire_review_pass_kind(kind: ReviewPassKind) -> signalbox_process_protocol::ReviewPassKind {
    match kind {
        ReviewPassKind::ImportExternalContext => {
            signalbox_process_protocol::ReviewPassKind::ImportExternalContext
        }
        ReviewPassKind::ReadOnlyReview => {
            signalbox_process_protocol::ReviewPassKind::ReadOnlyReview
        }
        ReviewPassKind::Judge => signalbox_process_protocol::ReviewPassKind::Judge,
        ReviewPassKind::Dedupe => signalbox_process_protocol::ReviewPassKind::Dedupe,
        ReviewPassKind::Publish => signalbox_process_protocol::ReviewPassKind::Publish,
        ReviewPassKind::Fix => signalbox_process_protocol::ReviewPassKind::Fix,
        ReviewPassKind::PropagateStack => {
            signalbox_process_protocol::ReviewPassKind::PropagateStack
        }
    }
}

fn review_run_snapshot(run: &ReviewRun) -> ReviewRunSnapshot {
    let state = match run.state() {
        ReviewRunState::Queued => ReviewRunLifecycle::Queued,
        ReviewRunState::Running { .. } => ReviewRunLifecycle::Running,
        ReviewRunState::Succeeded { .. } => ReviewRunLifecycle::Succeeded,
        ReviewRunState::Failed { .. } => ReviewRunLifecycle::Failed,
        ReviewRunState::Blocked { .. } => ReviewRunLifecycle::Blocked,
        ReviewRunState::Cancelled { .. } => ReviewRunLifecycle::Cancelled,
    };
    let policy = run.policy();
    ReviewRunSnapshot {
        target_id: wire_uuid(run.reference().target().into_uuid()),
        run_id: wire_uuid(run.reference().run().into_uuid()),
        workflow: wire_review_workflow(run.workflow()),
        policy_version: CanonicalU64::new(u64::from(policy.version().get())),
        minimum_judge_confidence: CanonicalU64::new(u64::from(
            policy.minimum_judge_confidence().basis_points(),
        )),
        minimum_publication_confidence: CanonicalU64::new(u64::from(
            policy.minimum_publication_confidence().basis_points(),
        )),
        state,
        pass_id: run
            .recorded_pass()
            .map(|reference| wire_uuid(reference.pass().into_uuid())),
    }
}

fn review_pass_snapshot(pass: &ReviewPass) -> ReviewPassSnapshot {
    let (state, turn, output_frontier) = match pass.state() {
        ReviewPassState::Queued => (ReviewPassLifecycle::Queued, None, None),
        ReviewPassState::Running { turn } => (
            ReviewPassLifecycle::Running,
            Some(wire_uuid(turn.into_uuid())),
            None,
        ),
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            ..
        } => (
            ReviewPassLifecycle::Succeeded,
            Some(wire_uuid(turn.into_uuid())),
            Some(wire_uuid(output_frontier.into_uuid())),
        ),
        ReviewPassState::Failed { turn } => (
            ReviewPassLifecycle::Failed,
            Some(wire_uuid(turn.into_uuid())),
            None,
        ),
        ReviewPassState::Blocked { turn, .. } => (
            ReviewPassLifecycle::Blocked,
            Some(wire_uuid(turn.into_uuid())),
            None,
        ),
        ReviewPassState::Cancelled { turn } => (
            ReviewPassLifecycle::Cancelled,
            turn.map(|turn| wire_uuid(turn.into_uuid())),
            None,
        ),
    };
    ReviewPassSnapshot {
        pass_id: wire_uuid(pass.reference().pass().into_uuid()),
        run_id: wire_uuid(pass.reference().run().run().into_uuid()),
        target_id: wire_uuid(pass.reference().target().into_uuid()),
        kind: wire_review_pass_kind(pass.kind()),
        session_id: wire_uuid(pass.session().into_uuid()),
        accepted_input_id: wire_uuid(pass.accepted_input().into_uuid()),
        origin_turn_id: wire_uuid(pass.origin_turn().into_uuid()),
        state,
        turn_id: turn,
        output_frontier_id: output_frontier,
    }
}

const fn wire_review_finding_status(
    status: signalbox_domain::ReviewFindingStatus,
) -> WireReviewFindingStatus {
    match status {
        signalbox_domain::ReviewFindingStatus::Open => WireReviewFindingStatus::Open,
        signalbox_domain::ReviewFindingStatus::Accepted => WireReviewFindingStatus::Accepted,
        signalbox_domain::ReviewFindingStatus::Rejected => WireReviewFindingStatus::Rejected,
        signalbox_domain::ReviewFindingStatus::Duplicate => WireReviewFindingStatus::Duplicate,
        signalbox_domain::ReviewFindingStatus::Superseded => WireReviewFindingStatus::Superseded,
        signalbox_domain::ReviewFindingStatus::Stale => WireReviewFindingStatus::Stale,
        signalbox_domain::ReviewFindingStatus::Posted => WireReviewFindingStatus::Posted,
        signalbox_domain::ReviewFindingStatus::Fixed => WireReviewFindingStatus::Fixed,
        signalbox_domain::ReviewFindingStatus::BlockedWithReason => {
            WireReviewFindingStatus::BlockedWithReason
        }
    }
}

fn review_finding_snapshot(finding: &ReviewFinding) -> ReviewFindingSnapshot {
    let reference = finding.proposal().reference();
    let content = finding.proposal().content();
    let location = content.location();
    let line_range = location.line_range();
    let diff_side = location.diff_side().map(|side| match side {
        ReviewFindingDiffSide::Left => WireReviewDiffSide::Left,
        ReviewFindingDiffSide::Right => WireReviewDiffSide::Right,
    });
    let severity = match content.severity() {
        ReviewFindingSeverity::Info => WireReviewSeverity::Info,
        ReviewFindingSeverity::Low => WireReviewSeverity::Low,
        ReviewFindingSeverity::Medium => WireReviewSeverity::Medium,
        ReviewFindingSeverity::High => WireReviewSeverity::High,
        ReviewFindingSeverity::Critical => WireReviewSeverity::Critical,
    };
    let event_count = u64::try_from(finding.events().len()).unwrap_or(u64::MAX);
    ReviewFindingSnapshot {
        target_id: wire_uuid(reference.target().into_uuid()),
        run_id: wire_uuid(reference.run().run().into_uuid()),
        producing_pass_id: wire_uuid(reference.pass().pass().into_uuid()),
        finding: ReviewFindingInput {
            finding_id: wire_uuid(reference.finding().into_uuid()),
            file_path: location.file_path().as_str().to_owned(),
            line_start: line_range.map(|range| CanonicalU64::new(u64::from(range.start()))),
            line_end: line_range.map(|range| CanonicalU64::new(u64::from(range.end()))),
            diff_side,
            title: content.title().as_str().to_owned(),
            body: content.body().as_str().to_owned(),
            severity,
            is_real_confidence: CanonicalU64::new(u64::from(
                content.is_real_confidence().basis_points(),
            )),
            severity_label_confidence: CanonicalU64::new(u64::from(
                content.severity_label_confidence().basis_points(),
            )),
            category: content.category().as_str().to_owned(),
            recommended_fix: content
                .recommended_fix()
                .map(|text| text.as_str().to_owned()),
        },
        status: wire_review_finding_status(finding.status()),
        event_count: CanonicalU64::new(event_count),
    }
}

async fn complete_review_pass<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    pool: &PgPool,
    current_run: signalbox_domain::ReviewRun,
    current_pass: ReviewPass,
    next: ReviewPassState,
) -> Result<Option<(ReviewPass, ReviewRun)>, ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let turn = match &next {
        ReviewPassState::Succeeded { turn, .. }
        | ReviewPassState::Failed { turn }
        | ReviewPassState::Blocked { turn, .. }
        | ReviewPassState::Cancelled { turn: Some(turn) } => *turn,
        ReviewPassState::Queued
        | ReviewPassState::Running { .. }
        | ReviewPassState::Cancelled { turn: None } => {
            return Err(ProcessConnectionError::EncodeInvariant);
        }
    };
    let row = match sqlx::query(
        "SELECT session_id, origin_accepted_input_id, state_kind,
                terminal_disposition_kind, terminal_frontier_id
           FROM turn_lifecycle
          WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_optional(pool)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            return write_review_invalid(writer, version, request_id)
                .await
                .map(|()| None);
        }
        Err(_) => {
            return write_review_unavailable(writer, version, request_id, false)
                .await
                .map(|()| None);
        }
    };
    let canonical_session: uuid::Uuid = match row.try_get("session_id") {
        Ok(value) => value,
        Err(_) => {
            return write_review_internal(writer, version, request_id)
                .await
                .map(|()| None);
        }
    };
    let canonical_input: uuid::Uuid = match row.try_get("origin_accepted_input_id") {
        Ok(value) => value,
        Err(_) => {
            return write_review_internal(writer, version, request_id)
                .await
                .map(|()| None);
        }
    };
    let state: String = match row.try_get("state_kind") {
        Ok(value) => value,
        Err(_) => {
            return write_review_internal(writer, version, request_id)
                .await
                .map(|()| None);
        }
    };
    let disposition: Option<String> = match row.try_get("terminal_disposition_kind") {
        Ok(value) => value,
        Err(_) => {
            return write_review_internal(writer, version, request_id)
                .await
                .map(|()| None);
        }
    };
    let frontier: Option<uuid::Uuid> = match row.try_get("terminal_frontier_id") {
        Ok(value) => value,
        Err(_) => {
            return write_review_internal(writer, version, request_id)
                .await
                .map(|()| None);
        }
    };
    let expected_frontier = match &next {
        ReviewPassState::Succeeded {
            output_frontier, ..
        } => Some(output_frontier.into_uuid()),
        _ => frontier,
    };
    if canonical_session != current_pass.session().into_uuid()
        || canonical_input != current_pass.accepted_input().into_uuid()
        || state != "terminal"
        || frontier != expected_frontier
    {
        return write_review_invalid(writer, version, request_id)
            .await
            .map(|()| None);
    }
    let turn_outcome = match disposition.as_deref() {
        Some("completed") => ReviewPassTurnOutcome::Completed,
        Some("failed") => ReviewPassTurnOutcome::Failed,
        Some("refused") => ReviewPassTurnOutcome::Refused,
        Some("cancelled") => ReviewPassTurnOutcome::Cancelled,
        Some("reconciliation_required") => ReviewPassTurnOutcome::ReconciliationRequired,
        _ => {
            return write_review_internal(writer, version, request_id)
                .await
                .map(|()| None);
        }
    };
    let evidence = ReviewPassTurnEvidence::new(
        turn,
        current_pass.session(),
        current_pass.accepted_input(),
        turn_outcome,
        frontier.map(ContextFrontierId::from_uuid),
    );
    let policy = current_run.policy();
    let pass_reference = current_pass.reference();
    let next_run = match &next {
        ReviewPassState::Succeeded { .. } => ReviewRunState::Succeeded {
            concluding_pass: pass_reference,
        },
        ReviewPassState::Failed { .. } => ReviewRunState::Failed {
            failed_pass: pass_reference,
        },
        ReviewPassState::Blocked { .. } => ReviewRunState::Blocked {
            blocking_pass: pass_reference,
        },
        ReviewPassState::Cancelled { .. } => ReviewRunState::Cancelled {
            last_pass: Some(pass_reference),
        },
        ReviewPassState::Queued | ReviewPassState::Running { .. } => {
            return Err(ProcessConnectionError::EncodeInvariant);
        }
    };
    if current_pass.state() == &next {
        if current_run.state() != next_run {
            return write_review_invalid(writer, version, request_id)
                .await
                .map(|()| None);
        }
        return Ok(Some((current_pass, current_run)));
    }
    let pass = match current_pass.transition(next, Some(evidence)) {
        Ok(pass) => pass,
        Err(_) => {
            return write_review_invalid(writer, version, request_id)
                .await
                .map(|()| None);
        }
    };
    let pass_evidence = ReviewPassEvidence::from_pass(&pass, policy);
    let run = match current_run.transition(next_run, Some(pass_evidence.clone())) {
        Ok(run) => run,
        Err(_) => {
            return write_review_invalid(writer, version, request_id)
                .await
                .map(|()| None);
        }
    };
    Ok(Some((pass, run)))
}

fn review_finding_content(
    input: ReviewFindingInput,
) -> Option<(CanonicalUuid, ReviewFindingContent)> {
    let line_range = match (input.line_start, input.line_end) {
        (None, None) => None,
        (Some(start), Some(end)) => Some(
            ReviewLineRange::try_new(
                u32::try_from(start.value()).ok()?,
                u32::try_from(end.value()).ok()?,
            )
            .ok()?,
        ),
        (Some(_), None) | (None, Some(_)) => return None,
    };
    let diff_side = input.diff_side.map(|side| match side {
        WireReviewDiffSide::Left => ReviewFindingDiffSide::Left,
        WireReviewDiffSide::Right => ReviewFindingDiffSide::Right,
    });
    let location = ReviewFindingLocation::new(
        ReviewKey::try_new(input.file_path).ok()?,
        line_range,
        diff_side,
    );
    let severity = match input.severity {
        WireReviewSeverity::Info => ReviewFindingSeverity::Info,
        WireReviewSeverity::Low => ReviewFindingSeverity::Low,
        WireReviewSeverity::Medium => ReviewFindingSeverity::Medium,
        WireReviewSeverity::High => ReviewFindingSeverity::High,
        WireReviewSeverity::Critical => ReviewFindingSeverity::Critical,
    };
    let is_real_confidence = ReviewConfidence::try_from_basis_points(
        u16::try_from(input.is_real_confidence.value()).ok()?,
    )
    .ok()?;
    let severity_label_confidence = ReviewConfidence::try_from_basis_points(
        u16::try_from(input.severity_label_confidence.value()).ok()?,
    )
    .ok()?;
    Some((
        input.finding_id,
        ReviewFindingContent::new(
            location,
            ReviewText::try_new(input.title).ok()?,
            ReviewText::try_new(input.body).ok()?,
            severity,
            ReviewFindingConfidenceAxes::new(is_real_confidence, severity_label_confidence),
            ReviewKey::try_new(input.category).ok()?,
            input
                .recommended_fix
                .map(ReviewText::try_new)
                .transpose()
                .ok()?,
        ),
    ))
}

async fn write_review_invalid<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::without_detail(ErrorCode::InvalidRequest),
    )
    .await
}

async fn write_review_internal<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        internal_protocol_error(None, InternalDiagnostic::ReviewWorkflowProjectionCorruption),
    )
    .await
}

async fn write_review_unavailable<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    commit_ambiguous: bool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::mutation_unavailable(commit_ambiguous),
    )
    .await
}

async fn write_review_not_found<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::without_detail(ErrorCode::NotFound),
    )
    .await
}

async fn write_review_store_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    error: ReviewWorkflowStoreError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match error {
        ReviewWorkflowStoreError::Database(_) => {
            write_review_unavailable(writer, version, request_id, false).await
        }
        ReviewWorkflowStoreError::CommitAmbiguous(_) => {
            write_review_unavailable(writer, version, request_id, true).await
        }
        ReviewWorkflowStoreError::Corruption(_) => {
            write_review_internal(writer, version, request_id).await
        }
        ReviewWorkflowStoreError::InvalidInsertion(_)
        | ReviewWorkflowStoreError::InvalidTransition(_)
        | ReviewWorkflowStoreError::NonAtomicPassResult
        | ReviewWorkflowStoreError::IncompleteFindingInventory
        | ReviewWorkflowStoreError::IncompletePublicationReconciliation
        | ReviewWorkflowStoreError::ReservationConflict(_) => {
            write_review_invalid(writer, version, request_id).await
        }
    }
}

fn wire_size(value: usize) -> Result<CanonicalU64, ProcessConnectionError> {
    u64::try_from(value)
        .map(CanonicalU64::new)
        .map_err(|_| ProcessConnectionError::EncodeInvariant)
}

async fn write_import_rejection<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    detail: RejectionDetail,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::invalid_import(detail),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn handle_begin_conversation_import<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    format: ConversationImportFormat,
    declared_size_bytes: CanonicalU64,
    limit: usize,
    import_permit: Option<OwnedSemaphorePermit>,
    acquired_bulk_ingest_at: Option<Instant>,
    pending: &mut Option<PendingConversationImport>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if pending.is_some() {
        return write_import_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::ConversationImportAlreadyInProgress {},
        )
        .await;
    }
    let limit_bytes = wire_size(limit)?;
    if declared_size_bytes.value() > limit_bytes.value() {
        return write_import_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::ConversationImportSourceTooLarge {
                limit_bytes,
                declared_size_bytes,
                actual_size_bytes: None,
            },
        )
        .await;
    }
    let import_permit = import_permit.ok_or(ProcessConnectionError::ImportBudgetClosed)?;
    let started_at = acquired_bulk_ingest_at.ok_or(ProcessConnectionError::ImportBudgetClosed)?;
    *pending = Some(PendingConversationImport {
        format,
        declared_size_bytes: declared_size_bytes.value(),
        actual_size_bytes: 0,
        source: Vec::new(),
        import_permit,
        started_at,
        idle_since: started_at,
    });
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ConversationImportBegun {
            declared_size_bytes,
        },
    )
    .await?;
    if let Some(active_import) = pending.as_mut() {
        active_import.idle_since = Instant::now();
    }
    Ok(())
}

async fn handle_append_conversation_import<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    chunk: Vec<u8>,
    limit: usize,
    pending: &mut Option<PendingConversationImport>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Some(active_import) = pending.as_mut() else {
        drop(chunk);
        return write_import_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::ConversationImportNotInProgress {},
        )
        .await;
    };
    let chunk_size =
        u64::try_from(chunk.len()).map_err(|_| ProcessConnectionError::EncodeInvariant)?;
    active_import.actual_size_bytes = active_import
        .actual_size_bytes
        .checked_add(chunk_size)
        .ok_or(ProcessConnectionError::EncodeInvariant)?;
    let limit_bytes = wire_size(limit)?;
    if active_import.actual_size_bytes > limit_bytes.value() {
        let detail = RejectionDetail::ConversationImportSourceTooLarge {
            limit_bytes,
            declared_size_bytes: CanonicalU64::new(active_import.declared_size_bytes),
            actual_size_bytes: Some(CanonicalU64::new(active_import.actual_size_bytes)),
        };
        drop(chunk);
        drop(pending.take());
        return write_import_rejection(writer, version, request_id, detail).await;
    }
    let required_capacity = usize::try_from(active_import.actual_size_bytes)
        .map_err(|_| ProcessConnectionError::EncodeInvariant)?;
    let declared_capacity = usize::try_from(active_import.declared_size_bytes)
        .map_err(|_| ProcessConnectionError::EncodeInvariant)?;
    let target_capacity = conversation_import_capacity_target(
        active_import.source.capacity(),
        required_capacity,
        declared_capacity,
        limit,
    );
    let additional_capacity = target_capacity
        .checked_sub(active_import.source.len())
        .ok_or(ProcessConnectionError::EncodeInvariant)?;
    if active_import
        .source
        .try_reserve_exact(additional_capacity)
        .is_err()
    {
        drop(chunk);
        drop(pending.take());
        return write_error(
            writer,
            version,
            request_id,
            unavailable_protocol_error(InternalDiagnostic::ConversationImportAllocationFailure),
        )
        .await;
    }
    active_import.source.extend_from_slice(&chunk);
    drop(chunk);
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ConversationImportAppended {
            assembled_size_bytes: CanonicalU64::new(active_import.actual_size_bytes),
        },
    )
    .await?;
    active_import.idle_since = Instant::now();
    Ok(())
}

fn conversation_import_capacity_target(
    current_capacity: usize,
    required_capacity: usize,
    declared_capacity: usize,
    limit: usize,
) -> usize {
    let growth_ceiling = if required_capacity <= declared_capacity {
        declared_capacity
    } else {
        limit
    };
    if required_capacity <= current_capacity {
        return current_capacity;
    }
    current_capacity
        .saturating_mul(2)
        .max(required_capacity)
        .min(growth_ceiling)
}

async fn handle_commit_conversation_import<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    limit: usize,
    pool: &PgPool,
    pending: &mut Option<PendingConversationImport>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Some(pending) = pending.take() else {
        return write_import_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::ConversationImportNotInProgress {},
        )
        .await;
    };
    let limit_bytes = wire_size(limit)?;
    let declared_size_bytes = CanonicalU64::new(pending.declared_size_bytes);
    let actual_size_bytes = CanonicalU64::new(pending.actual_size_bytes);
    if pending.actual_size_bytes > limit_bytes.value() {
        let detail = RejectionDetail::ConversationImportSourceTooLarge {
            limit_bytes,
            declared_size_bytes,
            actual_size_bytes: Some(actual_size_bytes),
        };
        drop(pending);
        return write_import_rejection(writer, version, request_id, detail).await;
    }
    if pending.actual_size_bytes != pending.declared_size_bytes {
        let detail = RejectionDetail::ConversationImportSourceSizeMismatch {
            declared_size_bytes,
            actual_size_bytes,
        };
        drop(pending);
        return write_import_rejection(writer, version, request_id, detail).await;
    }
    let observed_source_size =
        u64::try_from(pending.source.len()).map_err(|_| ProcessConnectionError::EncodeInvariant)?;
    if observed_source_size != pending.actual_size_bytes {
        drop(pending);
        return write_error(
            writer,
            version,
            request_id,
            internal_protocol_error(None, InternalDiagnostic::ConversationImportContractDefect),
        )
        .await;
    }
    handle_import_conversation(
        writer,
        version,
        request_id,
        pending.format,
        pending.source,
        pool,
        pending.import_permit,
    )
    .await
}

async fn handle_abort_conversation_import<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    pending: &mut Option<PendingConversationImport>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if pending.take().is_none() {
        return write_import_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::ConversationImportNotInProgress {},
        )
        .await;
    }
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ConversationImportAborted {},
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "the lifecycle boundary keeps request correlation, resource ownership, and state explicit"
)]
async fn handle_begin_blob_upload<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    expected_digest: CanonicalBlobDigest,
    expected_length_bytes: CanonicalU64,
    bulk_permit: Option<OwnedSemaphorePermit>,
    acquired_bulk_ingest_at: Option<Instant>,
    services: &ConnectionServices,
    pending: &mut Option<PendingBlobUpload>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Some(registry) = services.blob_store_registry.as_deref() else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::Unavailable),
        )
        .await;
    };
    if let Some(detail) = blob_upload_begin_preflight(
        pending.is_some(),
        expected_length_bytes,
        registry.max_blob_bytes(),
    ) {
        return write_blob_rejection(writer, version, request_id, detail).await;
    }
    let expected =
        ExpectedBlob::try_new(expected_digest.into_digest(), expected_length_bytes.value())
            .map_err(|_| ProcessConnectionError::EncodeInvariant)?;
    let bulk_permit = bulk_permit.ok_or(ProcessConnectionError::ImportBudgetClosed)?;
    let started_at = acquired_bulk_ingest_at.ok_or(ProcessConnectionError::ImportBudgetClosed)?;
    let repository = BlobCatalogRepository::new(services.pool.clone());
    match begin_blob_upload(registry, &repository, expected, bulk_permit, started_at).await {
        Ok(BeginBlobUploadOutcome::Begun(upload)) => {
            *pending = Some(*upload);
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::BlobUploadBegun {
                    expected_digest,
                    expected_length_bytes,
                },
            )
            .await?;
            if let Some(upload) = pending.as_mut() {
                upload.mark_activity_complete();
            }
            Ok(())
        }
        Ok(BeginBlobUploadOutcome::AlreadyPresent(expected)) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::BlobUploadAlreadyPresent {
                    digest: CanonicalBlobDigest::from_digest(expected.digest()),
                    byte_length: CanonicalU64::new(expected.byte_length()),
                },
            )
            .await
        }
        Err(error) => write_blob_upload_error(writer, version, request_id, expected, error).await,
    }
}

fn blob_upload_begin_preflight(
    upload_is_active: bool,
    expected_length_bytes: CanonicalU64,
    max_blob_bytes: u64,
) -> Option<RejectionDetail> {
    if upload_is_active {
        Some(RejectionDetail::BlobUploadAlreadyInProgress {})
    } else if !(1..=max_blob_bytes).contains(&expected_length_bytes.value()) {
        Some(RejectionDetail::BlobUploadLengthOutOfRange {
            min_length_bytes: CanonicalU64::new(1),
            max_length_bytes: CanonicalU64::new(max_blob_bytes),
            declared_length_bytes: expected_length_bytes,
        })
    } else {
        None
    }
}

async fn handle_append_blob_upload<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    chunk: Vec<u8>,
    pending: &mut Option<PendingBlobUpload>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Some(mut upload) = pending.take() else {
        drop(chunk);
        return write_blob_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::BlobUploadNotInProgress {},
        )
        .await;
    };
    let expected = upload.expected();
    match upload.append(&chunk).await {
        Ok(assembled_length_bytes) => {
            *pending = Some(upload);
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::BlobUploadAppended {
                    assembled_length_bytes: CanonicalU64::new(assembled_length_bytes),
                },
            )
            .await?;
            if let Some(upload) = pending.as_mut() {
                upload.mark_activity_complete();
            }
            Ok(())
        }
        Err(error) => {
            drop(upload);
            write_blob_upload_error(writer, version, request_id, expected, error).await
        }
    }
}

async fn handle_commit_blob_upload<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    services: &ConnectionServices,
    pending: &mut Option<PendingBlobUpload>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Some(upload) = pending.take() else {
        return write_blob_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::BlobUploadNotInProgress {},
        )
        .await;
    };
    let expected = upload.expected();
    let repository = BlobCatalogRepository::new(services.pool.clone());
    match upload.commit(&repository).await {
        Ok(committed) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::BlobUploadCommitted {
                    digest: CanonicalBlobDigest::from_digest(committed.digest()),
                    byte_length: CanonicalU64::new(committed.byte_length()),
                },
            )
            .await
        }
        Err(error) => write_blob_upload_error(writer, version, request_id, expected, error).await,
    }
}

async fn handle_abort_blob_upload<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    pending: &mut Option<PendingBlobUpload>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if pending.take().is_none() {
        return write_blob_rejection(
            writer,
            version,
            request_id,
            RejectionDetail::BlobUploadNotInProgress {},
        )
        .await;
    }
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::BlobUploadAborted {},
    )
    .await
}

async fn write_blob_rejection<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    detail: RejectionDetail,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::invalid_blob_upload(detail),
    )
    .await
}

async fn write_bulk_ingest_rejection<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    active_kind: BulkIngestKind,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::invalid_bulk_ingest(RejectionDetail::BulkIngestAlreadyInProgress {
            active_kind,
        }),
    )
    .await
}

async fn write_blob_upload_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    expected: ExpectedBlob,
    error: BlobUploadError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let protocol_error = match error {
        BlobUploadError::SizeExceeded { observed } => {
            ProtocolError::invalid_blob_upload(RejectionDetail::BlobUploadSizeExceeded {
                expected_length_bytes: CanonicalU64::new(expected.byte_length()),
                actual_length_bytes: CanonicalU64::new(observed),
            })
        }
        BlobUploadError::LengthMismatch { observed } => {
            ProtocolError::invalid_blob_upload(RejectionDetail::BlobUploadLengthMismatch {
                expected_length_bytes: CanonicalU64::new(expected.byte_length()),
                actual_length_bytes: CanonicalU64::new(observed),
            })
        }
        BlobUploadError::DigestMismatch { observed } => {
            ProtocolError::invalid_blob_upload(RejectionDetail::BlobUploadDigestMismatch {
                expected_digest: CanonicalBlobDigest::from_digest(expected.digest()),
                actual_digest: CanonicalBlobDigest::from_digest(observed),
            })
        }
        BlobUploadError::Unavailable => ProtocolError::without_detail(ErrorCode::Unavailable),
        BlobUploadError::CommitAmbiguous => {
            ProtocolError::without_detail(ErrorCode::CommitAmbiguous)
        }
        BlobUploadError::Integrity => ProtocolError::without_detail(ErrorCode::Internal),
    };
    write_error(writer, version, request_id, protocol_error).await
}

async fn handle_import_conversation<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    format: ConversationImportFormat,
    source: Vec<u8>,
    pool: &PgPool,
    import_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let outcome = match format {
        ConversationImportFormat::ClaudeCodeSessionJsonlV2 => {
            execute_import(ClaudeCodeJsonlConverter, source, pool.clone()).await
        }
        ConversationImportFormat::CodexRolloutJsonlV1 => {
            execute_import(CodexRolloutJsonlConverter, source, pool.clone()).await
        }
    };
    drop(import_permit);
    match outcome {
        Ok(ImportConversationOutcome::Inserted { conversation }) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::ConversationImportInserted {
                    imported_conversation_id: wire_uuid(conversation.into_uuid()),
                },
            )
            .await
        }
        Ok(ImportConversationOutcome::AlreadyImported { conversation }) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::ConversationImportAlreadyImported {
                    imported_conversation_id: wire_uuid(conversation.into_uuid()),
                },
            )
            .await
        }
        Err(OperationalImportError::InvalidSource(evidence)) => {
            write_import_rejection(
                writer,
                version,
                request_id,
                RejectionDetail::ConversationImportConversionFailed {
                    class: evidence.class,
                    record_ordinal: evidence.record_ordinal.map(CanonicalU64::new),
                },
            )
            .await
        }
        Err(OperationalImportError::Database) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::CommitAmbiguous),
            )
            .await
        }
        Err(OperationalImportError::Internal(diagnostic)) => {
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, diagnostic),
            )
            .await
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ImportRejectionEvidence {
    class: ConversationImportRejectionClass,
    record_ordinal: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationalImportError {
    InvalidSource(ImportRejectionEvidence),
    Database,
    Internal(InternalDiagnostic),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConversionFailureDisposition {
    Rejected(ImportRejectionEvidence),
    Internal,
}

trait ClassifyConversationImportError {
    fn disposition(self) -> ConversionFailureDisposition;
}

impl ClassifyConversationImportError for ClaudeCodeJsonlConversionError {
    fn disposition(self) -> ConversionFailureDisposition {
        claude_conversion_failure_disposition(self.failure())
    }
}

fn claude_conversion_failure_disposition(
    failure: ClaudeCodeJsonlConversionFailure,
) -> ConversionFailureDisposition {
    use ClaudeCodeJsonlConversionFailure as Failure;
    let evidence = match failure {
        Failure::EmptySource => {
            import_evidence(ConversationImportRejectionClass::EmptySource, None)
        }
        Failure::BlankLine { line } => {
            import_evidence(ConversationImportRejectionClass::BlankLine, Some(line))
        }
        Failure::InvalidUtf8 { line } => {
            import_evidence(ConversationImportRejectionClass::InvalidUtf8, Some(line))
        }
        Failure::InvalidJson { line } => {
            import_evidence(ConversationImportRejectionClass::InvalidJson, Some(line))
        }
        Failure::JsonDepthExceeded { line } => import_evidence(
            ConversationImportRejectionClass::JsonDepthExceeded,
            Some(line),
        ),
        Failure::TopLevelNotObject { line } => import_evidence(
            ConversationImportRejectionClass::TopLevelNotObject,
            Some(line),
        ),
        Failure::InvalidRecordType { line } => import_evidence(
            ConversationImportRejectionClass::InvalidRecordType,
            Some(line),
        ),
        Failure::InvalidSourceMetadata { line } => import_evidence(
            ConversationImportRejectionClass::InvalidSourceMetadata,
            Some(line),
        ),
        Failure::InvalidMessageEnvelope { line } => import_evidence(
            ConversationImportRejectionClass::InvalidMessageEnvelope,
            Some(line),
        ),
        Failure::InvalidMessageRole { line } => import_evidence(
            ConversationImportRejectionClass::InvalidMessageRole,
            Some(line),
        ),
        Failure::MessageRoleMismatch { line } => import_evidence(
            ConversationImportRejectionClass::MessageRoleMismatch,
            Some(line),
        ),
        Failure::InvalidMessageContent { line } => import_evidence(
            ConversationImportRejectionClass::InvalidMessageContent,
            Some(line),
        ),
        Failure::InvalidContentBlock { line, .. } => import_evidence(
            ConversationImportRejectionClass::InvalidContentBlock,
            Some(line),
        ),
        Failure::InvalidToolResultBlock { line, .. } => import_evidence(
            ConversationImportRejectionClass::InvalidToolResultBlock,
            Some(line),
        ),
        Failure::PositionExhausted | Failure::InvalidAggregate(_) => {
            return ConversionFailureDisposition::Internal;
        }
    };
    ConversionFailureDisposition::Rejected(evidence)
}

impl ClassifyConversationImportError for CodexRolloutJsonlConversionError {
    fn disposition(self) -> ConversionFailureDisposition {
        codex_conversion_failure_disposition(self.failure())
    }
}

fn codex_conversion_failure_disposition(
    failure: CodexRolloutJsonlConversionFailure,
) -> ConversionFailureDisposition {
    use CodexRolloutJsonlConversionFailure as Failure;
    let evidence = match failure {
        Failure::EmptySource => {
            import_evidence(ConversationImportRejectionClass::EmptySource, None)
        }
        Failure::BlankLine { line } => {
            import_evidence(ConversationImportRejectionClass::BlankLine, Some(line))
        }
        Failure::InvalidUtf8 { line } => {
            import_evidence(ConversationImportRejectionClass::InvalidUtf8, Some(line))
        }
        Failure::InvalidJson { line } => {
            import_evidence(ConversationImportRejectionClass::InvalidJson, Some(line))
        }
        Failure::JsonDepthExceeded { line } => import_evidence(
            ConversationImportRejectionClass::JsonDepthExceeded,
            Some(line),
        ),
        Failure::TopLevelNotObject { line } => import_evidence(
            ConversationImportRejectionClass::TopLevelNotObject,
            Some(line),
        ),
        Failure::InvalidRecordType { line } | Failure::InvalidResponseItemType { line } => {
            import_evidence(
                ConversationImportRejectionClass::InvalidRecordType,
                Some(line),
            )
        }
        Failure::InvalidSourceMetadata { line } => import_evidence(
            ConversationImportRejectionClass::InvalidSourceMetadata,
            Some(line),
        ),
        Failure::InvalidResponseItemEnvelope { line } => import_evidence(
            ConversationImportRejectionClass::InvalidMessageEnvelope,
            Some(line),
        ),
        Failure::InvalidMessageRole { line } => import_evidence(
            ConversationImportRejectionClass::InvalidMessageRole,
            Some(line),
        ),
        Failure::InvalidMessageContent { line } => import_evidence(
            ConversationImportRejectionClass::InvalidMessageContent,
            Some(line),
        ),
        Failure::InvalidMessageBlock { line, .. } => import_evidence(
            ConversationImportRejectionClass::InvalidContentBlock,
            Some(line),
        ),
        Failure::InvalidReasoning { line } | Failure::InvalidReasoningBlock { line, .. } => {
            import_evidence(
                ConversationImportRejectionClass::InvalidReasoning,
                Some(line),
            )
        }
        Failure::InvalidToolCall { line } => import_evidence(
            ConversationImportRejectionClass::InvalidToolCall,
            Some(line),
        ),
        Failure::InvalidToolResult { line } => import_evidence(
            ConversationImportRejectionClass::InvalidToolResult,
            Some(line),
        ),
        Failure::InvalidToolResultBlock { line, .. } => import_evidence(
            ConversationImportRejectionClass::InvalidToolResultBlock,
            Some(line),
        ),
        Failure::PositionExhausted | Failure::InvalidAggregate(_) => {
            return ConversionFailureDisposition::Internal;
        }
    };
    ConversionFailureDisposition::Rejected(evidence)
}

const fn import_evidence(
    class: ConversationImportRejectionClass,
    record_ordinal: Option<u64>,
) -> ImportRejectionEvidence {
    ImportRejectionEvidence {
        class,
        record_ordinal,
    }
}

/// Converts typed import evidence into closed operational diagnostics.
///
/// Payload-bearing converter and repository errors are consumed here without
/// formatting. Only a fixed classification crosses into the Internal log
/// record, so source content, durable values, and database prose remain absent.
fn operational_import_error<ConverterError>(
    error: ImportConversationError<ConverterError, ImportedConversationRepositoryError>,
) -> OperationalImportError
where
    ConverterError: ClassifyConversationImportError,
{
    match error {
        ImportConversationError::Conversion(error) => match error.disposition() {
            ConversionFailureDisposition::Rejected(evidence) => {
                OperationalImportError::InvalidSource(evidence)
            }
            ConversionFailureDisposition::Internal => OperationalImportError::Internal(
                InternalDiagnostic::ConversationImportContractDefect,
            ),
        },
        ImportConversationError::Store(ImportedConversationRepositoryError::Database(_)) => {
            OperationalImportError::Database
        }
        ImportConversationError::Store(error) => {
            OperationalImportError::Internal(imported_conversation_internal_diagnostic(&error))
        }
        ImportConversationError::ConverterIdentityMismatch { .. }
        | ImportConversationError::ConverterFormatMismatch { .. }
        | ImportConversationError::ConverterEntryIdentitySequenceMismatch
        | ImportConversationError::StoreSourceDigestMismatch { .. }
        | ImportConversationError::StoreInsertedIdentityMismatch { .. } => {
            OperationalImportError::Internal(InternalDiagnostic::ConversationImportContractDefect)
        }
    }
}

async fn execute_import<Converter>(
    converter: Converter,
    source: Vec<u8>,
    pool: PgPool,
) -> Result<ImportConversationOutcome, OperationalImportError>
where
    Converter: ImportedConversationConverter + Send + 'static,
    Converter::Error: ClassifyConversationImportError,
{
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        runtime.block_on(async move {
            let mut service = ImportConversationService::new(
                UuidV7ImportedConversationIdGenerator,
                converter,
                ImportedConversationRepository::new(pool),
            );
            service
                .execute(&source)
                .await
                .map_err(operational_import_error)
        })
    })
    .await
    .map_err(|_| {
        OperationalImportError::Internal(InternalDiagnostic::ConversationImportWorkerTerminated)
    })?
}

fn domain_imported_relationship(
    relationship: WireImportedSessionRelationship,
) -> DomainImportedSessionRelationship {
    match relationship {
        WireImportedSessionRelationship::Resume => DomainImportedSessionRelationship::Resume,
        WireImportedSessionRelationship::Fork => DomainImportedSessionRelationship::Fork,
    }
}

#[derive(Clone, Copy, Debug)]
struct WireImportedContinuationRequest {
    command_uuid: uuid::Uuid,
    conversation: CanonicalUuid,
    through_position: CanonicalU64,
    relationship: WireImportedSessionRelationship,
    initial_model_selection: WireModelSelection,
    model_settings: WireModelSettingsOverlay,
}

async fn handle_create_session_from_imported_frontier<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    wire_request: WireImportedContinuationRequest,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let command_id = DurableCommandId::from_uuid(wire_request.command_uuid);
    let conversation_id = ImportedConversationId::from_uuid(wire_request.conversation.into_uuid());
    let relationship = domain_imported_relationship(wire_request.relationship);
    let model_selection = domain_model_selection(wire_request.initial_model_selection);
    let caller_model_settings = domain_model_settings_overlay(wire_request.model_settings);
    let through_position = wire_request.through_position;
    let repository =
        ImportedSessionRepository::new(pool.clone(), model_configuration.session_credential_pin());

    match repository.load(command_id).await {
        Ok(Some(recorded)) => {
            let command = recorded.command();
            if command.imported_conversation() == conversation_id
                && command.imported_frontier().through_position().as_u64()
                    == through_position.value()
                && command.relationship() == relationship
                && command.initial_configuration_defaults().model() == model_selection
                && command
                    .initial_configuration_defaults()
                    .dangerous_tool_auto_approval()
                    == DangerousToolAutoApproval::Disabled
                && command
                    .initial_configuration_defaults()
                    .system_prompt()
                    .is_none()
                && command
                    .initial_configuration_defaults()
                    .model_settings()
                    .precedence()
                    .session()
                    == caller_model_settings
            {
                return write_message(
                    writer,
                    version,
                    request_id,
                    ServerMessage::SessionCreated {
                        session_id: wire_uuid(recorded.applied_result().session().into_uuid()),
                        model_settings: wire_model_settings(
                            recorded
                                .command()
                                .initial_configuration_defaults()
                                .model_settings(),
                        ),
                    },
                )
                .await;
            }
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Ok(None) => {}
        Err(ImportedSessionRepositoryError::DifferentCommandKind { .. }) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Err(ImportedSessionRepositoryError::Database(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(ImportedSessionRepositoryError::CommitAmbiguous(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await;
        }
        Err(
            error @ (ImportedSessionRepositoryError::Preparation(_)
            | ImportedSessionRepositoryError::IdentityCollision(_)
            | ImportedSessionRepositoryError::Corruption(_)),
        ) => {
            let diagnostic = imported_session_internal_diagnostic(&error);
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, diagnostic),
            )
            .await;
        }
    }

    // An unclaimed command resolves its wire address against the immutable
    // imported aggregate before any application construction, so an absent
    // conversation or an out-of-range position wins over a settings admission
    // failure and each still leaves the command identity unclaimed.
    let Some(position) = ImportedTranscriptPosition::try_from_u64(through_position.value()) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let conversation = match ImportedConversationRepository::new(pool.clone())
        .load(conversation_id)
        .await
    {
        Ok(Some(conversation)) => conversation,
        Ok(None) => {
            return write_error(
                writer,
                version,
                request_id,
                imported_conversation_not_found(wire_request.conversation),
            )
            .await;
        }
        Err(ImportedConversationRepositoryError::Database(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(
            error @ (ImportedConversationRepositoryError::IdentityCollision(_)
            | ImportedConversationRepositoryError::Corruption(_)),
        ) => {
            let diagnostic = imported_conversation_internal_diagnostic(&error);
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, diagnostic),
            )
            .await;
        }
    };
    let Some(frontier) = conversation
        .frontiers()
        .find(|frontier| frontier.through_position() == position)
    else {
        return write_error(
            writer,
            version,
            request_id,
            imported_position_out_of_range(
                wire_request.conversation,
                through_position,
                last_imported_position(&conversation),
            ),
        )
        .await;
    };
    let last_position = last_imported_position(&conversation);

    let model_settings = match validate_session_model_settings(
        model_configuration,
        model_selection,
        caller_model_settings,
    ) {
        Ok(settings) => settings,
        Err(error) => {
            return write_error(
                writer,
                version,
                request_id,
                model_settings_protocol_error(error),
            )
            .await;
        }
    };
    let Some(defaults) = SessionConfigurationDefaults::complete_with_model_settings(
        model_selection,
        DangerousToolAutoApproval::Disabled,
        None,
        model_settings,
    ) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };

    if model_configuration
        .resolve_session_model(model_selection)
        .is_err()
    {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    }

    let request = CreateSessionFromImportedFrontierRequest::try_new(
        command_id,
        frontier,
        relationship,
        defaults,
    );
    let Ok(request) = request else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let mut service = CreateSessionFromImportedFrontierService::new(
        UuidV7CreateSessionFromImportedFrontierIdGenerator,
        repository,
    );
    match service.execute(request).await {
        Ok(CreateSessionFromImportedFrontierOutcome::Applied(result)) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionCreated {
                    session_id: wire_uuid(result.session().into_uuid()),
                    model_settings: wire_model_settings(model_settings),
                },
            )
            .await
        }
        Ok(CreateSessionFromImportedFrontierOutcome::ImportedConversationNotFound { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                imported_conversation_not_found(wire_request.conversation),
            )
            .await
        }
        Ok(CreateSessionFromImportedFrontierOutcome::ImportedFrontierNotFound { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                imported_position_out_of_range(
                    wire_request.conversation,
                    through_position,
                    last_position,
                ),
            )
            .await
        }
        Ok(CreateSessionFromImportedFrontierOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(ImportedSessionRepositoryError::Database(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await
        }
        Err(ImportedSessionRepositoryError::CommitAmbiguous(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await
        }
        Err(
            error @ (ImportedSessionRepositoryError::DifferentCommandKind { .. }
            | ImportedSessionRepositoryError::Preparation(_)
            | ImportedSessionRepositoryError::IdentityCollision(_)
            | ImportedSessionRepositoryError::Corruption(_)),
        ) => {
            let diagnostic = imported_session_internal_diagnostic(&error);
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, diagnostic),
            )
            .await
        }
    }
}

async fn handle_compact_session<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: signalbox_process_protocol::CommandId,
    session_id: CanonicalUuid,
    through_position: Option<CanonicalU64>,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let command = DurableCommandId::from_uuid(command_id.into_uuid());
    let session = SessionId::from_uuid(session_id.into_uuid());
    let requested_through_position = through_position.map(CanonicalU64::value);
    let repository = ContextCompactionRepository::new(services.pool.clone());
    match repository
        .lookup_command(command, session, requested_through_position)
        .await
    {
        Ok(ContextCompactionCommandLookup::Unseen) => {}
        Ok(ContextCompactionCommandLookup::Replayed(applied)) => {
            return write_context_compaction_receipt(
                writer, version, request_id, session_id, applied,
            )
            .await;
        }
        Ok(ContextCompactionCommandLookup::ConflictingReuse) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Ok(ContextCompactionCommandLookup::Pending | ContextCompactionCommandLookup::Failed) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Unavailable),
            )
            .await;
        }
        Err(error) => {
            return write_context_compaction_repository_error(
                writer,
                version,
                request_id,
                session,
                services.recovery_reporter.as_ref(),
                error,
            )
            .await;
        }
    }
    let defaults = match ProcessReadRepository::new(services.pool.clone())
        .read_session_defaults(session, None)
        .await
    {
        Ok(ProcessSessionDefaultsRead::Read(defaults)) => defaults,
        Ok(ProcessSessionDefaultsRead::SessionNotFound) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::NotFound),
            )
            .await;
        }
        Ok(ProcessSessionDefaultsRead::VersionNotFound) => {
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    Some(session.into_uuid()),
                    InternalDiagnostic::SessionDefaultsVersionMissing,
                ),
            )
            .await;
        }
        Err(error) => {
            return write_context_compaction_read_error(
                writer, version, request_id, session, error,
            )
            .await;
        }
    };
    let selection = match defaults.defaults().model() {
        ModelSelectionRequest::Direct(selection) => selection,
        ModelSelectionRequest::Alias(alias) => {
            let Some(definition) = services.model_configuration.resolve_alias(alias) else {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::Unavailable),
                )
                .await;
            };
            definition.selected()
        }
    };
    let Some(route) = services.model_configuration.resolve_direct_model(selection) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::Unavailable),
        )
        .await;
    };
    let credential_reference =
        match signalbox_persistence::session_credentials::current_session_credential_with_migration_fallback(
            &services.pool,
            session,
            route.model_family(),
            route.migration_credential_family(),
        )
        .await
        {
            Ok(reference) => reference.as_str().to_owned(),
            Err(sqlx::Error::RowNotFound) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    internal_protocol_error(
                        Some(session.into_uuid()),
                        InternalDiagnostic::SessionModelCredentialMissing,
                    ),
                )
                .await;
            }
            Err(_) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::Unavailable),
                )
                .await;
            }
        };
    let target = match services
        .model_configuration
        .target_catalog()
        .resolve(FrozenModelSelection::Direct(selection))
    {
        Ok(resolved) => resolved.target(),
        Err(_) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Unavailable),
            )
            .await;
        }
    };
    let prepared = loop {
        let request = PrepareContextCompactionRequest {
            command,
            session,
            requested_through_position,
            automatic_for_turn: None,
            defaults_version: defaults.version(),
            selection,
            target,
            credential_reference: credential_reference.clone(),
            call: ModelCallId::from_uuid(uuid::Uuid::now_v7()),
            compaction: ContextCompactionId::from_uuid(uuid::Uuid::now_v7()),
            summary_entry: SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7()),
            result_frontier: ContextFrontierId::from_uuid(uuid::Uuid::now_v7()),
        };
        match repository.prepare(request).await {
            Ok(PrepareContextCompactionOutcome::Prepared(prepared)) => break prepared,
            Ok(PrepareContextCompactionOutcome::Replayed(applied)) => {
                return write_context_compaction_receipt(
                    writer, version, request_id, session_id, applied,
                )
                .await;
            }
            Ok(PrepareContextCompactionOutcome::ConflictingReuse) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::ConflictingReuse),
                )
                .await;
            }
            Ok(PrepareContextCompactionOutcome::SessionNotFound) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::NotFound),
                )
                .await;
            }
            Ok(PrepareContextCompactionOutcome::InvalidBoundary) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::InvalidRequest),
                )
                .await;
            }
            Ok(
                PrepareContextCompactionOutcome::DefaultsChanged
                | PrepareContextCompactionOutcome::Busy
                | PrepareContextCompactionOutcome::NoBoundary
                | PrepareContextCompactionOutcome::AutomaticAlreadyAttempted
                | PrepareContextCompactionOutcome::FailedReplay,
            ) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::Unavailable),
                )
                .await;
            }
            Err(ContextCompactionRepositoryError::IdentityCollision) => continue,
            Err(error) => {
                return write_context_compaction_repository_error(
                    writer,
                    version,
                    request_id,
                    session,
                    services.recovery_reporter.as_ref(),
                    error,
                )
                .await;
            }
        }
    };
    let rendered_range = match load_context_compaction_range(&services.pool, &prepared).await {
        Ok(rendered) => rendered,
        Err(error) => {
            return fail_context_compaction_before_response(
                writer,
                version,
                request_id,
                services.recovery_reporter.as_ref(),
                &repository,
                &prepared,
                error,
            )
            .await;
        }
    };
    if let Err(error) = authorize_context_compaction_until_resolved(&repository, &prepared).await {
        return write_context_compaction_repository_error(
            writer,
            version,
            request_id,
            session,
            services.recovery_reporter.as_ref(),
            error,
        )
        .await;
    }
    let request = ContextCompactionModelRequest {
        call: prepared.call(),
        session,
        selection: prepared.selection(),
        target: prepared.target(),
        credential_reference: prepared.credential_reference().to_owned(),
        system_prompt: services.model_configuration.compaction_prompt().to_owned(),
        rendered_range,
    };
    let result = match services.context_compaction_model.execute(request).await {
        Ok(result) => result,
        Err(error) => {
            let disposition = context_compaction_failure_disposition(error);
            if let Err(repository_error) =
                fail_context_compaction_until_resolved(&repository, &prepared, disposition).await
            {
                return write_context_compaction_repository_error(
                    writer,
                    version,
                    request_id,
                    session,
                    services.recovery_reporter.as_ref(),
                    repository_error,
                )
                .await;
            }
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Unavailable),
            )
            .await;
        }
    };
    let usage = ContextCompactionTokenUsage::unreported()
        .with_input_tokens(result.usage.input_tokens)
        .with_output_tokens(result.usage.output_tokens)
        .with_cache_creation_input_tokens(result.usage.cache_creation_input_tokens)
        .with_cache_read_input_tokens(result.usage.cache_read_input_tokens);
    let exceeds_limits = context_compaction_usage_exceeds_configured_limits(
        &services.model_configuration,
        prepared.target(),
        result.usage,
    )
    .unwrap_or_else(|| {
        record_internal_diagnostic(
            Some(session.into_uuid()),
            InternalDiagnostic::ContextCompactionUnconfiguredTarget,
        );
        true
    });
    if exceeds_limits {
        if let Err(repository_error) = fail_context_compaction_with_usage_until_resolved(
            &repository,
            &prepared,
            FailedContextCompactionDisposition::KnownFailed,
            usage,
        )
        .await
        {
            return write_context_compaction_repository_error(
                writer,
                version,
                request_id,
                session,
                services.recovery_reporter.as_ref(),
                repository_error,
            )
            .await;
        }
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::Unavailable),
        )
        .await;
    }
    let applied = match complete_context_compaction_until_resolved(
        &repository,
        &prepared,
        &result.summary,
        usage,
    )
    .await
    {
        Ok(applied) => applied,
        Err(error) => {
            return write_context_compaction_repository_error(
                writer,
                version,
                request_id,
                session,
                services.recovery_reporter.as_ref(),
                error,
            )
            .await;
        }
    };
    write_context_compaction_receipt(writer, version, request_id, session_id, applied).await
}

#[derive(Debug)]
pub(crate) enum AutomaticContextCompactionError {
    Read(ProcessReadError),
    Credential(ModelCallRepositoryError),
    Repository(ContextCompactionRepositoryError),
    Model,
    Configuration,
    State,
    Integrity,
    AlreadyAttempted,
}

impl fmt::Display for AutomaticContextCompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("automatic context compaction failed")
    }
}

impl Error for AutomaticContextCompactionError {}

impl ClassifyOperatorFailure for AutomaticContextCompactionError {
    fn operator_failure_class(&self) -> signalbox_application::OperatorFailureClass {
        match self {
            Self::Credential(error) => error.operator_failure_class(),
            Self::Repository(error) => error.operator_failure_class(),
            Self::Read(ProcessReadError::Database(_)) | Self::Model => {
                signalbox_application::OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                }
            }
            Self::Read(ProcessReadError::Corruption(_)) | Self::Integrity => {
                signalbox_application::OperatorFailureClass::FailClosedCorruption
            }
            Self::Configuration | Self::State | Self::AlreadyAttempted => {
                signalbox_application::OperatorFailureClass::CallerOrHubBug
            }
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Credential(error) => error.operator_failure_cause_code(),
            Self::Read(ProcessReadError::Database(_)) => "context_compaction_read_database",
            Self::Read(ProcessReadError::Corruption(_)) => "context_compaction_read_corruption",
            Self::Repository(ContextCompactionRepositoryError::Database(_)) => {
                "context_compaction_repository_database"
            }
            Self::Repository(ContextCompactionRepositoryError::CommitAmbiguous(_)) => {
                "context_compaction_repository_commit_ambiguous"
            }
            Self::Repository(ContextCompactionRepositoryError::IdentityCollision) => {
                "context_compaction_repository_identity_collision"
            }
            Self::Repository(ContextCompactionRepositoryError::Corruption(_)) => {
                "context_compaction_repository_corruption"
            }
            Self::Model => "context_compaction_model",
            Self::Configuration => "context_compaction_configuration",
            Self::State => "context_compaction_state",
            Self::Integrity => "context_compaction_integrity",
            Self::AlreadyAttempted => "context_compaction_already_attempted",
        }
    }
}

pub(crate) async fn compact_automatically(
    model_calls: &PostgresModelCallRepository,
    model_configuration: &HubModelConfiguration,
    model: &Arc<dyn ContextCompactionModel>,
    session: SessionId,
    turn: TurnId,
) -> Result<AppliedContextCompaction, AutomaticContextCompactionError> {
    let defaults = match ProcessReadRepository::new(model_calls.pool().clone())
        .read_session_defaults(session, None)
        .await
    {
        Ok(ProcessSessionDefaultsRead::Read(defaults)) => defaults,
        Ok(ProcessSessionDefaultsRead::SessionNotFound)
        | Ok(ProcessSessionDefaultsRead::VersionNotFound) => {
            return Err(AutomaticContextCompactionError::State);
        }
        Err(error) => return Err(AutomaticContextCompactionError::Read(error)),
    };
    let selection = match defaults.defaults().model() {
        ModelSelectionRequest::Direct(selection) => selection,
        ModelSelectionRequest::Alias(alias) => model_configuration
            .resolve_alias(alias)
            .ok_or(AutomaticContextCompactionError::Configuration)?
            .selected(),
    };
    let target = model_configuration
        .target_catalog()
        .resolve(FrozenModelSelection::Direct(selection))
        .map_err(|_| AutomaticContextCompactionError::Configuration)?
        .target();
    let credential_reference = model_calls
        .resolve_session_credential_reference(session, target)
        .await
        .map_err(AutomaticContextCompactionError::Credential)?;
    let repository = ContextCompactionRepository::new(model_calls.pool().clone());
    let prepared = loop {
        let request = PrepareContextCompactionRequest {
            command: DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
            session,
            requested_through_position: None,
            automatic_for_turn: Some(turn),
            defaults_version: defaults.version(),
            selection,
            target,
            credential_reference: credential_reference.as_str().to_owned(),
            call: ModelCallId::from_uuid(uuid::Uuid::now_v7()),
            compaction: ContextCompactionId::from_uuid(uuid::Uuid::now_v7()),
            summary_entry: SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7()),
            result_frontier: ContextFrontierId::from_uuid(uuid::Uuid::now_v7()),
        };
        match repository.prepare(request).await {
            Ok(PrepareContextCompactionOutcome::Prepared(prepared)) => break prepared,
            Ok(
                PrepareContextCompactionOutcome::Replayed(_)
                | PrepareContextCompactionOutcome::ConflictingReuse
                | PrepareContextCompactionOutcome::SessionNotFound
                | PrepareContextCompactionOutcome::DefaultsChanged
                | PrepareContextCompactionOutcome::Busy
                | PrepareContextCompactionOutcome::NoBoundary
                | PrepareContextCompactionOutcome::InvalidBoundary
                | PrepareContextCompactionOutcome::FailedReplay,
            ) => {
                return Err(AutomaticContextCompactionError::State);
            }
            Ok(PrepareContextCompactionOutcome::AutomaticAlreadyAttempted) => {
                return Err(AutomaticContextCompactionError::AlreadyAttempted);
            }
            Err(ContextCompactionRepositoryError::IdentityCollision) => continue,
            Err(error) => return Err(AutomaticContextCompactionError::Repository(error)),
        }
    };
    let rendered_range = match retry_context_compaction_range_database_reads(|| {
        load_context_compaction_range(model_calls.pool(), &prepared)
    })
    .await
    {
        Ok(rendered) => rendered,
        Err(ContextCompactionRangeLoadError::Read(error)) => {
            fail_context_compaction_until_resolved(
                &repository,
                &prepared,
                FailedContextCompactionDisposition::KnownFailed,
            )
            .await
            .map_err(AutomaticContextCompactionError::Repository)?;
            return Err(AutomaticContextCompactionError::Read(error));
        }
        Err(ContextCompactionRangeLoadError::Integrity) => {
            fail_context_compaction_until_resolved(
                &repository,
                &prepared,
                FailedContextCompactionDisposition::KnownFailed,
            )
            .await
            .map_err(AutomaticContextCompactionError::Repository)?;
            return Err(AutomaticContextCompactionError::Integrity);
        }
    };
    authorize_context_compaction_until_resolved(&repository, &prepared)
        .await
        .map_err(AutomaticContextCompactionError::Repository)?;
    let request = ContextCompactionModelRequest {
        call: prepared.call(),
        session,
        selection: prepared.selection(),
        target: prepared.target(),
        credential_reference: prepared.credential_reference().to_owned(),
        system_prompt: model_configuration.compaction_prompt().to_owned(),
        rendered_range,
    };
    let result = match model.execute(request).await {
        Ok(result) => result,
        Err(error) => {
            fail_context_compaction_until_resolved(
                &repository,
                &prepared,
                context_compaction_failure_disposition(error),
            )
            .await
            .map_err(AutomaticContextCompactionError::Repository)?;
            return Err(AutomaticContextCompactionError::Model);
        }
    };
    let usage = ContextCompactionTokenUsage::unreported()
        .with_input_tokens(result.usage.input_tokens)
        .with_output_tokens(result.usage.output_tokens)
        .with_cache_creation_input_tokens(result.usage.cache_creation_input_tokens)
        .with_cache_read_input_tokens(result.usage.cache_read_input_tokens);
    complete_context_compaction_until_resolved(&repository, &prepared, &result.summary, usage)
        .await
        .map_err(AutomaticContextCompactionError::Repository)
}

async fn load_context_compaction_range(
    pool: &PgPool,
    prepared: &PreparedContextCompaction,
) -> Result<String, ContextCompactionRangeLoadError> {
    let entries = ProcessReadRepository::new(pool.clone())
        .read_selected_transcript_entries(
            prepared.summarized_positions(),
            prepared.summarized_entries(),
        )
        .await?;
    let Some(first) = entries.first() else {
        return Err(ContextCompactionRangeLoadError::Integrity);
    };
    let Some(through) = entries.last() else {
        return Err(ContextCompactionRangeLoadError::Integrity);
    };
    if transcript_entry_reference(first) != prepared.first()
        || transcript_entry_reference(through) != prepared.through()
    {
        return Err(ContextCompactionRangeLoadError::Integrity);
    }
    let values = entries
        .iter()
        .map(context_compaction_entry_value)
        .collect::<Vec<_>>();
    serde_json::to_string(&values).map_err(|_| ContextCompactionRangeLoadError::Integrity)
}

async fn retry_context_compaction_range_database_reads<Load, LoadFuture>(
    mut load: Load,
) -> Result<String, ContextCompactionRangeLoadError>
where
    Load: FnMut() -> LoadFuture,
    LoadFuture: Future<Output = Result<String, ContextCompactionRangeLoadError>>,
{
    loop {
        match load().await {
            Err(ContextCompactionRangeLoadError::Read(ProcessReadError::Database(_))) => {
                sleep(CONTEXT_COMPACTION_PERSISTENCE_RETRY_INTERVAL).await;
            }
            result => return result,
        }
    }
}

fn transcript_entry_reference(
    entry: &ProcessTranscriptEntry,
) -> signalbox_domain::SemanticTranscriptEntryRef {
    let (source_session, entry) = match entry {
        ProcessTranscriptEntry::DelegatedTask {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::DelegationMessage {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::DelegationResult {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::ModelIdentityChanged {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::ContextSummary {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::User {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::Assistant {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::AssistantToolUse {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::ToolExecutionResult {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::ToolDenied {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::ToolClosed {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::TurnFailed {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::TurnCompleted {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::TurnCancelled {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::ImportedText {
            source_session,
            entry,
            ..
        }
        | ProcessTranscriptEntry::Imported {
            source_session,
            entry,
            ..
        } => (*source_session, *entry),
    };
    signalbox_domain::SemanticTranscriptEntryRef::from_source(source_session, entry)
}

fn context_compaction_entry_value(entry: &ProcessTranscriptEntry) -> serde_json::Value {
    let reference = transcript_entry_reference(entry);
    let source_session_id = reference
        .source_session()
        .into_uuid()
        .hyphenated()
        .to_string();
    let entry_id = reference.entry().into_uuid().hyphenated().to_string();
    match entry {
        ProcessTranscriptEntry::DelegatedTask {
            entry_index,
            spawning_request,
            parent_session,
            parent_turn,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "delegated_task",
            "spawning_request_id": spawning_request.into_uuid().hyphenated().to_string(),
            "parent_session_id": parent_session.into_uuid().hyphenated().to_string(),
            "parent_turn_id": parent_turn.into_uuid().hyphenated().to_string(),
            "content": content,
        }),
        ProcessTranscriptEntry::DelegationMessage {
            entry_index,
            spawning_request,
            message,
            sender,
            recipient,
            ordinal,
            delivery_sequence,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "delegation_message",
            "spawning_request_id": spawning_request.into_uuid().hyphenated().to_string(),
            "message_id": message.into_uuid().hyphenated().to_string(),
            "sender_session_id": sender.into_uuid().hyphenated().to_string(),
            "recipient_session_id": recipient.into_uuid().hyphenated().to_string(),
            "ordinal": ordinal,
            "delivery_sequence": delivery_sequence,
            "content": content,
        }),
        ProcessTranscriptEntry::DelegationResult {
            entry_index,
            awaiting_request,
            spawning_request,
            child,
            mode,
            delivery_sequence,
            outcome,
            content,
            reason,
            provenance,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "delegation_result",
            "await_request_id": awaiting_request.into_uuid().hyphenated().to_string(),
            "spawning_request_id": spawning_request.into_uuid().hyphenated().to_string(),
            "child_session_id": child.into_uuid().hyphenated().to_string(),
            "mode": match mode {
                DispatchedDelegationWaitMode::Foreground => WireDelegationWaitMode::Foreground,
                DispatchedDelegationWaitMode::Background => WireDelegationWaitMode::Background,
            },
            "delivery_sequence": delivery_sequence,
            "outcome": wire_delegation_outcome(*outcome),
            "content": content,
            "reason": wire_delegation_reason(*reason),
            "provenance": wire_delegation_provenance(*provenance),
        }),
        ProcessTranscriptEntry::ModelIdentityChanged {
            entry_index,
            turn,
            defaults_version,
            selected,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "model_identity_changed",
            "turn_id": turn.into_uuid().hyphenated().to_string(),
            "defaults_version": defaults_version,
            "selected_model_id": selected.into_uuid().hyphenated().to_string(),
        }),
        ProcessTranscriptEntry::ContextSummary {
            entry_index,
            model_call,
            first,
            through,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "context_summary",
            "model_call_id": model_call.into_uuid().hyphenated().to_string(),
            "first_source_session_id": first.source_session().into_uuid().hyphenated().to_string(),
            "first_entry_id": first.entry().into_uuid().hyphenated().to_string(),
            "through_source_session_id": through.source_session().into_uuid().hyphenated().to_string(),
            "through_entry_id": through.entry().into_uuid().hyphenated().to_string(),
            "content": content,
        }),
        ProcessTranscriptEntry::User {
            entry_index,
            accepted_input,
            turn,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "user",
            "accepted_input_id": accepted_input.into_uuid().hyphenated().to_string(),
            "turn_id": turn.into_uuid().hyphenated().to_string(),
            "content": content,
        }),
        ProcessTranscriptEntry::Assistant {
            entry_index,
            turn,
            model_call,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "assistant",
            "turn_id": turn.into_uuid().hyphenated().to_string(),
            "model_call_id": model_call.into_uuid().hyphenated().to_string(),
            "content": content,
        }),
        ProcessTranscriptEntry::AssistantToolUse {
            entry_index,
            turn,
            model_call,
            request,
            name,
            arguments,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "assistant_tool_use",
            "turn_id": turn.into_uuid().hyphenated().to_string(),
            "model_call_id": model_call.into_uuid().hyphenated().to_string(),
            "tool_request_id": request.into_uuid().hyphenated().to_string(),
            "name": name,
            "arguments": arguments,
        }),
        ProcessTranscriptEntry::ToolExecutionResult {
            entry_index,
            request,
            attempt,
            disposition: _,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "tool_execution_result",
            "tool_request_id": request.into_uuid().hyphenated().to_string(),
            "tool_attempt_id": attempt.into_uuid().hyphenated().to_string(),
            "content": content,
        }),
        ProcessTranscriptEntry::ToolDenied {
            entry_index,
            request,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "tool_denied",
            "tool_request_id": request.into_uuid().hyphenated().to_string(),
            "content": content,
        }),
        ProcessTranscriptEntry::ToolClosed {
            entry_index,
            request,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "tool_closed_by_turn_end",
            "tool_request_id": request.into_uuid().hyphenated().to_string(),
            "content": content,
        }),
        ProcessTranscriptEntry::TurnFailed {
            entry_index, turn, ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "turn_failed",
            "turn_id": turn.into_uuid().hyphenated().to_string(),
        }),
        ProcessTranscriptEntry::TurnCompleted {
            entry_index, turn, ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "turn_completed",
            "turn_id": turn.into_uuid().hyphenated().to_string(),
        }),
        ProcessTranscriptEntry::TurnCancelled {
            entry_index, turn, ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "turn_cancelled",
            "turn_id": turn.into_uuid().hyphenated().to_string(),
        }),
        ProcessTranscriptEntry::ImportedText {
            entry_index,
            imported_conversation,
            imported_entry,
            source_speaker,
            content,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "imported_text",
            "imported_conversation_id": imported_conversation.into_uuid().hyphenated().to_string(),
            "imported_entry_id": imported_entry.into_uuid().hyphenated().to_string(),
            "source_speaker": imported_source_speaker_label(*source_speaker),
            "content": content,
        }),
        ProcessTranscriptEntry::Imported {
            entry_index,
            imported_conversation,
            imported_entry,
            source_speaker,
            content_kind,
            ..
        } => serde_json::json!({
            "position": entry_index + 1,
            "source_session_id": source_session_id,
            "entry_id": entry_id,
            "type": "imported",
            "imported_conversation_id": imported_conversation.into_uuid().hyphenated().to_string(),
            "imported_entry_id": imported_entry.into_uuid().hyphenated().to_string(),
            "source_speaker": imported_source_speaker_label(*source_speaker),
            "content_kind": imported_content_kind_label(*content_kind),
        }),
    }
}

const fn imported_source_speaker_label(speaker: ProcessImportedSourceSpeaker) -> &'static str {
    match speaker {
        ProcessImportedSourceSpeaker::NotAttested => "not_attested",
        ProcessImportedSourceSpeaker::AttestedAbsent => "attested_absent",
        ProcessImportedSourceSpeaker::User => "user",
        ProcessImportedSourceSpeaker::Assistant => "assistant",
    }
}

const fn imported_content_kind_label(kind: ProcessImportedContentKind) -> &'static str {
    match kind {
        ProcessImportedContentKind::SourceEvent => "source_event",
        ProcessImportedContentKind::SourceMessageBlock => "source_message_block",
        ProcessImportedContentKind::Text => "text",
        ProcessImportedContentKind::ToolCall => "tool_call",
        ProcessImportedContentKind::ToolResult => "tool_result",
        ProcessImportedContentKind::Thinking => "thinking",
        ProcessImportedContentKind::RedactedThinking => "redacted_thinking",
        ProcessImportedContentKind::Document => "document",
        ProcessImportedContentKind::MessageContentAbsent => "message_content_absent",
    }
}

async fn authorize_context_compaction_until_resolved(
    repository: &ContextCompactionRepository,
    prepared: &PreparedContextCompaction,
) -> Result<(), ContextCompactionRepositoryError> {
    loop {
        match repository.authorize(prepared).await {
            Ok(()) => return Ok(()),
            Err(
                ContextCompactionRepositoryError::Database(_)
                | ContextCompactionRepositoryError::CommitAmbiguous(_),
            ) => sleep(CONTEXT_COMPACTION_PERSISTENCE_RETRY_INTERVAL).await,
            Err(error) => return Err(error),
        }
    }
}

/// Applies the completion, retrying only the outcomes an identical retry can
/// still change.
///
/// A transient database failure may succeed next time, and an unproven commit
/// is resolved by `complete` rereading its own terminal facts under the session
/// lock and returning the applied result. Every other class is a decided fact —
/// including a uniqueness violation on a result identity, which repeating the
/// same statements can never clear — so it returns rather than blocking the
/// session forever.
async fn complete_context_compaction_until_resolved(
    repository: &ContextCompactionRepository,
    prepared: &PreparedContextCompaction,
    summary: &str,
    usage: ContextCompactionTokenUsage,
) -> Result<AppliedContextCompaction, ContextCompactionRepositoryError> {
    loop {
        match repository.complete(prepared, summary, usage).await {
            Ok(applied) => return Ok(applied),
            Err(
                ContextCompactionRepositoryError::Database(_)
                | ContextCompactionRepositoryError::CommitAmbiguous(_),
            ) => sleep(CONTEXT_COMPACTION_PERSISTENCE_RETRY_INTERVAL).await,
            Err(error) => return Err(error),
        }
    }
}

async fn fail_context_compaction_until_resolved(
    repository: &ContextCompactionRepository,
    prepared: &PreparedContextCompaction,
    disposition: FailedContextCompactionDisposition,
) -> Result<(), ContextCompactionRepositoryError> {
    loop {
        match repository.fail(prepared, disposition).await {
            Ok(()) => return Ok(()),
            Err(
                ContextCompactionRepositoryError::Database(_)
                | ContextCompactionRepositoryError::CommitAmbiguous(_),
            ) => sleep(CONTEXT_COMPACTION_PERSISTENCE_RETRY_INTERVAL).await,
            Err(error) => return Err(error),
        }
    }
}

async fn fail_context_compaction_with_usage_until_resolved(
    repository: &ContextCompactionRepository,
    prepared: &PreparedContextCompaction,
    disposition: FailedContextCompactionDisposition,
    usage: ContextCompactionTokenUsage,
) -> Result<(), ContextCompactionRepositoryError> {
    loop {
        match repository
            .fail_with_usage(prepared, disposition, usage)
            .await
        {
            Ok(()) => return Ok(()),
            Err(
                ContextCompactionRepositoryError::Database(_)
                | ContextCompactionRepositoryError::CommitAmbiguous(_),
            ) => sleep(CONTEXT_COMPACTION_PERSISTENCE_RETRY_INTERVAL).await,
            Err(error) => return Err(error),
        }
    }
}

const fn context_compaction_failure_disposition(
    error: ContextCompactionModelError,
) -> FailedContextCompactionDisposition {
    match error {
        ContextCompactionModelError::CancelledBeforeSend
        | ContextCompactionModelError::CancellationConfirmed => {
            FailedContextCompactionDisposition::Cancelled
        }
        ContextCompactionModelError::BoundaryLoss
        | ContextCompactionModelError::CorrelationMismatch => {
            FailedContextCompactionDisposition::Ambiguous
        }
        ContextCompactionModelError::Refused => FailedContextCompactionDisposition::Refused,
        ContextCompactionModelError::UnconfiguredTarget
        | ContextCompactionModelError::PreparationFailed
        | ContextCompactionModelError::PreparationDefect
        | ContextCompactionModelError::ProviderError
        | ContextCompactionModelError::ProvenUnsent
        | ContextCompactionModelError::ProviderTargetSubstituted
        | ContextCompactionModelError::IncompleteSummary
        | ContextCompactionModelError::NonTextSummary
        | ContextCompactionModelError::InvalidSummary => {
            FailedContextCompactionDisposition::KnownFailed
        }
    }
}

async fn fail_context_compaction_before_response<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    recovery_reporter: Option<&FatalRecoveryReporter>,
    repository: &ContextCompactionRepository,
    prepared: &PreparedContextCompaction,
    error: ContextCompactionRangeLoadError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match error {
        ContextCompactionRangeLoadError::Read(error) => {
            if let Err(repository_error) = fail_context_compaction_until_resolved(
                repository,
                prepared,
                FailedContextCompactionDisposition::KnownFailed,
            )
            .await
            {
                return write_context_compaction_repository_error(
                    writer,
                    version,
                    request_id,
                    prepared.session(),
                    recovery_reporter,
                    repository_error,
                )
                .await;
            }
            write_context_compaction_read_error(
                writer,
                version,
                request_id,
                prepared.session(),
                error,
            )
            .await
        }
        ContextCompactionRangeLoadError::Integrity => {
            if let Err(repository_error) = fail_context_compaction_until_resolved(
                repository,
                prepared,
                FailedContextCompactionDisposition::KnownFailed,
            )
            .await
            {
                return write_context_compaction_repository_error(
                    writer,
                    version,
                    request_id,
                    prepared.session(),
                    recovery_reporter,
                    repository_error,
                )
                .await;
            }
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    Some(prepared.session().into_uuid()),
                    InternalDiagnostic::ContextCompactionRangeCorruption,
                ),
            )
            .await
        }
    }
}

async fn write_context_compaction_receipt<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    applied: AppliedContextCompaction,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_mutation_receipt_via_spool(
        writer,
        version,
        request_id,
        ServerMessage::SessionCompacted {
            session_id,
            context_compaction_id: wire_uuid(applied.compaction.into_uuid()),
            model_call_id: wire_uuid(applied.call.into_uuid()),
            through_position: CanonicalU64::new(applied.through_position),
            summary_entry_id: wire_uuid(applied.summary_entry.into_uuid()),
            result_frontier_id: wire_uuid(applied.result_frontier.into_uuid()),
        },
    )
    .await
}

/// Answers one explicit compaction repository failure, reporting first when it
/// left a durable outcome this process cannot decide.
///
/// The automatic sibling reaches the same signal through the scheduler pass's
/// execution role. A connection handler has none, and it cannot terminalize the
/// record either: `prepare` returned no `PreparedContextCompaction`, so `fail`
/// has nothing to name, replay of the same command finds it `Pending`, and a
/// fresh command finds the nonterminal call. Startup recovery does reconcile
/// exactly this state — `active_sessions` includes sessions holding a
/// nonterminal compaction call — but only the next incarnation runs it, so
/// without this report the session's compaction boundary stays owned by a call
/// nothing terminalizes for the life of the process, with nothing telling an
/// operator to restart.
async fn write_context_compaction_repository_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session: SessionId,
    recovery_reporter: Option<&FatalRecoveryReporter>,
    error: ContextCompactionRepositoryError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    if crate::commit_outcome_is_unknown(&error)
        && let Some(reporter) = recovery_reporter
    {
        reporter.report_recovery_required();
    }
    let response = match error {
        ContextCompactionRepositoryError::Database(_) => ProtocolError::mutation_unavailable(false),
        ContextCompactionRepositoryError::CommitAmbiguous(_) => {
            ProtocolError::mutation_unavailable(true)
        }
        ContextCompactionRepositoryError::IdentityCollision => internal_protocol_error(
            Some(session.into_uuid()),
            InternalDiagnostic::ContextCompactionIdentityCollision,
        ),
        ContextCompactionRepositoryError::Corruption(_) => internal_protocol_error(
            Some(session.into_uuid()),
            InternalDiagnostic::ContextCompactionRepositoryCorruption,
        ),
    };
    write_error(writer, version, request_id, response).await
}

async fn write_context_compaction_read_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session: SessionId,
    error: ProcessReadError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let response = match error {
        ProcessReadError::Database(_) => ProtocolError::mutation_unavailable(false),
        ProcessReadError::Corruption(_) => internal_protocol_error(
            Some(session.into_uuid()),
            InternalDiagnostic::ContextCompactionReadCorruption,
        ),
    };
    write_error(writer, version, request_id, response).await
}

#[derive(Debug)]
enum ContextCompactionRangeLoadError {
    Read(ProcessReadError),
    Integrity,
}

impl From<ProcessReadError> for ContextCompactionRangeLoadError {
    fn from(error: ProcessReadError) -> Self {
        Self::Read(error)
    }
}

/// Returns the greatest selectable imported position on a loaded aggregate.
///
/// An imported conversation's normalized entry sequence is nonempty and its
/// positions are contiguous from one, so the entry count is that bound.
fn last_imported_position(conversation: &ImportedConversation) -> u64 {
    conversation
        .entries()
        .last()
        .map_or(0, |entry| entry.position().as_u64())
}

/// Names the absent target as an imported conversation rather than a session.
fn imported_conversation_not_found(imported_conversation_id: CanonicalUuid) -> ProtocolError {
    ProtocolError::rejected(RejectionDetail::ImportedConversationNotFound {
        imported_conversation_id,
    })
}

/// Distinguishes a valid identity carrying an out-of-range position from an
/// absent identity, naming the conversation's selectable range.
fn imported_position_out_of_range(
    imported_conversation_id: CanonicalUuid,
    requested_position: CanonicalU64,
    last_position: u64,
) -> ProtocolError {
    // Imported positions are the contiguous sequence `1..=last_position`, so a
    // position this handler could not resolve is always beyond a positive
    // bound. A loaded aggregate that contradicts that is corrupt, and the
    // closed wire shape has no way to state the contradiction.
    if last_position == 0 || requested_position.value() <= last_position {
        return internal_protocol_error(None, InternalDiagnostic::ImportedFrontierRangeCorruption);
    }
    ProtocolError::rejected(RejectionDetail::ImportedFrontierPositionOutOfRange {
        imported_conversation_id,
        requested_position,
        last_position: CanonicalU64::new(last_position),
    })
}

async fn handle_read_imported_conversation<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    imported_conversation_id: CanonicalUuid,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let conversation_id = ImportedConversationId::from_uuid(imported_conversation_id.into_uuid());
    let load = ImportedConversationRepository::new(pool.clone())
        .load(conversation_id)
        .await;
    let conversation = match load {
        Ok(Some(conversation)) => conversation,
        Ok(None) => {
            drop(snapshot_permit);
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::imported_conversation_absent(),
            )
            .await;
        }
        Err(ImportedConversationRepositoryError::Database(_)) => {
            drop(snapshot_permit);
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Unavailable),
            )
            .await;
        }
        Err(
            error @ (ImportedConversationRepositoryError::IdentityCollision(_)
            | ImportedConversationRepositoryError::Corruption(_)),
        ) => {
            let diagnostic = imported_conversation_internal_diagnostic(&error);
            drop(snapshot_permit);
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, diagnostic),
            )
            .await;
        }
    };
    let spool_result =
        spool_imported_conversation(&conversation, imported_conversation_id, version, request_id)
            .await;
    drop(conversation);
    drop(snapshot_permit);
    let mut spool = match spool_result {
        Ok(spool) => spool,
        Err(error) => return write_snapshot_spool_error(writer, version, request_id, error).await,
    };
    write_spooled_file(writer, &mut spool).await
}

async fn spool_imported_conversation(
    conversation: &ImportedConversation,
    imported_conversation_id: CanonicalUuid,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<tokio::fs::File, SnapshotSpoolError> {
    let standard_file = tempfile::tempfile().map_err(SnapshotSpoolError::Io)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::ImportedConversationStart {
            imported_conversation_id,
        },
    )
    .await?;
    let mut entry_count = 0_u64;
    for entry in conversation.entries() {
        write_spool_message(
            &mut file,
            version,
            request_id,
            ServerMessage::ImportedConversationEntry {
                position: CanonicalU64::new(entry.position().as_u64()),
                imported_entry_id: wire_uuid(entry.identity().into_uuid()),
                source_speaker: wire_imported_speaker_attestation(entry.source_speaker()),
                content_kind: wire_imported_content_kind(process_imported_content_kind(
                    entry.content(),
                )),
                text_preview: imported_text_preview(entry.content()),
            },
        )
        .await?;
        entry_count = entry_count
            .checked_add(1)
            .ok_or(SnapshotSpoolError::EncodeInvariant)?;
    }
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::ImportedConversationEnd {
            imported_conversation_id,
            entry_count: CanonicalU64::new(entry_count),
        },
    )
    .await?;
    file.flush().await.map_err(SnapshotSpoolError::Io)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)?;
    Ok(file)
}

/// Maps one entry's normalized content to the conservative wire kind through
/// the same content-variant classification the transcript projection uses.
const fn process_imported_content_kind(
    content: &ImportedTranscriptContent,
) -> ProcessImportedContentKind {
    match content {
        ImportedTranscriptContent::SourceEvent { .. } => ProcessImportedContentKind::SourceEvent,
        ImportedTranscriptContent::SourceMessageBlock { .. } => {
            ProcessImportedContentKind::SourceMessageBlock
        }
        ImportedTranscriptContent::Text(_) => ProcessImportedContentKind::Text,
        ImportedTranscriptContent::ToolCall { .. } => ProcessImportedContentKind::ToolCall,
        ImportedTranscriptContent::ToolResult { .. } => ProcessImportedContentKind::ToolResult,
        ImportedTranscriptContent::Thinking { .. } => ProcessImportedContentKind::Thinking,
        ImportedTranscriptContent::RedactedThinking { .. } => {
            ProcessImportedContentKind::RedactedThinking
        }
        ImportedTranscriptContent::Document { .. } => ProcessImportedContentKind::Document,
        ImportedTranscriptContent::MessageContentAbsent(_) => {
            ProcessImportedContentKind::MessageContentAbsent
        }
    }
}

/// Previews exactly the text the transcript projection already carries in
/// full; every other imported content stays behind its kind alone.
fn imported_text_preview(content: &ImportedTranscriptContent) -> Option<ImportedTextPreview> {
    match content {
        ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text)) => {
            Some(ImportedTextPreview::of_exact_text(text.as_str()))
        }
        ImportedTranscriptContent::Text(
            ImportedSourceAttestation::AttestedAbsent | ImportedSourceAttestation::NotAttested,
        )
        | ImportedTranscriptContent::SourceEvent { .. }
        | ImportedTranscriptContent::SourceMessageBlock { .. }
        | ImportedTranscriptContent::ToolCall { .. }
        | ImportedTranscriptContent::ToolResult { .. }
        | ImportedTranscriptContent::Thinking { .. }
        | ImportedTranscriptContent::RedactedThinking { .. }
        | ImportedTranscriptContent::Document { .. }
        | ImportedTranscriptContent::MessageContentAbsent(_) => None,
    }
}

const fn wire_imported_speaker_attestation(
    attestation: &ImportedSourceAttestation<DomainImportedSpeaker>,
) -> ImportedSourceSpeaker {
    match attestation {
        ImportedSourceAttestation::NotAttested => ImportedSourceSpeaker::NotAttested {},
        ImportedSourceAttestation::AttestedAbsent => ImportedSourceSpeaker::AttestedAbsent {},
        ImportedSourceAttestation::Attested(DomainImportedSpeaker::User) => {
            ImportedSourceSpeaker::Attested {
                speaker: ImportedSpeaker::User,
            }
        }
        ImportedSourceAttestation::Attested(DomainImportedSpeaker::Assistant) => {
            ImportedSourceSpeaker::Attested {
                speaker: ImportedSpeaker::Assistant,
            }
        }
    }
}

struct WireCreateSessionRequest {
    command_uuid: uuid::Uuid,
    initial_model_selection: WireModelSelection,
    model_settings: WireModelSettingsOverlay,
    system_prompt: SystemPromptMember,
    placement: WireSessionPlacement,
}

async fn handle_create_session<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    wire_request: WireCreateSessionRequest,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let WireCreateSessionRequest {
        command_uuid,
        initial_model_selection,
        model_settings,
        system_prompt,
        placement,
    } = wire_request;
    let Ok(system_prompt) = domain_system_prompt(system_prompt) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(placement) = domain_session_placement(placement) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let model_selection = domain_model_selection(initial_model_selection);
    let caller_model_settings = domain_model_settings_overlay(model_settings);
    let command_id = DurableCommandId::from_uuid(command_uuid);
    let repository = CreateSessionRepository::new(
        services.pool.clone(),
        services.model_configuration.session_credential_pin(),
    );
    match repository.load(command_id).await {
        Ok(Some(recorded)) => {
            let command = recorded.command();
            let defaults = command.initial_configuration_defaults();
            if defaults.model() == model_selection
                && defaults.dangerous_tool_auto_approval() == DangerousToolAutoApproval::Disabled
                && defaults.system_prompt() == system_prompt.as_ref()
                && defaults.model_settings().precedence().session() == caller_model_settings
                && command.template_provenance().is_none()
                && command.placement() == &placement
            {
                return write_message(
                    writer,
                    version,
                    request_id,
                    ServerMessage::SessionCreated {
                        session_id: wire_uuid(recorded.applied_result().session().into_uuid()),
                        model_settings: wire_model_settings(defaults.model_settings()),
                    },
                )
                .await;
            }
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Ok(None) => {}
        Err(CreateSessionRepositoryError::DifferentCommandKind { .. }) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::Database(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::CommitAmbiguous(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::Corruption(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    None,
                    InternalDiagnostic::TemplateSessionCreationCorruption,
                ),
            )
            .await;
        }
    }
    let model_settings = match validate_session_model_settings(
        services.model_configuration.as_ref(),
        model_selection,
        caller_model_settings,
    ) {
        Ok(settings) => settings,
        Err(error) => {
            return write_error(
                writer,
                version,
                request_id,
                model_settings_protocol_error(error),
            )
            .await;
        }
    };
    let Some(defaults) = SessionConfigurationDefaults::complete_with_model_settings(
        model_selection,
        DangerousToolAutoApproval::Disabled,
        system_prompt,
        model_settings,
    ) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let request = CreateSessionRequest::try_new(command_id, defaults);
    let Ok(request) = request.map(|request| request.with_placement(placement)) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    execute_create_session_request(
        writer,
        version,
        request_id,
        request,
        &services.pool,
        services.model_configuration.as_ref(),
    )
    .await
}

async fn handle_create_session_from_template<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    template_name: String,
    placement: WireSessionPlacement,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Ok(template_name) = SessionTemplateName::try_new(template_name) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(placement) = domain_session_placement(placement) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let command_id = DurableCommandId::from_uuid(command_id);
    let repository = CreateSessionRepository::new(
        services.pool.clone(),
        services.model_configuration.session_credential_pin(),
    );
    match repository.load(command_id).await {
        Ok(Some(recorded)) => {
            let recorded_name = recorded
                .command()
                .template_provenance()
                .map(SessionTemplateProvenance::name);
            if recorded_name == Some(&template_name) && recorded.command().placement() == &placement
            {
                return write_message(
                    writer,
                    version,
                    request_id,
                    ServerMessage::SessionCreated {
                        session_id: wire_uuid(recorded.applied_result().session().into_uuid()),
                        model_settings: wire_model_settings(
                            recorded
                                .command()
                                .initial_configuration_defaults()
                                .model_settings(),
                        ),
                    },
                )
                .await;
            }
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Ok(None) => {}
        Err(CreateSessionRepositoryError::DifferentCommandKind { .. }) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::Database(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::CommitAmbiguous(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::Corruption(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    None,
                    InternalDiagnostic::TemplateSessionCreationCorruption,
                ),
            )
            .await;
        }
    }

    let Some(template) = services.template_configuration.resolve(&template_name) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let request = CreateSessionRequest::try_new_from_template(
        command_id,
        template.provenance().clone(),
        template.defaults().clone(),
    );
    let Ok(request) = request.map(|request| request.with_placement(placement)) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    execute_create_session_request(
        writer,
        version,
        request_id,
        request,
        &services.pool,
        services.model_configuration.as_ref(),
    )
    .await
}

struct WireSessionPlacementUpdateRequest {
    command_id: signalbox_process_protocol::CommandId,
    session_id: CanonicalUuid,
    expected_version: CanonicalU64,
    replacement: WireSessionPlacement,
}

async fn handle_update_session_placement<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: WireSessionPlacementUpdateRequest,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let WireSessionPlacementUpdateRequest {
        command_id,
        session_id,
        expected_version,
        replacement,
    } = request;
    let Some(expected_version) = SessionPlacementVersion::try_from_u64(expected_version.value())
    else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(replacement) = domain_session_placement(replacement) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let session = SessionId::from_uuid(session_id.into_uuid());
    let Ok(request) = UpdateSessionPlacementRequest::try_new(
        DurableCommandId::from_uuid(command_id.into_uuid()),
        session,
        expected_version,
        replacement,
    ) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let mut service =
        UpdateSessionPlacementService::new(SessionPlacementRepository::new(pool.clone()));
    match service.execute(request).await {
        Ok(UpdateSessionPlacementOutcome::Recorded(UpdateSessionPlacementResult::Applied(
            applied,
        ))) => {
            let recorded = applied.event().placement();
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionPlacementUpdated {
                    session_id,
                    placement_version: CanonicalU64::new(recorded.version().as_u64()),
                    placement: wire_session_placement(recorded.placement()),
                },
            )
            .await
        }
        Ok(UpdateSessionPlacementOutcome::Recorded(UpdateSessionPlacementResult::Rejected(
            rejected,
        ))) => {
            // Both version-bearing kinds carry their current version by
            // construction, so an absent one is placement-state corruption
            // rather than a rejection this connection can state on the wire.
            let error = match (rejected.kind(), rejected.current_version()) {
                (UpdateSessionPlacementRejectionKind::SessionNotFound, _) => {
                    ProtocolError::rejected(RejectionDetail::SessionNotFound {
                        session_id: wire_uuid(rejected.session().into_uuid()),
                    })
                }
                (UpdateSessionPlacementRejectionKind::CurrentVersionMismatch, Some(current)) => {
                    ProtocolError::rejected(
                        RejectionDetail::SessionPlacementCurrentVersionMismatch {
                            session_id: wire_uuid(rejected.session().into_uuid()),
                            expected_placement_version: CanonicalU64::new(
                                rejected.expected_version().as_u64(),
                            ),
                            current_placement_version: CanonicalU64::new(current.as_u64()),
                        },
                    )
                }
                (UpdateSessionPlacementRejectionKind::VersionExhausted, Some(current)) => {
                    ProtocolError::rejected(RejectionDetail::SessionPlacementVersionExhausted {
                        session_id: wire_uuid(rejected.session().into_uuid()),
                        current_placement_version: CanonicalU64::new(current.as_u64()),
                    })
                }
                (
                    UpdateSessionPlacementRejectionKind::CurrentVersionMismatch
                    | UpdateSessionPlacementRejectionKind::VersionExhausted,
                    None,
                ) => internal_protocol_error(
                    Some(rejected.session().into_uuid()),
                    InternalDiagnostic::ProcessReadCorruption,
                ),
            };
            write_error(writer, version, request_id, error).await
        }
        Ok(UpdateSessionPlacementOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(SessionPlacementRepositoryError::InvalidCommandId) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::InvalidRequest),
            )
            .await
        }
        Err(SessionPlacementRepositoryError::Database(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await
        }
        Err(SessionPlacementRepositoryError::CommitAmbiguous(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await
        }
        Err(SessionPlacementRepositoryError::Corruption(_)) => {
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, InternalDiagnostic::ProcessReadCorruption),
            )
            .await
        }
    }
}

async fn handle_list_templates<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    templates: &SessionTemplateConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::TemplatesStart {},
    )
    .await?;
    for (name, template_version) in templates.summaries() {
        write_message(
            writer,
            version,
            request_id,
            ServerMessage::TemplateSummary {
                name: name.as_str().to_owned(),
                version: CanonicalU64::new(template_version.as_u64()),
            },
        )
        .await?;
    }
    let template_count = u64::try_from(templates.summaries().len())
        .map_err(|_| ProcessConnectionError::EncodeInvariant)?;
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::TemplatesEnd {
            template_count: CanonicalU64::new(template_count),
        },
    )
    .await
}

async fn execute_create_session_request<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: CreateSessionRequest,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let repository =
        CreateSessionRepository::new(pool.clone(), model_configuration.session_credential_pin());
    match repository.load(request.command_id()).await {
        Ok(Some(recorded)) => {
            let command = recorded.command();
            if command.initial_configuration_defaults() == request.initial_configuration_defaults()
                && command.template_provenance() == request.template_provenance()
                && command.placement() == request.placement()
            {
                return write_message(
                    writer,
                    version,
                    request_id,
                    ServerMessage::SessionCreated {
                        session_id: wire_uuid(recorded.applied_result().session().into_uuid()),
                        model_settings: wire_model_settings(
                            recorded
                                .command()
                                .initial_configuration_defaults()
                                .model_settings(),
                        ),
                    },
                )
                .await;
            }
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Ok(None) => {}
        Err(CreateSessionRepositoryError::DifferentCommandKind { .. }) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::Database(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::CommitAmbiguous(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await;
        }
        Err(CreateSessionRepositoryError::Corruption(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    None,
                    InternalDiagnostic::TemplateSessionCreationCorruption,
                ),
            )
            .await;
        }
    }

    if model_configuration
        .resolve_session_model(request.initial_configuration_defaults().model())
        .is_err()
    {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    }

    let model_settings = request.initial_configuration_defaults().model_settings();
    let mut service = CreateSessionService::new(UuidV7SessionIdGenerator, repository);
    match service.execute(request).await {
        Ok(CreateSessionOutcome::Applied(result)) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionCreated {
                    session_id: wire_uuid(result.session().into_uuid()),
                    model_settings: wire_model_settings(model_settings),
                },
            )
            .await
        }
        Ok(CreateSessionOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(CreateSessionError::Transaction(CreateSessionRepositoryError::Database(_))) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await
        }
        Err(CreateSessionError::Transaction(CreateSessionRepositoryError::CommitAmbiguous(_))) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await
        }
        Err(
            error @ (CreateSessionError::Preparation(_)
            | CreateSessionError::Transaction(
                CreateSessionRepositoryError::DifferentCommandKind { .. }
                | CreateSessionRepositoryError::Corruption(_),
            )),
        ) => {
            let diagnostic = create_session_internal_diagnostic(&error);
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(None, diagnostic),
            )
            .await
        }
    }
}

async fn handle_list_sessions<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let spool_result = spool_session_summaries(
        ProcessReadRepository::new(pool.clone()),
        version,
        request_id,
    )
    .await;
    drop(snapshot_permit);
    let mut spool = match spool_result {
        Ok(spool) => spool,
        Err(SessionListSpoolError::Read(error)) => {
            return write_process_read_error(writer, version, request_id, None, error).await;
        }
        Err(SessionListSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    write_spooled_file(writer, &mut spool.file).await
}

async fn handle_list_model_aliases<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut aliases = configuration.model_aliases().collect::<Vec<_>>();
    aliases.sort_unstable_by_key(|(alias, _)| alias.into_uuid());
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ModelAliasesStart {},
    )
    .await?;
    for (alias, selection) in &aliases {
        write_message(
            writer,
            version,
            request_id,
            ServerMessage::ModelAliasSummary {
                alias_id: wire_uuid(alias.into_uuid()),
                selection_id: wire_uuid(selection.into_uuid()),
            },
        )
        .await?;
    }
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ModelAliasesEnd {
            alias_count: CanonicalU64::new(
                u64::try_from(aliases.len())
                    .map_err(|_| ProcessConnectionError::EncodeInvariant)?,
            ),
        },
    )
    .await
}

async fn handle_list_model_capabilities<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let catalog = configuration.model_capability_catalog();
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ModelCapabilitiesStart {},
    )
    .await?;
    let mut capability_count = 0_u64;
    for (selection, capabilities) in catalog.iter() {
        capability_count = capability_count
            .checked_add(1)
            .ok_or(ProcessConnectionError::EncodeInvariant)?;
        write_message(
            writer,
            version,
            request_id,
            ServerMessage::ModelCapabilityItem {
                selection_id: wire_uuid(selection.into_uuid()),
                capabilities: WireModelCapabilities {
                    reasoning_levels: capabilities
                        .reasoning_levels()
                        .iter()
                        .copied()
                        .map(wire_reasoning_level)
                        .collect(),
                    fast_mode_supported: !matches!(
                        capabilities.fast_mode(),
                        signalbox_domain::FastModeSupport::Unsupported
                    ),
                    service_tiers: capabilities
                        .service_tiers()
                        .iter()
                        .copied()
                        .map(wire_service_tier)
                        .collect(),
                },
            },
        )
        .await?;
    }
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ModelCapabilitiesEnd {
            capability_count: CanonicalU64::new(capability_count),
        },
    )
    .await
}

struct SessionListSpool {
    file: tokio::fs::File,
}

enum SessionListSpoolError {
    Read(ProcessReadError),
    Spool(SnapshotSpoolError),
}

#[derive(Debug)]
enum SnapshotSpoolError {
    Io(io::Error),
    Encode(FrameEncodeError),
    EncodeInvariant,
}

impl SnapshotSpoolError {
    fn from_connection(error: ProcessConnectionError) -> Self {
        match error {
            ProcessConnectionError::PeerIo(error) | ProcessConnectionError::SpoolIo(error) => {
                Self::Io(error)
            }
            ProcessConnectionError::Encode(error) => Self::Encode(error),
            ProcessConnectionError::EncodeInvariant
            | ProcessConnectionError::InboundFrameBudgetClosed
            | ProcessConnectionError::ImportBudgetClosed
            | ProcessConnectionError::ReviewCommandBudgetClosed
            | ProcessConnectionError::SnapshotReaderBudgetClosed => Self::EncodeInvariant,
        }
    }
}

async fn write_snapshot_spool_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    error: SnapshotSpoolError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match error {
        SnapshotSpoolError::Io(error) => {
            tracing::warn!(error = %error, "process snapshot spooling failed before response");
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Unavailable),
            )
            .await
        }
        SnapshotSpoolError::Encode(error) => Err(ProcessConnectionError::Encode(error)),
        SnapshotSpoolError::EncodeInvariant => Err(ProcessConnectionError::EncodeInvariant),
    }
}

async fn spool_session_summaries(
    repository: ProcessReadRepository,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<SessionListSpool, SessionListSpoolError> {
    let mut reader = repository
        .open_session_summaries()
        .await
        .map_err(SessionListSpoolError::Read)?;
    let standard_file = tempfile::tempfile()
        .map_err(SnapshotSpoolError::Io)
        .map_err(SessionListSpoolError::Spool)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::SessionsStart {},
    )
    .await
    .map_err(SessionListSpoolError::Spool)?;
    while let Some(summary) = reader
        .next_summary()
        .await
        .map_err(SessionListSpoolError::Read)?
    {
        write_spool_message(
            &mut file,
            version,
            request_id,
            ServerMessage::SessionSummary {
                session_id: wire_uuid(summary.session().into_uuid()),
                defaults_version: CanonicalU64::new(summary.defaults_version()),
                model_selection: wire_model_selection(summary.model_selection()),
                placement_version: CanonicalU64::new(summary.placement().version().as_u64()),
                placement: wire_session_placement(summary.placement().placement()),
                runner: summary
                    .runner()
                    .map(wire_runner_projection)
                    .transpose()
                    .map_err(SessionListSpoolError::Spool)?,
            },
        )
        .await
        .map_err(SessionListSpoolError::Spool)?;
    }
    let session_count = reader
        .summary_count()
        .ok_or(SnapshotSpoolError::EncodeInvariant)
        .map_err(SessionListSpoolError::Spool)?;
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::SessionsEnd {
            session_count: CanonicalU64::new(session_count),
        },
    )
    .await
    .map_err(SessionListSpoolError::Spool)?;
    file.flush()
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(SessionListSpoolError::Spool)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(SessionListSpoolError::Spool)?;
    Ok(SessionListSpool { file })
}

struct WireMetadataPageRequest {
    required_tags: Vec<String>,
    title_contains: Option<String>,
    include_archived: bool,
    page_size: CanonicalU64,
    after_session_id: Option<CanonicalUuid>,
}

async fn handle_list_session_metadata<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: WireMetadataPageRequest,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let query = SessionMetadataListQuery::try_new(
        request.required_tags,
        request.title_contains,
        request.include_archived,
        request.page_size.value(),
        request
            .after_session_id
            .map(|value| SessionId::from_uuid(value.into_uuid())),
    );
    let Ok(query) = query else {
        drop(snapshot_permit);
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let spool_result = spool_session_metadata_page(
        SessionMetadataRepository::new(pool.clone()),
        query,
        version,
        request_id,
    )
    .await;
    drop(snapshot_permit);
    let mut spool = match spool_result {
        Ok(spool) => spool,
        Err(MetadataPageSpoolError::Read(error)) => {
            return write_session_metadata_read_error(writer, version, request_id, None, error)
                .await;
        }
        Err(MetadataPageSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    write_spooled_file(writer, &mut spool.file).await
}

enum MetadataPageSpoolError {
    Read(SessionMetadataRepositoryError),
    Spool(SnapshotSpoolError),
}

async fn spool_session_metadata_page(
    repository: SessionMetadataRepository,
    query: SessionMetadataListQuery,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<SessionListSpool, MetadataPageSpoolError> {
    let mut page = ListSessionMetadataService::new(repository)
        .execute(query)
        .await
        .map_err(MetadataPageSpoolError::Read)?;
    let standard_file = tempfile::tempfile()
        .map_err(SnapshotSpoolError::Io)
        .map_err(MetadataPageSpoolError::Spool)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::SessionMetadataPageStart {},
    )
    .await
    .map_err(MetadataPageSpoolError::Spool)?;
    let mut session_count = 0_u64;
    while let Some(item) = page
        .next_item()
        .await
        .map_err(MetadataPageSpoolError::Read)?
    {
        let (title, tags, last_writer) = wire_list_metadata(&item)
            .ok_or(SnapshotSpoolError::EncodeInvariant)
            .map_err(MetadataPageSpoolError::Spool)?;
        write_spool_message(
            &mut file,
            version,
            request_id,
            ServerMessage::SessionMetadataSummary {
                session_id: wire_uuid(item.session().into_uuid()),
                defaults_version: CanonicalU64::new(item.defaults_version().as_u64()),
                model_selection: wire_domain_model_selection(item.model_selection()),
                dangerous_tool_auto_approval: matches!(
                    item.dangerous_tool_auto_approval(),
                    DangerousToolAutoApproval::ApproveAll
                ),
                title,
                tags,
                archived: item.archived(),
                last_writer,
            },
        )
        .await
        .map_err(MetadataPageSpoolError::Spool)?;
        session_count = session_count
            .checked_add(1)
            .ok_or(SnapshotSpoolError::EncodeInvariant)
            .map_err(MetadataPageSpoolError::Spool)?;
    }
    let next_after_session_id = page
        .next_after_session()
        .map(|session| wire_uuid(session.into_uuid()));
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::SessionMetadataPageEnd {
            session_count: CanonicalU64::new(session_count),
            next_after_session_id,
        },
    )
    .await
    .map_err(MetadataPageSpoolError::Spool)?;
    file.flush()
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(MetadataPageSpoolError::Spool)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(MetadataPageSpoolError::Spool)?;
    Ok(SessionListSpool { file })
}

struct WireConversationPageRequest {
    title_contains: Option<String>,
    origin: WireConversationOriginFilter,
    include_archived: bool,
    page_size: CanonicalU64,
    after: Option<WireConversationCursor>,
}

async fn handle_list_conversations<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: WireConversationPageRequest,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let query = ConversationListQuery::try_new(
        request.title_contains,
        application_origin_filter(request.origin),
        request.include_archived,
        request.page_size.value(),
        request.after.map(application_cursor),
    );
    let Ok(query) = query else {
        drop(snapshot_permit);
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let spool_result = spool_conversation_page(
        ConversationListingRepository::new(pool.clone()),
        query,
        version,
        request_id,
    )
    .await;
    drop(snapshot_permit);
    let mut spool = match spool_result {
        Ok(spool) => spool,
        Err(ConversationPageSpoolError::Read(error)) => {
            return write_conversation_listing_read_error(writer, version, request_id, error).await;
        }
        Err(ConversationPageSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    write_spooled_file(writer, &mut spool.file).await
}

enum ConversationPageSpoolError {
    Read(ConversationListingRepositoryError),
    Spool(SnapshotSpoolError),
}

async fn spool_conversation_page(
    repository: ConversationListingRepository,
    query: ConversationListQuery,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<SessionListSpool, ConversationPageSpoolError> {
    let mut page = ListConversationsService::new(repository)
        .execute(query)
        .await
        .map_err(ConversationPageSpoolError::Read)?;
    let standard_file = tempfile::tempfile()
        .map_err(SnapshotSpoolError::Io)
        .map_err(ConversationPageSpoolError::Spool)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::ConversationPageStart {},
    )
    .await
    .map_err(ConversationPageSpoolError::Spool)?;
    let mut conversation_count = 0_u64;
    while let Some(item) = page
        .next_item()
        .await
        .map_err(ConversationPageSpoolError::Read)?
    {
        write_spool_message(
            &mut file,
            version,
            request_id,
            ServerMessage::ConversationSummary {
                conversation: wire_conversation_summary(item),
            },
        )
        .await
        .map_err(ConversationPageSpoolError::Spool)?;
        conversation_count = conversation_count
            .checked_add(1)
            .ok_or(SnapshotSpoolError::EncodeInvariant)
            .map_err(ConversationPageSpoolError::Spool)?;
    }
    let next_after = page.next_after().map(wire_cursor);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::ConversationPageEnd {
            conversation_count: CanonicalU64::new(conversation_count),
            next_after,
        },
    )
    .await
    .map_err(ConversationPageSpoolError::Spool)?;
    file.flush()
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(ConversationPageSpoolError::Spool)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(ConversationPageSpoolError::Spool)?;
    Ok(SessionListSpool { file })
}

const fn application_origin_filter(
    origin: WireConversationOriginFilter,
) -> ConversationOriginFilter {
    match origin {
        WireConversationOriginFilter::Native => ConversationOriginFilter::Native,
        WireConversationOriginFilter::Imported => ConversationOriginFilter::Imported,
        WireConversationOriginFilter::All => ConversationOriginFilter::All,
    }
}

fn application_cursor(cursor: WireConversationCursor) -> ConversationListCursor {
    match cursor.origin() {
        WireConversationOrigin::NativeSession => ConversationListCursor::NativeSession(
            SessionId::from_uuid(cursor.conversation_id().into_uuid()),
        ),
        WireConversationOrigin::ImportedConversation => {
            ConversationListCursor::ImportedConversation(ImportedConversationId::from_uuid(
                cursor.conversation_id().into_uuid(),
            ))
        }
    }
}

fn wire_cursor(cursor: ConversationListCursor) -> WireConversationCursor {
    match cursor {
        ConversationListCursor::NativeSession(session) => WireConversationCursor::new(
            WireConversationOrigin::NativeSession,
            wire_uuid(session.into_uuid()),
        ),
        ConversationListCursor::ImportedConversation(conversation) => WireConversationCursor::new(
            WireConversationOrigin::ImportedConversation,
            wire_uuid(conversation.into_uuid()),
        ),
    }
}

fn wire_conversation_summary(item: ConversationListItem) -> WireConversationSummary {
    match item {
        ConversationListItem::NativeSession {
            session,
            title,
            archived,
            defaults_version,
        } => WireConversationSummary::NativeSession {
            session_id: wire_uuid(session.into_uuid()),
            title,
            archived,
            defaults_version: CanonicalU64::new(defaults_version.as_u64()),
        },
        ConversationListItem::ImportedConversation {
            conversation,
            title,
            entry_count,
            format,
        } => WireConversationSummary::ImportedConversation {
            imported_conversation_id: wire_uuid(conversation.into_uuid()),
            title,
            entry_count: CanonicalU64::new(entry_count),
            source_format: wire_imported_source_format(format),
        },
    }
}

const fn wire_imported_source_format(
    format: ImportedConversationFormat,
) -> WireImportedConversationSourceFormat {
    match format {
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1 => {
            WireImportedConversationSourceFormat::ClaudeCodeSessionJsonlV1
        }
        ImportedConversationFormat::ClaudeCodeSessionJsonlV2 => {
            WireImportedConversationSourceFormat::ClaudeCodeSessionJsonlV2
        }
        ImportedConversationFormat::CodexRolloutJsonlV1 => {
            WireImportedConversationSourceFormat::CodexRolloutJsonlV1
        }
    }
}

async fn write_conversation_listing_read_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    error: ConversationListingRepositoryError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let response = match error {
        ConversationListingRepositoryError::Database(_) => {
            ProtocolError::without_detail(ErrorCode::Unavailable)
        }
        ConversationListingRepositoryError::Corruption(_) => {
            internal_protocol_error(None, InternalDiagnostic::ConversationListingCorruption)
        }
    };
    write_error(writer, version, request_id, response).await
}

async fn handle_read_session_metadata<Writer>(
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
    let service = LoadSessionMetadataService::new(SessionMetadataRepository::new(pool.clone()));
    let loaded = service
        .execute(SessionId::from_uuid(session_id.into_uuid()))
        .await;
    drop(snapshot_permit);
    match loaded {
        Ok(Some(snapshot)) => {
            let (metadata, last_writer) =
                wire_metadata_snapshot(&snapshot).ok_or(ProcessConnectionError::EncodeInvariant)?;
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionMetadata {
                    session_id,
                    metadata,
                    last_writer,
                },
            )
            .await
        }
        Ok(None) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::NotFound),
            )
            .await
        }
        Err(error) => {
            write_session_metadata_read_error(writer, version, request_id, Some(session_id), error)
                .await
        }
    }
}

async fn handle_read_session_defaults<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    defaults_version: Option<CanonicalU64>,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let named_version = match defaults_version {
        None => None,
        Some(value) => match SessionConfigurationDefaultsVersion::try_from_u64(value.value()) {
            Some(version) => Some(version),
            None => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::InvalidRequest),
                )
                .await;
            }
        },
    };
    let repository = ProcessReadRepository::new(pool.clone());
    match repository
        .read_session_defaults(SessionId::from_uuid(session_id.into_uuid()), named_version)
        .await
    {
        Ok(ProcessSessionDefaultsRead::Read(read)) => {
            let system_prompt = match wire_system_prompt(read.defaults().system_prompt()) {
                Some(system_prompt) => system_prompt,
                None => return Err(ProcessConnectionError::EncodeInvariant),
            };
            write_message_via_spool(
                writer,
                version,
                request_id,
                ServerMessage::SessionDefaults {
                    session_id,
                    defaults_version: CanonicalU64::new(read.version().as_u64()),
                    model_selection: wire_domain_model_selection(read.defaults().model()),
                    model_settings: wire_model_settings(read.defaults().model_settings()),
                    dangerous_tool_auto_approval: matches!(
                        read.defaults().dangerous_tool_auto_approval(),
                        DangerousToolAutoApproval::ApproveAll
                    ),
                    system_prompt,
                },
            )
            .await
        }
        Ok(ProcessSessionDefaultsRead::SessionNotFound) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::NotFound),
            )
            .await
        }
        Ok(ProcessSessionDefaultsRead::VersionNotFound) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::defaults_epoch_not_found(),
            )
            .await
        }
        Err(error) => {
            write_process_read_error(writer, version, request_id, Some(session_id), error).await
        }
    }
}

async fn handle_replace_session_metadata<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    metadata: WireSessionMetadata,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let replacement = SessionMetadataContent::try_new(
        metadata.title().map(str::to_owned),
        metadata.tags().map(str::to_owned).collect(),
        metadata
            .attributes()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        metadata.archived(),
    );
    let Ok(replacement) = replacement else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let request = ReplaceSessionMetadataRequest::try_new(
        DurableCommandId::from_uuid(command_id),
        SessionId::from_uuid(session_id.into_uuid()),
        replacement,
    );
    let Ok(request) = request else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let mut service =
        ReplaceSessionMetadataService::new(SessionMetadataRepository::new(pool.clone()));
    match service.execute(request).await {
        Ok(ReplaceSessionMetadataOutcome::Recorded(ReplaceSessionMetadataResult::Applied(
            applied,
        ))) => {
            let (metadata, last_writer) = wire_metadata_snapshot(applied.snapshot())
                .ok_or(ProcessConnectionError::EncodeInvariant)?;
            let last_writer = last_writer.ok_or(ProcessConnectionError::EncodeInvariant)?;
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionMetadataReplaced {
                    session_id,
                    metadata,
                    last_writer,
                },
            )
            .await
        }
        Ok(ReplaceSessionMetadataOutcome::Recorded(ReplaceSessionMetadataResult::Rejected(
            ReplaceSessionMetadataRejectedResult::SessionNotFound(rejected),
        ))) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::rejected(RejectionDetail::SessionNotFound {
                    session_id: wire_uuid(rejected.session().into_uuid()),
                }),
            )
            .await
        }
        Ok(ReplaceSessionMetadataOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(SessionMetadataRepositoryError::Database(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await
        }
        Err(SessionMetadataRepositoryError::CommitAmbiguous(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await
        }
        Err(
            error @ (SessionMetadataRepositoryError::DifferentCommandKind { .. }
            | SessionMetadataRepositoryError::Corruption(_)),
        ) => {
            let diagnostic = session_metadata_internal_diagnostic(&error);
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(Some(session_id.into_uuid()), diagnostic),
            )
            .await
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the complete defaults replacement remains explicit at the wire adapter"
)]
async fn handle_replace_session_defaults<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    expected_defaults_version: CanonicalU64,
    model_selection: WireModelSelection,
    model_settings: WireModelSettingsOverlay,
    dangerous_tool_auto_approval: bool,
    system_prompt: SystemPromptMember,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Some(expected_version) =
        SessionConfigurationDefaultsVersion::try_from_u64(expected_defaults_version.value())
    else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let prompt_member_is_absent = system_prompt.value().is_none();
    let Ok(system_prompt) = domain_system_prompt(system_prompt) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let replacement_model = domain_model_selection(model_selection);
    let session = SessionId::from_uuid(session_id.into_uuid());
    let caller_model_settings = domain_model_settings_overlay(model_settings);
    let dangerous_tool_auto_approval = if dangerous_tool_auto_approval {
        DangerousToolAutoApproval::ApproveAll
    } else {
        DangerousToolAutoApproval::Disabled
    };
    let durable_command_id = DurableCommandId::from_uuid(command_id);
    let repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    match repository.load(durable_command_id).await {
        Ok(Some(recorded)) => {
            let command = recorded.command();
            let replacement = command.replacement();
            if command.session() == session
                && command.expected_current_version() == expected_version
                && !prompt_member_is_absent
                && replacement.model() == replacement_model
                && replacement.dangerous_tool_auto_approval() == dangerous_tool_auto_approval
                && replacement.system_prompt() == system_prompt.as_ref()
                && command.caller_model_settings() == caller_model_settings
            {
                return write_replace_session_defaults_result(
                    writer,
                    version,
                    request_id,
                    session_id,
                    recorded.result(),
                )
                .await;
            }
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Ok(None) => {}
        Err(ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { .. }) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Err(ReplaceSessionDefaultsRepositoryError::Database { .. }) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(ReplaceSessionDefaultsRepositoryError::Corruption(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    Some(session_id.into_uuid()),
                    InternalDiagnostic::SessionDefaultsCorruption,
                ),
            )
            .await;
        }
    }
    let prompt_member = if prompt_member_is_absent {
        PromptMemberStatement::Unstated
    } else {
        PromptMemberStatement::Stated
    };
    // The immutable catalog decides an unknown direct selection or alias before
    // any application construction below, so that read-only fact can never be
    // recorded under this command identity as a defaults-version mismatch by
    // the rejection-only boundary. Unlike a capability rejection, it depends on
    // no defaults snapshot and therefore cannot lose a race worth replaying.
    if model_configuration
        .resolve_direct_selection(replacement_model)
        .is_none()
    {
        return write_error(
            writer,
            version,
            request_id,
            model_settings_protocol_error(ModelSettingsAdmissionError::UnknownModel),
        )
        .await;
    }
    let prior_model_settings = match ProcessReadRepository::new(pool.clone())
        .read_session_defaults(session, None)
        .await
    {
        Ok(ProcessSessionDefaultsRead::Read(read)) if read.version() == expected_version => {
            Some(read.defaults().model_settings())
        }
        Ok(ProcessSessionDefaultsRead::Read(read)) if read.version() > expected_version => None,
        Ok(ProcessSessionDefaultsRead::Read(_)) => {
            let placeholder = SessionConfigurationDefaults::complete(
                replacement_model,
                dangerous_tool_auto_approval,
                system_prompt.clone(),
            );
            let command = DomainReplaceSessionDefaults::with_model_settings_adjustments(
                durable_command_id,
                session,
                expected_version,
                placeholder,
                caller_model_settings,
                Vec::new(),
            );
            match handle_defaults_rejection_only(&repository, command, prompt_member).await {
                Ok(Some(outcome)) => {
                    return write_replace_session_defaults_outcome(
                        writer, version, request_id, session_id, outcome,
                    )
                    .await;
                }
                Ok(None) => {
                    match ProcessReadRepository::new(pool.clone())
                        .read_session_defaults(session, Some(expected_version))
                        .await
                    {
                        Ok(ProcessSessionDefaultsRead::Read(read)) => {
                            Some(read.defaults().model_settings())
                        }
                        Ok(
                            ProcessSessionDefaultsRead::SessionNotFound
                            | ProcessSessionDefaultsRead::VersionNotFound,
                        ) => {
                            return write_error(
                                writer,
                                version,
                                request_id,
                                internal_protocol_error(
                                    Some(session_id.into_uuid()),
                                    InternalDiagnostic::SessionDefaultsCorruption,
                                ),
                            )
                            .await;
                        }
                        Err(ProcessReadError::Database(_)) => {
                            return write_error(
                                writer,
                                version,
                                request_id,
                                ProtocolError::mutation_unavailable(false),
                            )
                            .await;
                        }
                        Err(ProcessReadError::Corruption(_)) => {
                            return write_error(
                                writer,
                                version,
                                request_id,
                                internal_protocol_error(
                                    Some(session_id.into_uuid()),
                                    InternalDiagnostic::SessionDefaultsCorruption,
                                ),
                            )
                            .await;
                        }
                    }
                }
                Err(ReplaceSessionDefaultsRepositoryError::Database {
                    commit_ambiguous, ..
                }) => {
                    return write_error(
                        writer,
                        version,
                        request_id,
                        ProtocolError::mutation_unavailable(commit_ambiguous),
                    )
                    .await;
                }
                Err(error) => {
                    let diagnostic = session_defaults_internal_diagnostic(&error);
                    return write_error(
                        writer,
                        version,
                        request_id,
                        internal_protocol_error(Some(session_id.into_uuid()), diagnostic),
                    )
                    .await;
                }
            }
        }
        Ok(ProcessSessionDefaultsRead::SessionNotFound) => None,
        Ok(ProcessSessionDefaultsRead::VersionNotFound) => {
            Some(ValidatedModelSettings::provider_defaults())
        }
        Err(ProcessReadError::Database(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(ProcessReadError::Corruption(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    Some(session_id.into_uuid()),
                    InternalDiagnostic::SessionDefaultsCorruption,
                ),
            )
            .await;
        }
    };
    let (model_settings, model_settings_adjustments) = match prior_model_settings {
        Some(prior_model_settings) => match validate_replacement_model_settings(
            model_configuration,
            replacement_model,
            caller_model_settings,
            prior_model_settings,
        ) {
            Ok(settings) => settings,
            Err(error) => {
                // Validation used an unlocked defaults snapshot. Re-enter the
                // pointer-locked rejection boundary before surfacing the
                // caller error, so a racing advance records and replays its
                // authoritative mismatch instead.
                let placeholder = SessionConfigurationDefaults::complete(
                    replacement_model,
                    dangerous_tool_auto_approval,
                    system_prompt.clone(),
                );
                let command = DomainReplaceSessionDefaults::with_model_settings_adjustments(
                    durable_command_id,
                    session,
                    expected_version,
                    placeholder,
                    caller_model_settings,
                    Vec::new(),
                );
                match handle_defaults_rejection_only(&repository, command, prompt_member).await {
                    Ok(Some(outcome)) => {
                        return write_replace_session_defaults_outcome(
                            writer, version, request_id, session_id, outcome,
                        )
                        .await;
                    }
                    Ok(None) => {
                        return write_error(
                            writer,
                            version,
                            request_id,
                            model_settings_protocol_error(error),
                        )
                        .await;
                    }
                    Err(ReplaceSessionDefaultsRepositoryError::Database {
                        commit_ambiguous,
                        ..
                    }) => {
                        return write_error(
                            writer,
                            version,
                            request_id,
                            ProtocolError::mutation_unavailable(commit_ambiguous),
                        )
                        .await;
                    }
                    Err(error) => {
                        let diagnostic = session_defaults_internal_diagnostic(&error);
                        return write_error(
                            writer,
                            version,
                            request_id,
                            internal_protocol_error(Some(session_id.into_uuid()), diagnostic),
                        )
                        .await;
                    }
                }
            }
        },
        // A stale epoch can never move backward, and an absent session must be
        // classified by the durable command boundary. The catalog identity was
        // already decided above, so preserve the canonical caller overlay while
        // supplying an inert replacement snapshot and let the transaction
        // record and replay its authoritative rejection first.
        None => (
            ValidatedModelSettings::provider_defaults(),
            Vec::<DomainModelChangeAdjustment>::new().into_boxed_slice(),
        ),
    };
    let Some(replacement) = SessionConfigurationDefaults::complete_with_model_settings(
        replacement_model,
        dangerous_tool_auto_approval,
        system_prompt,
        model_settings,
    ) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    // A member the frame could not state must not silently clear a prompt
    // the current epoch carries; the transaction refuses that atomically
    // under the compare-and-set lock, recording nothing.
    let request = ReplaceSessionDefaultsRequest::try_new_with_model_settings_adjustments(
        durable_command_id,
        session,
        expected_version,
        replacement,
        caller_model_settings,
        model_settings_adjustments.into_vec(),
        prompt_member,
    );
    let Ok(request) = request else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let mut service = ReplaceSessionDefaultsService::new(repository);
    match service.execute(request).await {
        Ok(outcome) => {
            write_replace_session_defaults_outcome(writer, version, request_id, session_id, outcome)
                .await
        }
        Err(ReplaceSessionDefaultsRepositoryError::Database {
            commit_ambiguous, ..
        }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(commit_ambiguous),
            )
            .await
        }
        Err(
            error @ (ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { .. }
            | ReplaceSessionDefaultsRepositoryError::Corruption(_)),
        ) => {
            let diagnostic = session_defaults_internal_diagnostic(&error);
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(Some(session_id.into_uuid()), diagnostic),
            )
            .await
        }
    }
}

async fn handle_defaults_rejection_only(
    repository: &ReplaceSessionDefaultsRepository,
    command: DomainReplaceSessionDefaults,
    prompt_member: PromptMemberStatement,
) -> Result<Option<ReplaceSessionDefaultsOutcome>, ReplaceSessionDefaultsRepositoryError> {
    let outcome = repository
        .handle_rejection_only_where_prompt_member(command, prompt_member)
        .await?;
    Ok(match outcome {
        ReplaceSessionDefaultsRejectionOnlyOutcome::CurrentVersionMatched => None,
        ReplaceSessionDefaultsRejectionOnlyOutcome::Handled(outcome) => Some(match outcome {
            ReplaceSessionDefaultsHandlingOutcome::Applied(result) => {
                ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(
                    result,
                ))
            }
            ReplaceSessionDefaultsHandlingOutcome::Rejected(result) => {
                ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Rejected(
                    result,
                ))
            }
            ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse { command_id } => {
                ReplaceSessionDefaultsOutcome::ConflictingReuse { command_id }
            }
            ReplaceSessionDefaultsHandlingOutcome::PromptRequiresStatedMember => {
                ReplaceSessionDefaultsOutcome::PromptRequiresStatedMember
            }
        }),
    })
}

async fn write_replace_session_defaults_outcome<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    outcome: ReplaceSessionDefaultsOutcome,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match outcome {
        ReplaceSessionDefaultsOutcome::Recorded(result) => {
            write_replace_session_defaults_result(writer, version, request_id, session_id, &result)
                .await
        }
        // Frame validation rejects an absent system-prompt member, so this
        // repository outcome cannot be client-triggered.
        ReplaceSessionDefaultsOutcome::PromptRequiresStatedMember => {
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    Some(session_id.into_uuid()),
                    InternalDiagnostic::SystemPromptMemberMissing,
                ),
            )
            .await
        }
        ReplaceSessionDefaultsOutcome::ConflictingReuse { .. } => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
    }
}

async fn write_replace_session_defaults_result<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    result: &ReplaceSessionDefaultsResult,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match result {
        ReplaceSessionDefaultsResult::Applied(applied) => {
            let installed = applied.installed();
            let system_prompt = SystemPromptMember::present(
                wire_system_prompt(installed.defaults().system_prompt())
                    .ok_or(ProcessConnectionError::EncodeInvariant)?,
            );
            write_mutation_receipt_via_spool(
                writer,
                version,
                request_id,
                ServerMessage::SessionDefaultsReplaced {
                    session_id,
                    defaults_version: CanonicalU64::new(installed.version().as_u64()),
                    model_selection: wire_domain_model_selection(installed.defaults().model()),
                    model_settings: wire_model_settings(installed.defaults().model_settings()),
                    dangerous_tool_auto_approval: matches!(
                        installed.defaults().dangerous_tool_auto_approval(),
                        DangerousToolAutoApproval::ApproveAll
                    ),
                    system_prompt,
                },
            )
            .await
        }
        ReplaceSessionDefaultsResult::Rejected(rejected) => {
            let detail = match rejected {
                ReplaceSessionDefaultsRejectedResult::SessionNotFound(rejected) => {
                    RejectionDetail::SessionNotFound {
                        session_id: wire_uuid(rejected.session().into_uuid()),
                    }
                }
                ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(rejected) => {
                    RejectionDetail::DefaultsVersionMismatch {
                        session_id: wire_uuid(rejected.session().into_uuid()),
                        expected: CanonicalU64::new(rejected.expected().as_u64()),
                        current: CanonicalU64::new(rejected.current().as_u64()),
                    }
                }
                ReplaceSessionDefaultsRejectedResult::VersionExhausted(rejected) => {
                    RejectionDetail::DefaultsVersionExhausted {
                        session_id: wire_uuid(rejected.session().into_uuid()),
                        current: CanonicalU64::new(rejected.current().as_u64()),
                    }
                }
            };
            write_error(writer, version, request_id, ProtocolError::rejected(detail)).await
        }
    }
}

async fn write_session_metadata_read_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: Option<CanonicalUuid>,
    error: SessionMetadataRepositoryError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let response = match error {
        SessionMetadataRepositoryError::Database(_)
        | SessionMetadataRepositoryError::CommitAmbiguous(_) => {
            ProtocolError::without_detail(ErrorCode::Unavailable)
        }
        SessionMetadataRepositoryError::DifferentCommandKind { .. } => internal_protocol_error(
            session_id.map(CanonicalUuid::into_uuid),
            InternalDiagnostic::SessionMetadataCommandKindMismatch,
        ),
        SessionMetadataRepositoryError::Corruption(_) => internal_protocol_error(
            session_id.map(CanonicalUuid::into_uuid),
            InternalDiagnostic::SessionMetadataCorruption,
        ),
    };
    write_error(writer, version, request_id, response).await
}

#[derive(Debug)]
struct ConfiguredSubmitInputTransaction<'configuration> {
    repository: SubmitInputRepository,
    model_configuration: &'configuration HubModelConfiguration,
}

impl SubmitInputTransaction for ConfiguredSubmitInputTransaction<'_> {
    type Error = SubmitInputRepositoryError;

    async fn handle<NextTurn, NextToolCancellation>(
        &mut self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        cancellation_identities: CancelledModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
        next_tool_cancellation: NextToolCancellation,
    ) -> Result<SubmitInputOutcome, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        NextToolCancellation: FnMut(
                &[signalbox_domain::ToolRequestId],
            ) -> (
                Vec<signalbox_domain::SemanticTranscriptEntryId>,
                signalbox_domain::ContextFrontierId,
            ) + Send,
    {
        let outcome = self
            .repository
            .handle_with_candidates_alias_resolver(
                command,
                accepted_input,
                turn,
                cancellation_identities,
                next_reclassified_turn,
                next_tool_cancellation,
                |alias| self.model_configuration.resolve_alias(alias),
            )
            .await?;

        Ok(match outcome {
            SubmitInputHandlingOutcome::Recorded(result) => SubmitInputOutcome::Recorded(result),
            SubmitInputHandlingOutcome::ConflictingReuse { command_id } => {
                SubmitInputOutcome::ConflictingReuse { command_id }
            }
        })
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the closed submit request is kept explicit at this wire-to-application adapter"
)]
async fn handle_submit_input<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    content: InputContent,
    expected_defaults_version: Option<CanonicalU64>,
    model_settings: WireModelSettingsOverlay,
    delivery: Option<InputDelivery>,
    pool: &PgPool,
    eligibility_nudge: &InProcessEligibilityNudge,
    tool_dispatch_gate: &InProcessToolDispatchGate,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Ok(content) = admitted_user_content(content) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let session = SessionId::from_uuid(session_id.into_uuid());
    let command_id = DurableCommandId::from_uuid(command_id);
    let repository = SubmitInputRepository::with_model_capabilities(
        pool.clone(),
        model_configuration.model_capability_catalog(),
    );
    let expected_version = expected_defaults_version
        .and_then(|version| SessionConfigurationDefaultsVersion::try_from_u64(version.value()));
    let model_settings = domain_model_settings_overlay(model_settings);
    let configuration = || {
        expected_version.map(|version| {
            PerInputConfigurationChoices::with_model_settings(
                version,
                ModelSelectionOverride::UseSessionDefault,
                model_settings,
            )
        })
    };
    let delivery = match delivery {
        None | Some(InputDelivery::StartWhenIdle {}) => configuration()
            .map(|configuration| DeliveryRequest::StartWhenNoActiveTurn { configuration }),
        Some(InputDelivery::Steer {
            expected_active_turn_id,
        }) if expected_defaults_version.is_none()
            && model_settings == DomainModelSettingsOverlay::inherit_all() =>
        {
            Some(DeliveryRequest::NextSafePoint {
                expected_active_turn: TurnId::from_uuid(expected_active_turn_id.into_uuid()),
            })
        }
        Some(InputDelivery::Queue {
            expected_active_turn_id,
        }) => configuration().map(|configuration| DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: TurnId::from_uuid(expected_active_turn_id.into_uuid()),
            configuration,
        }),
        Some(InputDelivery::Steer { .. }) => None,
    };
    let Some(delivery) = delivery else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let request = SubmitInputRequest::try_new(command_id, session, content, delivery);
    let Ok(request) = request else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    run_submit_input(
        writer,
        version,
        request_id,
        session_id,
        request,
        repository,
        eligibility_nudge,
        tool_dispatch_gate,
        model_configuration,
    )
    .await
}

/// Reconciles the exact active turn parked on an ambiguous model call.
///
/// The parked turn's terminal disposition is proof-bearing, so the user
/// supplies the interrupt authority the accepted lifecycle already defines and
/// the successor input the session continues with. The narrow precondition read
/// keeps this verb from becoming a general active-turn cancellation surface;
/// the authoritative transaction still revalidates the exact expected active
/// turn under the session lock.
#[expect(
    clippy::too_many_arguments,
    reason = "the closed reconciliation request is kept explicit at this wire-to-application adapter"
)]
async fn handle_reconcile_turn<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    expected_active_turn_id: CanonicalUuid,
    content: InputContent,
    expected_defaults_version: CanonicalU64,
    model_settings: WireModelSettingsOverlay,
    pool: &PgPool,
    eligibility_nudge: &InProcessEligibilityNudge,
    tool_dispatch_gate: &InProcessToolDispatchGate,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let expected_active_turn = TurnId::from_uuid(expected_active_turn_id.into_uuid());
    let command_id = DurableCommandId::from_uuid(command_id);
    let repository = SubmitInputRepository::with_model_capabilities(
        pool.clone(),
        model_configuration.model_capability_catalog(),
    );
    // A command identity that already names durable intent must reach the
    // replay boundary unconditionally (INV-012): the first handling already
    // released the wait, so re-applying the current-state precondition would
    // answer a retry of a committed decision with a refusal instead of its
    // recorded result.
    let command_is_claimed = match repository.load(command_id).await {
        Ok(Some(_)) | Err(SubmitInputRepositoryError::DifferentCommandKind { .. }) => true,
        Ok(None) => false,
        Err(error) => {
            return write_submit_input_repository_error(
                writer, version, request_id, session_id, error,
            )
            .await;
        }
    };
    let Some(expected_version) =
        SessionConfigurationDefaultsVersion::try_from_u64(expected_defaults_version.value())
    else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(content) = admitted_user_content(content) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let model_settings = domain_model_settings_overlay(model_settings);
    if !command_is_claimed {
        match ProcessReadRepository::new(pool.clone())
            .model_call_recovery_precondition(session)
            .await
        {
            // An absent session is left to the authoritative transaction, whose
            // recorded `SessionNotFound` the wire contract promises.
            Ok(ProcessModelCallRecoveryPrecondition::SessionAbsent) => {}
            Ok(ProcessModelCallRecoveryPrecondition::Parked { turn })
                if turn == expected_active_turn => {}
            Ok(
                ProcessModelCallRecoveryPrecondition::NoParkedTurn
                | ProcessModelCallRecoveryPrecondition::Parked { .. },
            ) => {
                // The claim probe and this read are separate statements, so an
                // equal-identity request that overlapped ours can have released
                // the wait in between. Rechecking the claim before refusing
                // keeps the loser of that race on the replay boundary instead
                // of answering a committed decision with a refusal (INV-012).
                match repository.load(command_id).await {
                    Ok(Some(_)) | Err(SubmitInputRepositoryError::DifferentCommandKind { .. }) => {}
                    Ok(None) => {
                        return write_error(
                            writer,
                            version,
                            request_id,
                            ProtocolError::rejected(
                                RejectionDetail::TurnNotAwaitingReconciliation {
                                    session_id,
                                    turn_id: expected_active_turn_id,
                                },
                            ),
                        )
                        .await;
                    }
                    Err(error) => {
                        return write_submit_input_repository_error(
                            writer, version, request_id, session_id, error,
                        )
                        .await;
                    }
                }
            }
            Err(error) => {
                return write_process_read_error(
                    writer,
                    version,
                    request_id,
                    Some(session_id),
                    error,
                )
                .await;
            }
        }
    }
    let request = SubmitInputRequest::try_new(
        command_id,
        session,
        content,
        DeliveryRequest::Interrupt {
            expected_active_turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::with_model_settings(
                expected_version,
                ModelSelectionOverride::UseSessionDefault,
                model_settings,
            ),
        },
    );
    let Ok(request) = request else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    run_submit_input(
        writer,
        version,
        request_id,
        session_id,
        request,
        repository,
        eligibility_nudge,
        tool_dispatch_gate,
        model_configuration,
    )
    .await
}

const fn decode_descendant_scope(
    value: WireDescendantTerminationScope,
) -> DescendantTerminationScope {
    match value {
        WireDescendantTerminationScope::ParentAlone => DescendantTerminationScope::ParentAlone,
        WireDescendantTerminationScope::ParentAndDescendants => {
            DescendantTerminationScope::ParentAndDescendants
        }
    }
}

/// Stops the exact active turn through the accepted interrupt treatment.
///
/// The delivery is the `Interrupt` treatment the turn lifecycle already
/// defines: cancellation authority exists only as an applied interrupt bound
/// to an immediate successor (INV-029), so the stop carries the successor
/// content and no standalone cancellation command is introduced. The
/// authoritative transaction validates the expected active turn under the
/// session lock and records every typed refusal, so no precondition read runs
/// here.
#[expect(
    clippy::too_many_arguments,
    reason = "the closed stop request is kept explicit at this wire-to-application adapter"
)]
async fn handle_stop_turn<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    expected_active_turn_id: CanonicalUuid,
    content: InputContent,
    expected_defaults_version: CanonicalU64,
    descendant_scope: DescendantTerminationScope,
    model_settings: WireModelSettingsOverlay,
    pool: &PgPool,
    eligibility_nudge: &InProcessEligibilityNudge,
    tool_dispatch_gate: &InProcessToolDispatchGate,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let expected_active_turn = TurnId::from_uuid(expected_active_turn_id.into_uuid());
    let command_id = DurableCommandId::from_uuid(command_id);
    let repository = SubmitInputRepository::with_model_capabilities(
        pool.clone(),
        model_configuration.model_capability_catalog(),
    );
    let Some(expected_version) =
        SessionConfigurationDefaultsVersion::try_from_u64(expected_defaults_version.value())
    else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(content) = admitted_user_content(content) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let model_settings = domain_model_settings_overlay(model_settings);
    let request = SubmitInputRequest::try_new(
        command_id,
        session,
        content,
        DeliveryRequest::Interrupt {
            expected_active_turn,
            descendant_scope,
            configuration: PerInputConfigurationChoices::with_model_settings(
                expected_version,
                ModelSelectionOverride::UseSessionDefault,
                model_settings,
            ),
        },
    );
    let Ok(request) = request else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    run_submit_input(
        writer,
        version,
        request_id,
        session_id,
        request,
        repository,
        eligibility_nudge,
        tool_dispatch_gate,
        model_configuration,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "the shared submit-input execution keeps its wire and application collaborators explicit"
)]
async fn run_submit_input<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    request: SubmitInputRequest,
    repository: SubmitInputRepository,
    eligibility_nudge: &InProcessEligibilityNudge,
    tool_dispatch_gate: &InProcessToolDispatchGate,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut service = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        ConfiguredSubmitInputTransaction {
            repository,
            model_configuration,
        },
        eligibility_nudge.clone(),
        tool_dispatch_gate.clone(),
    );
    match service.execute(request).await {
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(result),
        ))) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::InputSubmitted {
                    session_id,
                    accepted_input_id: wire_uuid(result.accepted_input().into_uuid()),
                    acceptance_position: CanonicalU64::new(result.acceptance_position().as_u64()),
                    turn_id: wire_uuid(result.turn().into_uuid()),
                    model_settings: wire_model_settings(
                        result.origin_configuration().effective().model_settings(),
                    ),
                },
            )
            .await
        }
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(rejected))) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::rejected(map_rejection(rejected)?),
            )
            .await
        }
        Ok(SubmitInputOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(result),
        ))) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SteeringSubmitted {
                    session_id,
                    accepted_input_id: wire_uuid(result.accepted_input().into_uuid()),
                    acceptance_position: CanonicalU64::new(result.acceptance_position().as_u64()),
                    source_turn_id: wire_uuid(result.binding().source_turn().into_uuid()),
                },
            )
            .await
        }
        Err(error) => {
            write_submit_input_repository_error(writer, version, request_id, session_id, error)
                .await
        }
    }
}

/// Closed submit-input disposition for one model-execution repository error.
///
/// The mapping retains the exact typed variant before its source is erased.
/// No database detail, transition label, or corruption payload enters the
/// diagnostic, so credentials and caller or model content remain excluded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SubmitInputModelExecutionDiagnostic {
    DatabaseUnavailable,
    CommitAmbiguous,
    Internal(InternalDiagnostic),
}

impl SubmitInputModelExecutionDiagnostic {
    fn into_protocol_error(self, session_id: CanonicalUuid) -> ProtocolError {
        match self {
            Self::DatabaseUnavailable => ProtocolError::mutation_unavailable(false),
            Self::CommitAmbiguous => ProtocolError::mutation_unavailable(true),
            Self::Internal(diagnostic) => {
                internal_protocol_error(Some(session_id.into_uuid()), diagnostic)
            }
        }
    }
}

fn submit_input_model_execution_diagnostic(
    error: &signalbox_persistence::model_execution::ModelCallRepositoryError,
) -> SubmitInputModelExecutionDiagnostic {
    use signalbox_persistence::model_execution::ModelCallRepositoryError;

    match error {
        ModelCallRepositoryError::Database {
            commit_ambiguous, ..
        } => match commit_ambiguous {
            true => SubmitInputModelExecutionDiagnostic::CommitAmbiguous,
            false => SubmitInputModelExecutionDiagnostic::DatabaseUnavailable,
        },
        ModelCallRepositoryError::Corruption(_) => SubmitInputModelExecutionDiagnostic::Internal(
            InternalDiagnostic::SubmitInputModelExecutionCorruption,
        ),
        ModelCallRepositoryError::IdentityCollision(_) => {
            SubmitInputModelExecutionDiagnostic::Internal(
                InternalDiagnostic::SubmitInputModelExecutionIdentityCollision,
            )
        }
        ModelCallRepositoryError::NoLiveExecution => SubmitInputModelExecutionDiagnostic::Internal(
            InternalDiagnostic::SubmitInputModelExecutionNoLiveExecution,
        ),
        ModelCallRepositoryError::InvalidTransition(_) => {
            SubmitInputModelExecutionDiagnostic::Internal(
                InternalDiagnostic::SubmitInputModelExecutionInvalidTransition,
            )
        }
    }
}

async fn write_submit_input_repository_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    error: SubmitInputRepositoryError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let protocol_error = match error {
        SubmitInputRepositoryError::Database(_) => ProtocolError::mutation_unavailable(false),
        SubmitInputRepositoryError::CommitAmbiguous(_) => ProtocolError::mutation_unavailable(true),
        SubmitInputRepositoryError::ModelExecution(error) => {
            submit_input_model_execution_diagnostic(error.as_ref()).into_protocol_error(session_id)
        }
        SubmitInputRepositoryError::DifferentCommandKind { .. } => internal_protocol_error(
            Some(session_id.into_uuid()),
            InternalDiagnostic::SubmitInputCommandKindMismatch,
        ),
        SubmitInputRepositoryError::AcceptedInputIdentityCollision { .. } => {
            internal_protocol_error(
                Some(session_id.into_uuid()),
                InternalDiagnostic::SubmitInputIdentityCollision,
            )
        }
        SubmitInputRepositoryError::UnsupportedModelSetting(error) => {
            ProtocolError::rejected(wire_unsupported_model_setting(error))
        }
        SubmitInputRepositoryError::Corruption(_) => internal_protocol_error(
            Some(session_id.into_uuid()),
            InternalDiagnostic::SubmitInputCorruption,
        ),
    };
    write_error(writer, version, request_id, protocol_error).await
}

/// Converts imported-session repository evidence into one typed Internal diagnostic.
fn imported_session_internal_diagnostic(
    error: &ImportedSessionRepositoryError,
) -> InternalDiagnostic {
    match error {
        ImportedSessionRepositoryError::Database(_) => InternalDiagnostic::ImportedSessionDatabase,
        ImportedSessionRepositoryError::CommitAmbiguous(_) => {
            InternalDiagnostic::ImportedSessionCommitAmbiguous
        }
        ImportedSessionRepositoryError::DifferentCommandKind { .. } => {
            InternalDiagnostic::ImportedSessionCommandKindMismatch
        }
        ImportedSessionRepositoryError::Preparation(_) => {
            InternalDiagnostic::ImportedSessionPreparation
        }
        ImportedSessionRepositoryError::IdentityCollision(_) => {
            InternalDiagnostic::ImportedSessionIdentityCollision
        }
        ImportedSessionRepositoryError::Corruption(_) => {
            InternalDiagnostic::ImportedSessionCorruption
        }
    }
}

/// Converts imported-conversation evidence without formatting its payload.
fn imported_conversation_internal_diagnostic(
    error: &ImportedConversationRepositoryError,
) -> InternalDiagnostic {
    match error {
        ImportedConversationRepositoryError::Database(_) => {
            InternalDiagnostic::ImportedConversationDatabase
        }
        ImportedConversationRepositoryError::IdentityCollision(_) => {
            InternalDiagnostic::ImportedConversationIdentityCollision
        }
        ImportedConversationRepositoryError::Corruption(_) => {
            InternalDiagnostic::ImportedConversationCorruption
        }
    }
}

/// Converts create-session evidence without formatting command or database detail.
fn create_session_internal_diagnostic(
    error: &CreateSessionError<CreateSessionRepositoryError>,
) -> InternalDiagnostic {
    match error {
        CreateSessionError::Preparation(_) => InternalDiagnostic::SessionCreationPreparation,
        CreateSessionError::Transaction(CreateSessionRepositoryError::Database(_)) => {
            InternalDiagnostic::SessionCreationDatabase
        }
        CreateSessionError::Transaction(CreateSessionRepositoryError::CommitAmbiguous(_)) => {
            InternalDiagnostic::SessionCreationCommitAmbiguous
        }
        CreateSessionError::Transaction(CreateSessionRepositoryError::DifferentCommandKind {
            ..
        }) => InternalDiagnostic::SessionCreationCommandKindMismatch,
        CreateSessionError::Transaction(CreateSessionRepositoryError::Corruption(_)) => {
            InternalDiagnostic::SessionCreationCorruption
        }
    }
}

/// Converts metadata evidence without formatting command or durable content.
fn session_metadata_internal_diagnostic(
    error: &SessionMetadataRepositoryError,
) -> InternalDiagnostic {
    match error {
        SessionMetadataRepositoryError::Database(_) => InternalDiagnostic::SessionMetadataDatabase,
        SessionMetadataRepositoryError::CommitAmbiguous(_) => {
            InternalDiagnostic::SessionMetadataCommitAmbiguous
        }
        SessionMetadataRepositoryError::DifferentCommandKind { .. } => {
            InternalDiagnostic::SessionMetadataCommandKindMismatch
        }
        SessionMetadataRepositoryError::Corruption(_) => {
            InternalDiagnostic::SessionMetadataCorruption
        }
    }
}

/// Converts defaults-replacement evidence into one typed Internal diagnostic.
fn session_defaults_internal_diagnostic(
    error: &ReplaceSessionDefaultsRepositoryError,
) -> InternalDiagnostic {
    match error {
        ReplaceSessionDefaultsRepositoryError::Database {
            commit_ambiguous: false,
            ..
        } => InternalDiagnostic::SessionDefaultsDatabase,
        ReplaceSessionDefaultsRepositoryError::Database {
            commit_ambiguous: true,
            ..
        } => InternalDiagnostic::SessionDefaultsCommitAmbiguous,
        ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { .. } => {
            InternalDiagnostic::SessionDefaultsCommandKindMismatch
        }
        ReplaceSessionDefaultsRepositoryError::Corruption(_) => {
            InternalDiagnostic::SessionDefaultsCorruption
        }
    }
}

/// Records one user tool decision through the canonical decision command.
///
/// A claimed command identity reaches the durable replay boundary
/// unconditionally (INV-012). Otherwise a narrow read refuses, before any
/// command is recorded, a decision whose named session does not own the named
/// request; an absent request is left to the transaction's recorded
/// `request_not_found`, and every other outcome is the recorded result of the
/// canonical command.
#[expect(
    clippy::too_many_arguments,
    reason = "the closed decision request is kept explicit at this wire-to-application adapter"
)]
async fn handle_decide_tool_request<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    decision: ToolDecision,
    pool: &PgPool,
    eligibility_nudge: &InProcessEligibilityNudge,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let request = ToolRequestId::from_uuid(tool_request_id.into_uuid());
    let command_id = DurableCommandId::from_uuid(command_id);
    let domain_decision = match decision {
        ToolDecision::Approve {} => ToolApprovalDecision::Approve,
        ToolDecision::Deny { reason } => match ToolDenialReason::try_new(reason) {
            Ok(reason) => ToolApprovalDecision::Deny {
                reason: Some(reason),
            },
            Err(_) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::InvalidRequest),
                )
                .await;
            }
        },
    };
    let Ok(command) = DecideToolRequest::try_new(command_id, request, domain_decision) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let command_is_claimed = match repository.load_recorded_decision(command_id).await {
        Ok(Some(_)) | Err(ToolLoopRepositoryError::DifferentCommandKind) => true,
        Ok(None) => false,
        Err(error) => {
            return write_tool_loop_error(writer, version, request_id, session_id, error).await;
        }
    };
    if !command_is_claimed {
        match ProcessReadRepository::new(pool.clone())
            .tool_request_session(request)
            .await
        {
            // An absent request is left to the authoritative transaction,
            // whose recorded `request_not_found` rejection the wire
            // contract promises.
            Ok(None) => {}
            Ok(Some(owning_session)) if owning_session == session => {}
            Ok(Some(_)) => {
                // The claim probe and this read are separate statements, so an
                // equal-identity request that overlapped ours can have
                // recorded the decision in between. Rechecking the claim
                // before refusing keeps the loser of that race on the replay
                // boundary instead of answering a committed decision with a
                // refusal (INV-012).
                match repository.load_recorded_decision(command_id).await {
                    Ok(Some(_)) | Err(ToolLoopRepositoryError::DifferentCommandKind) => {}
                    Ok(None) => {
                        return write_error(
                            writer,
                            version,
                            request_id,
                            ProtocolError::rejected(RejectionDetail::ToolRequestNotInSession {
                                session_id,
                                tool_request_id,
                            }),
                        )
                        .await;
                    }
                    Err(error) => {
                        return write_tool_loop_error(
                            writer, version, request_id, session_id, error,
                        )
                        .await;
                    }
                }
            }
            Err(error) => {
                return write_process_read_error(
                    writer,
                    version,
                    request_id,
                    Some(session_id),
                    error,
                )
                .await;
            }
        }
    }
    let mut service = DecideToolRequestService::new(UuidV7ToolLoopIdGenerator, repository);
    match service.execute(command).await {
        Ok(prepared) => match prepared.result() {
            DecideToolRequestResult::Applied(applied) => {
                // An applied decision can open the executing phase; the nudge
                // lets the scheduler resume the tool round promptly, and the
                // durable sweep remains the backstop.
                let _ = eligibility_nudge.nudge(session);
                write_message(
                    writer,
                    version,
                    request_id,
                    ServerMessage::ToolRequestDecided {
                        tool_request_id,
                        decision: wire_tool_decision(applied.resolution().decision())?,
                    },
                )
                .await
            }
            DecideToolRequestResult::Rejected(rejected) => {
                let detail = match *rejected {
                    DecideToolRequestRejectedResult::RequestNotFound { request } => {
                        RejectionDetail::ToolRequestNotFound {
                            tool_request_id: wire_uuid(request.into_uuid()),
                        }
                    }
                    DecideToolRequestRejectedResult::AlreadyResolved { request } => {
                        RejectionDetail::ToolRequestAlreadyResolved {
                            tool_request_id: wire_uuid(request.into_uuid()),
                        }
                    }
                    DecideToolRequestRejectedResult::NotEarliestUndecided { request, earliest } => {
                        RejectionDetail::ToolRequestNotEarliestUndecided {
                            tool_request_id: wire_uuid(request.into_uuid()),
                            earliest_tool_request_id: wire_uuid(earliest.into_uuid()),
                        }
                    }
                };
                write_error(writer, version, request_id, ProtocolError::rejected(detail)).await
            }
        },
        Err(error) => write_tool_loop_error(writer, version, request_id, session_id, error).await,
    }
}

fn wire_tool_decision(
    decision: &ToolApprovalDecision,
) -> Result<ToolDecision, ProcessConnectionError> {
    match decision {
        ToolApprovalDecision::Approve => Ok(ToolDecision::Approve {}),
        ToolApprovalDecision::Deny {
            reason: Some(reason),
        } => Ok(ToolDecision::Deny {
            reason: reason.as_str().to_owned(),
        }),
        // The wire surface requires a denial reason, so every
        // decision it records carries one; a reason-free denial cannot be
        // projected as this receipt.
        ToolApprovalDecision::Deny { reason: None } => Err(ProcessConnectionError::EncodeInvariant),
    }
}

async fn write_tool_loop_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    error: ToolLoopRepositoryError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let protocol_error = match error {
        ToolLoopRepositoryError::Database {
            commit_ambiguous, ..
        } => ProtocolError::mutation_unavailable(commit_ambiguous),
        // Any difference under a claimed identity — including a different
        // command kind — is conflicting reuse, per the identity-and-commands
        // registry contract.
        ToolLoopRepositoryError::ConflictingCommandReuse
        | ToolLoopRepositoryError::DifferentCommandKind => {
            ProtocolError::without_detail(ErrorCode::ConflictingReuse)
        }
        ToolLoopRepositoryError::IdentityCollision => internal_protocol_error(
            Some(session_id.into_uuid()),
            InternalDiagnostic::ToolLoopIdentityCollision,
        ),
        ToolLoopRepositoryError::Corruption(_) => internal_protocol_error(
            Some(session_id.into_uuid()),
            InternalDiagnostic::ToolLoopCorruption,
        ),
        ToolLoopRepositoryError::InvalidTransition(_) => internal_protocol_error(
            Some(session_id.into_uuid()),
            InternalDiagnostic::ToolLoopInvalidTransition,
        ),
    };
    write_error(writer, version, request_id, protocol_error).await
}

fn admitted_user_content(content: InputContent) -> Result<UserContent, ()> {
    let content = content.into_string();
    if content.len() > MAX_SUBMITTED_INPUT_BYTES {
        return Err(());
    }
    UserContent::try_text(content).map_err(|_| ())
}

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
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptTextEntry::User {
                        accepted_input_id: wire_uuid(accepted_input.into_uuid()),
                        turn_id: wire_uuid(turn.into_uuid()),
                    },
                },
            )
            .await?;
            write_content(writer, version, request_id, *entry_index, content).await
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
) -> Result<Option<signalbox_domain::SessionSystemPrompt>, ()> {
    match member.value() {
        None | Some(None) => Ok(None),
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
            content: InputContent::new(content.clone()),
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
        } => TurnState::ActiveAwaitingModelCallRecovery {
            ended_attempt_id: wire_uuid(ended_attempt.into_uuid()),
            recovery_model_call_id: wire_uuid(recovery_call.into_uuid()),
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
        } => TurnState::ActiveAwaitingToolRecovery {
            ended_attempt_id: wire_uuid(ended_attempt.into_uuid()),
            recovery_tool_attempt_id: wire_uuid(recovery_attempt.into_uuid()),
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
                        match call.provider_failure_cause() {
                            Some(cause) => FailedTerminalModelCall::known_failed_with_cause(
                                model_call_id,
                                wire_provider_failure_cause(cause),
                            ),
                            None => FailedTerminalModelCall::new(
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
    GoalRepositoryCorruption,
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
            Self::ReviewWorkflowProjectionCorruption
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
            | Self::SessionCreationCorruption
            | Self::ConversationListingCorruption
            | Self::SessionMetadataCorruption
            | Self::SessionDefaultsCorruption
            | Self::SessionDelegationCorruption
            | Self::SubmitInputCorruption
            | Self::SubmitInputModelExecutionCorruption
            | Self::ToolLoopCorruption
            | Self::ProcessReadCorruption
            | Self::GoalRepositoryCorruption => OperatorFailureClass::FailClosedCorruption,
        }
    }

    const fn cause_code(self) -> &'static str {
        match self {
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
            Self::GoalRepositoryCorruption => "goal_repository_corruption",
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
    }
}

fn wire_goal_blocked_provenance(value: GoalBlockProvenance) -> WireGoalBlockedProvenance {
    match value {
        GoalBlockProvenance::Model { provenance, .. } => WireGoalBlockedProvenance::Model {
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
    }
}

const fn wire_goal_command_rejection(
    value: DomainGoalCommandRejection,
) -> WireGoalCommandRejection {
    match value {
        DomainGoalCommandRejection::SessionNotFound => WireGoalCommandRejection::SessionNotFound,
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
            session: event.session(),
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
        content: String,
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
            DispatchedOutboxEventKind::SessionCreated => Self::SessionCreated,
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
            DispatchedOutboxEventKind::GoalTurnRetired { turn } => {
                Self::GoalTurnRetired { turn: *turn }
            }
            DispatchedOutboxEventKind::TurnActivated {
                turn,
                current_attempt,
            } => Self::TurnActivated {
                turn: *turn,
                current_attempt: *current_attempt,
            },
            DispatchedOutboxEventKind::TurnFailed {
                turn,
                failure_entry,
                terminal_frontier,
            } => Self::TurnFailed {
                turn: *turn,
                failure_entry: *failure_entry,
                terminal_frontier: *terminal_frontier,
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
            DispatchedOutboxEventKind::TurnCompleted {
                turn,
                call,
                completion_entry,
                terminal_frontier,
            } => Self::TurnCompleted {
                turn: *turn,
                call: *call,
                completion_entry: *completion_entry,
                terminal_frontier: *terminal_frontier,
            },
            DispatchedOutboxEventKind::TurnRefused {
                turn,
                call,
                terminal_frontier,
            } => Self::TurnRefused {
                turn: *turn,
                call: *call,
                terminal_frontier: *terminal_frontier,
            },
            DispatchedOutboxEventKind::TurnCancelled {
                turn,
                cancellation_entry,
                terminal_frontier,
            } => Self::TurnCancelled {
                turn: *turn,
                cancellation_entry: *cancellation_entry,
                terminal_frontier: *terminal_frontier,
            },
            DispatchedOutboxEventKind::TurnReconciliationRequired {
                turn,
                operation,
                terminal_frontier,
            } => Self::TurnReconciliationRequired {
                turn: *turn,
                operation: *operation,
                terminal_frontier: *terminal_frontier,
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
                content: InputContent::new(content.clone()),
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
        SessionInputPosition, SessionMetadataLastWriter, SessionMetadataUpdatedAt,
        SessionModelSettingsChanged, SettingOverlay, SubmitInputRejectedResult,
        ToolApprovalDecision, ToolAttemptId, ToolRequestId, TurnAttemptId, TurnId,
        TurnModelSettingsResolved, ValidatedModelSettings,
    };
    use signalbox_process_protocol::{
        CanonicalU64, CanonicalUuid, ClientRequest, CommandId, ConversationImportRejectionClass,
        DelegationToolRequestState as WireDelegationToolRequestState, ErrorCode, ErrorDetail,
        FrameEncodeError, GoalLifecycleState, ImportedContentKind, ImportedSourceSpeaker,
        ImportedSpeaker, InputContent, MAX_CONTENT_FRAGMENT_BYTES, MetadataActor, ProtocolVersion,
        RejectionDetail, ReviewFindingInput, ReviewSeverity,
        RunnerPlacementRevision as WireRunnerPlacementRevision,
        RunnerSandboxProfile as WireRunnerSandboxProfile,
        RunnerStateTransitionState as WireRunnerStateTransitionState,
        RunnerWorkingDirectory as WireRunnerWorkingDirectory, ServerFrame, ServerMessage,
        SessionEvent, ToolBatchState, ToolDecision, TranscriptEntry, TranscriptTextEntry,
        TurnState, decode_server_line, encode_server_line,
    };
    use sqlx::postgres::PgPoolOptions;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt, BufReader, duplex},
        sync::{Semaphore, watch},
        time::{Duration, Instant, timeout},
    };
    use uuid::Uuid;

    use super::{
        CommittedForegroundDelivery, ContextCompactionRangeLoadError, ConversationImportState,
        ConversionFailureDisposition, GENERAL_BUFFERED_INBOUND_FRAMES, INBOUND_READ_AHEAD_BYTES,
        ImportedConversationRepositoryError, InboundFrameBudgets, IncomingLine, InternalDiagnostic,
        MAX_ACTIVE_CONNECTIONS, MAX_BUFFERED_INBOUND_FRAMES, MAX_CONCURRENT_IMPORTS,
        MAX_CONCURRENT_REVIEW_COMMANDS, MAX_FRAME_BYTES, MAX_IMPORT_ADMISSION_WAITERS,
        MAX_SUBMITTED_INPUT_BYTES, OperationalImportError, PendingConversationImport,
        ProcessConnectionError, ProcessRuntimeError, ProcessUpdateEvent, ProtocolError,
        RESERVED_ACTIVE_IMPORT_INBOUND_FRAMES, RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS,
        RequestId, ReviewCommandAdmission, SnapshotReaderAdmission, SnapshotSpoolError,
        SubmitInputModelExecutionDiagnostic, acquire_import_permit, acquire_import_waiter_permit,
        acquire_inbound_frame_permit, acquire_inbound_frame_permit_after_input,
        acquire_review_command_permit, acquire_review_command_permit_while_buffered,
        acquire_snapshot_reader_permit, admit_snapshot_reader, admitted_user_content,
        blob_upload_begin_preflight, canonical_review_request_digest,
        claude_conversion_failure_disposition, codex_conversion_failure_disposition,
        consume_snapshot_queued_update, context_compaction_failure_disposition, execute_import,
        foreground_peer_activity, handle_append_conversation_import,
        handle_begin_conversation_import, handle_commit_conversation_import, import_evidence,
        imported_conversation_internal_diagnostic, inspect_connection_completion,
        internal_protocol_error, map_rejection, nudge_after_process_await_rejection,
        nudge_after_process_message_rejection, nudge_delegation_issuer, nudge_delegation_wake,
        observe_outbox_metrics_once, operational_import_error, preserve_committed_foreground_wait,
        process_delegation_rejection, process_delegation_rejection_for_recipient, read_frame_line,
        retain_inbound_frame_permit_during_import_admission,
        retry_context_compaction_range_database_reads, run_until_shutdown,
        snapshot_reader_capacity, spool_error_display, spool_goal_snapshot,
        submit_input_model_execution_diagnostic, unavailable_protocol_error, wire_goal_event,
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
            content: "synthetic prompt with tool arguments".to_owned(),
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
        assert_eq!(snapshot_reader_capacity(3), Some(1));
        assert!(snapshot_reader_capacity(2).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_reader_budget_reserves_two_pool_connections() -> Result<(), Box<dyn Error>> {
        let max_pool_connections = 10;
        let capacity = snapshot_reader_capacity(max_pool_connections)
            .ok_or_else(|| io::Error::other("the production pool must admit snapshot readers"))?;
        assert_eq!(
            capacity,
            usize::try_from(max_pool_connections - RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS)?
        );
        assert!(snapshot_reader_capacity(2).is_none());

        let budget = Arc::new(Semaphore::new(capacity));
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let mut permits = Vec::new();
        for _ in 0..capacity {
            permits.push(
                acquire_snapshot_reader_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
                    .await?
                    .ok_or_else(|| io::Error::other("the running fixture must acquire a permit"))?,
            );
        }
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
        Ok(())
    }

    /// The wire vocabulary as text. The review read verbs are enumerated from
    /// the protocol itself so a later one cannot be admitted by a list here
    /// staying silent about it.
    const WIRE_VOCABULARY: &str = include_str!("../../../crates/process-protocol/src/lib.rs");

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
        let (mut writer, mut reader) = duplex(1);
        let write = tokio::spawn(async move {
            let result = handle_commit_conversation_import(
                &mut writer,
                ProtocolVersion::One,
                request_id,
                limit,
                &pool,
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
        let (mut writer, mut reader) = duplex(1_024);

        handle_commit_conversation_import(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            limit,
            &pool,
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

        let outcome = execute_import(
            ThreadReportingRejectConverter(thread_sender),
            Vec::new(),
            pool,
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

        let outcome = execute_import(PanickingConverter, Vec::new(), pool).await;

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
        let exact = InputContent::new("\u{1}".repeat(MAX_SUBMITTED_INPUT_BYTES));
        assert!(admitted_user_content(exact).is_ok());
    }

    #[test]
    fn process_submission_rejects_content_over_the_bound() {
        assert!(
            admitted_user_content(InputContent::new("x".repeat(MAX_SUBMITTED_INPUT_BYTES + 1)))
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
                    content: InputContent::new("\u{1}".repeat(MAX_SUBMITTED_INPUT_BYTES)),
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
                    content: InputContent::new("\u{1}".repeat(MAX_SUBMITTED_INPUT_BYTES)),
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
        let update =
            ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::GoalTurnRetired { turn })
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

        let cancelled =
            ProcessUpdateEvent::from_outbox(&DispatchedOutboxEventKind::TurnCancelled {
                turn,
                cancellation_entry: entry,
                terminal_frontier: frontier,
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
        let reconciliation = ProcessUpdateEvent::from_outbox(
            &DispatchedOutboxEventKind::TurnReconciliationRequired {
                turn,
                operation: DispatchedReconciliationOperation::ModelCall(call),
                terminal_frontier: frontier,
            },
        )
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
}
