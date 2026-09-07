//! Process-protocol runtime coverage.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

#[path = "../support/mod.rs"]
mod support;

use std::{
    collections::{HashSet, VecDeque},
    error::Error,
    fs,
    future::{Future, pending},
    io::{self, ErrorKind},
    os::unix::fs::PermissionsExt,
    panic::{AssertUnwindSafe, resume_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::FutureExt;
use signalbox_application::{
    AuthorizeModelCallOutcome, ClassifyOperatorFailure,
    CreateSessionFromImportedFrontierIdGenerator, CreateSessionFromImportedFrontierOutcome,
    CreateSessionFromImportedFrontierRequest, CreateSessionFromImportedFrontierService,
    EligibilityPass, EligibilitySweep, EligibilitySweepBatch, ImportConversationOutcome,
    ImportConversationService, ImportedConversationIdGenerator, InProcessAttemptDispatchGate,
    InProcessEligibilityNudge, InProcessEligibilityWorkSource, InProcessToolDispatchGate,
    ModelCallCredentialReference, ModelCallExecutionOutcome, ModelCallExecutionService,
    ModelCallInputTokenCount, ModelCallInputTokenCounter, NoToolCatalog, OperatorFailureClass,
    PreparedModelOperation, ReplaceSessionMetadataOutcome, ReplaceSessionMetadataRequest,
    ReplaceSessionMetadataService, SchedulerLoop, SchedulerLoopExit, SchedulerPassExpiryHandler,
    SchedulerPassOccupancyBound, ScriptedModelCallProvider, ScriptedModelCallStep,
    StaleActiveTurnBound, StartEligibleTurnOutcome, StartEligibleTurnService, StartupScanService,
    TurnLivenessScanInterval, UuidV7ModelCallExecutionIdGenerator,
    UuidV7StartEligibleTurnIdGenerator, UuidV7StartupScanIdGenerator,
};
use signalbox_blob_store::{
    BlobObjectKey, BlobPutOutcome, BlobReader, BlobStore, BlobStoreError, BlobStoreFuture,
    ExpectedBlob, OpenedBlob,
};
use signalbox_conversation_import_claude_code::ClaudeCodeJsonlConverter;
use signalbox_domain::{
    ActiveTurnPhase, Actor, AssistantResponsePart, AssistantText, BlobDigest, ContextCompactionId,
    ContextCompactionTokenUsage, ContextFrontierId, DirectModelSelection, DurableCommandId,
    FailedModelCallTurnIdentities, ImportedConversationFormat, ImportedConversationId,
    ImportedSessionRelationship, ImportedTranscriptEntryId, InitialToolApproval, ModelCallId,
    ModelCallTerminalIdentities, ModelCallTerminalObservation, ModelCallTerminalOutcome,
    ModelSelectionRequest, ModelTargetCatalog, NormalizedToolArguments,
    PhysicalCancellationModelCallTurnIdentities, ProviderModelIdentity,
    ReplaceSessionMetadataResult, ResolvedProviderTarget, SemanticTranscriptEntryId,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionId,
    SessionMetadataContent, ToolCallProposal, ToolName, ToolRequestId, ToolResponsePartIdentity,
    ToolRoundModelCallIdentities, ToolUsingAssistantResponse, TurnId,
};
use signalbox_model_provider_runtime::{
    RuntimeContextCompactionModel, RuntimeInputTokenCountError, RuntimeModelCallProvider,
    RuntimeModelCallProviderError,
};
use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CancellationSignal, CompletionEvidence, CompletionFinish,
    DeliveryMode, ExchangeFacts, InputTokenCountOutcome, LossCause, MessagePart,
    ModelInputTokenCounter, ModelOperation, ModelRuntime, NativeErrorFacts, Observation,
    ObservationFact, ObservationSink, PreparationOutcome, ProviderErrorEvidence, ProviderErrorKind,
    ProviderReportedModel, Script, ScriptedModel, ScriptedPrepared, TerminalEvidence,
    TerminalReport, TokenUsage, ToolCallsAtLoss,
};
use signalbox_persistence::{
    blob::BlobCatalogRepository,
    context_compaction::{
        ContextCompactionCorruption, ContextCompactionRepository, ContextCompactionRepositoryError,
        FailedContextCompactionDisposition, PrepareContextCompactionOutcome,
        PrepareContextCompactionRequest,
    },
    conversation_import::ImportedConversationRepository,
    create_session_from_imported_frontier::ImportedSessionRepository,
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
    model_execution::{PostgresModelCallRepository, PrepareInitialModelCallOutcome},
    scheduler::PostgresEligibilitySweep,
    session_metadata::SessionMetadataRepository,
    start_eligible_turn::StartEligibleTurnRepository,
    startup::PostgresStartupScanRepository,
    test_support::{FleetSoakCensus, FleetSoakCensusRepository},
    turn_liveness::TurnLivenessPersistenceBounds,
};
use signalbox_process_protocol::{
    BlobChunk, CanonicalBlobDigest, CanonicalDigest, CanonicalU64, CanonicalUuid, ClientFrame,
    ClientRequest, CommandId, CommissionedSessionFence, ConversationImportFormat,
    ConversationImportSource, ConversationOriginFilter, ConversationSummary, CurrentModelCallState,
    DescendantTerminationScope, EffectiveModelSettings, ErrorCode, ErrorDetail, FastMode,
    GoalHistoryEvent, GoalLifecycleState, ImportedContentKind, ImportedConversationSourceFormat,
    ImportedSourceSpeaker, ImportedSpeaker, ImportedTextPreview, InputContent, InputDelivery,
    MAX_SESSION_METADATA_INDEXED_UTF8_BYTES, MetadataActor, ModelChangeAdjustment, ModelSelection,
    ModelSettingSource, ModelSettingsOverlay, ModelSettingsPrecedence, ModelSettingsSnapshot,
    OperatorStatusEndMessage, OperatorStatusMessage, ProtocolVersion, ReasoningLevel,
    RejectionDetail, RequestId, ReviewConcernTerminalOutcome, ReviewDiffSide,
    ReviewExternalObjectKind, ReviewFindingEvent, ReviewFindingInput, ReviewFindingStatus,
    ReviewImportTerminalOutcome, ReviewJudgmentDisposition, ReviewJudgmentEffectTerminalOutcome,
    ReviewJudgmentPlanMember, ReviewOrchestrationConcernInput, ReviewOrchestrationConcernStatus,
    ReviewOrchestrationCounts, ReviewOrchestrationSnapshot, ReviewOrchestrationState,
    ReviewPassTerminalOutcome, ReviewPublicationOutcome, ReviewPublicationTerminalOutcome,
    ReviewRepairOutcome, ReviewRepairTerminalOutcome, ReviewSeverity, ReviewTargetSubject,
    ReviewWorkflow, ServerFrame, ServerMessage, SessionEvent, SessionLifecycleEffect,
    SessionMetadata, SessionPlacement, SettingOverlay, SystemPromptMember, SystemPromptText,
    ToolDecision, TranscriptEntry, TranscriptTextEntry, TurnState, UserAttachmentKind,
    UserInputContent, UserInputPart, decode_server_line, encode_client_line,
};
use signalboxd::{
    ActivatedTurnPass, AttachmentPreparingModelCallProvider, BlobStorageClass, BlobStoreRegistry,
    ContextGuardedTurnPass, ContextGuardedTurnPassError, ExpiredPassRecoveryPolicy,
    FatalExecutionSupervisor, HubModelConfiguration, LocalProcessListener,
    PostgresProviderModelExecution, ProcessProviderTextDeltaSink, ProcessRuntime,
    ProcessRuntimeError, ReportedUsageCompaction, ReportedUsageCompactionError,
    SessionTemplateConfiguration, TurnLivenessNumericBounds, TurnLivenessRuntime,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tempfile::TempDir;
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::watch,
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;

mod blob_objects;
mod compaction;
mod credential_exclusions;
mod credential_pool;
mod fixtures;
mod fleet_soak;
mod imported_conversations;
mod input_admission;
mod oauth;
mod program;
mod reconciliation;
mod review_orchestration;
mod session_configuration;
mod session_metadata;
mod spawn_session;
mod stop_turn;
mod streaming;
mod tool_decisions;

use blob_objects::*;
use compaction::*;
use fixtures::*;
use input_admission::*;
use reconciliation::*;
use stop_turn::*;
