//! Terminal client for the closed local Signalbox process protocol.

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use arguments::{
    Command, DangerousToolAutoApprovalArgument, DelegationTextArgument, GoalCommand,
    GoalTextArgument, ImportSourceArgument, ParseOutcome, ReviewCommand, SendDeliveryArgument,
    SessionCommand, SystemPromptArgument, ThroughPositionArgument,
};
use connection::ProcessClient;
use error::ClientError;
use presentation::{
    BlobUploadPresentation, ChildResultPresentation, ConversationRow, ImportedEntryRow,
    OperatorStatusPresentationCounts, Output, SessionAwaitRegisteredPresentation,
    SessionMessageSentPresentation, SessionMetadataRow, SessionSpawnedPresentation,
    SnapshotSelection,
};
use rustix::{
    fd::OwnedFd,
    fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, fchmod, fstat, openat, statat},
};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use signalbox_process_protocol::{
    BlobChunk, CanonicalBlobDigest, CanonicalU64, CanonicalUuid, ClientFrame, ClientRequest,
    CommandId, ConversationCursor, ConversationImportFormat, ConversationImportSource,
    ConversationOrigin, ConversationOriginFilter, ConversationSummary, DelegationMessageDirection,
    DelegationOutcome, DelegationPolicy, DelegationProvenance, DelegationReason,
    DelegationWaitMode, DescendantTerminationScope, ErrorCode, ErrorDetail, FrameEncodeError,
    GoalHistoryEvent, GoalLifecycleState, InputContent, InputDelivery, MAX_BLOB_CHUNK_BYTES,
    MAX_BLOB_READ_BYTES, MAX_CONTENT_FRAGMENT_BYTES, MAX_CONVERSATION_IMPORT_CHUNK_BYTES,
    MAX_FRAME_BYTES, ModelCallDisposition, ModelCallState, ModelSelection, ModelSettingsOverlay,
    OperatorStatusMessage, ProtocolVersion, RejectionDetail, RequestId,
    ReviewConcernTerminalOutcome, ReviewFindingEvent, ReviewFindingInput, ReviewFindingStatus,
    ReviewImportTerminalOutcome, ReviewJudgmentEffectTerminalOutcome, ReviewJudgmentPlanMember,
    ReviewOrchestrationConcernInput, ReviewOrchestrationState, ReviewPassLifecycle,
    ReviewPassSnapshot, ReviewPassTerminalOutcome, ReviewPublicationOutcome,
    ReviewPublicationTerminalOutcome, ReviewRepairOutcome, ReviewRepairTerminalOutcome,
    ReviewRunSnapshot, RunnerConnectionHealth, RunnerProjection, RunnerProjectionState,
    RunnerStateTransitionState, ServerFrame, ServerMessage, SessionEvent, SessionLifecycleMembers,
    SessionPlacement, SystemPromptMember, SystemPromptText, ToolBatchState, ToolDecision,
    TurnState, decode_server_line, encode_client_line, encode_server_line,
};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use transcript::{SnapshotIdentitySet, SnapshotRecord, TranscriptSnapshot, read_snapshot};
use uuid::Uuid;

mod arguments;
mod chat;
mod connection;
mod error;
mod presentation;
mod transcript;

// numeric-bound: guard - one submitted message exhausting wire-frame memory
const MAX_INPUT_CONTENT_FRAME_BYTES: usize = MAX_FRAME_BYTES / 4 * 3;
// numeric-bound: guard - prevents a system prompt from exceeding one wire frame
const MAX_SYSTEM_PROMPT_FRAME_BYTES: usize = MAX_FRAME_BYTES / 4 * 3;
// numeric-bound: guard - prevents review input from exceeding one wire frame
const MAX_REVIEW_JSON_INPUT_BYTES: usize = MAX_FRAME_BYTES / 4 * 3;
// numeric-bound: guard - prevents import source from exceeding one wire frame
const MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES: usize = MAX_FRAME_BYTES / 4 * 3;
/// Bounded memory used while hashing one client-local blob source.
const BLOB_HASH_BUFFER_BYTES: usize = 64 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClientDeploymentLimits {
    max_message_utf8_bytes: Option<usize>,
    max_system_prompt_utf8_bytes: Option<usize>,
    terminal_input_channel_capacity: Option<usize>,
    min_metadata_page_size: Option<u64>,
    max_metadata_page_size: Option<u64>,
    max_review_findings_per_run: Option<u64>,
}

impl ClientDeploymentLimits {
    #[cfg(test)]
    const fn unbounded() -> Self {
        Self {
            max_message_utf8_bytes: None,
            max_system_prompt_utf8_bytes: None,
            terminal_input_channel_capacity: None,
            min_metadata_page_size: None,
            max_metadata_page_size: None,
            max_review_findings_per_run: None,
        }
    }
}

async fn read_deployment_limits(
    client: &mut ProcessClient,
) -> Result<ClientDeploymentLimits, ClientError> {
    let mut connection = client
        .request(ClientRequest::ReadDeploymentLimits {})
        .await?;
    match connection.message().await? {
        ServerMessage::DeploymentLimits {
            max_message_utf8_bytes,
            max_system_prompt_utf8_bytes,
            terminal_input_channel_capacity,
            min_metadata_page_size,
            max_metadata_page_size,
            max_review_findings_per_run,
        } => Ok(ClientDeploymentLimits {
            max_message_utf8_bytes: optional_usize_limit(max_message_utf8_bytes)?,
            max_system_prompt_utf8_bytes: optional_usize_limit(max_system_prompt_utf8_bytes)?,
            terminal_input_channel_capacity: optional_usize_limit(terminal_input_channel_capacity)?,
            min_metadata_page_size: min_metadata_page_size.map(CanonicalU64::value),
            max_metadata_page_size: max_metadata_page_size.map(CanonicalU64::value),
            max_review_findings_per_run: max_review_findings_per_run.map(CanonicalU64::value),
        }),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail)),
        _ => Err(ClientError::Protocol(
            "deployment limits read returned an unexpected response",
        )),
    }
}

fn optional_usize_limit(value: Option<CanonicalU64>) -> Result<Option<usize>, ClientError> {
    value
        .map(|value| {
            usize::try_from(value.value())
                .map_err(|_| ClientError::Protocol("deployment limit is not representable"))
        })
        .transpose()
}

/// Maximum time a terminal follower waits before rereading recovery state.
// numeric-bound: interval - exposes reconciliation exhaustion without busy polling
#[cfg(not(test))]
const FOLLOW_RECOVERY_REFETCH_INTERVAL: Duration = Duration::from_secs(30);
/// Short equivalent used by deterministic socket tests.
// numeric-bound: interval - keeps follower refetch tests bounded
#[cfg(test)]
const FOLLOW_RECOVERY_REFETCH_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewConcernsFile {
    concerns: Vec<ReviewOrchestrationConcernInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewFindingsFile {
    findings: Vec<ReviewFindingInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewJudgmentMembersFile {
    members: Vec<ReviewJudgmentPlanMember>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewRepairOutcomesFile {
    outcomes: Vec<ReviewRepairOutcome>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewPublicationOutcomesFile {
    outcomes: Vec<ReviewPublicationOutcome>,
}

/// One complete bounded `list_session_metadata` request.

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionMetadataPageRequest {
    pub(crate) required_tags: Vec<String>,
    pub(crate) title_contains: Option<String>,
    pub(crate) include_archived: bool,
    pub(crate) page_size: CanonicalU64,
    pub(crate) after_session_id: Option<CanonicalUuid>,
}

impl SessionMetadataPageRequest {
    fn request(&self) -> ClientRequest {
        ClientRequest::ListSessionMetadata {
            required_tags: self.required_tags.clone(),
            title_contains: self.title_contains.clone(),
            include_archived: self.include_archived,
            page_size: self.page_size,
            after_session_id: self.after_session_id,
        }
    }
}

/// One complete bounded `list_conversations` request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConversationsPageRequest {
    pub(crate) title_contains: Option<String>,
    pub(crate) origin: ConversationOriginFilter,
    pub(crate) include_archived: bool,
    pub(crate) page_size: CanonicalU64,
    pub(crate) after: Option<ConversationCursor>,
}

impl ConversationsPageRequest {
    fn request(&self) -> ClientRequest {
        ClientRequest::ListConversations {
            title_contains: self.title_contains.clone(),
            origin: self.origin,
            include_archived: self.include_archived,
            page_size: self.page_size,
            after: self.after,
        }
    }
}

enum PreparedImport {
    File(tokio::fs::File),
    Scan(PreparedImportScan),
}

struct PreparedBlobSource {
    path: PathBuf,
    file: tokio::fs::File,
}

struct PreparedImportScan {
    root: OwnedFd,
    paths: Vec<ScannedImportPath>,
}

struct ScannedImportPath {
    relative: PathBuf,
    display: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConversationImportOutcome {
    Inserted(CanonicalUuid),
    AlreadyImported(CanonicalUuid),
}

enum ConversationImportResponse {
    Begun(CanonicalU64),
    Appended(CanonicalU64),
    Inserted(CanonicalUuid),
    AlreadyImported(CanonicalUuid),
    Error {
        code: ErrorCode,
        message: String,
        detail: ErrorDetail,
    },
    Unexpected,
}

enum BlobUploadResponse {
    Begun {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
    },
    AlreadyPresent {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
    },
    Appended(CanonicalU64),
    Committed {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
    },
    Error {
        code: ErrorCode,
        message: String,
        detail: ErrorDetail,
    },
    Unexpected,
}

enum DelegationResponse {
    Spawned {
        tool_request_id: CanonicalUuid,
        child_session_id: CanonicalUuid,
        relationship: DelegationPolicy,
    },
    AwaitRegistered {
        tool_request_id: CanonicalUuid,
        child_session_id: CanonicalUuid,
        mode: DelegationWaitMode,
    },
    ChildResult {
        await_request_id: CanonicalUuid,
        spawning_request_id: CanonicalUuid,
        child_session_id: CanonicalUuid,
        outcome: DelegationOutcome,
        content: Option<String>,
        reason: DelegationReason,
        provenance: DelegationProvenance,
    },
    MessageSent {
        tool_request_id: CanonicalUuid,
        message_id: CanonicalUuid,
        direction: DelegationMessageDirection,
        ordinal: CanonicalU64,
        delivery_sequence: CanonicalU64,
    },
    Error {
        code: ErrorCode,
        message: String,
        detail: ErrorDetail,
    },
    Unexpected,
}

#[derive(Clone, Copy)]
enum DelegationRejectionOperation {
    Spawn,
    Await {
        child: CanonicalUuid,
        mode: DelegationWaitMode,
    },
    Message {
        peer: CanonicalUuid,
    },
}

impl DelegationRejectionOperation {
    const fn is_spawn(self) -> bool {
        match self {
            Self::Spawn => true,
            Self::Await { .. } | Self::Message { .. } => false,
        }
    }

    const fn is_await(self) -> bool {
        match self {
            Self::Await { .. } => true,
            Self::Spawn | Self::Message { .. } => false,
        }
    }

    const fn is_message(self) -> bool {
        match self {
            Self::Message { .. } => true,
            Self::Spawn | Self::Await { .. } => false,
        }
    }

    const fn peer(self) -> Option<CanonicalUuid> {
        match self {
            Self::Spawn => None,
            Self::Await { child, .. } => Some(child),
            Self::Message { peer } => Some(peer),
        }
    }
}

#[derive(Clone, Copy)]
struct DelegationRejectionExpectation {
    session: CanonicalUuid,
    turn: CanonicalUuid,
    tool_request: CanonicalUuid,
    operation: DelegationRejectionOperation,
}

fn delegation_rejection_matches(
    detail: Option<RejectionDetail>,
    expected: DelegationRejectionExpectation,
) -> bool {
    let Some(detail) = detail else {
        return false;
    };
    match detail {
        RejectionDetail::DelegationRequestNotInTurn {
            session_id,
            turn_id,
            tool_request_id,
        } => {
            session_id == expected.session
                && turn_id == expected.turn
                && tool_request_id == expected.tool_request
        }
        RejectionDetail::DelegationToolRequestNotExecutable {
            tool_request_id, ..
        } => tool_request_id == expected.tool_request,
        RejectionDetail::SessionNotFound { session_id } => session_id == expected.session,
        RejectionDetail::ToolRequestNotFound { tool_request_id } => {
            tool_request_id == expected.tool_request
        }
        RejectionDetail::ToolRequestNotInSession {
            session_id,
            tool_request_id,
        } => session_id == expected.session && tool_request_id == expected.tool_request,
        RejectionDetail::DelegationSpawnConflict { tool_request_id } => {
            expected.operation.is_spawn() && tool_request_id == expected.tool_request
        }
        RejectionDetail::DelegatedChildIdentityCollision { .. } => false,
        RejectionDetail::DelegationRelationNotFound {
            session_id,
            peer_session_id,
        } => {
            !expected.operation.is_spawn()
                && session_id == expected.session
                && expected.operation.peer() == Some(peer_session_id)
        }
        RejectionDetail::DelegationAwaitConflict { tool_request_id } => {
            expected.operation.is_await() && tool_request_id == expected.tool_request
        }
        RejectionDetail::DelegationMessageConflict { tool_request_id } => {
            expected.operation.is_message() && tool_request_id == expected.tool_request
        }
        RejectionDetail::DelegationMessageIdentityCollision { .. } => false,
        RejectionDetail::DelegationEventOrdinalExhausted { .. } => false,
        RejectionDetail::DelegationDeliverySequenceExhausted {
            recipient_session_id,
            ..
        } => match expected.operation {
            DelegationRejectionOperation::Spawn => false,
            DelegationRejectionOperation::Await {
                mode: DelegationWaitMode::Background,
                ..
            } => recipient_session_id == expected.session,
            DelegationRejectionOperation::Await {
                mode: DelegationWaitMode::Foreground,
                ..
            } => false,
            DelegationRejectionOperation::Message { peer } => recipient_session_id == peer,
        },
        RejectionDetail::UnsupportedReasoningLevel { .. }
        | RejectionDetail::UnsupportedFastMode { .. }
        | RejectionDetail::UnsupportedServiceTier { .. }
        | RejectionDetail::AttachmentBlobNotFound { .. }
        | RejectionDetail::AttachmentByteBudgetExceeded { .. }
        | RejectionDetail::SessionPlacementCurrentVersionMismatch { .. }
        | RejectionDetail::SessionPlacementVersionExhausted { .. }
        | RejectionDetail::GoalCommandRejected { .. }
        | RejectionDetail::SessionLifecycleCommandRejected { .. }
        | RejectionDetail::ActiveTurnPresent { .. }
        | RejectionDetail::CommissionTargetBusy { .. }
        | RejectionDetail::ActiveTurnMismatch { .. }
        | RejectionDetail::NoActiveTurn { .. }
        | RejectionDetail::TurnNotAwaitingReconciliation { .. }
        | RejectionDetail::InterruptAlreadyApplied { .. }
        | RejectionDetail::InterruptUnavailableWhileAwaitingApproval { .. }
        | RejectionDetail::SafePointUnavailableWhileStopping { .. }
        | RejectionDetail::ToolRequestAlreadyResolved { .. }
        | RejectionDetail::ToolRequestNotDelegateDenied { .. }
        | RejectionDetail::ToolRequestNotTerminallyDenied { .. }
        | RejectionDetail::ToolDenialAlreadyOverridden { .. }
        | RejectionDetail::ToolRequestNotEarliestUndecided { .. }
        | RejectionDetail::DefaultsVersionMismatch { .. }
        | RejectionDetail::UnknownModelAlias { .. }
        | RejectionDetail::AcceptancePositionExhausted { .. }
        | RejectionDetail::DefaultsVersionExhausted { .. }
        | RejectionDetail::ImportedConversationNotFound { .. }
        | RejectionDetail::ImportedFrontierPositionOutOfRange { .. }
        | RejectionDetail::ConversationImportAlreadyInProgress {}
        | RejectionDetail::ConversationImportNotInProgress {}
        | RejectionDetail::ConversationImportSourceTooLarge { .. }
        | RejectionDetail::ConversationImportSourceSizeMismatch { .. }
        | RejectionDetail::ConversationImportConversionFailed { .. } => false,
        RejectionDetail::BulkIngestAlreadyInProgress { .. }
        | RejectionDetail::BlobUploadAlreadyInProgress {}
        | RejectionDetail::BlobUploadNotInProgress {}
        | RejectionDetail::BlobUploadLengthOutOfRange { .. }
        | RejectionDetail::BlobUploadSizeExceeded { .. }
        | RejectionDetail::BlobUploadLengthMismatch { .. }
        | RejectionDetail::BlobUploadDigestMismatch { .. }
        | RejectionDetail::BlobReadLengthOutOfRange { .. }
        | RejectionDetail::BlobReadRangeOutOfBounds { .. } => false,
    }
}

fn classify_delegation_response(message: ServerMessage) -> DelegationResponse {
    match message {
        ServerMessage::SessionSpawned {
            tool_request_id,
            child_session_id,
            relationship,
        } => DelegationResponse::Spawned {
            tool_request_id,
            child_session_id,
            relationship,
        },
        ServerMessage::SessionAwaitRegistered {
            tool_request_id,
            child_session_id,
            mode,
        } => DelegationResponse::AwaitRegistered {
            tool_request_id,
            child_session_id,
            mode,
        },
        ServerMessage::ChildResult {
            await_request_id,
            spawning_request_id,
            child_session_id,
            outcome,
            content,
            reason,
            provenance,
        } => DelegationResponse::ChildResult {
            await_request_id,
            spawning_request_id,
            child_session_id,
            outcome,
            content,
            reason,
            provenance,
        },
        ServerMessage::SessionMessageSent {
            tool_request_id,
            message_id,
            direction,
            ordinal,
            delivery_sequence,
        } => DelegationResponse::MessageSent {
            tool_request_id,
            message_id,
            direction,
            ordinal,
            delivery_sequence,
        },
        ServerMessage::Error {
            code,
            message,
            detail,
        } => DelegationResponse::Error {
            code,
            message,
            detail,
        },
        ServerMessage::SessionCreated { .. }
        | ServerMessage::SessionCommissioned { .. }
        | ServerMessage::SessionLifecycleCommandApplied { .. }
        | ServerMessage::SessionPlacementUpdated { .. }
        | ServerMessage::InputSubmitted { .. }
        | ServerMessage::SteeringSubmitted { .. }
        | ServerMessage::GoalTransitionApplied { .. }
        | ServerMessage::GoalHistoryStart { .. }
        | ServerMessage::GoalHistoryState { .. }
        | ServerMessage::GoalHistoryItem { .. }
        | ServerMessage::GoalHistoryEnd { .. }
        | ServerMessage::SessionsStart {}
        | ServerMessage::SessionSummary { .. }
        | ServerMessage::SessionsEnd { .. }
        | ServerMessage::OperatorStatus(..)
        | ServerMessage::TemplatesStart {}
        | ServerMessage::TemplateSummary { .. }
        | ServerMessage::TemplatesEnd { .. }
        | ServerMessage::SessionMetadataPageStart {}
        | ServerMessage::SessionMetadataSummary { .. }
        | ServerMessage::SessionMetadataPageEnd { .. }
        | ServerMessage::ConversationPageStart {}
        | ServerMessage::ConversationSummary { .. }
        | ServerMessage::ConversationPageEnd { .. }
        | ServerMessage::ModelAliasesStart {}
        | ServerMessage::ModelAliasSummary { .. }
        | ServerMessage::ModelAliasesEnd { .. }
        | ServerMessage::ModelCapabilitiesStart {}
        | ServerMessage::ModelCapabilityItem { .. }
        | ServerMessage::ModelCapabilitiesEnd { .. }
        | ServerMessage::SessionMetadata { .. }
        | ServerMessage::SessionMetadataReplaced { .. }
        | ServerMessage::SessionDefaultsReplaced { .. }
        | ServerMessage::SessionDefaults { .. }
        | ServerMessage::ToolRequestDecided { .. }
        | ServerMessage::ToolDenialOverridden { .. }
        | ServerMessage::SessionCompacted { .. }
        | ServerMessage::ConversationImportBegun { .. }
        | ServerMessage::ConversationImportAppended { .. }
        | ServerMessage::ConversationImportInserted { .. }
        | ServerMessage::ConversationImportAlreadyImported { .. }
        | ServerMessage::ConversationImportAborted {}
        | ServerMessage::BlobUploadBegun { .. }
        | ServerMessage::BlobUploadAlreadyPresent { .. }
        | ServerMessage::BlobUploadAppended { .. }
        | ServerMessage::BlobUploadCommitted { .. }
        | ServerMessage::BlobUploadAborted {}
        | ServerMessage::BlobMetadata { .. }
        | ServerMessage::BlobChunkRead { .. }
        | ServerMessage::ImportedConversationStart { .. }
        | ServerMessage::ImportedConversationEntry { .. }
        | ServerMessage::ImportedConversationEnd { .. }
        | ServerMessage::TranscriptSnapshotStart { .. }
        | ServerMessage::TranscriptTurn { .. }
        | ServerMessage::TranscriptModelCallUsage { .. }
        | ServerMessage::TranscriptModelCallsEnd { .. }
        | ServerMessage::TranscriptEntry { .. }
        | ServerMessage::TranscriptUserEntry { .. }
        | ServerMessage::TranscriptTextEntry { .. }
        | ServerMessage::TranscriptContent { .. }
        | ServerMessage::TranscriptSnapshotEnd { .. }
        | ServerMessage::SessionEvent { .. }
        | ServerMessage::ProviderTextDelta { .. }
        | ServerMessage::ReviewTargetCreated { .. }
        | ServerMessage::ReviewRunStarted { .. }
        | ServerMessage::ReviewPassActivated { .. }
        | ServerMessage::ReviewPassCompleted { .. }
        | ServerMessage::ReviewFindingsRecorded { .. }
        | ServerMessage::ReviewFindingEventRecorded { .. }
        | ServerMessage::ReviewExternalLinkReserved { .. }
        | ServerMessage::ReviewExternalLinkAttached { .. }
        | ServerMessage::ReviewTarget { .. }
        | ServerMessage::ReviewRun { .. }
        | ServerMessage::ReviewFinding { .. }
        | ServerMessage::ReviewFindingsStart { .. }
        | ServerMessage::ReviewFindingItem { .. }
        | ServerMessage::ReviewFindingsEnd { .. }
        | ServerMessage::ReviewOrchestrationStarted { .. }
        | ServerMessage::ReviewOrchestrationAdvanced { .. }
        | ServerMessage::ReviewOrchestration { .. }
        | ServerMessage::DeploymentLimits { .. } => DelegationResponse::Unexpected,
    }
}

fn classify_conversation_import_response(message: ServerMessage) -> ConversationImportResponse {
    match message {
        ServerMessage::ConversationImportBegun {
            declared_size_bytes,
        } => ConversationImportResponse::Begun(declared_size_bytes),
        ServerMessage::ConversationImportAppended {
            assembled_size_bytes,
        } => ConversationImportResponse::Appended(assembled_size_bytes),
        ServerMessage::ConversationImportInserted {
            imported_conversation_id,
        } => ConversationImportResponse::Inserted(imported_conversation_id),
        ServerMessage::ConversationImportAlreadyImported {
            imported_conversation_id,
        } => ConversationImportResponse::AlreadyImported(imported_conversation_id),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => ConversationImportResponse::Error {
            code,
            message,
            detail,
        },
        ServerMessage::SessionCreated { .. }
        | ServerMessage::SessionCommissioned { .. }
        | ServerMessage::SessionLifecycleCommandApplied { .. }
        | ServerMessage::SessionSpawned { .. }
        | ServerMessage::SessionAwaitRegistered { .. }
        | ServerMessage::ChildResult { .. }
        | ServerMessage::SessionMessageSent { .. }
        | ServerMessage::SessionPlacementUpdated { .. }
        | ServerMessage::InputSubmitted { .. }
        | ServerMessage::SteeringSubmitted { .. }
        | ServerMessage::GoalTransitionApplied { .. }
        | ServerMessage::GoalHistoryStart { .. }
        | ServerMessage::GoalHistoryState { .. }
        | ServerMessage::GoalHistoryItem { .. }
        | ServerMessage::GoalHistoryEnd { .. }
        | ServerMessage::SessionsStart {}
        | ServerMessage::SessionSummary { .. }
        | ServerMessage::SessionsEnd { .. }
        | ServerMessage::OperatorStatus(..)
        | ServerMessage::TemplatesStart {}
        | ServerMessage::TemplateSummary { .. }
        | ServerMessage::TemplatesEnd { .. }
        | ServerMessage::SessionMetadataPageStart {}
        | ServerMessage::SessionMetadataSummary { .. }
        | ServerMessage::SessionMetadataPageEnd { .. }
        | ServerMessage::ConversationPageStart {}
        | ServerMessage::ConversationSummary { .. }
        | ServerMessage::ConversationPageEnd { .. }
        | ServerMessage::ModelAliasesStart {}
        | ServerMessage::ModelAliasSummary { .. }
        | ServerMessage::ModelAliasesEnd { .. }
        | ServerMessage::ModelCapabilitiesStart {}
        | ServerMessage::ModelCapabilityItem { .. }
        | ServerMessage::ModelCapabilitiesEnd { .. }
        | ServerMessage::SessionMetadata { .. }
        | ServerMessage::SessionMetadataReplaced { .. }
        | ServerMessage::SessionDefaultsReplaced { .. }
        | ServerMessage::SessionDefaults { .. }
        | ServerMessage::ToolRequestDecided { .. }
        | ServerMessage::ToolDenialOverridden { .. }
        | ServerMessage::SessionCompacted { .. }
        | ServerMessage::ConversationImportAborted {}
        | ServerMessage::BlobUploadBegun { .. }
        | ServerMessage::BlobUploadAlreadyPresent { .. }
        | ServerMessage::BlobUploadAppended { .. }
        | ServerMessage::BlobUploadCommitted { .. }
        | ServerMessage::BlobUploadAborted {}
        | ServerMessage::BlobMetadata { .. }
        | ServerMessage::BlobChunkRead { .. }
        | ServerMessage::ImportedConversationStart { .. }
        | ServerMessage::ImportedConversationEntry { .. }
        | ServerMessage::ImportedConversationEnd { .. }
        | ServerMessage::TranscriptSnapshotStart { .. }
        | ServerMessage::TranscriptTurn { .. }
        | ServerMessage::TranscriptModelCallUsage { .. }
        | ServerMessage::TranscriptModelCallsEnd { .. }
        | ServerMessage::TranscriptEntry { .. }
        | ServerMessage::TranscriptUserEntry { .. }
        | ServerMessage::TranscriptTextEntry { .. }
        | ServerMessage::TranscriptContent { .. }
        | ServerMessage::TranscriptSnapshotEnd { .. }
        | ServerMessage::SessionEvent { .. }
        | ServerMessage::ProviderTextDelta { .. }
        | ServerMessage::ReviewTargetCreated { .. }
        | ServerMessage::ReviewRunStarted { .. }
        | ServerMessage::ReviewPassActivated { .. }
        | ServerMessage::ReviewPassCompleted { .. }
        | ServerMessage::ReviewFindingsRecorded { .. }
        | ServerMessage::ReviewFindingEventRecorded { .. }
        | ServerMessage::ReviewExternalLinkReserved { .. }
        | ServerMessage::ReviewExternalLinkAttached { .. }
        | ServerMessage::ReviewTarget { .. }
        | ServerMessage::ReviewRun { .. }
        | ServerMessage::ReviewFinding { .. }
        | ServerMessage::ReviewFindingsStart { .. }
        | ServerMessage::ReviewFindingItem { .. }
        | ServerMessage::ReviewFindingsEnd { .. }
        | ServerMessage::ReviewOrchestrationStarted { .. }
        | ServerMessage::ReviewOrchestrationAdvanced { .. }
        | ServerMessage::ReviewOrchestration { .. }
        | ServerMessage::DeploymentLimits { .. } => ConversationImportResponse::Unexpected,
    }
}

fn classify_blob_upload_response(message: ServerMessage) -> BlobUploadResponse {
    match message {
        ServerMessage::BlobUploadBegun {
            expected_digest,
            expected_length_bytes,
        } => BlobUploadResponse::Begun {
            digest: expected_digest,
            byte_length: expected_length_bytes,
        },
        ServerMessage::BlobUploadAlreadyPresent {
            digest,
            byte_length,
        } => BlobUploadResponse::AlreadyPresent {
            digest,
            byte_length,
        },
        ServerMessage::BlobUploadAppended {
            assembled_length_bytes,
        } => BlobUploadResponse::Appended(assembled_length_bytes),
        ServerMessage::BlobUploadCommitted {
            digest,
            byte_length,
        } => BlobUploadResponse::Committed {
            digest,
            byte_length,
        },
        ServerMessage::Error {
            code,
            message,
            detail,
        } => BlobUploadResponse::Error {
            code,
            message,
            detail,
        },
        ServerMessage::SessionCreated { .. }
        | ServerMessage::SessionCommissioned { .. }
        | ServerMessage::SessionLifecycleCommandApplied { .. }
        | ServerMessage::SessionSpawned { .. }
        | ServerMessage::SessionAwaitRegistered { .. }
        | ServerMessage::ChildResult { .. }
        | ServerMessage::SessionMessageSent { .. }
        | ServerMessage::SessionPlacementUpdated { .. }
        | ServerMessage::InputSubmitted { .. }
        | ServerMessage::SteeringSubmitted { .. }
        | ServerMessage::GoalTransitionApplied { .. }
        | ServerMessage::GoalHistoryStart { .. }
        | ServerMessage::GoalHistoryState { .. }
        | ServerMessage::GoalHistoryItem { .. }
        | ServerMessage::GoalHistoryEnd { .. }
        | ServerMessage::SessionsStart {}
        | ServerMessage::SessionSummary { .. }
        | ServerMessage::SessionsEnd { .. }
        | ServerMessage::OperatorStatus(..)
        | ServerMessage::TemplatesStart {}
        | ServerMessage::TemplateSummary { .. }
        | ServerMessage::TemplatesEnd { .. }
        | ServerMessage::SessionMetadataPageStart {}
        | ServerMessage::SessionMetadataSummary { .. }
        | ServerMessage::SessionMetadataPageEnd { .. }
        | ServerMessage::ConversationPageStart {}
        | ServerMessage::ConversationSummary { .. }
        | ServerMessage::ConversationPageEnd { .. }
        | ServerMessage::ModelAliasesStart {}
        | ServerMessage::ModelAliasSummary { .. }
        | ServerMessage::ModelAliasesEnd { .. }
        | ServerMessage::ModelCapabilitiesStart {}
        | ServerMessage::ModelCapabilityItem { .. }
        | ServerMessage::ModelCapabilitiesEnd { .. }
        | ServerMessage::SessionMetadata { .. }
        | ServerMessage::SessionMetadataReplaced { .. }
        | ServerMessage::SessionDefaultsReplaced { .. }
        | ServerMessage::SessionDefaults { .. }
        | ServerMessage::ToolRequestDecided { .. }
        | ServerMessage::ToolDenialOverridden { .. }
        | ServerMessage::SessionCompacted { .. }
        | ServerMessage::ConversationImportBegun { .. }
        | ServerMessage::ConversationImportAppended { .. }
        | ServerMessage::ConversationImportInserted { .. }
        | ServerMessage::ConversationImportAlreadyImported { .. }
        | ServerMessage::ConversationImportAborted {}
        | ServerMessage::BlobUploadAborted {}
        | ServerMessage::BlobMetadata { .. }
        | ServerMessage::BlobChunkRead { .. }
        | ServerMessage::ImportedConversationStart { .. }
        | ServerMessage::ImportedConversationEntry { .. }
        | ServerMessage::ImportedConversationEnd { .. }
        | ServerMessage::TranscriptSnapshotStart { .. }
        | ServerMessage::TranscriptTurn { .. }
        | ServerMessage::TranscriptModelCallUsage { .. }
        | ServerMessage::TranscriptModelCallsEnd { .. }
        | ServerMessage::TranscriptEntry { .. }
        | ServerMessage::TranscriptUserEntry { .. }
        | ServerMessage::TranscriptTextEntry { .. }
        | ServerMessage::TranscriptContent { .. }
        | ServerMessage::TranscriptSnapshotEnd { .. }
        | ServerMessage::SessionEvent { .. }
        | ServerMessage::ProviderTextDelta { .. }
        | ServerMessage::ReviewTargetCreated { .. }
        | ServerMessage::ReviewRunStarted { .. }
        | ServerMessage::ReviewPassActivated { .. }
        | ServerMessage::ReviewPassCompleted { .. }
        | ServerMessage::ReviewFindingsRecorded { .. }
        | ServerMessage::ReviewFindingEventRecorded { .. }
        | ServerMessage::ReviewExternalLinkReserved { .. }
        | ServerMessage::ReviewExternalLinkAttached { .. }
        | ServerMessage::ReviewTarget { .. }
        | ServerMessage::ReviewRun { .. }
        | ServerMessage::ReviewFinding { .. }
        | ServerMessage::ReviewFindingsStart { .. }
        | ServerMessage::ReviewFindingItem { .. }
        | ServerMessage::ReviewFindingsEnd { .. }
        | ServerMessage::ReviewOrchestrationStarted { .. }
        | ServerMessage::ReviewOrchestrationAdvanced { .. }
        | ServerMessage::ReviewOrchestration { .. }
        | ServerMessage::DeploymentLimits { .. } => BlobUploadResponse::Unexpected,
    }
}

#[derive(Default)]
pub(crate) struct ImportScanSummary {
    pub(crate) imported: usize,
    pub(crate) already_imported: usize,
    pub(crate) skipped: usize,
}

/// Parses and runs one terminal-client invocation.
pub async fn run(
    arguments: impl IntoIterator<Item = OsString>,
    socket_environment: Option<OsString>,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    let parsed = match arguments::parse(arguments) {
        Ok(ParseOutcome::Help(help)) => {
            return if write!(stdout, "{help}").is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Ok(ParseOutcome::Run(arguments)) => arguments,
        Err(error) => {
            let _ = write!(stderr, "{error}");
            return ExitCode::from(2);
        }
    };
    let raw_output = parsed.raw_output;
    let result = execute(parsed, socket_environment, stdin, stdout, stderr).await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut output = Output::new(stdout, stderr, raw_output);
            let _ = output.error(&error);
            ExitCode::FAILURE
        }
    }
}

/// Parses and runs one invocation against the process terminal.
///
/// The interactive `chat` verb uses asynchronous standard-input lines and
/// catches terminal interrupts. Every other verb retains the one-shot standard
/// input and output path exposed by [`run`].
pub async fn run_terminal(
    arguments: impl IntoIterator<Item = OsString>,
    socket_environment: Option<OsString>,
) -> ExitCode {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    let parsed = match arguments::parse(arguments.clone()) {
        Ok(ParseOutcome::Help(help)) => {
            return if write!(std::io::stdout().lock(), "{help}").is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Ok(ParseOutcome::Run(arguments)) => arguments,
        Err(error) => {
            let _ = write!(std::io::stderr().lock(), "{error}");
            return ExitCode::from(2);
        }
    };
    let Command::Chat { session_id } = parsed.command else {
        return run(
            arguments,
            socket_environment,
            &mut std::io::stdin().lock(),
            &mut std::io::stdout().lock(),
            &mut std::io::stderr().lock(),
        )
        .await;
    };
    let raw_output = parsed.raw_output;
    let result = async {
        let socket = socket_path(parsed.socket, socket_environment)?;
        let mut client = ProcessClient::new(socket);
        let mut stdout = std::io::stdout().lock();
        let mut stderr = std::io::stderr().lock();
        let mut output = Output::new(&mut stdout, &mut stderr, raw_output);
        let deployment_limits = read_deployment_limits(&mut client).await?;
        let input = chat::terminal_input(deployment_limits.terminal_input_channel_capacity)?;
        chat::run(
            &mut client,
            &mut output,
            session_id,
            input,
            deployment_limits,
        )
        .await
    }
    .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut stdout = std::io::stdout().lock();
            let mut stderr = std::io::stderr().lock();
            let mut output = Output::new(&mut stdout, &mut stderr, raw_output);
            let _ = output.error(&error);
            ExitCode::FAILURE
        }
    }
}

fn command_uses_deployment_limits(command: &Command) -> bool {
    match command {
        Command::Send { .. }
        | Command::Steer { .. }
        | Command::Reconcile { .. }
        | Command::Stop { .. }
        | Command::Search(_)
        | Command::Conversations(_)
        | Command::Create {
            system_prompt_file: Some(_),
            ..
        }
        | Command::Model {
            system_prompt: SystemPromptArgument::File(_),
            ..
        } => true,
        Command::Review(command) => matches!(
            command.as_ref(),
            ReviewCommand::RecordFinding { .. }
                | ReviewCommand::RecordFindings { .. }
                | ReviewCommand::ListFindings { .. }
        ),
        _ => false,
    }
}

fn validate_message_policy(
    content: &str,
    limits: Option<ClientDeploymentLimits>,
) -> Result<(), ClientError> {
    let limits = limits.ok_or(ClientError::Protocol("deployment limits were not read"))?;
    if limits
        .max_message_utf8_bytes
        .is_some_and(|maximum| content.len() > maximum)
    {
        return Err(ClientError::Input(
            "standard-input content exceeds the deployment UTF-8 byte limit",
        ));
    }
    Ok(())
}

fn validate_system_prompt_policy(
    system_prompt: &SystemPromptText,
    limits: Option<ClientDeploymentLimits>,
) -> Result<(), ClientError> {
    let limits = limits.ok_or(ClientError::Protocol("deployment limits were not read"))?;
    if limits
        .max_system_prompt_utf8_bytes
        .is_some_and(|maximum| system_prompt.as_str().len() > maximum)
    {
        return Err(ClientError::Input(
            "system prompt exceeds the deployment UTF-8 byte limit",
        ));
    }
    Ok(())
}

fn validate_metadata_page_policy(
    page_size: CanonicalU64,
    limits: Option<ClientDeploymentLimits>,
) -> Result<(), ClientError> {
    let limits = limits.ok_or(ClientError::Protocol("deployment limits were not read"))?;
    if limits
        .min_metadata_page_size
        .is_some_and(|minimum| page_size.value() < minimum)
        || limits
            .max_metadata_page_size
            .is_some_and(|maximum| page_size.value() > maximum)
    {
        return Err(ClientError::Input(
            "the result limit is outside the deployment page-size range",
        ));
    }
    Ok(())
}

async fn execute(
    arguments: arguments::Arguments,
    socket_environment: Option<OsString>,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), ClientError> {
    let input = if matches!(
        arguments.command,
        Command::Send { .. }
            | Command::Steer { .. }
            | Command::Reconcile { .. }
            | Command::Stop { .. }
    ) {
        Some(read_input(stdin)?)
    } else {
        None
    };
    let prepared_import = match &arguments.command {
        Command::Import {
            source: ImportSourceArgument::File(path),
            ..
        } => Some(PreparedImport::File(open_import_source(path).await?)),
        Command::Import {
            source: ImportSourceArgument::Scan(path),
            ..
        } => Some(PreparedImport::Scan(collect_import_paths(path)?)),
        Command::BlobUpload { .. }
        | Command::BlobMetadata { .. }
        | Command::BlobRead { .. }
        | Command::Create { .. }
        | Command::Place { .. }
        | Command::Continue { .. }
        | Command::Compact { .. }
        | Command::Session(_)
        | Command::Goal(_)
        | Command::Imported { .. }
        | Command::Status
        | Command::List
        | Command::Templates
        | Command::Search(_)
        | Command::Conversations(_)
        | Command::Send { .. }
        | Command::Steer { .. }
        | Command::Model { .. }
        | Command::Transcript { .. }
        | Command::Follow { .. }
        | Command::Chat { .. }
        | Command::Reconcile { .. }
        | Command::Review(_)
        | Command::Stop { .. }
        | Command::Approve { .. }
        | Command::Deny { .. } => None,
    };
    let prepared_blob = match &arguments.command {
        Command::BlobUpload { source } => Some(open_blob_source(source)?),
        Command::Create { .. }
        | Command::Place { .. }
        | Command::Continue { .. }
        | Command::Compact { .. }
        | Command::Session(_)
        | Command::Goal(_)
        | Command::Imported { .. }
        | Command::Status
        | Command::List
        | Command::Templates
        | Command::Search(_)
        | Command::Conversations(_)
        | Command::Send { .. }
        | Command::Steer { .. }
        | Command::Model { .. }
        | Command::Transcript { .. }
        | Command::Follow { .. }
        | Command::Chat { .. }
        | Command::Reconcile { .. }
        | Command::Review(_)
        | Command::Stop { .. }
        | Command::Approve { .. }
        | Command::Deny { .. }
        | Command::BlobMetadata { .. }
        | Command::BlobRead { .. }
        | Command::Import { .. } => None,
    };
    let system_prompt_text = match &arguments.command {
        Command::Create {
            system_prompt_file: Some(path),
            ..
        }
        | Command::Model {
            system_prompt: SystemPromptArgument::File(path),
            ..
        } => Some(read_system_prompt_file(path).await?),
        Command::Create { .. }
        | Command::Place { .. }
        | Command::Compact { .. }
        | Command::Session(_)
        | Command::Goal(_)
        | Command::Status
        | Command::List
        | Command::Templates
        | Command::Search(_)
        | Command::Conversations(_)
        | Command::Send { .. }
        | Command::Steer { .. }
        | Command::Reconcile { .. }
        | Command::Stop { .. }
        | Command::Approve { .. }
        | Command::Deny { .. }
        | Command::Model { .. }
        | Command::Transcript { .. }
        | Command::Follow { .. }
        | Command::Chat { .. }
        | Command::Continue { .. }
        | Command::Imported { .. }
        | Command::Review(_)
        | Command::Import { .. }
        | Command::BlobUpload { .. }
        | Command::BlobMetadata { .. }
        | Command::BlobRead { .. } => None,
    };
    let socket = socket_path(arguments.socket, socket_environment)?;
    let mut client = ProcessClient::new(socket);
    let mut output = Output::new(stdout, stderr, arguments.raw_output);
    let deployment_limits = if command_uses_deployment_limits(&arguments.command) {
        Some(read_deployment_limits(&mut client).await?)
    } else {
        None
    };
    if let Some(input) = input.as_deref() {
        validate_message_policy(input, deployment_limits)?;
    }
    if let Some(system_prompt) = system_prompt_text.as_ref() {
        validate_system_prompt_policy(system_prompt, deployment_limits)?;
    }
    match &arguments.command {
        Command::Search(page) => validate_metadata_page_policy(page.page_size, deployment_limits)?,
        Command::Conversations(page) => {
            validate_metadata_page_policy(page.page_size, deployment_limits)?
        }
        _ => {}
    }

    match arguments.command {
        Command::Create {
            selection,
            template,
            command_id,
            system_prompt_file: _,
            placement,
        } => match (selection, template) {
            (Some(selection), None) => {
                create(
                    &mut client,
                    &mut output,
                    selection,
                    command_id,
                    system_prompt_text,
                    placement,
                )
                .await
            }
            (None, Some(template)) => {
                create_from_template(&mut client, &mut output, template, command_id, placement)
                    .await
            }
            _ => Err(ClientError::Protocol(
                "create source was internally invalid",
            )),
        },
        Command::Place {
            session_id,
            expected_placement_version,
            replacement,
            command_id,
        } => {
            update_session_placement(
                &mut client,
                &mut output,
                session_id,
                expected_placement_version,
                replacement,
                command_id,
            )
            .await
        }
        Command::Continue {
            imported_conversation_id,
            through_position,
            relationship,
            selection,
            command_id,
        } => {
            continue_imported(
                &mut client,
                &mut output,
                imported_conversation_id,
                through_position,
                relationship,
                selection,
                command_id,
            )
            .await
        }
        Command::Compact {
            session_id,
            through_position,
            command_id,
        } => {
            compact(
                &mut client,
                &mut output,
                session_id,
                through_position,
                command_id,
            )
            .await
        }
        Command::Imported {
            imported_conversation_id,
        } => imported(&mut client, &mut output, imported_conversation_id).await,
        Command::Session(command) => session_delegation(&mut client, &mut output, command).await,
        Command::Goal(command) => goal(&mut client, &mut output, command).await,
        Command::Status => status(&mut client, &mut output).await,
        Command::List => list(&mut client, &mut output).await,
        Command::Templates => list_templates(&mut client, &mut output).await,
        Command::Search(page) => search(&mut client, &mut output, page).await,
        Command::Conversations(page) => conversations(&mut client, &mut output, page).await,
        Command::Send {
            session_id,
            command_id,
            defaults_version,
            delivery,
        } => {
            let input = input.ok_or(ClientError::Input("standard-input content was not read"))?;
            send(
                &mut client,
                &mut output,
                session_id,
                command_id,
                defaults_version,
                delivery,
                input,
            )
            .await
        }
        Command::Steer {
            session_id,
            command_id,
            turn_id,
        } => {
            let input = input.ok_or(ClientError::Input("standard-input content was not read"))?;
            steer(
                &mut client,
                &mut output,
                session_id,
                command_id,
                turn_id,
                input,
            )
            .await
        }
        Command::Model {
            session_id,
            selection,
            command_id,
            defaults_version,
            dangerous_tool_auto_approval,
            system_prompt,
        } => {
            let system_prompt = match system_prompt {
                SystemPromptArgument::Keep => ModelSystemPromptChoice::Keep,
                SystemPromptArgument::Clear => ModelSystemPromptChoice::Clear,
                SystemPromptArgument::File(_) => ModelSystemPromptChoice::Replace(
                    system_prompt_text
                        .ok_or(ClientError::Input("system prompt file was not read"))?,
                ),
            };
            replace_session_model(
                &mut client,
                &mut output,
                session_id,
                selection,
                command_id,
                defaults_version,
                dangerous_tool_auto_approval,
                system_prompt,
            )
            .await
        }
        Command::Transcript { session_id } => {
            let mut snapshot = transcript(&mut client, session_id).await?;
            output.snapshot(&mut snapshot)?;
            Ok(())
        }
        Command::Follow { session_id } => follow(&mut client, &mut output, session_id).await,
        Command::Chat { .. } => Err(ClientError::Input(
            "chat requires the process terminal input path",
        )),
        Command::Import { format, .. } => {
            match prepared_import.ok_or(ClientError::Input("import source was not prepared"))? {
                PreparedImport::File(file) => {
                    let outcome = import_conversation_file(&mut client, format, file).await?;
                    write_single_import_outcome(&mut output, outcome)
                }
                PreparedImport::Scan(scan) => {
                    scan_conversations(&mut client, &mut output, format, scan).await
                }
            }
        }
        Command::BlobUpload { .. } => {
            let source = prepared_blob.ok_or(ClientError::Input("blob source was not prepared"))?;
            upload_blob(&mut client, &mut output, source).await
        }
        Command::BlobMetadata { digest } => {
            read_blob_metadata(&mut client, &mut output, digest).await
        }
        Command::BlobRead {
            digest,
            offset_bytes,
            length_bytes,
            output,
        } => {
            let bytes = read_blob_chunk(&mut client, digest, offset_bytes, length_bytes).await?;
            write_blob_output(&output, &bytes).await
        }
        Command::Reconcile {
            session_id,
            turn_id,
            command_id,
            defaults_version,
        } => {
            let input = input.ok_or(ClientError::Input("standard-input content was not read"))?;
            reconcile(
                &mut client,
                &mut output,
                session_id,
                turn_id,
                command_id,
                defaults_version,
                input,
            )
            .await
        }
        Command::Review(command) => {
            review(&mut client, &mut output, *command, deployment_limits).await
        }
        Command::Stop {
            session_id,
            turn_id,
            command_id,
            defaults_version,
            descendants,
        } => {
            let input = input.ok_or(ClientError::Input("standard-input content was not read"))?;
            stop(
                &mut client,
                &mut output,
                session_id,
                turn_id,
                command_id,
                defaults_version,
                descendants,
                input,
            )
            .await
        }
        Command::Approve {
            session_id,
            tool_request_id,
            command_id,
        } => {
            decide(
                &mut client,
                &mut output,
                session_id,
                tool_request_id,
                command_id,
                ToolDecision::Approve {},
            )
            .await
        }
        Command::Deny {
            session_id,
            tool_request_id,
            reason,
            command_id,
        } => {
            decide(
                &mut client,
                &mut output,
                session_id,
                tool_request_id,
                command_id,
                ToolDecision::Deny { reason },
            )
            .await
        }
    }
}

async fn open_import_source(path: &Path) -> Result<tokio::fs::File, ClientError> {
    tokio::fs::File::open(path)
        .await
        .map_err(ClientError::source_file)
}

fn open_blob_source(path: &Path) -> Result<PreparedBlobSource, ClientError> {
    let descriptor = openat(
        CWD,
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .map_err(|source| ClientError::blob_source_file(path, source))?;
    let status = fstat(&descriptor)
        .map_err(std::io::Error::from)
        .map_err(|source| ClientError::blob_source_file(path, source))?;
    if FileType::from_raw_mode(status.st_mode) != FileType::RegularFile {
        return Err(ClientError::blob_source_file(
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "blob upload source is not a regular file",
            ),
        ));
    }
    Ok(PreparedBlobSource {
        path: path.to_path_buf(),
        file: tokio::fs::File::from_std(File::from(descriptor)),
    })
}

async fn upload_blob(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    source: PreparedBlobSource,
) -> Result<(), ClientError> {
    let PreparedBlobSource { path, mut file } = source;
    let (expected_digest, expected_length_bytes) = hash_blob_source(&mut file, &path).await?;
    if expected_length_bytes.value() == 0 {
        return Err(ClientError::Input("blob source must be nonempty"));
    }
    let first = upload_blob_once(
        client,
        output,
        &mut file,
        &path,
        expected_digest,
        expected_length_bytes,
    )
    .await;
    match first {
        Err(error) if error.is_ambiguous_mutation() => {
            verify_blob_source_unchanged(&mut file, &path, expected_digest, expected_length_bytes)
                .await?;
            upload_blob_once(
                client,
                output,
                &mut file,
                &path,
                expected_digest,
                expected_length_bytes,
            )
            .await
        }
        result => result,
    }
}

async fn upload_blob_once(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    file: &mut tokio::fs::File,
    path: &Path,
    expected_digest: CanonicalBlobDigest,
    expected_length_bytes: CanonicalU64,
) -> Result<(), ClientError> {
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(|source| ClientError::blob_source_file(path, source))?;
    let mut connection = client
        .setup_request(ClientRequest::BeginBlobUpload {
            expected_digest,
            expected_length_bytes,
        })
        .await?;
    match classify_blob_upload_response(connection.message().await?) {
        BlobUploadResponse::AlreadyPresent {
            digest,
            byte_length,
        } if digest == expected_digest && byte_length == expected_length_bytes => {
            verify_blob_source_unchanged(file, path, expected_digest, expected_length_bytes)
                .await?;
            output.blob_uploaded(
                digest,
                byte_length.value(),
                BlobUploadPresentation::AlreadyPresent,
            )?;
            return Ok(());
        }
        BlobUploadResponse::Begun {
            digest,
            byte_length,
        } if digest == expected_digest && byte_length == expected_length_bytes => {}
        BlobUploadResponse::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        BlobUploadResponse::AlreadyPresent { .. }
        | BlobUploadResponse::Begun { .. }
        | BlobUploadResponse::Appended(_)
        | BlobUploadResponse::Committed { .. }
        | BlobUploadResponse::Unexpected => {
            return Err(ClientError::Protocol(
                "blob upload begin returned an unexpected response",
            ));
        }
    }

    let mut assembled_length = 0_u64;
    loop {
        let mut chunk = Vec::with_capacity(MAX_BLOB_CHUNK_BYTES);
        (&mut *file)
            .take(u64::try_from(MAX_BLOB_CHUNK_BYTES).unwrap_or(u64::MAX))
            .read_to_end(&mut chunk)
            .await
            .map_err(|source| ClientError::blob_source_file(path, source))?;
        if chunk.is_empty() {
            break;
        }
        assembled_length = assembled_length
            .checked_add(u64::try_from(chunk.len()).map_err(|_| {
                ClientError::Protocol("blob upload chunk length is not representable")
            })?)
            .ok_or(ClientError::Protocol("blob upload length overflowed"))?;
        client
            .continue_setup_request(
                &mut connection,
                ClientRequest::AppendBlobUpload {
                    chunk: BlobChunk::new(chunk),
                },
            )
            .await?;
        match classify_blob_upload_response(connection.message().await?) {
            BlobUploadResponse::Appended(admitted) if admitted.value() == assembled_length => {}
            BlobUploadResponse::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(code, message, detail)),
            BlobUploadResponse::Begun { .. }
            | BlobUploadResponse::AlreadyPresent { .. }
            | BlobUploadResponse::Appended(_)
            | BlobUploadResponse::Committed { .. }
            | BlobUploadResponse::Unexpected => {
                return Err(ClientError::Protocol(
                    "blob upload append returned an unexpected response",
                ));
            }
        }
    }

    client
        .continue_mutation_request(&mut connection, ClientRequest::CommitBlobUpload {})
        .await?;
    match classify_blob_upload_response(connection.message().await.map_err(ClientError::mutation)?)
    {
        BlobUploadResponse::Committed {
            digest,
            byte_length,
        } if digest == expected_digest && byte_length == expected_length_bytes => {
            output.blob_uploaded(
                digest,
                byte_length.value(),
                BlobUploadPresentation::Committed,
            )?;
            Ok(())
        }
        BlobUploadResponse::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        BlobUploadResponse::Begun { .. }
        | BlobUploadResponse::AlreadyPresent { .. }
        | BlobUploadResponse::Appended(_)
        | BlobUploadResponse::Committed { .. }
        | BlobUploadResponse::Unexpected => Err(ClientError::Protocol(
            "blob upload commit returned an unexpected response",
        )
        .mutation()),
    }
}

async fn read_blob_metadata(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    digest: CanonicalBlobDigest,
) -> Result<(), ClientError> {
    let mut connection = client
        .setup_request(ClientRequest::ReadBlobMetadata { digest })
        .await?;
    match connection.message().await? {
        ServerMessage::BlobMetadata {
            digest: returned_digest,
            byte_length,
            replica_count,
        } if returned_digest == digest => output
            .blob_metadata(digest, byte_length.value(), replica_count.value())
            .map_err(ClientError::from),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail)),
        _ => Err(ClientError::Protocol(
            "blob metadata returned an unexpected response",
        )),
    }
}

async fn read_blob_chunk(
    client: &mut ProcessClient,
    digest: CanonicalBlobDigest,
    offset_bytes: CanonicalU64,
    length_bytes: CanonicalU64,
) -> Result<Vec<u8>, ClientError> {
    if !(1..=MAX_BLOB_READ_BYTES as u64).contains(&length_bytes.value()) {
        return Err(ClientError::BlobReadLengthOutOfRange);
    }
    let mut connection = client
        .setup_request(ClientRequest::ReadBlobChunk {
            digest,
            offset_bytes,
            length_bytes,
        })
        .await?;
    match connection.message().await? {
        ServerMessage::BlobChunkRead {
            digest: returned_digest,
            offset_bytes: returned_offset,
            bytes,
        } if returned_digest == digest
            && returned_offset == offset_bytes
            && u64::try_from(bytes.as_bytes().len()) == Ok(length_bytes.value()) =>
        {
            Ok(bytes.into_bytes())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail)),
        _ => Err(ClientError::Protocol(
            "blob range returned an unexpected response",
        )),
    }
}

async fn write_blob_output(path: &Path, bytes: &[u8]) -> Result<(), ClientError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|source| ClientError::blob_output_file(path, source))?;
    fchmod(temporary.as_file(), Mode::RUSR | Mode::WUSR)
        .map_err(std::io::Error::from)
        .map_err(|source| ClientError::blob_output_file(path, source))?;
    let mut file = tokio::fs::File::from_std(
        temporary
            .reopen()
            .map_err(|source| ClientError::blob_output_file(path, source))?,
    );
    file.write_all(bytes)
        .await
        .map_err(|source| ClientError::blob_output_file(path, source))?;
    file.sync_all()
        .await
        .map_err(|source| ClientError::blob_output_file(path, source))?;
    drop(file);
    temporary
        .persist_noclobber(path)
        .map_err(|error| ClientError::blob_output_file(path, error.error))?;
    tokio::fs::File::open(parent)
        .await
        .map_err(|source| ClientError::blob_output_file(path, source))?
        .sync_all()
        .await
        .map_err(|source| ClientError::blob_output_file(path, source))?;
    Ok(())
}

async fn hash_blob_source(
    file: &mut tokio::fs::File,
    path: &Path,
) -> Result<(CanonicalBlobDigest, CanonicalU64), ClientError> {
    let mut digest = Sha256::new();
    let mut observed_length = 0_u64;
    let mut buffer = vec![0_u8; BLOB_HASH_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|source| ClientError::blob_source_file(path, source))?;
        if read == 0 {
            break;
        }
        observed_length = observed_length
            .checked_add(u64::try_from(read).map_err(|_| {
                ClientError::Protocol("blob source read length is not representable")
            })?)
            .ok_or(ClientError::Protocol("blob source length overflowed"))?;
        digest.update(&buffer[..read]);
    }
    Ok((
        CanonicalBlobDigest::from_bytes(digest.finalize().into()),
        CanonicalU64::new(observed_length),
    ))
}

async fn verify_blob_source_unchanged(
    file: &mut tokio::fs::File,
    path: &Path,
    expected_digest: CanonicalBlobDigest,
    expected_length: CanonicalU64,
) -> Result<(), ClientError> {
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(|source| ClientError::blob_source_file(path, source))?;
    let (actual_digest, actual_length) = hash_blob_source(file, path).await?;
    if actual_digest != expected_digest || actual_length != expected_length {
        return Err(ClientError::Input(
            "blob source changed after it was hashed",
        ));
    }
    Ok(())
}

async fn read_import_file(file: tokio::fs::File) -> Result<Vec<u8>, ClientError> {
    let read_limit = u64::try_from(MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES)
        .ok()
        .and_then(|bound| bound.checked_add(1))
        .ok_or(ClientError::Protocol(
            "conversation import read bound overflow",
        ))?;
    let mut source = Vec::new();
    file.take(read_limit)
        .read_to_end(&mut source)
        .await
        .map_err(ClientError::source_file)?;
    if source.len() > MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES {
        return Err(ClientError::SourceExceedsFrame);
    }
    Ok(source)
}

async fn read_review_json_file<Value: DeserializeOwned>(path: &Path) -> Result<Value, ClientError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(ClientError::review_input_file)?;
    let read_limit = u64::try_from(MAX_REVIEW_JSON_INPUT_BYTES)
        .ok()
        .and_then(|bound| bound.checked_add(1))
        .ok_or(ClientError::Protocol("review JSON read bound overflow"))?;
    let mut bounded = file.take(read_limit);
    let mut bytes = Vec::new();
    bounded
        .read_to_end(&mut bytes)
        .await
        .map_err(ClientError::review_input_file)?;
    if bytes.len() > MAX_REVIEW_JSON_INPUT_BYTES {
        return Err(ClientError::ReviewInputExceedsFrame);
    }
    serde_json::from_slice(&bytes).map_err(ClientError::review_input_json)
}

async fn read_system_prompt_file(path: &Path) -> Result<SystemPromptText, ClientError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(ClientError::system_prompt_file)?;
    let read_limit = u64::try_from(MAX_SYSTEM_PROMPT_FRAME_BYTES)
        .ok()
        .and_then(|bound| bound.checked_add(1))
        .ok_or(ClientError::Protocol("system prompt read bound overflow"))?;
    let mut bounded = file.take(read_limit);
    let mut bytes = Vec::new();
    bounded
        .read_to_end(&mut bytes)
        .await
        .map_err(ClientError::system_prompt_file)?;
    if bytes.is_empty() {
        return Err(ClientError::Input(
            "the system prompt file must not be empty",
        ));
    }
    if bytes.len() > MAX_SYSTEM_PROMPT_FRAME_BYTES {
        return Err(ClientError::Input(
            "the system prompt exceeds the wire-frame byte limit",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ClientError::Input("the system prompt must be valid UTF-8"))?;
    SystemPromptText::try_new(text)
        .map_err(|_| ClientError::Input("the system prompt must not contain U+0000"))
}

async fn read_goal_text_argument(argument: GoalTextArgument) -> Result<String, ClientError> {
    match argument {
        GoalTextArgument::Inline(text) => validate_goal_text_input(text),
        GoalTextArgument::File(path) => read_goal_text_file(&path).await,
    }
}

async fn read_delegation_text_argument(
    argument: DelegationTextArgument,
) -> Result<String, ClientError> {
    match argument {
        DelegationTextArgument::Inline(text) => validate_delegation_content(text),
        DelegationTextArgument::File(path) => read_delegation_content_file(&path).await,
    }
}

async fn read_delegation_content_file(path: &Path) -> Result<String, ClientError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| ClientError::delegation_content_file(path, error))?;
    let read_limit = u64::try_from(MAX_CONTENT_FRAGMENT_BYTES)
        .ok()
        .and_then(|bound| bound.checked_add(1))
        .ok_or(ClientError::Protocol(
            "delegation content read bound overflow",
        ))?;
    let mut bounded = file.take(read_limit);
    let mut bytes = Vec::new();
    bounded
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| ClientError::delegation_content_file(path, error))?;
    if bytes.len() > MAX_CONTENT_FRAGMENT_BYTES {
        return Err(ClientError::Input(
            "delegation content exceeds the 1 MiB UTF-8 byte limit",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|error| ClientError::delegation_content_file_utf8(path, error))?;
    validate_delegation_content(text)
}

fn validate_delegation_content(text: String) -> Result<String, ClientError> {
    if text.is_empty() || text.len() > MAX_CONTENT_FRAGMENT_BYTES || text.contains('\0') {
        return Err(ClientError::Input(
            "delegation content must be nonempty, at most 1 MiB, and contain no U+0000",
        ));
    }
    Ok(text)
}

async fn read_goal_text_file(path: &Path) -> Result<String, ClientError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| ClientError::goal_text_file(path, error))?;
    let read_limit = u64::try_from(MAX_CONTENT_FRAGMENT_BYTES)
        .ok()
        .and_then(|bound| bound.checked_add(1))
        .ok_or(ClientError::Protocol("goal text read bound overflow"))?;
    let mut bounded = file.take(read_limit);
    let mut bytes = Vec::new();
    bounded
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| ClientError::goal_text_file(path, error))?;
    if bytes.len() > MAX_CONTENT_FRAGMENT_BYTES {
        return Err(ClientError::Input(
            "goal text exceeds the 1 MiB UTF-8 byte limit",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ClientError::Input("goal text must be valid UTF-8"))?;
    validate_goal_text_input(text)
}

fn validate_goal_text_input(text: String) -> Result<String, ClientError> {
    if text.is_empty() {
        return Err(ClientError::Input("goal text must not be empty"));
    }
    if text.len() > MAX_CONTENT_FRAGMENT_BYTES {
        return Err(ClientError::Input(
            "goal text exceeds the 1 MiB UTF-8 byte limit",
        ));
    }
    if text.contains('\0') {
        return Err(ClientError::Input("goal text must not contain U+0000"));
    }
    Ok(text)
}

fn socket_path(
    override_path: Option<PathBuf>,
    socket_environment: Option<OsString>,
) -> Result<PathBuf, ClientError> {
    let path = match override_path {
        Some(path) if !path.as_os_str().is_empty() => path,
        Some(_) => return Err(ClientError::Input("--socket requires a nonempty path")),
        None => {
            let value = socket_environment.ok_or(ClientError::Input(
                "set SIGNALBOX_SOCKET_PATH or pass --socket",
            ))?;
            if value.is_empty() {
                return Err(ClientError::Input(
                    "set SIGNALBOX_SOCKET_PATH or pass --socket",
                ));
            }
            PathBuf::from(value)
        }
    };
    if !path.is_absolute() {
        return Err(ClientError::Input(
            "the local process socket path must be absolute",
        ));
    }
    Ok(path)
}

fn read_input(stdin: &mut dyn Read) -> Result<String, ClientError> {
    let mut bytes = Vec::new();
    stdin
        .take((MAX_INPUT_CONTENT_FRAME_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Err(ClientError::Input(
            "standard-input content must not be empty",
        ));
    }
    if bytes.len() > MAX_INPUT_CONTENT_FRAME_BYTES {
        return Err(ClientError::Input(
            "standard-input content exceeds the wire-frame UTF-8 byte guard",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ClientError::Input("standard-input content must be valid UTF-8"))?;
    if text.contains('\0') {
        return Err(ClientError::Input(
            "standard-input content must not contain U+0000",
        ));
    }
    Ok(text)
}

async fn create(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    selection: ModelSelection,
    command_id: Option<CommandId>,
    system_prompt: Option<SystemPromptText>,
    placement: SessionPlacement,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::CreateSession {
            command_id,
            initial_model_selection: selection,
            model_settings: ModelSettingsOverlay::inherit_all(),
            system_prompt: SystemPromptMember::present(system_prompt),
            placement,
            lifecycle: SessionLifecycleMembers::default(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionCreated {
            session_id,
            model_settings,
        } if model_settings.matches_model(&selection) => {
            output.session_created(session_id)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("create returned an unexpected response").mutation()),
    }
}

async fn session_delegation(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    command: SessionCommand,
) -> Result<(), ClientError> {
    match command {
        SessionCommand::Spawn {
            session_id,
            turn_id,
            tool_request_id,
            task,
            relationship,
        } => {
            let task = read_delegation_text_argument(task).await?;
            let mut connection = client
                .mutation_request(ClientRequest::SpawnSession {
                    session_id,
                    turn_id,
                    tool_request_id,
                    task,
                    relationship,
                })
                .await?;
            match classify_delegation_response(
                connection.message().await.map_err(ClientError::mutation)?,
            ) {
                DelegationResponse::Spawned {
                    tool_request_id: recorded_request,
                    child_session_id,
                    relationship: recorded_relationship,
                } => {
                    if recorded_request == tool_request_id
                        && child_session_id != session_id
                        && recorded_relationship == relationship
                    {
                        output.session_spawned(SessionSpawnedPresentation {
                            tool_request_id,
                            child_session_id,
                            relationship,
                        })?;
                        Ok(())
                    } else {
                        Err(
                            ClientError::Protocol("spawn returned an unexpected receipt")
                                .mutation(),
                        )
                    }
                }
                DelegationResponse::Error {
                    code,
                    message,
                    detail,
                } => {
                    if code == ErrorCode::Rejected
                        && !delegation_rejection_matches(
                            detail.value(),
                            DelegationRejectionExpectation {
                                session: session_id,
                                turn: turn_id,
                                tool_request: tool_request_id,
                                operation: DelegationRejectionOperation::Spawn,
                            },
                        )
                    {
                        return Err(ClientError::Protocol(
                            "spawn returned an incoherent rejection",
                        )
                        .mutation());
                    }
                    Err(ClientError::remote(code, message, detail).mutation())
                }
                DelegationResponse::AwaitRegistered { .. }
                | DelegationResponse::ChildResult { .. }
                | DelegationResponse::MessageSent { .. }
                | DelegationResponse::Unexpected => {
                    Err(ClientError::Protocol("spawn returned an unexpected receipt").mutation())
                }
            }
        }
        SessionCommand::Await {
            session_id,
            turn_id,
            tool_request_id,
            child_session_id,
            mode,
        } => {
            let mut connection = client
                .mutation_request(ClientRequest::AwaitSession {
                    session_id,
                    turn_id,
                    tool_request_id,
                    child_session_id,
                    mode,
                })
                .await?;
            match classify_delegation_response(
                connection.message().await.map_err(ClientError::mutation)?,
            ) {
                DelegationResponse::AwaitRegistered {
                    tool_request_id: recorded_request,
                    child_session_id: recorded_child,
                    mode: recorded_mode,
                } => {
                    if recorded_request == tool_request_id
                        && recorded_child == child_session_id
                        && recorded_mode == mode
                        && mode == DelegationWaitMode::Background
                        && session_id != child_session_id
                    {
                        output.session_await_registered(SessionAwaitRegisteredPresentation {
                            tool_request_id,
                            child_session_id,
                            mode,
                        })?;
                        Ok(())
                    } else {
                        Err(
                            ClientError::Protocol("await returned an unexpected response")
                                .mutation(),
                        )
                    }
                }
                DelegationResponse::ChildResult {
                    await_request_id: recorded_request,
                    spawning_request_id,
                    child_session_id: recorded_child,
                    outcome,
                    content,
                    reason,
                    provenance,
                } => {
                    if mode == DelegationWaitMode::Foreground
                        && recorded_request == tool_request_id
                        && recorded_child == child_session_id
                        && session_id != child_session_id
                        && delegation_provenance_matches(
                            DelegationProvenanceExpectation {
                                parent_session_id: session_id,
                                child_session_id,
                            },
                            provenance,
                        )
                    {
                        output.child_result(ChildResultPresentation {
                            await_request_id: tool_request_id,
                            spawning_request_id,
                            child_session_id,
                            outcome,
                            content: content.as_ref(),
                            reason,
                            provenance,
                        })?;
                        Ok(())
                    } else {
                        Err(
                            ClientError::Protocol("await returned an unexpected response")
                                .mutation(),
                        )
                    }
                }
                DelegationResponse::Error {
                    code,
                    message,
                    detail,
                } => {
                    if code == ErrorCode::Rejected
                        && !delegation_rejection_matches(
                            detail.value(),
                            DelegationRejectionExpectation {
                                session: session_id,
                                turn: turn_id,
                                tool_request: tool_request_id,
                                operation: DelegationRejectionOperation::Await {
                                    child: child_session_id,
                                    mode,
                                },
                            },
                        )
                    {
                        return Err(ClientError::Protocol(
                            "await returned an incoherent rejection",
                        )
                        .mutation());
                    }
                    Err(ClientError::remote(code, message, detail).mutation())
                }
                DelegationResponse::Spawned { .. }
                | DelegationResponse::MessageSent { .. }
                | DelegationResponse::Unexpected => {
                    Err(ClientError::Protocol("await returned an unexpected response").mutation())
                }
            }
        }
        SessionCommand::Message {
            session_id,
            turn_id,
            tool_request_id,
            peer_session_id,
            content,
        } => {
            let content = read_delegation_text_argument(content).await?;
            let mut connection = client
                .mutation_request(ClientRequest::SendSessionMessage {
                    session_id,
                    turn_id,
                    tool_request_id,
                    peer_session_id,
                    content,
                })
                .await?;
            match classify_delegation_response(
                connection.message().await.map_err(ClientError::mutation)?,
            ) {
                DelegationResponse::MessageSent {
                    tool_request_id: recorded_request,
                    message_id,
                    direction,
                    ordinal,
                    delivery_sequence,
                } => {
                    if recorded_request == tool_request_id && session_id != peer_session_id {
                        output.session_message_sent(SessionMessageSentPresentation {
                            tool_request_id,
                            peer_session_id,
                            message_id,
                            direction,
                            ordinal: ordinal.value(),
                            delivery_sequence: delivery_sequence.value(),
                        })?;
                        Ok(())
                    } else {
                        Err(
                            ClientError::Protocol("message returned an unexpected receipt")
                                .mutation(),
                        )
                    }
                }
                DelegationResponse::Error {
                    code,
                    message,
                    detail,
                } => {
                    if code == ErrorCode::Rejected
                        && !delegation_rejection_matches(
                            detail.value(),
                            DelegationRejectionExpectation {
                                session: session_id,
                                turn: turn_id,
                                tool_request: tool_request_id,
                                operation: DelegationRejectionOperation::Message {
                                    peer: peer_session_id,
                                },
                            },
                        )
                    {
                        return Err(ClientError::Protocol(
                            "message returned an incoherent rejection",
                        )
                        .mutation());
                    }
                    Err(ClientError::remote(code, message, detail).mutation())
                }
                DelegationResponse::Spawned { .. }
                | DelegationResponse::AwaitRegistered { .. }
                | DelegationResponse::ChildResult { .. }
                | DelegationResponse::Unexpected => {
                    Err(ClientError::Protocol("message returned an unexpected receipt").mutation())
                }
            }
        }
    }
}

struct DelegationProvenanceExpectation {
    parent_session_id: CanonicalUuid,
    child_session_id: CanonicalUuid,
}

fn delegation_provenance_matches(
    expectation: DelegationProvenanceExpectation,
    provenance: DelegationProvenance,
) -> bool {
    match provenance {
        DelegationProvenance::ChildTurn {
            child_session_id, ..
        } => child_session_id == expectation.child_session_id,
        DelegationProvenance::ParentTurnCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentGoalCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentLifecycleCommand {
            parent_session_id, ..
        } => parent_session_id == expectation.parent_session_id,
    }
}

async fn create_from_template(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    template_name: String,
    command_id: Option<CommandId>,
    placement: SessionPlacement,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::CreateSessionFromTemplate {
            command_id,
            template_name,
            placement,
            lifecycle: SessionLifecycleMembers::default(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionCreated { session_id, .. } => {
            output.session_created(session_id)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(
            ClientError::Protocol("template creation returned an unexpected response").mutation(),
        ),
    }
}

async fn update_session_placement(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    expected_placement_version: CanonicalU64,
    replacement: SessionPlacement,
    command_id: Option<CommandId>,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    let requested_session = session_id;
    let requested_replacement = replacement.clone();
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::UpdateSessionPlacement {
            command_id,
            session_id,
            expected_placement_version,
            replacement,
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionPlacementUpdated {
            session_id,
            placement_version,
            placement,
        } => {
            if !placement_update_receipt_matches(
                session_id,
                placement_version,
                &placement,
                requested_session,
                expected_placement_version,
                &requested_replacement,
            ) {
                return Err(
                    ClientError::Protocol("place returned an incoherent receipt").mutation(),
                );
            }
            output.session_placement_updated(
                session_id,
                placement_version.value(),
                &placement_display(&placement),
            )?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => {
            if code == ErrorCode::Rejected
                && !placement_update_rejection_matches(
                    detail.value(),
                    requested_session,
                    expected_placement_version,
                )
            {
                return Err(ClientError::Protocol("place returned incoherent rejection").mutation());
            }
            Err(ClientError::remote(code, message, detail).mutation())
        }
        _ => Err(ClientError::Protocol("place returned an unexpected response").mutation()),
    }
}

fn placement_update_receipt_matches(
    actual_session: CanonicalUuid,
    actual_version: CanonicalU64,
    actual_placement: &SessionPlacement,
    requested_session: CanonicalUuid,
    expected_version: CanonicalU64,
    requested_placement: &SessionPlacement,
) -> bool {
    actual_session == requested_session
        && expected_version.value().checked_add(1) == Some(actual_version.value())
        && actual_placement == requested_placement
}

fn placement_update_rejection_matches(
    detail: Option<RejectionDetail>,
    requested_session: CanonicalUuid,
    expected_version: CanonicalU64,
) -> bool {
    match detail {
        Some(RejectionDetail::SessionNotFound { session_id }) => session_id == requested_session,
        Some(RejectionDetail::SessionPlacementCurrentVersionMismatch {
            session_id,
            expected_placement_version,
            ..
        }) => session_id == requested_session && expected_placement_version == expected_version,
        Some(RejectionDetail::SessionPlacementVersionExhausted {
            session_id,
            current_placement_version,
        }) => session_id == requested_session && current_placement_version == expected_version,
        _ => false,
    }
}

async fn continue_imported(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    imported_conversation_id: CanonicalUuid,
    through_position: ThroughPositionArgument,
    relationship: signalbox_process_protocol::ImportedSessionRelationship,
    selection: ModelSelection,
    command_id: Option<CommandId>,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    // An imported conversation is immutable, so its final position is stable:
    // resolving the sentinel here and sending the concrete ordinal keeps the
    // durable command byte-exact under replay.
    let through_position = match through_position {
        ThroughPositionArgument::Exact(position) => position,
        ThroughPositionArgument::Latest => {
            // The reader already rejects an empty inventory, so the resolved
            // count is a selectable position.
            let entry_count =
                read_imported_conversation(client, imported_conversation_id, |_| Ok(())).await?;
            output.resolved_through_position(entry_count)?;
            CanonicalU64::new(entry_count)
        }
    };
    let mut connection = client
        .mutation_request(ClientRequest::CreateSessionFromImportedFrontier {
            command_id,
            imported_conversation_id,
            through_position,
            relationship,
            initial_model_selection: selection,
            model_settings: ModelSettingsOverlay::inherit_all(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionCreated {
            session_id,
            model_settings,
        } if model_settings.matches_model(&selection) => {
            output.session_created(session_id)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("continue returned an unexpected response").mutation()),
    }
}

async fn compact(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    through_position: Option<CanonicalU64>,
    command_id: Option<CommandId>,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::CompactSession {
            command_id,
            session_id,
            through_position,
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionCompacted {
            session_id: compacted_session,
            context_compaction_id,
            model_call_id,
            through_position,
            summary_entry_id,
            result_frontier_id,
        } if compacted_session == session_id => Ok(output.session_compacted(
            session_id,
            context_compaction_id,
            model_call_id,
            through_position.value(),
            summary_entry_id,
            result_frontier_id,
        )?),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("compact returned an unexpected response").mutation()),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ObservedSessionDefaults {
    version: CanonicalU64,
    model_settings: ModelSettingsOverlay,
    dangerous_tool_auto_approval: bool,
    system_prompt: Option<SystemPromptText>,
}

/// The model verb's resolved replacement choice for the session system
/// prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ModelSystemPromptChoice {
    /// Copy the observed epoch's exact prompt forward unchanged.
    Keep,
    /// Replace the prompt with exact user-supplied file content.
    Replace(SystemPromptText),
    /// Install the replacement epoch without a prompt.
    Clear,
}

#[allow(clippy::too_many_arguments)]
async fn replace_session_model(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    selection: ModelSelection,
    command_id: Option<CommandId>,
    defaults_version: Option<CanonicalU64>,
    dangerous_tool_auto_approval: Option<DangerousToolAutoApprovalArgument>,
    system_prompt: ModelSystemPromptChoice,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let observed = match (defaults_version, dangerous_tool_auto_approval) {
        (Some(version), Some(posture)) => {
            // Recovery pins version and posture from the printed facts. A
            // copied-forward prompt is re-read from the immutable epoch the
            // printed version names, so the retried payload is byte-exact
            // regardless of later concurrent replacements.
            let named_defaults = read_session_defaults(client, session_id, Some(version)).await?;
            let system_prompt = match &system_prompt {
                ModelSystemPromptChoice::Keep => named_defaults.system_prompt,
                ModelSystemPromptChoice::Replace(_) | ModelSystemPromptChoice::Clear => None,
            };
            ObservedSessionDefaults {
                version,
                model_settings: named_defaults.model_settings,
                dangerous_tool_auto_approval: matches!(
                    posture,
                    DangerousToolAutoApprovalArgument::ApproveAll
                ),
                system_prompt,
            }
        }
        (None, None) => read_session_defaults(client, session_id, None).await?,
        (Some(_), None) | (None, Some(_)) => {
            return Err(ClientError::Input(
                "model recovery requires the complete printed defaults facts",
            ));
        }
    };
    output.recovery_value("defaults_version", &observed.version.value().to_string())?;
    output.recovery_value(
        "dangerous_tool_auto_approval",
        if observed.dangerous_tool_auto_approval {
            "approve-all"
        } else {
            "disabled"
        },
    )?;
    let replacement_system_prompt = match system_prompt {
        ModelSystemPromptChoice::Keep => observed.system_prompt.clone(),
        ModelSystemPromptChoice::Replace(text) => Some(text),
        ModelSystemPromptChoice::Clear => None,
    };

    let mut connection = client
        .mutation_request(ClientRequest::ReplaceSessionDefaults {
            command_id,
            session_id,
            expected_defaults_version: observed.version,
            model_selection: selection,
            model_settings: observed.model_settings,
            dangerous_tool_auto_approval: observed.dangerous_tool_auto_approval,
            system_prompt: SystemPromptMember::present(replacement_system_prompt.clone()),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionDefaultsReplaced {
            session_id: replaced_session,
            defaults_version: installed_version,
            model_selection,
            model_settings,
            dangerous_tool_auto_approval,
            system_prompt: receipt_system_prompt,
            ..
        } if replaced_session == session_id
            && model_selection == selection
            && replacement_receipt_settings_match(observed.model_settings, &model_settings)
            && dangerous_tool_auto_approval == observed.dangerous_tool_auto_approval
            && receipt_system_prompt.value() == Some(&replacement_system_prompt)
            && observed
                .version
                .value()
                .checked_add(1)
                .is_some_and(|expected| installed_version.value() == expected) =>
        {
            output.session_defaults_replaced(
                replaced_session,
                installed_version.value(),
                &selection_display(model_selection),
            )?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(
            ClientError::Protocol("model replacement returned an unexpected response").mutation(),
        ),
    }
}

fn replacement_receipt_settings_match(
    requested: ModelSettingsOverlay,
    returned: &signalbox_process_protocol::ModelSettingsSnapshot,
) -> bool {
    returned.precedence.session == requested
}

async fn read_session_defaults(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
    defaults_version: Option<CanonicalU64>,
) -> Result<ObservedSessionDefaults, ClientError> {
    let mut connection = client
        .request(ClientRequest::ReadSessionDefaults {
            session_id,
            defaults_version,
        })
        .await?;
    match connection.message().await? {
        ServerMessage::SessionDefaults {
            session_id: read_session,
            defaults_version: read_version,
            model_selection: _,
            model_settings,
            dangerous_tool_auto_approval,
            system_prompt,
            ..
        } if read_session == session_id
            && defaults_version.is_none_or(|named| named == read_version) =>
        {
            Ok(ObservedSessionDefaults {
                version: read_version,
                model_settings: model_settings.precedence.session,
                dangerous_tool_auto_approval,
                system_prompt,
            })
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail)),
        _ => Err(ClientError::Protocol(
            "session defaults read returned an unexpected response",
        )),
    }
}

async fn read_session_metadata_page(
    client: &mut ProcessClient,
    page: &SessionMetadataPageRequest,
    mut consume: impl FnMut(&ServerFrame) -> Result<(), ClientError>,
) -> Result<Option<CanonicalUuid>, ClientError> {
    let mut connection = client.request(page.request()).await?;
    match connection.message().await? {
        ServerMessage::SessionMetadataPageStart {} => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "session metadata page did not begin with its start frame",
            ));
        }
    }
    let mut prior_session = page.after_session_id;
    let mut last_in_page = None;
    let mut summary_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::SessionMetadataSummary { session_id, .. } => {
                if prior_session
                    .is_some_and(|prior: CanonicalUuid| prior.into_uuid() >= session_id.into_uuid())
                {
                    return Err(ClientError::Protocol(
                        "session metadata summaries were not strictly ordered",
                    ));
                }
                summary_count = summary_count.checked_add(1).ok_or(ClientError::Protocol(
                    "session metadata summary count overflowed",
                ))?;
                if summary_count > page.page_size.value() {
                    return Err(ClientError::Protocol(
                        "session metadata page exceeded its requested bound",
                    ));
                }
                prior_session = Some(*session_id);
                last_in_page = Some(*session_id);
                consume(&frame)?;
            }
            ServerMessage::SessionMetadataPageEnd {
                session_count,
                next_after_session_id,
            } => {
                if session_count.value() != summary_count
                    || next_after_session_id.is_some() && *next_after_session_id != last_in_page
                {
                    return Err(ClientError::Protocol(
                        "session metadata page count or cursor was invalid",
                    ));
                }
                return Ok(*next_after_session_id);
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "session metadata page sequence or count was invalid",
                ));
            }
        }
    }
}

fn collect_import_paths(root: &Path) -> Result<PreparedImportScan, ClientError> {
    let root_metadata = std::fs::symlink_metadata(root).map_err(ClientError::scan_directory)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(ClientError::Input("--scan requires a directory"));
    }

    let root_fd = openat(
        CWD,
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .map_err(ClientError::scan_directory)?;
    let root_directory = Dir::read_from(&root_fd)
        .map_err(std::io::Error::from)
        .map_err(ClientError::scan_directory)?;
    let mut pending = vec![(PathBuf::new(), root_directory)];
    let mut paths = Vec::new();
    while let Some((relative_directory, directory)) = pending.last_mut() {
        let Some(entry) = directory.read() else {
            pending.pop();
            continue;
        };
        let entry = entry
            .map_err(std::io::Error::from)
            .map_err(ClientError::scan_directory)?;
        let name_bytes = entry.file_name().to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        let name = OsStr::from_bytes(name_bytes);
        let relative = relative_directory.join(name);
        let descriptor = directory
            .fd()
            .map_err(std::io::Error::from)
            .map_err(ClientError::scan_directory)?;
        let status = statat(descriptor, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(std::io::Error::from)
            .map_err(ClientError::scan_directory)?;
        match FileType::from_raw_mode(status.st_mode) {
            FileType::Directory => {
                let child = openat(
                    descriptor,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(std::io::Error::from)
                .map_err(ClientError::scan_directory)?;
                let child = Dir::new(child)
                    .map_err(std::io::Error::from)
                    .map_err(ClientError::scan_directory)?;
                pending.push((relative, child));
            }
            FileType::RegularFile if relative.extension() == Some(OsStr::new("jsonl")) => {
                paths.push(ScannedImportPath {
                    display: root.join(&relative),
                    relative,
                });
            }
            FileType::RegularFile
            | FileType::Symlink
            | FileType::Fifo
            | FileType::Socket
            | FileType::CharacterDevice
            | FileType::BlockDevice
            | FileType::Unknown => {}
        }
    }
    paths.sort_by(|left, right| left.display.cmp(&right.display));
    Ok(PreparedImportScan {
        root: root_fd,
        paths,
    })
}

fn open_scanned_import_source(
    root: &OwnedFd,
    relative: &Path,
) -> Result<tokio::fs::File, ClientError> {
    let mut components = relative.components().peekable();
    let mut current = None;
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return Err(ClientError::Protocol(
                "scan produced a non-relative candidate path",
            ));
        };
        let parent = current.as_ref().unwrap_or(root);
        let flags = if components.peek().is_some() {
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        } else {
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC
        };
        current = Some(
            openat(parent, name, flags, Mode::empty())
                .map_err(std::io::Error::from)
                .map_err(ClientError::source_file)?,
        );
    }
    let descriptor = current.ok_or(ClientError::Protocol(
        "scan produced an empty candidate path",
    ))?;
    let status = fstat(&descriptor)
        .map_err(std::io::Error::from)
        .map_err(ClientError::source_file)?;
    if FileType::from_raw_mode(status.st_mode) != FileType::RegularFile {
        return Err(ClientError::source_file(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "scan candidate is no longer a regular file",
        )));
    }
    Ok(tokio::fs::File::from_std(File::from(descriptor)))
}

fn source_fits_single_shot_import(
    format: ConversationImportFormat,
    source: &[u8],
    request_id: RequestId,
) -> Result<bool, ClientError> {
    if source.len() > MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES {
        return Ok(false);
    }
    let frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request_id,
        ClientRequest::ImportConversation {
            format,
            source: ConversationImportSource::new(source.to_vec()),
        },
    )
    .map_err(FrameEncodeError::Validation)?;
    match encode_client_line(&frame) {
        Ok(_) => Ok(true),
        Err(FrameEncodeError::OversizedFrame) => Ok(false),
        Err(FrameEncodeError::Validation(error)) => {
            Err(ClientError::Encode(FrameEncodeError::Validation(error)))
        }
        Err(FrameEncodeError::Json(error)) => {
            Err(ClientError::Encode(FrameEncodeError::Json(error)))
        }
    }
}

async fn import_conversation_file(
    client: &mut ProcessClient,
    format: ConversationImportFormat,
    file: tokio::fs::File,
) -> Result<ConversationImportOutcome, ClientError> {
    let declared_size_bytes = file
        .metadata()
        .await
        .map_err(ClientError::source_file)?
        .len();
    if declared_size_bytes
        <= u64::try_from(MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES).unwrap_or(u64::MAX)
    {
        let source = read_import_file(file).await?;
        import_conversation_source(client, format, source).await
    } else {
        import_conversation_chunked(client, format, CanonicalU64::new(declared_size_bytes), file)
            .await
    }
}

async fn import_conversation_source(
    client: &mut ProcessClient,
    format: ConversationImportFormat,
    source: Vec<u8>,
) -> Result<ConversationImportOutcome, ClientError> {
    if source_fits_single_shot_import(format, &source, client.pending_request_id()?)? {
        import_conversation(client, format, source).await
    } else {
        let declared_size_bytes = u64::try_from(source.len())
            .map(CanonicalU64::new)
            .map_err(|_| ClientError::Protocol("import source size is not representable"))?;
        import_conversation_chunked(client, format, declared_size_bytes, source.as_slice()).await
    }
}

async fn import_conversation_chunked<Source>(
    client: &mut ProcessClient,
    format: ConversationImportFormat,
    declared_size_bytes: CanonicalU64,
    mut source: Source,
) -> Result<ConversationImportOutcome, ClientError>
where
    Source: tokio::io::AsyncRead + Unpin,
{
    let mut connection = client
        .setup_request(ClientRequest::BeginConversationImport {
            format,
            declared_size_bytes,
        })
        .await?;
    match classify_conversation_import_response(connection.message().await?) {
        ConversationImportResponse::Begun(admitted) if admitted == declared_size_bytes => {}
        ConversationImportResponse::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        ConversationImportResponse::Begun(_)
        | ConversationImportResponse::Appended(_)
        | ConversationImportResponse::Inserted(_)
        | ConversationImportResponse::AlreadyImported(_)
        | ConversationImportResponse::Unexpected => {
            return Err(ClientError::Protocol(
                "conversation import begin returned an unexpected response",
            ));
        }
    }

    let mut assembled_size_bytes = 0_u64;
    loop {
        let read_limit =
            conversation_import_chunk_read_limit(declared_size_bytes, assembled_size_bytes);
        if read_limit == 0 {
            break;
        }
        let mut chunk = Vec::with_capacity(MAX_CONVERSATION_IMPORT_CHUNK_BYTES);
        (&mut source)
            .take(read_limit)
            .read_to_end(&mut chunk)
            .await
            .map_err(ClientError::source_file)?;
        let chunk_size = chunk.len();
        if chunk_size == 0 {
            break;
        }
        assembled_size_bytes = assembled_size_bytes
            .checked_add(u64::try_from(chunk_size).map_err(|_| {
                ClientError::Protocol("conversation import chunk size is not representable")
            })?)
            .ok_or(ClientError::Protocol(
                "conversation import assembled size overflowed",
            ))?;
        client
            .continue_setup_request(
                &mut connection,
                ClientRequest::AppendConversationImport {
                    chunk: ConversationImportSource::new(chunk),
                },
            )
            .await?;
        match classify_conversation_import_response(connection.message().await?) {
            ConversationImportResponse::Appended(admitted)
                if admitted.value() == assembled_size_bytes => {}
            ConversationImportResponse::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(code, message, detail)),
            ConversationImportResponse::Begun(_)
            | ConversationImportResponse::Appended(_)
            | ConversationImportResponse::Inserted(_)
            | ConversationImportResponse::AlreadyImported(_)
            | ConversationImportResponse::Unexpected => {
                return Err(ClientError::Protocol(
                    "conversation import append returned an unexpected response",
                ));
            }
        }
    }

    client
        .continue_mutation_request(&mut connection, ClientRequest::CommitConversationImport {})
        .await?;
    match classify_conversation_import_response(
        connection.message().await.map_err(ClientError::mutation)?,
    ) {
        ConversationImportResponse::Inserted(imported_conversation_id) => Ok(
            ConversationImportOutcome::Inserted(imported_conversation_id),
        ),
        ConversationImportResponse::AlreadyImported(imported_conversation_id) => Ok(
            ConversationImportOutcome::AlreadyImported(imported_conversation_id),
        ),
        ConversationImportResponse::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        ConversationImportResponse::Begun(_)
        | ConversationImportResponse::Appended(_)
        | ConversationImportResponse::Unexpected => Err(ClientError::Protocol(
            "conversation import commit returned an unexpected response",
        )
        .mutation()),
    }
}

fn conversation_import_chunk_read_limit(
    declared_size_bytes: CanonicalU64,
    assembled_size_bytes: u64,
) -> u64 {
    declared_size_bytes
        .value()
        .saturating_add(1)
        .saturating_sub(assembled_size_bytes)
        .min(u64::try_from(MAX_CONVERSATION_IMPORT_CHUNK_BYTES).unwrap_or(u64::MAX))
}

async fn import_conversation(
    client: &mut ProcessClient,
    format: ConversationImportFormat,
    source: Vec<u8>,
) -> Result<ConversationImportOutcome, ClientError> {
    let mut connection = client
        .mutation_request(ClientRequest::ImportConversation {
            format,
            source: ConversationImportSource::new(source),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::ConversationImportInserted {
            imported_conversation_id,
        } => Ok(ConversationImportOutcome::Inserted(
            imported_conversation_id,
        )),
        ServerMessage::ConversationImportAlreadyImported {
            imported_conversation_id,
        } => Ok(ConversationImportOutcome::AlreadyImported(
            imported_conversation_id,
        )),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("import returned an unexpected response").mutation()),
    }
}

fn write_single_import_outcome(
    output: &mut Output<'_>,
    outcome: ConversationImportOutcome,
) -> Result<(), ClientError> {
    match outcome {
        ConversationImportOutcome::Inserted(imported_conversation_id) => {
            output.conversation_import_inserted(imported_conversation_id)?;
        }
        ConversationImportOutcome::AlreadyImported(imported_conversation_id) => {
            output.conversation_import_already_imported(imported_conversation_id)?;
        }
    }
    Ok(())
}

async fn scan_conversations(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    format: ConversationImportFormat,
    scan: PreparedImportScan,
) -> Result<(), ClientError> {
    let mut summary = ImportScanSummary::default();
    for path in scan.paths {
        let outcome = match open_scanned_import_source(&scan.root, &path.relative) {
            Ok(file) => import_conversation_file(client, format, file).await,
            Err(error) => Err(error),
        };
        match outcome {
            Ok(ConversationImportOutcome::Inserted(imported_conversation_id)) => {
                summary.imported += 1;
                output
                    .conversation_import_scan_inserted(&path.display, imported_conversation_id)?;
            }
            Ok(ConversationImportOutcome::AlreadyImported(imported_conversation_id)) => {
                summary.already_imported += 1;
                output.conversation_import_scan_already_imported(
                    &path.display,
                    imported_conversation_id,
                )?;
            }
            Err(error) => {
                summary.skipped += 1;
                output.conversation_import_scan_skipped(&path.display, &error)?;
            }
        }
    }
    output.conversation_import_scan_summary(&summary)?;
    if summary.skipped == 0 {
        Ok(())
    } else {
        Err(ClientError::ScanIncomplete {
            skipped_files: summary.skipped,
        })
    }
}

/// Prints one imported conversation's selectable positions and its total.
///
/// The complete sequence is spooled and validated before presentation, so the
/// wire's intentionally unbounded entry sequence never becomes unbounded client
/// memory.
async fn imported(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    imported_conversation_id: CanonicalUuid,
) -> Result<(), ClientError> {
    let mut spool = tempfile::tempfile()?;
    let entry_count = read_imported_conversation(client, imported_conversation_id, |frame| {
        spool.write_all(&encode_server_line(frame)?)?;
        Ok(())
    })
    .await?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::ImportedConversationEntry {
                position,
                imported_entry_id,
                source_speaker,
                content_kind,
                text_preview,
            } => output.imported_conversation_entry(&ImportedEntryRow {
                position: position.value(),
                imported_entry_id: *imported_entry_id,
                source_speaker: *source_speaker,
                content_kind: *content_kind,
                text_preview: text_preview.as_ref(),
            })?,
            _ => {
                return Err(ClientError::Protocol(
                    "imported-entry spool contained a non-entry frame",
                ));
            }
        }
        line.clear();
    }
    output.imported_conversation_entry_count(entry_count)?;
    Ok(())
}

/// Reads one imported conversation's complete entry sequence, returning its
/// validated entry count, which is also its greatest selectable position.
async fn read_imported_conversation(
    client: &mut ProcessClient,
    imported_conversation_id: CanonicalUuid,
    mut consume: impl FnMut(&ServerFrame) -> Result<(), ClientError>,
) -> Result<u64, ClientError> {
    let mut connection = client
        .request(ClientRequest::ReadImportedConversation {
            imported_conversation_id,
        })
        .await?;
    match connection.message().await? {
        ServerMessage::ImportedConversationStart {
            imported_conversation_id: started,
        } if started == imported_conversation_id => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "imported conversation did not begin with its start frame",
            ));
        }
    }
    let mut entry_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::ImportedConversationEntry { position, .. } => {
                let expected = entry_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("imported entry count overflowed"))?;
                if position.value() != expected {
                    return Err(ClientError::Protocol(
                        "imported entry positions were not the contiguous sequence from one",
                    ));
                }
                consume(&frame)?;
                entry_count = expected;
            }
            ServerMessage::ImportedConversationEnd {
                imported_conversation_id: ended,
                entry_count: declared,
            } if *ended == imported_conversation_id && declared.value() == entry_count => {
                // An imported conversation's normalized entry sequence is
                // nonempty, so an empty inventory is never a valid read of one.
                if entry_count == 0 {
                    return Err(ClientError::Protocol(
                        "imported conversation reported no entries",
                    ));
                }
                return Ok(entry_count);
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "imported conversation sequence or count was invalid",
                ));
            }
        }
    }
}

async fn goal(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    command: GoalCommand,
) -> Result<(), ClientError> {
    match command {
        GoalCommand::Attach {
            session_id,
            statement,
            command_id,
        } => {
            let statement = read_goal_text_argument(statement).await?;
            goal_mutation(client, output, session_id, command_id, |command_id| {
                ClientRequest::AttachGoal {
                    command_id,
                    session_id,
                    statement,
                }
            })
            .await
        }
        GoalCommand::Show { session_id } => goal_show(client, output, session_id).await,
        GoalCommand::Resume {
            session_id,
            guidance,
            command_id,
        } => {
            let guidance = match guidance {
                Some(guidance) => Some(read_goal_text_argument(guidance).await?),
                None => None,
            };
            goal_mutation(client, output, session_id, command_id, |command_id| {
                ClientRequest::ResumeGoal {
                    command_id,
                    session_id,
                    guidance,
                }
            })
            .await
        }
        GoalCommand::Stop {
            session_id,
            command_id,
            descendants,
        } => {
            goal_mutation(client, output, session_id, command_id, |command_id| {
                ClientRequest::StopGoal {
                    command_id,
                    session_id,
                    descendant_scope: descendant_scope(descendants),
                }
            })
            .await
        }
        GoalCommand::Supersede {
            session_id,
            statement,
            command_id,
        } => {
            let statement = read_goal_text_argument(statement).await?;
            goal_mutation(client, output, session_id, command_id, |command_id| {
                ClientRequest::SupersedeGoal {
                    command_id,
                    session_id,
                    statement,
                }
            })
            .await
        }
    }
}

async fn goal_mutation<BuildRequest>(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    expected_session: CanonicalUuid,
    command_id: Option<CommandId>,
    build_request: BuildRequest,
) -> Result<(), ClientError>
where
    BuildRequest: FnOnce(CommandId) -> ClientRequest,
{
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client.mutation_request(build_request(command_id)).await?;
    let receipt = decode_goal_mutation_receipt(
        expected_session,
        connection.message().await.map_err(ClientError::mutation)?,
    )?;
    output
        .goal_transition_applied(
            receipt.session_id,
            receipt.event_ordinal,
            receipt.generation,
        )
        .map_err(ClientError::from)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GoalMutationReceipt {
    session_id: CanonicalUuid,
    event_ordinal: u64,
    generation: u64,
}

fn decode_goal_mutation_receipt(
    expected_session: CanonicalUuid,
    message: ServerMessage,
) -> Result<GoalMutationReceipt, ClientError> {
    match message {
        ServerMessage::GoalTransitionApplied {
            session_id,
            event_ordinal,
            generation,
        } if session_id == expected_session => Ok(GoalMutationReceipt {
            session_id,
            event_ordinal: event_ordinal.value(),
            generation: generation.value(),
        }),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("goal mutation returned an unexpected response").mutation()),
    }
}

#[derive(Debug)]
struct GoalHistoryProjection {
    generation: u64,
    statement: String,
    state: GoalLifecycleState,
}

#[derive(Debug, Default)]
struct GoalHistoryReplay {
    current: Option<GoalHistoryProjection>,
}

impl GoalHistoryReplay {
    fn apply(&mut self, generation: u64, event: &GoalHistoryEvent) -> Result<(), ClientError> {
        let current = self.current.take();
        let next = match (current, event) {
            (None, GoalHistoryEvent::Commissioned { statement, .. }) if generation == 1 => {
                GoalHistoryProjection {
                    generation,
                    statement: statement.clone(),
                    state: GoalLifecycleState::Pursuing {},
                }
            }
            (Some(current), GoalHistoryEvent::Commissioned { statement, .. })
                if goal_state_admits_commission(&current.state)
                    && current.generation.checked_add(1) == Some(generation) =>
            {
                GoalHistoryProjection {
                    generation,
                    statement: statement.clone(),
                    state: GoalLifecycleState::Pursuing {},
                }
            }
            (Some(mut current), GoalHistoryEvent::Blocked { reason, need, .. })
                if generation == current.generation && goal_state_is_pursuing(&current.state) =>
            {
                current.state = GoalLifecycleState::Blocked {
                    reason: *reason,
                    need: need.clone(),
                };
                current
            }
            (Some(mut current), GoalHistoryEvent::Resumed { .. })
                if generation == current.generation && goal_state_is_blocked(&current.state) =>
            {
                current.state = GoalLifecycleState::Pursuing {};
                current
            }
            (
                Some(mut current),
                GoalHistoryEvent::Achieved {
                    turn_id,
                    tool_request_id,
                    ..
                },
            ) if generation == current.generation && goal_state_is_pursuing(&current.state) => {
                current.state = GoalLifecycleState::Achieved {
                    turn_id: *turn_id,
                    tool_request_id: *tool_request_id,
                };
                current
            }
            (Some(mut current), GoalHistoryEvent::UserStopped { .. })
                if generation == current.generation && goal_state_is_open(&current.state) =>
            {
                current.state = GoalLifecycleState::UserStopped {};
                current
            }
            (
                Some(current),
                GoalHistoryEvent::Superseded {
                    replacement_statement,
                    ..
                },
            ) if generation == current.generation && goal_state_is_open(&current.state) => {
                let successor = current
                    .generation
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("goal history generation overflowed"))?;
                GoalHistoryProjection {
                    generation: successor,
                    statement: replacement_statement.clone(),
                    state: GoalLifecycleState::Pursuing {},
                }
            }
            (Some(mut current), GoalHistoryEvent::SessionClosed { outcome, .. })
                if generation == current.generation && goal_state_is_open(&current.state) =>
            {
                current.state = GoalLifecycleState::SessionClosed { outcome: *outcome };
                current
            }
            _ => {
                return Err(ClientError::Protocol(
                    "goal history contained an invalid lifecycle transition",
                ));
            }
        };
        self.current = Some(next);
        Ok(())
    }

    fn validate_projection(
        self,
        generation: u64,
        statement: &str,
        state: &GoalLifecycleState,
    ) -> Result<(), ClientError> {
        match self.current {
            Some(current)
                if current.generation == generation
                    && current.statement == statement
                    && current.state == *state =>
            {
                Ok(())
            }
            Some(_) | None => Err(ClientError::Protocol(
                "goal history did not derive its declared current projection",
            )),
        }
    }
}

const fn goal_state_is_pursuing(state: &GoalLifecycleState) -> bool {
    match state {
        GoalLifecycleState::Pursuing {} => true,
        GoalLifecycleState::Blocked { .. }
        | GoalLifecycleState::Achieved { .. }
        | GoalLifecycleState::UserStopped {}
        | GoalLifecycleState::Superseded { .. }
        | GoalLifecycleState::SessionClosed { .. } => false,
    }
}

const fn goal_state_is_blocked(state: &GoalLifecycleState) -> bool {
    match state {
        GoalLifecycleState::Blocked { .. } => true,
        GoalLifecycleState::Pursuing {}
        | GoalLifecycleState::Achieved { .. }
        | GoalLifecycleState::UserStopped {}
        | GoalLifecycleState::Superseded { .. }
        | GoalLifecycleState::SessionClosed { .. } => false,
    }
}

const fn goal_state_is_open(state: &GoalLifecycleState) -> bool {
    match state {
        GoalLifecycleState::Pursuing {} | GoalLifecycleState::Blocked { .. } => true,
        GoalLifecycleState::Achieved { .. }
        | GoalLifecycleState::UserStopped {}
        | GoalLifecycleState::Superseded { .. }
        | GoalLifecycleState::SessionClosed { .. } => false,
    }
}

const fn goal_state_admits_commission(state: &GoalLifecycleState) -> bool {
    match state {
        GoalLifecycleState::Achieved { .. } | GoalLifecycleState::UserStopped {} => true,
        GoalLifecycleState::Pursuing {}
        | GoalLifecycleState::Blocked { .. }
        | GoalLifecycleState::Superseded { .. }
        | GoalLifecycleState::SessionClosed { .. } => false,
    }
}

async fn goal_show(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
) -> Result<(), ClientError> {
    let mut connection = client
        .request(ClientRequest::ReadGoal { session_id })
        .await?;
    let first = connection.frame().await?;
    let (current_generation, current_statement) = match first.message() {
        ServerMessage::GoalHistoryStart {
            session_id: observed,
            current_generation,
            current_statement,
        } if *observed == session_id => (current_generation.value(), current_statement.clone()),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(*code, message.clone(), *detail)),
        _ => {
            return Err(ClientError::Protocol(
                "goal history did not begin with its selected session",
            ));
        }
    };
    let state_frame = connection.frame().await?;
    let current_state = match state_frame.message() {
        ServerMessage::GoalHistoryState { current_state } => current_state.clone(),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(*code, message.clone(), *detail)),
        _ => {
            return Err(ClientError::Protocol(
                "goal history did not carry its current state after its projection",
            ));
        }
    };
    let mut spool = tempfile::tempfile()?;
    let mut replay = GoalHistoryReplay::default();
    let mut event_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::GoalHistoryItem {
                event_ordinal,
                generation,
                event,
            } if event_ordinal.value() == event_count.saturating_add(1) => {
                event_count = event_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("goal event count overflowed"))?;
                replay.apply(generation.value(), event)?;
                spool.write_all(&encode_server_line(&frame)?)?;
            }
            ServerMessage::GoalHistoryEnd {
                event_count: declared,
            } if declared.value() == event_count => break,
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "goal history sequence or count was invalid",
                ));
            }
        }
    }
    replay.validate_projection(current_generation, &current_statement, &current_state)?;
    output.goal_current(
        session_id,
        current_generation,
        &current_statement,
        &current_state,
    )?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::GoalHistoryItem {
                event_ordinal,
                generation,
                event,
            } => output.goal_history_event(event_ordinal.value(), generation.value(), event)?,
            _ => {
                return Err(ClientError::Protocol(
                    "goal-history spool contained an unexpected frame",
                ));
            }
        }
        line.clear();
    }
    Ok(())
}

async fn list(client: &mut ProcessClient, output: &mut Output<'_>) -> Result<(), ClientError> {
    let mut spool = tempfile::tempfile()?;
    read_session_summaries(client, |_, frame| {
        spool.write_all(&encode_server_line(frame)?)?;
        Ok(())
    })
    .await?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::SessionSummary {
                session_id,
                defaults_version,
                model_selection,
                placement_version,
                placement,
                runner,
            } => output.session_summary(
                *session_id,
                defaults_version.value(),
                &selection_display(*model_selection),
                placement_version.value(),
                &placement_display(placement),
                runner.as_ref(),
            )?,
            _ => {
                return Err(ClientError::Protocol(
                    "session-summary spool contained a non-summary frame",
                ));
            }
        }
        line.clear();
    }
    Ok(())
}

async fn list_templates(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
) -> Result<(), ClientError> {
    let mut connection = client.request(ClientRequest::ListTemplates {}).await?;
    match connection.message().await? {
        ServerMessage::TemplatesStart {} => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "template list did not begin with its start frame",
            ));
        }
    }
    let mut spool = tempfile::tempfile()?;
    let mut prior_name: Option<String> = None;
    let mut summary_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::TemplateSummary { name, .. } => {
                if prior_name
                    .as_ref()
                    .is_some_and(|prior| prior.as_str() >= name.as_str())
                {
                    return Err(ClientError::Protocol(
                        "template summaries were not strictly ordered",
                    ));
                }
                summary_count = summary_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("template summary count overflowed"))?;
                prior_name = Some(name.clone());
                spool.write_all(&encode_server_line(&frame)?)?;
            }
            ServerMessage::TemplatesEnd { template_count }
                if template_count.value() == summary_count =>
            {
                break;
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "template list sequence or count was invalid",
                ));
            }
        }
    }
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::TemplateSummary { name, version } => {
                output.template_summary(name, version.value())?;
            }
            _ => {
                return Err(ClientError::Protocol(
                    "template-summary spool contained a non-summary frame",
                ));
            }
        }
        line.clear();
    }
    Ok(())
}

async fn search(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    page: SessionMetadataPageRequest,
) -> Result<(), ClientError> {
    let mut spool = tempfile::tempfile()?;
    let next_after_session_id = read_session_metadata_page(client, &page, |frame| {
        spool.write_all(&encode_server_line(frame)?)?;
        Ok(())
    })
    .await?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::SessionMetadataSummary {
                session_id,
                defaults_version,
                model_selection,
                dangerous_tool_auto_approval,
                title,
                tags,
                archived,
                last_writer,
            } => output.session_metadata_summary(&SessionMetadataRow {
                session_id: *session_id,
                defaults_version: defaults_version.value(),
                selection: &selection_display(*model_selection),
                dangerous_tool_auto_approval: *dangerous_tool_auto_approval,
                archived: *archived,
                last_writer: *last_writer,
                tags,
                title: title.as_deref(),
            })?,
            _ => {
                return Err(ClientError::Protocol(
                    "session-metadata spool contained a non-summary frame",
                ));
            }
        }
        line.clear();
    }
    if let Some(next_after_session_id) = next_after_session_id {
        output.next_page_cursor(next_after_session_id)?;
    }
    Ok(())
}

/// Orders unified cursors exactly as the daemon lists rows: by identity UUID
/// value, native before imported for a theoretical equal identity.
fn conversation_cursor_key(cursor: ConversationCursor) -> (Uuid, u8) {
    let origin_rank = match cursor.origin() {
        ConversationOrigin::NativeSession => 0,
        ConversationOrigin::ImportedConversation => 1,
    };
    (cursor.conversation_id().into_uuid(), origin_rank)
}

async fn read_conversation_page(
    client: &mut ProcessClient,
    page: &ConversationsPageRequest,
    mut consume: impl FnMut(&ServerFrame) -> Result<(), ClientError>,
) -> Result<Option<ConversationCursor>, ClientError> {
    let mut connection = client.request(page.request()).await?;
    match connection.message().await? {
        ServerMessage::ConversationPageStart {} => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "conversation page did not begin with its start frame",
            ));
        }
    }
    let mut prior_cursor = page.after;
    let mut last_in_page = None;
    let mut summary_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::ConversationSummary { conversation } => {
                let cursor = conversation.cursor();
                if prior_cursor.is_some_and(|prior| {
                    conversation_cursor_key(prior) >= conversation_cursor_key(cursor)
                }) {
                    return Err(ClientError::Protocol(
                        "conversation summaries were not strictly ordered",
                    ));
                }
                summary_count = summary_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("conversation count overflowed"))?;
                if summary_count > page.page_size.value() {
                    return Err(ClientError::Protocol(
                        "conversation page exceeded its requested bound",
                    ));
                }
                prior_cursor = Some(cursor);
                last_in_page = Some(cursor);
                consume(&frame)?;
            }
            ServerMessage::ConversationPageEnd {
                conversation_count,
                next_after,
            } => {
                if conversation_count.value() != summary_count
                    || next_after.is_some() && *next_after != last_in_page
                {
                    return Err(ClientError::Protocol(
                        "conversation page count or cursor was invalid",
                    ));
                }
                return Ok(*next_after);
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "conversation page sequence or count was invalid",
                ));
            }
        }
    }
}

async fn conversations(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    page: ConversationsPageRequest,
) -> Result<(), ClientError> {
    let mut spool = tempfile::tempfile()?;
    let next_after = read_conversation_page(client, &page, |frame| {
        spool.write_all(&encode_server_line(frame)?)?;
        Ok(())
    })
    .await?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::ConversationSummary { conversation } => match conversation {
                ConversationSummary::NativeSession {
                    session_id,
                    title,
                    archived,
                    defaults_version,
                } => output.conversation_summary(&ConversationRow::Native {
                    session_id: *session_id,
                    archived: *archived,
                    defaults_version: defaults_version.value(),
                    title: title.as_deref(),
                })?,
                ConversationSummary::ImportedConversation {
                    imported_conversation_id,
                    title,
                    entry_count,
                    source_format,
                } => output.conversation_summary(&ConversationRow::Imported {
                    imported_conversation_id: *imported_conversation_id,
                    format: imported_source_format_label(*source_format),
                    entry_count: entry_count.value(),
                    title: title.as_deref(),
                })?,
            },
            _ => {
                return Err(ClientError::Protocol(
                    "conversation spool contained a non-summary frame",
                ));
            }
        }
        line.clear();
    }
    if let Some(next_after) = next_after {
        output.next_conversation_cursor(
            conversation_origin_label(next_after.origin()),
            next_after.conversation_id(),
        )?;
    }
    Ok(())
}

/// The exact origin spelling the `--after` cursor argument accepts back.
const fn conversation_origin_label(origin: ConversationOrigin) -> &'static str {
    match origin {
        ConversationOrigin::NativeSession => "native",
        ConversationOrigin::ImportedConversation => "imported",
    }
}

const fn imported_source_format_label(
    format: signalbox_process_protocol::ImportedConversationSourceFormat,
) -> &'static str {
    match format {
        signalbox_process_protocol::ImportedConversationSourceFormat::ClaudeCodeSessionJsonlV1 => {
            "claude-code-session-jsonl-v1"
        }
        signalbox_process_protocol::ImportedConversationSourceFormat::ClaudeCodeSessionJsonlV2 => {
            "claude-code-session-jsonl-v2"
        }
        signalbox_process_protocol::ImportedConversationSourceFormat::CodexRolloutJsonlV1 => {
            "codex-rollout-jsonl-v1"
        }
    }
}

async fn send(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    command_id: Option<CommandId>,
    defaults_version: Option<CanonicalU64>,
    delivery: SendDeliveryArgument,
    content: String,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let delivery = match delivery {
        SendDeliveryArgument::StartWhenIdle => None,
        SendDeliveryArgument::Queue {
            expected_active_turn_id,
        } => {
            let expected_active_turn = match expected_active_turn_id {
                Some(turn_id) => turn_id,
                None => observe_active_turn(client, session_id).await?,
            };
            output.recovery_value("turn", &expected_active_turn.to_string())?;
            Some(InputDelivery::Queue {
                expected_active_turn_id: expected_active_turn,
            })
        }
    };
    let defaults_version =
        resolve_defaults_version(client, output, session_id, defaults_version).await?;

    let receipt = submit_input(
        client,
        command_id,
        session_id,
        InputContent::new(content),
        Some(defaults_version),
        delivery,
    )
    .await?;
    let SubmitInputReceipt::Turn { turn_id } = receipt else {
        return Err(ClientError::Protocol("send returned a steering receipt").mutation());
    };

    await_and_report_turn(client, output, session_id, turn_id).await
}

async fn steer(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    command_id: Option<CommandId>,
    turn_id: Option<CanonicalUuid>,
    content: String,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let expected_active_turn = match turn_id {
        Some(turn_id) => turn_id,
        None => observe_active_turn(client, session_id).await?,
    };
    output.recovery_value("turn", &expected_active_turn.to_string())?;

    let receipt = submit_input(
        client,
        command_id,
        session_id,
        InputContent::new(content),
        None,
        Some(InputDelivery::Steer {
            expected_active_turn_id: expected_active_turn,
        }),
    )
    .await?;
    let SubmitInputReceipt::Steering {
        accepted_input_id,
        acceptance_position,
        source_turn_id,
    } = receipt
    else {
        return Err(ClientError::Protocol("steer returned a turn-origin receipt").mutation());
    };
    if source_turn_id != expected_active_turn {
        return Err(ClientError::Protocol("steer returned another source turn").mutation());
    }
    output.steering_submitted(accepted_input_id, acceptance_position, source_turn_id)?;
    Ok(())
}

/// Supplies the user reconciliation decision a turn parked on an ambiguous
/// model call requires, then continues the session with the given content.
///
/// The parked turn terminalizes as reconciliation-required — its ambiguity is
/// recorded, never resolved into a fabricated outcome — and the content becomes
/// the immediate successor turn this verb then follows to its own terminal.
async fn reconcile(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    command_id: Option<CommandId>,
    defaults_version: Option<CanonicalU64>,
    content: String,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let defaults_version =
        resolve_defaults_version(client, output, session_id, defaults_version).await?;

    let successor_turn_id = reconcile_turn(
        client,
        command_id,
        session_id,
        turn_id,
        InputContent::new(content),
        defaults_version,
    )
    .await?;

    await_and_report_turn(client, output, session_id, successor_turn_id).await
}

/// Requests cancellation of the exact active turn through the interrupt
/// treatment, then continues the session with the given content.
///
/// The stopped turn terminalizes through the existing lifecycle — a prepared
/// call cancels directly, an issued call first enters its durable
/// cancellation-requested state — and the content becomes the
/// immediate-successor turn this verb then follows to its own terminal.
#[expect(
    clippy::too_many_arguments,
    reason = "the closed stop request keeps each recovery and scope input explicit"
)]
async fn stop(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    turn_id: Option<CanonicalUuid>,
    command_id: Option<CommandId>,
    defaults_version: Option<CanonicalU64>,
    descendants: bool,
    content: String,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let expected_active_turn = match turn_id {
        Some(turn_id) => turn_id,
        None => observe_active_turn(client, session_id).await?,
    };
    output.recovery_value("turn", &expected_active_turn.to_string())?;
    let defaults_version =
        resolve_defaults_version(client, output, session_id, defaults_version).await?;

    let successor_turn_id = stop_turn(
        client,
        command_id,
        session_id,
        expected_active_turn,
        InputContent::new(content),
        defaults_version,
        descendant_scope(descendants),
    )
    .await?;

    await_and_report_turn(client, output, session_id, successor_turn_id).await
}

/// Reads the authoritative transcript and returns the single turn holding the
/// session's active slot.
async fn observe_active_turn(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
) -> Result<CanonicalUuid, ClientError> {
    let mut snapshot = transcript(client, session_id).await?;
    snapshot
        .active_turn()?
        .ok_or(ClientError::Input("the session has no active turn"))
}

async fn resolve_defaults_version(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    defaults_version: Option<CanonicalU64>,
) -> Result<CanonicalU64, ClientError> {
    let defaults_version = match defaults_version {
        Some(version) => version,
        None => {
            let mut selected = None;
            read_session_summaries(client, |summary, _| {
                if summary.session_id == session_id {
                    selected = Some(CanonicalU64::new(summary.defaults_version));
                }
                Ok(())
            })
            .await?;
            selected.ok_or(ClientError::Input("the selected session was not listed"))?
        }
    };
    output.recovery_value("defaults_version", &defaults_version.value().to_string())?;
    Ok(defaults_version)
}

async fn await_and_report_turn(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<(), ClientError> {
    match await_turn_terminal(client, session_id, turn_id).await? {
        TurnTerminal::Completed => {
            let mut snapshot = transcript(client, session_id).await?;
            let state = snapshot.turn_state(turn_id)?;
            if !matches!(state.as_ref(), Some(TurnState::Completed { .. })) {
                return Err(ClientError::Protocol(
                    "terminal reread did not retain completed turn state",
                ));
            }
            write_assistant_texts(&mut snapshot, output, turn_id)?;
            Ok(())
        }
        TurnTerminal::Failed => {
            let mut snapshot = transcript(client, session_id).await?;
            match snapshot.turn_state(turn_id)? {
                Some(TurnState::Failed {
                    terminal_model_call,
                    ..
                }) => Err(ClientError::TurnFailed(
                    terminal_model_call.and_then(|call| call.cause()),
                )),
                _ => Err(ClientError::Protocol(
                    "terminal reread did not retain failed turn state",
                )),
            }
        }
        TurnTerminal::Refused => Err(ClientError::TurnRefused),
        TurnTerminal::Cancelled => Err(ClientError::TurnCancelled),
        TurnTerminal::ReconciliationRequired => Err(ClientError::TurnReconciliationRequired),
    }
}

/// Supplies one user decision for a pending tool request and validates the
/// exact recorded receipt.
async fn decide(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    command_id: Option<CommandId>,
    decision: ToolDecision,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::DecideToolRequest {
            command_id,
            session_id,
            tool_request_id,
            decision: decision.clone(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::ToolRequestDecided {
            tool_request_id: decided_request,
            decision: recorded_decision,
        } if decided_request == tool_request_id && recorded_decision == decision => {
            output.tool_request_decided(tool_request_id, &decision)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("decision returned an unexpected receipt").mutation()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SubmitInputReceipt {
    Turn {
        turn_id: CanonicalUuid,
    },
    Steering {
        accepted_input_id: CanonicalUuid,
        acceptance_position: u64,
        source_turn_id: CanonicalUuid,
    },
}

async fn submit_input(
    client: &mut ProcessClient,
    command_id: CommandId,
    session_id: CanonicalUuid,
    content: InputContent,
    expected_defaults_version: Option<CanonicalU64>,
    delivery: Option<InputDelivery>,
) -> Result<SubmitInputReceipt, ClientError> {
    let mut connection = client
        .mutation_request(ClientRequest::SubmitInput {
            command_id,
            session_id,
            content: signalbox_process_protocol::UserInputContent::text(content.into_string()),
            expected_defaults_version,
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery,
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            turn_id,
            ..
        } if submitted_session == session_id => Ok(SubmitInputReceipt::Turn { turn_id }),
        ServerMessage::SteeringSubmitted {
            session_id: submitted_session,
            accepted_input_id,
            acceptance_position,
            source_turn_id,
        } if submitted_session == session_id => Ok(SubmitInputReceipt::Steering {
            accepted_input_id,
            acceptance_position: acceptance_position.value(),
            source_turn_id,
        }),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("submit returned an unexpected response").mutation()),
    }
}

async fn reconcile_turn(
    client: &mut ProcessClient,
    command_id: CommandId,
    session_id: CanonicalUuid,
    expected_active_turn_id: CanonicalUuid,
    content: InputContent,
    defaults_version: CanonicalU64,
) -> Result<CanonicalUuid, ClientError> {
    let mut connection = client
        .mutation_request(ClientRequest::ReconcileTurn {
            command_id,
            session_id,
            expected_active_turn_id,
            content: signalbox_process_protocol::UserInputContent::text(content.into_string()),
            expected_defaults_version: defaults_version,
            model_settings: ModelSettingsOverlay::inherit_all(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            turn_id,
            ..
        } if submitted_session == session_id => Ok(turn_id),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("reconcile returned an unexpected response").mutation()),
    }
}

const fn descendant_scope(descendants: bool) -> DescendantTerminationScope {
    if descendants {
        DescendantTerminationScope::ParentAndDescendants
    } else {
        DescendantTerminationScope::ParentAlone
    }
}

async fn stop_turn(
    client: &mut ProcessClient,
    command_id: CommandId,
    session_id: CanonicalUuid,
    expected_active_turn_id: CanonicalUuid,
    content: InputContent,
    defaults_version: CanonicalU64,
    descendant_scope: DescendantTerminationScope,
) -> Result<CanonicalUuid, ClientError> {
    let mut connection = client
        .mutation_request(ClientRequest::StopTurn {
            command_id,
            session_id,
            expected_active_turn_id,
            content: signalbox_process_protocol::UserInputContent::text(content.into_string()),
            expected_defaults_version: defaults_version,
            descendant_scope,
            model_settings: ModelSettingsOverlay::inherit_all(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            turn_id,
            ..
        } if submitted_session == session_id => Ok(turn_id),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("stop returned an unexpected response").mutation()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnTerminal {
    Completed,
    Failed,
    Refused,
    Cancelled,
    ReconciliationRequired,
}

async fn await_turn_terminal(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<TurnTerminal, ClientError> {
    loop {
        let mut connection = client
            .request(ClientRequest::FollowSession { session_id })
            .await?;
        let mut snapshot = read_snapshot(&mut connection, session_id).await?;
        let state = snapshot.turn_state(turn_id)?;
        if let Some(terminal) = terminal_snapshot_state(state.as_ref())? {
            return Ok(terminal);
        }
        queued_turn_recovery(&mut snapshot, turn_id)?;
        let mut poll_automatic_recovery = automatic_recovery_pending(&mut snapshot, turn_id)?;
        let mut observed_cursor = snapshot.cursor();
        loop {
            let message = if poll_automatic_recovery {
                match tokio::time::timeout(FOLLOW_RECOVERY_REFETCH_INTERVAL, connection.message())
                    .await
                {
                    Ok(message) => message?,
                    Err(_) => {
                        let mut refreshed = transcript(client, session_id).await?;
                        let refreshed_state = refreshed.turn_state(turn_id)?;
                        if let Some(terminal) = terminal_snapshot_state(refreshed_state.as_ref())? {
                            return Ok(terminal);
                        }
                        queued_turn_recovery(&mut refreshed, turn_id)?;
                        poll_automatic_recovery =
                            automatic_recovery_pending(&mut refreshed, turn_id)?;
                        continue;
                    }
                }
            } else {
                connection.message().await?
            };
            match message {
                ServerMessage::SessionEvent {
                    cursor,
                    session_id: event_session,
                    event,
                } if event_session == session_id => {
                    if cursor.value() <= observed_cursor {
                        continue;
                    }
                    observed_cursor = cursor.value();
                    if let Some(terminal) = terminal_event_state(&event, turn_id) {
                        return Ok(terminal);
                    }
                    if selected_turn_recovery_transition(&event, turn_id) {
                        let mut refreshed = transcript(client, session_id).await?;
                        let refreshed_state = refreshed.turn_state(turn_id)?;
                        if let Some(terminal) = terminal_snapshot_state(refreshed_state.as_ref())? {
                            return Ok(terminal);
                        }
                        queued_turn_recovery(&mut refreshed, turn_id)?;
                        poll_automatic_recovery =
                            automatic_recovery_pending(&mut refreshed, turn_id)?;
                        if poll_automatic_recovery {
                            continue;
                        }
                        if !runner_recovery_transition(&event) {
                            return Err(ClientError::Protocol(
                                "a recovery event did not produce recovery or terminal state",
                            ));
                        }
                    }
                    if child_lifecycle_terminalization(&event, session_id) {
                        let mut refreshed = transcript(client, session_id).await?;
                        let refreshed_state = refreshed.turn_state(turn_id)?;
                        if let Some(terminal) = terminal_snapshot_state(refreshed_state.as_ref())? {
                            return Ok(terminal);
                        }
                        poll_automatic_recovery =
                            automatic_recovery_pending(&mut refreshed, turn_id)?;
                    }
                    if session_recovery_transition(&event) {
                        let mut refreshed = transcript(client, session_id).await?;
                        let refreshed_state = refreshed.turn_state(turn_id)?;
                        if let Some(terminal) = terminal_snapshot_state(refreshed_state.as_ref())? {
                            return Ok(terminal);
                        }
                        queued_turn_recovery(&mut refreshed, turn_id)?;
                        poll_automatic_recovery =
                            automatic_recovery_pending(&mut refreshed, turn_id)?;
                    }
                }
                ServerMessage::ProviderTextDelta {
                    session_id: delta_session,
                    ..
                } if delta_session == session_id => {}
                ServerMessage::Error {
                    code: ErrorCode::ResyncRequired,
                    ..
                } => break,
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => return Err(ClientError::remote(code, message, detail)),
                _ => {
                    return Err(ClientError::Protocol(
                        "follow returned an unexpected response",
                    ));
                }
            }
        }
    }
}

/// Reports whether bounded daemon reconciliation still owns the wait the
/// follow is blocked on, so the follower keeps polling instead of exiting.
///
/// Both recovery waits carry the same durable budget and the same
/// `operator_action_required` projection, so a tool wait is followed on
/// exactly the terms a model-call wait is.
fn automatic_recovery_pending(
    snapshot: &mut TranscriptSnapshot,
    selected_turn: CanonicalUuid,
) -> Result<bool, ClientError> {
    let selected_state = snapshot
        .turn_state(selected_turn)?
        .ok_or(ClientError::Protocol(
            "follow snapshot omitted the submitted turn",
        ))?;
    if matches!(
        selected_state,
        TurnState::ActiveAwaitingModelCallRecovery {
            operator_action_required: false,
            ..
        } | TurnState::ActiveAwaitingToolRecovery {
            operator_action_required: false,
            ..
        }
    ) {
        return Ok(true);
    }
    if !matches!(selected_state, TurnState::Queued { .. }) {
        return Ok(false);
    }
    let Some(active_turn) = snapshot.active_turn()? else {
        return Ok(false);
    };
    Ok(matches!(
        snapshot.turn_state(active_turn)?,
        Some(
            TurnState::ActiveAwaitingModelCallRecovery {
                operator_action_required: false,
                ..
            } | TurnState::ActiveAwaitingToolRecovery {
                operator_action_required: false,
                ..
            }
        )
    ))
}

fn queued_turn_recovery(
    snapshot: &mut TranscriptSnapshot,
    selected_turn: CanonicalUuid,
) -> Result<(), ClientError> {
    let selected_state = snapshot
        .turn_state(selected_turn)?
        .ok_or(ClientError::Protocol(
            "follow snapshot omitted the submitted queued turn",
        ))?;
    if !matches!(selected_state, TurnState::Queued { .. }) {
        return Ok(());
    }

    if let Some(active_turn) = snapshot.active_turn()? {
        let active_state = snapshot
            .turn_state(active_turn)?
            .ok_or(ClientError::Protocol(
                "follow snapshot omitted the session active turn",
            ))?;
        blocker_recovery_snapshot_state(&active_state)?;
    }
    queued_turn_runner_recovery(snapshot.runner())
}

fn blocker_recovery_snapshot_state(state: &TurnState) -> Result<(), ClientError> {
    match state {
        TurnState::ActiveAwaitingModelCallRecovery {
            operator_action_required: true,
            ..
        }
        | TurnState::ActiveAwaitingToolRecovery {
            operator_action_required: true,
            ..
        } => Err(ClientError::TurnRecoveryRequired),
        TurnState::ActiveAwaitingModelCallRecovery {
            operator_action_required: false,
            ..
        }
        | TurnState::ActiveAwaitingToolRecovery {
            operator_action_required: false,
            ..
        } => Ok(()),
        TurnState::ActiveAwaitingRunnerRecovery { .. } => Err(ClientError::RunnerRecoveryRequired),
        TurnState::Queued { .. }
        | TurnState::QueuedDelegated { .. }
        | TurnState::QueuedDelegationWake { .. }
        | TurnState::DelegationTerminated { .. }
        | TurnState::ActiveRunning { .. }
        | TurnState::ActiveAwaitingToolApproval { .. }
        | TurnState::ActiveAwaitingChild { .. }
        | TurnState::Completed { .. }
        | TurnState::Failed { .. }
        | TurnState::Refused { .. }
        | TurnState::Cancelled { .. }
        | TurnState::ReconciliationRequired { .. }
        | TurnState::ToolReconciliationRequired { .. } => Ok(()),
    }
}

/// Reports whether this event terminalizes the followed session's own turn as
/// the child of a parent cascade.
///
/// A descendant cascade addresses its lifecycle disposition to the terminalized
/// child as well as to the commanding parent, and the child-addressed row names
/// the relationship rather than the child turn. A follower therefore cannot
/// project the disposition onto its tracked turn directly; it re-reads
/// authoritative turn state, where the retained delegated turn projects as
/// `TurnState::DelegationTerminated`.
fn child_lifecycle_terminalization(event: &SessionEvent, session_id: CanonicalUuid) -> bool {
    matches!(
        event,
        SessionEvent::ChildLifecycleDisposition {
            child_session_id,
            ..
        } if *child_session_id == session_id
    )
}

fn session_recovery_transition(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::ModelCallTransition {
            state: ModelCallState::Terminal {
                disposition: ModelCallDisposition::Ambiguous,
            },
            ..
        } | SessionEvent::ToolBatchTransition {
            state: ToolBatchState::RecoveryRequired { .. },
            ..
        } | SessionEvent::TurnReconciliationRequired { .. }
            | SessionEvent::TurnToolReconciliationRequired { .. }
    )
}

fn tool_recovery_transition(event: &SessionEvent, selected_turn: CanonicalUuid) -> bool {
    matches!(
        event,
        SessionEvent::ToolBatchTransition {
            turn_id,
            state: ToolBatchState::RecoveryRequired { .. },
            ..
        } if *turn_id == selected_turn
    )
}

fn model_call_recovery_transition(event: &SessionEvent, selected_turn: CanonicalUuid) -> bool {
    matches!(
        event,
        SessionEvent::ModelCallTransition {
            turn_id,
            state: ModelCallState::Terminal {
                disposition: ModelCallDisposition::Ambiguous,
            },
            ..
        } if *turn_id == selected_turn
    )
}

fn runner_recovery_transition(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::RunnerStateTransition {
            state: RunnerStateTransitionState::RunnerLostBeforePin
                | RunnerStateTransitionState::RunnerLost,
            ..
        }
    )
}

/// Rejects when the authoritative snapshot retains a current runner loss that
/// prevents the selected queued turn from activating.
fn queued_turn_runner_recovery(runner: Option<&RunnerProjection>) -> Result<(), ClientError> {
    if runner.is_some_and(|runner| {
        matches!(
            runner.state(),
            RunnerProjectionState::RunnerLostBeforePin | RunnerProjectionState::RunnerLost
        ) || matches!(
            runner.connection_health(),
            Some(RunnerConnectionHealth::Shutdown | RunnerConnectionHealth::Lost)
        )
    }) {
        return Err(ClientError::RunnerRecoveryRequired);
    }
    Ok(())
}

fn selected_turn_recovery_transition(event: &SessionEvent, selected_turn: CanonicalUuid) -> bool {
    model_call_recovery_transition(event, selected_turn)
        || tool_recovery_transition(event, selected_turn)
        || runner_recovery_transition(event)
}

fn terminal_snapshot_state(state: Option<&TurnState>) -> Result<Option<TurnTerminal>, ClientError> {
    match state {
        Some(TurnState::Completed { .. }) => Ok(Some(TurnTerminal::Completed)),
        Some(TurnState::Failed { .. }) => Ok(Some(TurnTerminal::Failed)),
        Some(TurnState::Refused { .. }) => Ok(Some(TurnTerminal::Refused)),
        Some(TurnState::Cancelled { .. }) => Ok(Some(TurnTerminal::Cancelled)),
        Some(TurnState::DelegationTerminated { .. }) => Ok(Some(TurnTerminal::Cancelled)),
        Some(
            TurnState::ReconciliationRequired { .. } | TurnState::ToolReconciliationRequired { .. },
        ) => Ok(Some(TurnTerminal::ReconciliationRequired)),
        Some(
            TurnState::Queued { .. }
            | TurnState::QueuedDelegated { .. }
            | TurnState::QueuedDelegationWake { .. }
            | TurnState::ActiveRunning { .. }
            | TurnState::ActiveAwaitingToolApproval { .. }
            | TurnState::ActiveAwaitingChild { .. },
        ) => Ok(None),
        Some(
            TurnState::ActiveAwaitingModelCallRecovery {
                operator_action_required: true,
                ..
            }
            | TurnState::ActiveAwaitingToolRecovery {
                operator_action_required: true,
                ..
            },
        ) => Err(ClientError::TurnRecoveryRequired),
        Some(
            TurnState::ActiveAwaitingModelCallRecovery {
                operator_action_required: false,
                ..
            }
            | TurnState::ActiveAwaitingToolRecovery {
                operator_action_required: false,
                ..
            },
        ) => Ok(None),
        Some(TurnState::ActiveAwaitingRunnerRecovery { .. }) => {
            Err(ClientError::RunnerRecoveryRequired)
        }
        None => Err(ClientError::Protocol(
            "follow snapshot omitted the submitted turn",
        )),
    }
}

fn terminal_event_state(
    event: &SessionEvent,
    selected_turn: CanonicalUuid,
) -> Option<TurnTerminal> {
    match event {
        SessionEvent::TurnCompleted { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Completed)
        }
        SessionEvent::TurnFailed { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Failed)
        }
        SessionEvent::TurnRefused { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Refused)
        }
        SessionEvent::TurnCancelled { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Cancelled)
        }
        SessionEvent::TurnReconciliationRequired { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::ReconciliationRequired)
        }
        SessionEvent::TurnToolReconciliationRequired { turn_id, .. }
            if *turn_id == selected_turn =>
        {
            Some(TurnTerminal::ReconciliationRequired)
        }
        SessionEvent::SessionCreated {}
        | SessionEvent::SessionModelSettingsChanged { .. }
        | SessionEvent::TurnModelSettingsResolved { .. }
        | SessionEvent::InputAccepted { .. }
        | SessionEvent::GoalTurnRetired { .. }
        | SessionEvent::TurnActivated { .. }
        | SessionEvent::ContextCompacted { .. }
        | SessionEvent::ToolApprovalDecided { .. }
        | SessionEvent::ModelCallTransition { .. }
        | SessionEvent::ToolBatchTransition { .. }
        | SessionEvent::RunnerStateTransition { .. }
        | SessionEvent::TurnCompleted { .. }
        | SessionEvent::TurnFailed { .. }
        | SessionEvent::TurnRefused { .. }
        | SessionEvent::TurnCancelled { .. }
        | SessionEvent::TurnReconciliationRequired { .. }
        | SessionEvent::TurnToolReconciliationRequired { .. }
        | SessionEvent::ChildSpawned { .. }
        | SessionEvent::ChildWaiting { .. }
        | SessionEvent::SessionMessage { .. }
        | SessionEvent::ChildResult { .. }
        | SessionEvent::ChildLifecycleDisposition { .. } => None,
    }
}

async fn transcript(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
) -> Result<TranscriptSnapshot, ClientError> {
    let mut connection = client
        .request(ClientRequest::ReadTranscript { session_id })
        .await?;
    read_snapshot(&mut connection, session_id).await
}

async fn follow(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
) -> Result<(), ClientError> {
    let mut displayed_entries = SnapshotIdentitySet::new()?;
    loop {
        let mut connection = client
            .request(ClientRequest::FollowSession { session_id })
            .await?;
        let mut snapshot = read_snapshot(&mut connection, session_id).await?;
        output.followed_snapshot(&mut snapshot, &mut displayed_entries)?;
        let mut observed_cursor = snapshot.cursor();
        loop {
            match connection.message().await? {
                ServerMessage::SessionEvent {
                    cursor,
                    session_id: event_session,
                    event,
                } if event_session == session_id => {
                    if cursor.value() <= observed_cursor {
                        continue;
                    }
                    observed_cursor = cursor.value();
                    output.event(observed_cursor, session_id, &event)?;
                    if let Some(selection) = terminal_snapshot_selection(&event, session_id) {
                        let mut refreshed = transcript(client, session_id).await?;
                        output.terminal_material(
                            &mut refreshed,
                            &mut displayed_entries,
                            selection,
                        )?;
                    }
                }
                ServerMessage::ProviderTextDelta {
                    session_id: delta_session,
                    turn_id,
                    model_call_id,
                    part_index,
                    content,
                } if delta_session == session_id => {
                    output.provider_text_delta(
                        session_id,
                        turn_id,
                        model_call_id,
                        part_index.value(),
                        content.as_str(),
                    )?;
                }
                ServerMessage::Error {
                    code: ErrorCode::ResyncRequired,
                    ..
                } => break,
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => return Err(ClientError::remote(code, message, detail)),
                _ => {
                    return Err(ClientError::Protocol(
                        "follow returned an unexpected response",
                    ));
                }
            }
        }
    }
}

fn terminal_snapshot_selection(
    event: &SessionEvent,
    session_id: CanonicalUuid,
) -> Option<SnapshotSelection> {
    if child_lifecycle_terminalization(event, session_id) {
        // The cascade names no terminal entry of its own, so the refresh it
        // requires selects whatever material the child produced before it.
        return Some(SnapshotSelection::All);
    }
    match event {
        SessionEvent::TurnCompleted {
            turn_id,
            model_call_id,
            completion_entry_id,
            ..
        } => Some(SnapshotSelection::Completed {
            turn_id: *turn_id,
            model_call_id: *model_call_id,
            terminal_entry_id: *completion_entry_id,
        }),
        SessionEvent::TurnFailed {
            turn_id,
            failure_entry_id,
            ..
        } => Some(SnapshotSelection::Failed {
            turn_id: *turn_id,
            terminal_entry_id: *failure_entry_id,
        }),
        SessionEvent::TurnCancelled {
            turn_id,
            cancellation_entry_id,
            ..
        } => Some(SnapshotSelection::Cancelled {
            turn_id: *turn_id,
            terminal_entry_id: *cancellation_entry_id,
        }),
        SessionEvent::ToolBatchTransition {
            turn_id,
            model_call_id,
            state: ToolBatchState::Proposed { .. },
        } => Some(SnapshotSelection::ToolBatchProposed {
            turn_id: *turn_id,
            model_call_id: *model_call_id,
        }),
        SessionEvent::ToolBatchTransition {
            turn_id,
            model_call_id,
            state: ToolBatchState::ResultsProjected { .. },
        } => Some(SnapshotSelection::ToolBatchResults {
            turn_id: *turn_id,
            model_call_id: *model_call_id,
        }),
        SessionEvent::ToolBatchTransition {
            state: ToolBatchState::RecoveryRequired { .. },
            ..
        } => None,
        SessionEvent::TurnToolReconciliationRequired {
            turn_id,
            tool_attempt_id,
            terminal_frontier_id,
        } => Some(SnapshotSelection::ToolReconciliation {
            turn_id: *turn_id,
            tool_attempt_id: *tool_attempt_id,
            terminal_frontier_id: *terminal_frontier_id,
        }),
        SessionEvent::TurnRefused { .. } | SessionEvent::TurnReconciliationRequired { .. } => None,
        SessionEvent::SessionCreated {}
        | SessionEvent::SessionModelSettingsChanged { .. }
        | SessionEvent::TurnModelSettingsResolved { .. }
        | SessionEvent::InputAccepted { .. }
        | SessionEvent::GoalTurnRetired { .. }
        | SessionEvent::TurnActivated { .. }
        | SessionEvent::ContextCompacted { .. }
        | SessionEvent::ModelCallTransition { .. }
        | SessionEvent::RunnerStateTransition { .. }
        | SessionEvent::ChildSpawned { .. }
        | SessionEvent::ChildWaiting { .. }
        | SessionEvent::SessionMessage { .. }
        | SessionEvent::ChildResult { .. }
        | SessionEvent::ToolApprovalDecided { .. }
        | SessionEvent::ChildLifecycleDisposition { .. } => None,
    }
}

fn write_assistant_texts(
    snapshot: &mut TranscriptSnapshot,
    output: &mut Output<'_>,
    selected_turn: CanonicalUuid,
) -> Result<(), ClientError> {
    let mut selected_entry = false;
    for record in snapshot.replay()? {
        match record? {
            SnapshotRecord::Entry(entry) => {
                selected_entry = matches!(
                    entry.kind,
                    transcript::SnapshotEntryKind::Text(
                        signalbox_process_protocol::TranscriptTextEntry::Assistant {
                            turn_id,
                            ..
                        }
                    ) if turn_id == selected_turn
                );
            }
            SnapshotRecord::Content(content) if selected_entry => {
                let ends_with_newline = content.content.as_str().ends_with('\n');
                output.assistant_text_fragment(
                    content.content.as_str(),
                    content.final_fragment,
                    ends_with_newline,
                )?;
                if content.final_fragment {
                    selected_entry = false;
                }
            }
            SnapshotRecord::Turn(_)
            | SnapshotRecord::ModelCallUsage(_)
            | SnapshotRecord::Content(_) => {}
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
enum OperatorStatusPhase {
    LifecycleWeeks,
    LifecycleDeadlineViolations,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OperatorStatusCounts {
    lifecycle_weeks: u64,
    lifecycle_deadline_violations: u64,
}

async fn status(client: &mut ProcessClient, output: &mut Output<'_>) -> Result<(), ClientError> {
    let mut connection = client.request(ClientRequest::ReadOperatorStatus {}).await?;
    match connection.message().await? {
        ServerMessage::OperatorStatus(message)
            if matches!(message.as_ref(), OperatorStatusMessage::Start {}) => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "operator status did not begin with its start frame",
            ));
        }
    }
    let mut spool = tempfile::tempfile()?;
    let mut phase = OperatorStatusPhase::LifecycleWeeks;
    let mut counts = OperatorStatusCounts::default();
    loop {
        let frame = connection.frame().await?;
        let item_phase = match frame.message() {
            ServerMessage::OperatorStatus(message) => match message.as_ref() {
                OperatorStatusMessage::LifecycleWeek(_) => {
                    counts.lifecycle_weeks = status_increment(counts.lifecycle_weeks)?;
                    Some(OperatorStatusPhase::LifecycleWeeks)
                }
                OperatorStatusMessage::LifecycleDeadlineViolation(_) => {
                    counts.lifecycle_deadline_violations =
                        status_increment(counts.lifecycle_deadline_violations)?;
                    Some(OperatorStatusPhase::LifecycleDeadlineViolations)
                }
                OperatorStatusMessage::End(item)
                    if counts
                        == (OperatorStatusCounts {
                            lifecycle_weeks: item.lifecycle_week_count.value(),
                            lifecycle_deadline_violations: item
                                .lifecycle_deadline_violation_count
                                .value(),
                        }) =>
                {
                    break;
                }
                OperatorStatusMessage::Start {} | OperatorStatusMessage::End(_) => {
                    return Err(ClientError::Protocol(
                        "operator status sequence or count was invalid",
                    ));
                }
            },
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "operator status sequence or count was invalid",
                ));
            }
        };
        let Some(item_phase) = item_phase else {
            return Err(ClientError::Protocol(
                "operator status sequence was invalid",
            ));
        };
        if item_phase < phase {
            return Err(ClientError::Protocol(
                "operator status sections were out of order",
            ));
        }
        phase = item_phase;
        spool.write_all(&encode_server_line(&frame)?)?;
    }
    output.operator_status_counts(OperatorStatusPresentationCounts {
        lifecycle_weeks: counts.lifecycle_weeks,
        lifecycle_deadline_violations: counts.lifecycle_deadline_violations,
    })?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        output.operator_status_item(decode_server_line(&line)?.message())?;
        line.clear();
    }
    Ok(output.operator_status_model_usage_omitted()?)
}

fn status_increment(value: u64) -> Result<u64, ClientError> {
    value
        .checked_add(1)
        .ok_or(ClientError::Protocol("operator status count overflowed"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionSummary {
    session_id: CanonicalUuid,
    defaults_version: u64,
}

async fn read_session_summaries(
    client: &mut ProcessClient,
    mut consume: impl FnMut(SessionSummary, &ServerFrame) -> Result<(), ClientError>,
) -> Result<(), ClientError> {
    let mut connection = client.request(ClientRequest::ListSessions {}).await?;
    match connection.message().await? {
        ServerMessage::SessionsStart {} => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "session list did not begin with its start frame",
            ));
        }
    }
    let mut prior_session = None;
    let mut summary_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::SessionSummary {
                session_id,
                defaults_version,
                ..
            } => {
                if prior_session
                    .is_some_and(|prior: CanonicalUuid| prior.into_uuid() >= session_id.into_uuid())
                {
                    return Err(ClientError::Protocol(
                        "session summaries were not strictly ordered",
                    ));
                }
                let summary = SessionSummary {
                    session_id: *session_id,
                    defaults_version: defaults_version.value(),
                };
                consume(summary, &frame)?;
                prior_session = Some(*session_id);
                summary_count = summary_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("session summary count overflowed"))?;
            }
            ServerMessage::SessionsEnd { session_count }
                if session_count.value() == summary_count =>
            {
                return Ok(());
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "session list sequence or count was invalid",
                ));
            }
        }
    }
}

fn placement_display(placement: &SessionPlacement) -> String {
    match placement {
        SessionPlacement::Pathless {} => String::from("placement=pathless"),
        SessionPlacement::Scoped { path } => format!("placement={path}"),
        SessionPlacement::RootGlobalRead { path, .. } => {
            format!("placement={path} root_global_read=acknowledged")
        }
    }
}

fn validate_review_finding_count(
    count: usize,
    limits: Option<ClientDeploymentLimits>,
) -> Result<(), ClientError> {
    let limits = limits.ok_or(ClientError::Protocol("deployment limits were not read"))?;
    if limits
        .max_review_findings_per_run
        .is_some_and(|maximum| u64::try_from(count).map_or(true, |count| count > maximum))
    {
        return Err(ClientError::Input(
            "review findings exceed the deployment count limit",
        ));
    }
    Ok(())
}

async fn review(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    command: ReviewCommand,
    deployment_limits: Option<ClientDeploymentLimits>,
) -> Result<(), ClientError> {
    match command {
        ReviewCommand::CreateTarget {
            command_id,
            target_id,
            provider,
            repository,
            subject,
            head_revision,
            base_revision,
            stack_parent_target_id,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::CreateReviewTarget {
                    command_id,
                    target_id,
                    provider,
                    repository,
                    subject,
                    head_revision,
                    base_revision,
                    stack_parent_target_id,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewTargetCreated {
                    target_id: recorded,
                } if recorded == target_id => {
                    output.review_acknowledgement(&format!("target={recorded} created"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review target creation returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::StartRun {
            command_id,
            target_id,
            run_id,
            pass_id,
            workflow,
            session_id,
            accepted_input_id,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::StartReviewRun {
                    command_id,
                    target_id,
                    run_id,
                    pass_id,
                    workflow,
                    session_id,
                    accepted_input_id,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewRunStarted {
                    run_id: recorded_run,
                    pass_id: recorded_pass,
                } if recorded_run == run_id && recorded_pass == pass_id => {
                    output.review_acknowledgement(&format!(
                        "run={recorded_run} pass={recorded_pass} started"
                    ))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review run creation returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::ActivatePass {
            command_id,
            run_id,
            pass_id,
            turn_id,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::ActivateReviewPass {
                    command_id,
                    run_id,
                    pass_id,
                    turn_id,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewPassActivated {
                    run_id: recorded_run,
                    pass_id: recorded_pass,
                } if recorded_run == run_id && recorded_pass == pass_id => {
                    output.review_acknowledgement(&format!(
                        "run={recorded_run} pass={recorded_pass} activated"
                    ))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review pass activation returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordFinding {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            finding,
        } => {
            validate_review_finding_count(1, deployment_limits)?;
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewFindings {
                    command_id,
                    run_id,
                    pass_id,
                    turn_id,
                    output_frontier_id,
                    findings: vec![finding],
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewFindingsRecorded {
                    run_id: recorded_run,
                    pass_id: recorded_pass,
                    finding_count,
                } if recorded_run == run_id
                    && recorded_pass == pass_id
                    && finding_count.value() == 1 =>
                {
                    output.review_acknowledgement(&format!(
                        "run={recorded_run} pass={recorded_pass} findings=1 recorded"
                    ))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review finding admission returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordFindings {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            findings_file,
        } => {
            let file: ReviewFindingsFile = read_review_json_file(&findings_file).await?;
            let finding_count = file.findings.len();
            validate_review_finding_count(finding_count, deployment_limits)?;
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewFindings {
                    command_id,
                    run_id,
                    pass_id,
                    turn_id,
                    output_frontier_id,
                    findings: file.findings,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewFindingsRecorded {
                    run_id: recorded_run,
                    pass_id: recorded_pass,
                    finding_count: recorded_count,
                } if recorded_run == run_id
                    && recorded_pass == pass_id
                    && usize::try_from(recorded_count.value()) == Ok(finding_count) =>
                {
                    output.review_acknowledgement(&format!(
                        "run={recorded_run} pass={recorded_pass} findings={finding_count} recorded"
                    ))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review finding inventory admission returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::CompletePass {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            outcome,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::CompleteReviewPass {
                    command_id,
                    run_id,
                    pass_id,
                    turn_id,
                    output_frontier_id,
                    outcome,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewPassCompleted {
                    run_id: recorded_run,
                    pass_id: recorded_pass,
                    state,
                } if recorded_run == run_id
                    && recorded_pass == pass_id
                    && review_pass_completion_is_coherent(outcome, state) =>
                {
                    output.review_acknowledgement(&format!(
                        "run={recorded_run} pass={recorded_pass} completed"
                    ))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review pass completion returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordFindingEvent {
            command_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            finding_id,
            event_ordinal,
            event,
        } => {
            let expected_status = review_finding_event_status(&event);
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewFindingEvent {
                    command_id,
                    run_id,
                    pass_id,
                    turn_id,
                    output_frontier_id,
                    finding_id,
                    event_ordinal,
                    event,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewFindingEventRecorded {
                    finding_id: recorded,
                    status,
                } if recorded == finding_id && status == expected_status => {
                    output.review_acknowledgement(&format!("finding={recorded} event recorded"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review finding event returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::StartOrchestration {
            command_id,
            attempt_id,
            target_id,
            concern_set_version,
            import_template_name,
            judgment_template_name,
            repair_template_name,
            publication_template_name,
            concerns_file,
        } => {
            let file: ReviewConcernsFile = read_review_json_file(&concerns_file).await?;
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::StartReviewOrchestration {
                    command_id,
                    attempt_id,
                    target_id,
                    concern_set_version,
                    import_template_name,
                    judgment_template_name,
                    repair_template_name,
                    publication_template_name,
                    concerns: file.concerns,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewOrchestrationStarted {
                    attempt_id: recorded,
                } if recorded == attempt_id => {
                    output.review_acknowledgement(&format!("attempt={recorded} started"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review orchestration start returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordImportOutcome {
            command_id,
            attempt_id,
            pass_id,
            external_link_id,
            context_digest,
            outcome,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewImportOutcome {
                    command_id,
                    attempt_id,
                    pass_id,
                    external_link_id,
                    context_digest,
                    outcome,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewOrchestrationAdvanced {
                    attempt_id: recorded,
                    state,
                } if recorded == attempt_id && review_import_state_is_coherent(outcome, state) => {
                    output.review_acknowledgement(&format!("attempt={recorded} advanced"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review import outcome returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordConcernOutcome {
            command_id,
            attempt_id,
            concern,
            pass_id,
            outcome,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewConcernOutcome {
                    command_id,
                    attempt_id,
                    concern,
                    pass_id,
                    outcome,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewOrchestrationAdvanced {
                    attempt_id: recorded,
                    state,
                } if recorded == attempt_id && review_concern_state_is_coherent(outcome, state) => {
                    output.review_acknowledgement(&format!("attempt={recorded} advanced"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review concern outcome returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordJudgmentPlan {
            command_id,
            attempt_id,
            analysis_pass_id,
            members_file,
        } => {
            let file: ReviewJudgmentMembersFile = read_review_json_file(&members_file).await?;
            let plan_is_empty = file.members.is_empty();
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewJudgmentPlan {
                    command_id,
                    attempt_id,
                    analysis_pass_id,
                    members: file.members,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewOrchestrationAdvanced {
                    attempt_id: recorded,
                    state,
                } if recorded == attempt_id
                    && review_judgment_plan_state_is_coherent(plan_is_empty, state) =>
                {
                    output.review_acknowledgement(&format!("attempt={recorded} advanced"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review judgment plan returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordJudgmentEffect {
            command_id,
            attempt_id,
            finding_id,
            event_pass_id,
            outcome,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewJudgmentEffect {
                    command_id,
                    attempt_id,
                    finding_id,
                    event_pass_id,
                    outcome,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewOrchestrationAdvanced {
                    attempt_id: recorded,
                    state,
                } if recorded == attempt_id
                    && review_judgment_effect_state_is_coherent(outcome, state) =>
                {
                    output.review_acknowledgement(&format!("attempt={recorded} advanced"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review judgment effect returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordRepairOutcomes {
            command_id,
            attempt_id,
            outcomes_file,
        } => {
            let file: ReviewRepairOutcomesFile = read_review_json_file(&outcomes_file).await?;
            let has_blocked = file
                .outcomes
                .iter()
                .any(|outcome| outcome.outcome == ReviewRepairTerminalOutcome::Blocked);
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewRepairOutcomes {
                    command_id,
                    attempt_id,
                    outcomes: file.outcomes,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewOrchestrationAdvanced {
                    attempt_id: recorded,
                    state,
                } if recorded == attempt_id
                    && review_repair_state_is_coherent(has_blocked, state) =>
                {
                    output.review_acknowledgement(&format!("attempt={recorded} advanced"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review repair outcomes returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::RecordPublicationOutcomes {
            command_id,
            attempt_id,
            outcomes_file,
        } => {
            let file: ReviewPublicationOutcomesFile = read_review_json_file(&outcomes_file).await?;
            let all_published = file
                .outcomes
                .iter()
                .all(|outcome| outcome.outcome == ReviewPublicationTerminalOutcome::Published);
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::RecordReviewPublicationOutcomes {
                    command_id,
                    attempt_id,
                    outcomes: file.outcomes,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewOrchestrationAdvanced {
                    attempt_id: recorded,
                    state,
                } if recorded == attempt_id
                    && review_publication_state_is_coherent(all_published, state) =>
                {
                    output.review_acknowledgement(&format!("attempt={recorded} advanced"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review publication outcomes returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::ReserveExternalLink {
            command_id,
            external_link_id,
            finding_id,
            provider,
            object_kind,
        } => {
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::ReserveReviewExternalLink {
                    command_id,
                    external_link_id,
                    finding_id,
                    provider,
                    object_kind,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewExternalLinkReserved {
                    external_link_id: recorded,
                } if recorded == external_link_id => {
                    output.review_acknowledgement(&format!("external_link={recorded} reserved"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review external-link reservation returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::AttachExternalLink {
            command_id,
            external_link_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            external_object,
            event_ordinal,
        } => {
            let expected_external_object = external_object.clone();
            let command_id = review_command_identity(output, command_id)?;
            let mut connection = client
                .mutation_request(ClientRequest::AttachReviewExternalLink {
                    command_id,
                    external_link_id,
                    run_id,
                    pass_id,
                    turn_id,
                    output_frontier_id,
                    external_object,
                    event_ordinal,
                })
                .await?;
            match connection.message().await.map_err(ClientError::mutation)? {
                ServerMessage::ReviewExternalLinkAttached {
                    external_link_id: recorded,
                    external_object: recorded_object,
                } if recorded == external_link_id
                    && recorded_object == expected_external_object =>
                {
                    output.review_acknowledgement(&format!("external_link={recorded} attached"))?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail).mutation()),
                _ => Err(ClientError::Protocol(
                    "review external-link attachment returned an unexpected response",
                )
                .mutation()),
            }
        }
        ReviewCommand::ReadOrchestration { attempt_id } => {
            let mut connection = client
                .request(ClientRequest::ReadReviewOrchestration { attempt_id })
                .await?;
            match connection.message().await? {
                ServerMessage::ReviewOrchestration { snapshot }
                    if snapshot.attempt_id == attempt_id =>
                {
                    output.review_orchestration(&snapshot)?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail)),
                _ => Err(ClientError::Protocol(
                    "review orchestration read returned an unexpected response",
                )),
            }
        }
        ReviewCommand::ReadTarget { target_id } => {
            let mut connection = client
                .request(ClientRequest::ReadReviewTarget { target_id })
                .await?;
            match connection.message().await? {
                ServerMessage::ReviewTarget { target } if target.target_id == target_id => {
                    output.review_target(&target)?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail)),
                _ => Err(ClientError::Protocol(
                    "review target read returned an unexpected response",
                )),
            }
        }
        ReviewCommand::ReadRun { run_id } => {
            let mut connection = client
                .request(ClientRequest::ReadReviewRun { run_id })
                .await?;
            match connection.message().await? {
                ServerMessage::ReviewRun { run, pass }
                    if run.run_id == run_id
                        && review_run_response_is_coherent(&run, pass.as_ref()) =>
                {
                    output.review_run(&run, pass.as_ref())?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail)),
                _ => Err(ClientError::Protocol(
                    "review run read returned an unexpected response",
                )),
            }
        }
        ReviewCommand::ReadFinding { finding_id } => {
            let mut connection = client
                .request(ClientRequest::ReadReviewFinding { finding_id })
                .await?;
            match connection.message().await? {
                ServerMessage::ReviewFinding { finding }
                    if finding.finding.finding_id == finding_id =>
                {
                    output.review_finding(&finding)?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail)),
                _ => Err(ClientError::Protocol(
                    "review finding read returned an unexpected response",
                )),
            }
        }
        ReviewCommand::ListFindings { run_id } => {
            let mut connection = client
                .request(ClientRequest::ListReviewFindings { run_id })
                .await?;
            let start = connection.frame().await?;
            match start.message() {
                ServerMessage::ReviewFindingsStart { run_id: selected } if *selected == run_id => {}
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => {
                    return Err(ClientError::remote(*code, message.clone(), *detail));
                }
                _ => {
                    return Err(ClientError::Protocol(
                        "review finding list did not start correctly",
                    ));
                }
            }
            let mut spool = tempfile::tempfile()?;
            let mut count = 0_u64;
            let mut previous_finding_id: Option<CanonicalUuid> = None;
            loop {
                let frame = connection.frame().await?;
                match frame.message() {
                    ServerMessage::ReviewFindingItem { finding } if finding.run_id == run_id => {
                        let finding_id = finding.finding.finding_id;
                        if previous_finding_id
                            .is_some_and(|previous| finding_id.into_uuid() <= previous.into_uuid())
                        {
                            return Err(ClientError::Protocol(
                                "review finding list identity order was invalid",
                            ));
                        }
                        previous_finding_id = Some(finding_id);
                        count = count.checked_add(1).ok_or(ClientError::Protocol(
                            "review finding list count overflowed",
                        ))?;
                        if deployment_limits
                            .and_then(|limits| limits.max_review_findings_per_run)
                            .is_some_and(|maximum| count > maximum)
                        {
                            return Err(ClientError::Protocol(
                                "review finding list exceeded its admitted bound",
                            ));
                        }
                        spool.write_all(&encode_server_line(&frame)?)?;
                    }
                    ServerMessage::ReviewFindingsEnd { finding_count }
                        if finding_count.value() == count =>
                    {
                        break;
                    }
                    ServerMessage::Error {
                        code,
                        message,
                        detail,
                    } => {
                        return Err(ClientError::remote(*code, message.clone(), *detail));
                    }
                    _ => {
                        return Err(ClientError::Protocol(
                            "review finding list sequence or count was invalid",
                        ));
                    }
                }
            }
            spool.seek(SeekFrom::Start(0))?;
            let mut reader = BufReader::new(spool);
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line)? != 0 {
                match decode_server_line(&line)?.message() {
                    ServerMessage::ReviewFindingItem { finding } => {
                        output.review_finding(finding)?;
                    }
                    _ => {
                        return Err(ClientError::Protocol(
                            "review finding spool contained a non-finding frame",
                        ));
                    }
                }
                line.clear();
            }
            Ok(())
        }
    }
}

const fn review_pass_completion_is_coherent(
    outcome: ReviewPassTerminalOutcome,
    state: ReviewPassLifecycle,
) -> bool {
    matches!(
        (outcome, state),
        (
            ReviewPassTerminalOutcome::Succeeded,
            ReviewPassLifecycle::Succeeded
        ) | (
            ReviewPassTerminalOutcome::Failed,
            ReviewPassLifecycle::Failed
        ) | (
            ReviewPassTerminalOutcome::Blocked,
            ReviewPassLifecycle::Blocked
        ) | (
            ReviewPassTerminalOutcome::Cancelled,
            ReviewPassLifecycle::Cancelled
        )
    )
}

const fn review_finding_event_status(event: &ReviewFindingEvent) -> ReviewFindingStatus {
    match event {
        ReviewFindingEvent::Accepted {} => ReviewFindingStatus::Accepted,
        ReviewFindingEvent::Rejected { .. } => ReviewFindingStatus::Rejected,
        ReviewFindingEvent::Duplicate { .. } => ReviewFindingStatus::Duplicate,
        ReviewFindingEvent::Superseded { .. } => ReviewFindingStatus::Superseded,
        ReviewFindingEvent::Stale {} => ReviewFindingStatus::Stale,
        ReviewFindingEvent::Fixed {} => ReviewFindingStatus::Fixed,
        ReviewFindingEvent::BlockedWithReason { .. } => ReviewFindingStatus::BlockedWithReason,
    }
}

const fn review_import_state_is_coherent(
    outcome: ReviewImportTerminalOutcome,
    state: ReviewOrchestrationState,
) -> bool {
    match outcome {
        ReviewImportTerminalOutcome::Succeeded => {
            matches!(state, ReviewOrchestrationState::AwaitingConcerns)
        }
        ReviewImportTerminalOutcome::Failed
        | ReviewImportTerminalOutcome::Blocked
        | ReviewImportTerminalOutcome::Cancelled => {
            matches!(state, ReviewOrchestrationState::ImportIncomplete)
        }
    }
}

const fn review_concern_state_is_coherent(
    outcome: ReviewConcernTerminalOutcome,
    state: ReviewOrchestrationState,
) -> bool {
    match outcome {
        ReviewConcernTerminalOutcome::Succeeded => matches!(
            state,
            ReviewOrchestrationState::AwaitingConcerns
                | ReviewOrchestrationState::FanoutIncomplete
                | ReviewOrchestrationState::AwaitingJudgment
        ),
        ReviewConcernTerminalOutcome::Failed
        | ReviewConcernTerminalOutcome::Blocked
        | ReviewConcernTerminalOutcome::Cancelled => matches!(
            state,
            ReviewOrchestrationState::AwaitingConcerns | ReviewOrchestrationState::FanoutIncomplete
        ),
    }
}

const fn review_judgment_plan_state_is_coherent(
    plan_is_empty: bool,
    state: ReviewOrchestrationState,
) -> bool {
    matches!(
        (plan_is_empty, state),
        (true, ReviewOrchestrationState::AwaitingRepair)
            | (false, ReviewOrchestrationState::AwaitingJudgmentEffects)
    )
}

const fn review_judgment_effect_state_is_coherent(
    outcome: ReviewJudgmentEffectTerminalOutcome,
    state: ReviewOrchestrationState,
) -> bool {
    match outcome {
        ReviewJudgmentEffectTerminalOutcome::Applied => matches!(
            state,
            ReviewOrchestrationState::AwaitingJudgmentEffects
                | ReviewOrchestrationState::AwaitingRepair
        ),
        ReviewJudgmentEffectTerminalOutcome::Failed
        | ReviewJudgmentEffectTerminalOutcome::Blocked
        | ReviewJudgmentEffectTerminalOutcome::Cancelled => {
            matches!(state, ReviewOrchestrationState::JudgmentIncomplete)
        }
    }
}

const fn review_repair_state_is_coherent(
    has_blocked: bool,
    state: ReviewOrchestrationState,
) -> bool {
    matches!(
        (has_blocked, state),
        (true, ReviewOrchestrationState::RepairIncomplete)
            | (false, ReviewOrchestrationState::AwaitingPublication)
    )
}

const fn review_publication_state_is_coherent(
    all_published: bool,
    state: ReviewOrchestrationState,
) -> bool {
    matches!(
        (all_published, state),
        (true, ReviewOrchestrationState::Complete)
            | (false, ReviewOrchestrationState::PublicationIncomplete)
    )
}

fn review_run_response_is_coherent(
    run: &ReviewRunSnapshot,
    pass: Option<&ReviewPassSnapshot>,
) -> bool {
    match (run.pass_id, pass) {
        (None, None) => true,
        (Some(pass_id), Some(pass)) => {
            pass.pass_id == pass_id && pass.run_id == run.run_id && pass.target_id == run.target_id
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

fn review_command_identity(
    output: &mut Output<'_>,
    supplied: Option<CommandId>,
) -> Result<CommandId, ClientError> {
    let (command_id, generated) = command_identity(supplied)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    Ok(command_id)
}

fn command_identity(supplied: Option<CommandId>) -> Result<(CommandId, bool), ClientError> {
    match supplied {
        Some(command_id) => Ok((command_id, false)),
        None => CommandId::try_from_uuid(Uuid::now_v7())
            .map(|command_id| (command_id, true))
            .map_err(|_| ClientError::Protocol("UUIDv7 generator produced a reserved value")),
    }
}

fn selection_display(selection: ModelSelection) -> String {
    match selection {
        ModelSelection::Direct { selection_id } => format!("model={selection_id}"),
        ModelSelection::Alias { alias_id } => format!("alias={alias_id}"),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        ffi::OsString,
        fs,
        io::{self, Cursor},
        os::unix::fs::{PermissionsExt, symlink},
        path::{Path, PathBuf},
        process::ExitCode,
        time::Duration,
    };

    use signalbox_process_protocol::{
        BlobChunk, BoundChildAction, CanonicalBlobDigest, CanonicalU64, CanonicalUuid, ClientFrame,
        ClientRequest, CommandId, ContentFragment, ConversationImportFormat,
        ConversationImportSource, ConversationOriginFilter, ConversationSummary,
        DelegationMessageDirection, DelegationOutcome, DelegationPolicy, DelegationProvenance,
        DelegationReason, DelegationWaitMode, DescendantTerminationScope, EffectiveModelSettings,
        ErrorCode, ErrorDetail, FastMode, FrameEncodeError, GoalCommandRejection, GoalHistoryEvent,
        GoalLifecycleState, ImportedContentKind, ImportedSessionRelationship,
        ImportedSourceSpeaker, InputContent, InputDelivery, MAX_BLOB_CHUNK_BYTES,
        MAX_CONVERSATION_IMPORT_CHUNK_BYTES, MAX_FRAME_BYTES, ModelCallDisposition, ModelCallState,
        ModelSelection, ModelSettingSource, ModelSettingsOverlay, ModelSettingsPrecedence,
        ModelSettingsSnapshot, ProtocolVersion, ReasoningLevel, RejectionDetail, RequestId,
        ReviewConcernTerminalOutcome, ReviewExternalObjectKind, ReviewFindingEvent,
        ReviewFindingInput, ReviewFindingSnapshot, ReviewFindingStatus,
        ReviewJudgmentEffectTerminalOutcome, ReviewOrchestrationState, ReviewPassKind,
        ReviewPassLifecycle, ReviewPassSnapshot, ReviewPassTerminalOutcome, ReviewRunLifecycle,
        ReviewRunSnapshot, ReviewSeverity, ReviewWorkflow, RunnerConnectionHealth,
        RunnerPlacementRevision, RunnerProjection, RunnerProjectionSelector, RunnerProjectionState,
        RunnerSandboxProfile, RunnerStateTransitionState, ServerFrame, ServerMessage, SessionEvent,
        SessionPlacement, SettingOverlay, SystemPromptMember, SystemPromptText, ToolBatchState,
        ToolDecision, TurnState, UserInputContent, decode_client_line, encode_server_line,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
        time::timeout,
    };
    use uuid::Uuid;

    fn provider_default_model_settings() -> ModelSettingsSnapshot {
        ModelSettingsSnapshot {
            precedence: ModelSettingsPrecedence {
                per_call: ModelSettingsOverlay::inherit_all(),
                session: ModelSettingsOverlay::inherit_all(),
                profile: ModelSettingsOverlay::inherit_all(),
                global_default: ModelSettingsOverlay::inherit_all(),
            },
            effective: EffectiveModelSettings {
                reasoning_level: None,
                fast_mode: FastMode::Disabled,
                service_tier: None,
            },
            reasoning_source: None,
            fast_mode_source: None,
            service_tier_source: None,
            validated_for_selection_id: None,
        }
    }

    fn session_reasoning_model_settings(selection_id: CanonicalUuid) -> ModelSettingsSnapshot {
        let mut session = ModelSettingsOverlay::inherit_all();
        session.reasoning_level = SettingOverlay::Value(ReasoningLevel::High);
        ModelSettingsSnapshot {
            precedence: ModelSettingsPrecedence {
                per_call: ModelSettingsOverlay::inherit_all(),
                session,
                profile: ModelSettingsOverlay::inherit_all(),
                global_default: ModelSettingsOverlay::inherit_all(),
            },
            effective: EffectiveModelSettings {
                reasoning_level: Some(ReasoningLevel::High),
                fast_mode: FastMode::Disabled,
                service_tier: None,
            },
            reasoning_source: Some(ModelSettingSource::Session),
            fast_mode_source: None,
            service_tier_source: None,
            validated_for_selection_id: Some(selection_id),
        }
    }

    use super::{
        ClientDeploymentLimits, ConversationImportOutcome, ConversationsPageRequest,
        DelegationRejectionExpectation, DelegationRejectionOperation, GoalHistoryReplay,
        MAX_CONTENT_FRAGMENT_BYTES, MAX_INPUT_CONTENT_FRAME_BYTES, MAX_REVIEW_JSON_INPUT_BYTES,
        MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES, ModelSystemPromptChoice, PreparedBlobSource,
        ProcessClient, ReviewCommand, ReviewConcernsFile, ReviewFindingsFile,
        SessionMetadataPageRequest, SnapshotSelection, SubmitInputReceipt, ThroughPositionArgument,
        TurnTerminal, await_turn_terminal, blocker_recovery_snapshot_state, collect_import_paths,
        continue_imported, conversation_import_chunk_read_limit, conversations, create, decide,
        decode_goal_mutation_receipt, delegation_rejection_matches, descendant_scope,
        hash_blob_source, import_conversation_file, imported, model_call_recovery_transition,
        open_blob_source, open_scanned_import_source, placement_update_receipt_matches,
        placement_update_rejection_matches, queued_turn_recovery, queued_turn_runner_recovery,
        read_blob_chunk, read_blob_metadata, read_delegation_content_file, read_deployment_limits,
        read_goal_text_file, read_import_file, read_input, read_review_json_file,
        read_system_prompt_file, reconcile_turn, replace_session_model,
        replacement_receipt_settings_match, review, review_concern_state_is_coherent,
        review_finding_event_status, review_judgment_effect_state_is_coherent,
        review_judgment_plan_state_is_coherent, review_pass_completion_is_coherent,
        review_publication_state_is_coherent, review_repair_state_is_coherent, run, search,
        selected_turn_recovery_transition, session_recovery_transition, socket_path,
        source_fits_single_shot_import, stop_turn, submit_input, terminal_event_state,
        terminal_snapshot_selection, terminal_snapshot_state, tool_recovery_transition,
        upload_blob, validate_message_policy, validate_metadata_page_policy,
        validate_review_finding_count, validate_system_prompt_policy, write_blob_output,
    };
    use crate::{
        child_lifecycle_terminalization, error::ClientError, presentation::Output,
        transcript::TranscriptSnapshot,
    };

    /// The session a follower reads. Only a delegation event addressed to this
    /// exact session terminalizes the follower's own turn.
    const FOLLOWED_SESSION_IDENTITY: u128 = 0x5e5;

    fn followed_session() -> CanonicalUuid {
        CanonicalUuid::from_uuid(Uuid::from_u128(FOLLOWED_SESSION_IDENTITY))
    }

    fn runner_projection(
        revision: u64,
        state: RunnerProjectionState,
        connection_health: Option<RunnerConnectionHealth>,
    ) -> RunnerProjection {
        let runner_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        RunnerProjection::try_new(
            RunnerProjectionSelector::Runner { runner_id },
            Some(runner_id),
            RunnerPlacementRevision::try_new(revision)
                .expect("the fixture placement revision is positive"),
            RunnerSandboxProfile::WorkspaceRestricted,
            None,
            None,
            None,
            connection_health,
            state,
        )
        .expect("the fixture runner projection is coherent")
    }

    fn delegation_rejection_expectation(
        operation: DelegationRejectionOperation,
    ) -> DelegationRejectionExpectation {
        DelegationRejectionExpectation {
            session: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            turn: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            tool_request: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            operation,
        }
    }

    /// INV-033: spawn rejection evidence names the exact logical tool request.
    #[test]
    fn inv033_spawn_rejection_requires_exact_tool_request() {
        let expected = delegation_rejection_expectation(DelegationRejectionOperation::Spawn);
        let exact = RejectionDetail::DelegationSpawnConflict {
            tool_request_id: expected.tool_request,
        };
        let cross_wired = RejectionDetail::DelegationSpawnConflict {
            tool_request_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
        };

        assert!(delegation_rejection_matches(Some(exact), expected));
        assert!(!delegation_rejection_matches(Some(cross_wired), expected));
    }

    /// INV-033: a spawn mutation cannot accept an await-family rejection.
    #[test]
    fn inv033_spawn_rejection_rejects_await_family() {
        let expected = delegation_rejection_expectation(DelegationRejectionOperation::Spawn);
        let await_rejection = RejectionDetail::DelegationAwaitConflict {
            tool_request_id: expected.tool_request,
        };

        assert!(!delegation_rejection_matches(
            Some(await_rejection),
            expected
        ));
    }

    /// INV-033: a child identity collision names only daemon-minted state and
    /// cannot authenticate which spawn mutation produced the rejection.
    #[test]
    fn inv033_spawn_rejects_uncorrelated_child_identity_collision() {
        let expected = delegation_rejection_expectation(DelegationRejectionOperation::Spawn);
        let uncorrelated = RejectionDetail::DelegatedChildIdentityCollision {
            child_session_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
        };

        assert!(!delegation_rejection_matches(Some(uncorrelated), expected));
    }

    /// INV-033: common delegation rejection evidence repeats the exact
    /// request-supplied session, turn, and logical request identities.
    #[test]
    fn inv033_await_rejection_requires_exact_request_tuple() {
        let child = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let expected = delegation_rejection_expectation(DelegationRejectionOperation::Await {
            child,
            mode: DelegationWaitMode::Background,
        });
        let exact = RejectionDetail::DelegationRequestNotInTurn {
            session_id: expected.session,
            turn_id: expected.turn,
            tool_request_id: expected.tool_request,
        };
        let cross_wired = RejectionDetail::DelegationRequestNotInTurn {
            session_id: expected.session,
            turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
            tool_request_id: expected.tool_request,
        };

        assert!(delegation_rejection_matches(Some(exact), expected));
        assert!(!delegation_rejection_matches(Some(cross_wired), expected));
    }

    /// INV-033: delegation-wide missing-identity rejections repeat the exact
    /// request-supplied session and logical tool request identities.
    #[test]
    fn inv033_delegation_missing_identity_rejections_require_exact_request() {
        let peer = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let expected =
            delegation_rejection_expectation(DelegationRejectionOperation::Message { peer });
        let other = CanonicalUuid::from_uuid(Uuid::from_u128(5));

        assert!(delegation_rejection_matches(
            Some(RejectionDetail::SessionNotFound {
                session_id: expected.session,
            }),
            expected
        ));
        assert!(!delegation_rejection_matches(
            Some(RejectionDetail::SessionNotFound { session_id: other }),
            expected
        ));
        assert!(delegation_rejection_matches(
            Some(RejectionDetail::ToolRequestNotFound {
                tool_request_id: expected.tool_request,
            }),
            expected
        ));
        assert!(!delegation_rejection_matches(
            Some(RejectionDetail::ToolRequestNotFound {
                tool_request_id: other,
            }),
            expected
        ));
        assert!(delegation_rejection_matches(
            Some(RejectionDetail::ToolRequestNotInSession {
                session_id: expected.session,
                tool_request_id: expected.tool_request,
            }),
            expected
        ));
        assert!(!delegation_rejection_matches(
            Some(RejectionDetail::ToolRequestNotInSession {
                session_id: expected.session,
                tool_request_id: other,
            }),
            expected
        ));
    }

    /// INV-033: await missing-relationship evidence repeats both requested
    /// endpoints.
    #[test]
    fn inv033_await_rejection_requires_exact_relationship_endpoints() {
        let child = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let expected = delegation_rejection_expectation(DelegationRejectionOperation::Await {
            child,
            mode: DelegationWaitMode::Background,
        });
        let exact = RejectionDetail::DelegationRelationNotFound {
            session_id: expected.session,
            peer_session_id: child,
        };
        let cross_wired = RejectionDetail::DelegationRelationNotFound {
            session_id: expected.session,
            peer_session_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
        };

        assert!(delegation_rejection_matches(Some(exact), expected));
        assert!(!delegation_rejection_matches(Some(cross_wired), expected));
    }

    /// INV-033: relationship event exhaustion does not carry enough evidence
    /// to correlate an await mutation, so the response remains ambiguous.
    #[test]
    fn inv033_await_rejects_uncorrelated_event_ordinal_exhaustion() {
        let child = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let expected = delegation_rejection_expectation(DelegationRejectionOperation::Await {
            child,
            mode: DelegationWaitMode::Background,
        });
        let uncorrelated = RejectionDetail::DelegationEventOrdinalExhausted {
            spawning_request_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
            last: CanonicalU64::new(u64::MAX),
        };

        assert!(!delegation_rejection_matches(Some(uncorrelated), expected));
    }

    /// INV-033: background-await delivery exhaustion names the requesting
    /// parent as the result recipient.
    #[test]
    fn inv033_background_await_delivery_exhaustion_requires_parent_recipient() {
        let child = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let expected = delegation_rejection_expectation(DelegationRejectionOperation::Await {
            child,
            mode: DelegationWaitMode::Background,
        });
        let exact = RejectionDetail::DelegationDeliverySequenceExhausted {
            recipient_session_id: expected.session,
            last: CanonicalU64::new(u64::MAX),
        };
        let cross_wired = RejectionDetail::DelegationDeliverySequenceExhausted {
            recipient_session_id: child,
            last: CanonicalU64::new(u64::MAX),
        };

        assert!(delegation_rejection_matches(Some(exact), expected));
        assert!(!delegation_rejection_matches(Some(cross_wired), expected));
    }

    /// INV-033: a foreground await cannot report background-only delivery
    /// sequence exhaustion.
    #[test]
    fn inv033_foreground_await_rejects_delivery_sequence_exhaustion() {
        let child = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let expected = delegation_rejection_expectation(DelegationRejectionOperation::Await {
            child,
            mode: DelegationWaitMode::Foreground,
        });
        let background_only = RejectionDetail::DelegationDeliverySequenceExhausted {
            recipient_session_id: expected.session,
            last: CanonicalU64::new(u64::MAX),
        };

        assert!(!delegation_rejection_matches(
            Some(background_only),
            expected
        ));
    }

    /// INV-033: message missing-relationship evidence repeats both requested
    /// endpoints.
    #[test]
    fn inv033_message_rejection_requires_exact_relationship_endpoints() {
        let peer = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let expected =
            delegation_rejection_expectation(DelegationRejectionOperation::Message { peer });
        let exact = RejectionDetail::DelegationRelationNotFound {
            session_id: expected.session,
            peer_session_id: peer,
        };
        let cross_wired = RejectionDetail::DelegationRelationNotFound {
            session_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
            peer_session_id: peer,
        };

        assert!(delegation_rejection_matches(Some(exact), expected));
        assert!(!delegation_rejection_matches(Some(cross_wired), expected));
    }

    /// INV-033: relationship event exhaustion cannot authenticate which peer
    /// message mutation exhausted the shared relationship ordinal.
    #[test]
    fn inv033_message_rejects_uncorrelated_event_ordinal_exhaustion() {
        let peer = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let expected =
            delegation_rejection_expectation(DelegationRejectionOperation::Message { peer });
        let uncorrelated = RejectionDetail::DelegationEventOrdinalExhausted {
            spawning_request_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
            last: CanonicalU64::new(u64::MAX),
        };

        assert!(!delegation_rejection_matches(Some(uncorrelated), expected));
    }

    /// INV-033: a message identity collision names only daemon-minted state
    /// and cannot authenticate which message mutation produced the rejection.
    #[test]
    fn inv033_message_rejects_uncorrelated_identity_collision() {
        let peer = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let expected =
            delegation_rejection_expectation(DelegationRejectionOperation::Message { peer });
        let uncorrelated = RejectionDetail::DelegationMessageIdentityCollision {
            message_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
        };

        assert!(!delegation_rejection_matches(Some(uncorrelated), expected));
    }

    /// INV-033: message delivery exhaustion names the requested peer as its
    /// recipient.
    #[test]
    fn inv033_message_delivery_exhaustion_requires_peer_recipient() {
        let peer = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let expected =
            delegation_rejection_expectation(DelegationRejectionOperation::Message { peer });
        let exact = RejectionDetail::DelegationDeliverySequenceExhausted {
            recipient_session_id: peer,
            last: CanonicalU64::new(u64::MAX),
        };
        let cross_wired = RejectionDetail::DelegationDeliverySequenceExhausted {
            recipient_session_id: expected.session,
            last: CanonicalU64::new(u64::MAX),
        };

        assert!(delegation_rejection_matches(Some(exact), expected));
        assert!(!delegation_rejection_matches(Some(cross_wired), expected));
    }

    /// INV-033: a message mutation cannot accept an await-family rejection.
    #[test]
    fn inv033_message_rejection_rejects_await_family() {
        let peer = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let expected =
            delegation_rejection_expectation(DelegationRejectionOperation::Message { peer });
        let await_rejection = RejectionDetail::DelegationAwaitConflict {
            tool_request_id: expected.tool_request,
        };

        assert!(!delegation_rejection_matches(
            Some(await_rejection),
            expected
        ));
    }

    async fn accept_request_and_reply(
        listener: &UnixListener,
        expected: &ClientRequest,
        response: ServerMessage,
    ) -> io::Result<()> {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(request.request(), expected);
        let frame =
            ServerFrame::try_new_for_version(request.version(), request.request_id(), response)
                .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&frame).map_err(io::Error::other)?)
            .await
    }

    fn deployment_limits_message(limits: ClientDeploymentLimits) -> ServerMessage {
        ServerMessage::DeploymentLimits {
            max_message_utf8_bytes: limits.max_message_utf8_bytes.map(|value| {
                CanonicalU64::new(u64::try_from(value).expect("fixture limit fits u64"))
            }),
            max_system_prompt_utf8_bytes: limits.max_system_prompt_utf8_bytes.map(|value| {
                CanonicalU64::new(u64::try_from(value).expect("fixture limit fits u64"))
            }),
            terminal_input_channel_capacity: limits.terminal_input_channel_capacity.map(|value| {
                CanonicalU64::new(u64::try_from(value).expect("fixture limit fits u64"))
            }),
            min_metadata_page_size: limits.min_metadata_page_size.map(CanonicalU64::new),
            max_metadata_page_size: limits.max_metadata_page_size.map(CanonicalU64::new),
            max_review_findings_per_run: limits.max_review_findings_per_run.map(CanonicalU64::new),
        }
    }

    #[tokio::test]
    async fn client_learns_the_exact_deployment_limits_over_the_connection()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let expected = ClientDeploymentLimits {
            max_message_utf8_bytes: Some(7),
            max_system_prompt_utf8_bytes: None,
            terminal_input_channel_capacity: Some(3),
            min_metadata_page_size: Some(2),
            max_metadata_page_size: None,
            max_review_findings_per_run: Some(5),
        };
        let server = tokio::spawn(async move {
            accept_request_and_reply(
                &listener,
                &ClientRequest::ReadDeploymentLimits {},
                deployment_limits_message(expected),
            )
            .await
        });
        let mut client = ProcessClient::new(socket);

        let observed = read_deployment_limits(&mut client).await?;

        assert_eq!(observed, expected);
        server.await??;
        Ok(())
    }

    #[test]
    fn learned_limits_enforce_finite_policy_and_admit_unbounded_policy() {
        let finite = ClientDeploymentLimits {
            max_message_utf8_bytes: Some(3),
            max_system_prompt_utf8_bytes: Some(3),
            min_metadata_page_size: Some(2),
            max_metadata_page_size: Some(4),
            max_review_findings_per_run: Some(2),
            ..ClientDeploymentLimits::unbounded()
        };
        let system_prompt = SystemPromptText::try_new(String::from("four"))
            .expect("fixture prompt is structurally valid");

        assert!(validate_message_policy("four", Some(finite)).is_err());
        assert!(validate_system_prompt_policy(&system_prompt, Some(finite)).is_err());
        assert!(validate_metadata_page_policy(CanonicalU64::new(1), Some(finite)).is_err());
        assert!(validate_metadata_page_policy(CanonicalU64::new(5), Some(finite)).is_err());
        assert!(validate_review_finding_count(3, Some(finite)).is_err());
        assert!(validate_message_policy("four", Some(ClientDeploymentLimits::unbounded())).is_ok());
        assert!(
            validate_metadata_page_policy(
                CanonicalU64::new(u64::MAX),
                Some(ClientDeploymentLimits::unbounded()),
            )
            .is_ok()
        );
        assert!(
            validate_review_finding_count(usize::MAX, Some(ClientDeploymentLimits::unbounded()))
                .is_ok()
        );
    }

    fn client_arguments(socket: &Path, command: &[&str]) -> Vec<OsString> {
        [OsString::from("--socket"), socket.as_os_str().to_owned()]
            .into_iter()
            .chain(command.iter().map(OsString::from))
            .collect()
    }

    #[test]
    fn s19_descendant_scope_follows_the_explicit_cli_choice() {
        assert_eq!(
            descendant_scope(false),
            DescendantTerminationScope::ParentAlone
        );
        assert_eq!(
            descendant_scope(true),
            DescendantTerminationScope::ParentAndDescendants
        );
    }

    #[test]
    fn inv033_goal_history_replay_accepts_supersession_lineage() -> Result<(), ClientError> {
        let first_command = CommandId::try_from_uuid(Uuid::from_u128(11))
            .expect("fixture command identity is admitted");
        let supersede_command = CommandId::try_from_uuid(Uuid::from_u128(12))
            .expect("fixture command identity is admitted");
        let stop_command = CommandId::try_from_uuid(Uuid::from_u128(13))
            .expect("fixture command identity is admitted");
        let mut replay = GoalHistoryReplay::default();

        replay.apply(
            1,
            &GoalHistoryEvent::Commissioned {
                statement: String::from("first scope"),
                command_id: first_command,
            },
        )?;
        replay.apply(
            1,
            &GoalHistoryEvent::Superseded {
                replacement_statement: String::from("replacement scope"),
                command_id: supersede_command,
            },
        )?;
        replay.apply(
            2,
            &GoalHistoryEvent::UserStopped {
                command_id: stop_command,
            },
        )?;

        replay.validate_projection(2, "replacement scope", &GoalLifecycleState::UserStopped {})
    }

    #[test]
    fn inv033_goal_history_replay_rejects_an_invalid_first_transition() {
        let command_id = CommandId::try_from_uuid(Uuid::from_u128(14))
            .expect("fixture command identity is admitted");
        let mut replay = GoalHistoryReplay::default();

        let result = replay.apply(1, &GoalHistoryEvent::UserStopped { command_id });

        assert!(matches!(result, Err(ClientError::Protocol(_))));
    }

    #[test]
    fn inv033_goal_history_replay_rejects_a_mismatched_current_projection() {
        let command_id = CommandId::try_from_uuid(Uuid::from_u128(15))
            .expect("fixture command identity is admitted");
        let mut replay = GoalHistoryReplay::default();
        replay
            .apply(
                1,
                &GoalHistoryEvent::Commissioned {
                    statement: String::from("commissioned scope"),
                    command_id,
                },
            )
            .expect("the commissioning event is valid");

        let result =
            replay.validate_projection(1, "different projection", &GoalLifecycleState::Pursuing {});

        assert!(matches!(result, Err(ClientError::Protocol(_))));
    }

    #[test]
    fn goal_mutation_receipt_rejects_a_cross_wired_session() {
        let selected_session = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let foreign_session = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let message = ServerMessage::GoalTransitionApplied {
            session_id: foreign_session,
            event_ordinal: CanonicalU64::new(1),
            generation: CanonicalU64::new(1),
        };

        let error = decode_goal_mutation_receipt(selected_session, message)
            .expect_err("foreign session receipt is rejected");

        assert!(error.is_ambiguous_mutation());
    }

    #[test]
    fn placement_update_receipt_requires_the_exact_successor_and_echoed_request() {
        let requested_session = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let foreign_session = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let expected_version = CanonicalU64::new(7);
        let successor_version = CanonicalU64::new(8);
        let requested_placement = SessionPlacement::Pathless {};
        let foreign_placement =
            SessionPlacement::try_scoped(String::from("projects.other.session"))
                .expect("fixture placement is admitted");

        assert!(placement_update_receipt_matches(
            requested_session,
            successor_version,
            &requested_placement,
            requested_session,
            expected_version,
            &requested_placement,
        ));
        assert!(!placement_update_receipt_matches(
            foreign_session,
            successor_version,
            &requested_placement,
            requested_session,
            expected_version,
            &requested_placement,
        ));
        assert!(!placement_update_receipt_matches(
            requested_session,
            expected_version,
            &requested_placement,
            requested_session,
            expected_version,
            &requested_placement,
        ));
        assert!(!placement_update_receipt_matches(
            requested_session,
            successor_version,
            &foreign_placement,
            requested_session,
            expected_version,
            &requested_placement,
        ));
    }

    #[test]
    fn placement_update_rejection_requires_the_exact_request_evidence() {
        let requested_session = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let foreign_session = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let expected_version = CanonicalU64::new(u64::MAX);

        assert!(placement_update_rejection_matches(
            Some(RejectionDetail::SessionNotFound {
                session_id: requested_session,
            }),
            requested_session,
            expected_version,
        ));
        assert!(!placement_update_rejection_matches(
            Some(RejectionDetail::SessionNotFound {
                session_id: foreign_session,
            }),
            requested_session,
            expected_version,
        ));
        assert!(placement_update_rejection_matches(
            Some(RejectionDetail::SessionPlacementCurrentVersionMismatch {
                session_id: requested_session,
                expected_placement_version: expected_version,
                current_placement_version: CanonicalU64::new(3),
            }),
            requested_session,
            expected_version,
        ));
        assert!(!placement_update_rejection_matches(
            Some(RejectionDetail::SessionPlacementCurrentVersionMismatch {
                session_id: requested_session,
                expected_placement_version: CanonicalU64::new(3),
                current_placement_version: CanonicalU64::new(4),
            }),
            requested_session,
            expected_version,
        ));
        assert!(placement_update_rejection_matches(
            Some(RejectionDetail::SessionPlacementVersionExhausted {
                session_id: requested_session,
                current_placement_version: expected_version,
            }),
            requested_session,
            expected_version,
        ));
        assert!(!placement_update_rejection_matches(
            Some(RejectionDetail::GoalCommandRejected {
                session_id: requested_session,
                reason: GoalCommandRejection::SessionNotFound,
            }),
            requested_session,
            expected_version,
        ));
        assert!(!placement_update_rejection_matches(
            None,
            requested_session,
            expected_version,
        ));
    }

    #[tokio::test]
    async fn goal_text_file_reads_the_exact_maximum() -> Result<(), Box<dyn Error>> {
        let file = tempfile::NamedTempFile::new()?;
        fs::write(file.path(), vec![b'g'; MAX_CONTENT_FRAGMENT_BYTES])?;

        let text = read_goal_text_file(file.path()).await?;

        assert_eq!(text.len(), MAX_CONTENT_FRAGMENT_BYTES);
        Ok(())
    }

    #[tokio::test]
    async fn goal_text_file_rejects_content_beyond_the_maximum() -> Result<(), Box<dyn Error>> {
        let file = tempfile::NamedTempFile::new()?;
        fs::write(file.path(), vec![b'g'; MAX_CONTENT_FRAGMENT_BYTES + 1])?;

        let result = read_goal_text_file(file.path()).await;

        assert!(matches!(result, Err(ClientError::Input(_))));
        Ok(())
    }

    #[tokio::test]
    async fn goal_text_file_error_retains_the_selected_path() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("missing-goal.txt");

        let error = read_goal_text_file(&path)
            .await
            .expect_err("the absent goal text file is rejected");

        assert!(error.to_string().contains(&path.display().to_string()));
        assert_eq!(
            error
                .source()
                .and_then(|source| source.downcast_ref::<std::io::Error>())
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::NotFound)
        );
        Ok(())
    }

    #[tokio::test]
    async fn delegation_content_file_utf8_error_retains_path_and_source()
    -> Result<(), Box<dyn Error>> {
        let file = tempfile::NamedTempFile::new()?;
        fs::write(file.path(), [0xff])?;

        let error = read_delegation_content_file(file.path())
            .await
            .expect_err("invalid UTF-8 delegation content is rejected");

        assert!(
            error
                .to_string()
                .contains(&file.path().display().to_string())
        );
        assert!(
            error
                .source()
                .and_then(|source| source.downcast_ref::<std::string::FromUtf8Error>())
                .is_some()
        );
        Ok(())
    }

    #[tokio::test]
    async fn review_findings_file_decodes_an_empty_complete_inventory()
    -> Result<(), Box<dyn std::error::Error>> {
        let file = tempfile::NamedTempFile::new()?;
        fs::write(file.path(), br#"{"findings":[]}"#)?;

        let decoded: ReviewFindingsFile = read_review_json_file(file.path()).await?;

        assert!(decoded.findings.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn review_concerns_file_decodes_its_exact_wrapper()
    -> Result<(), Box<dyn std::error::Error>> {
        let file = tempfile::NamedTempFile::new()?;
        fs::write(
            file.path(),
            br#"{"concerns":[{"key":"correctness","template_name":"review.correctness"}]}"#,
        )?;

        let decoded: ReviewConcernsFile = read_review_json_file(file.path()).await?;

        assert_eq!(decoded.concerns.len(), 1);
        assert_eq!(decoded.concerns[0].key, "correctness");
        Ok(())
    }

    #[tokio::test]
    async fn review_concerns_file_rejects_an_unknown_wrapper_member()
    -> Result<(), Box<dyn std::error::Error>> {
        let file = tempfile::NamedTempFile::new()?;
        fs::write(
            file.path(),
            br#"{"concerns":[{"key":"correctness","template_name":"review.correctness"}],"future":true}"#,
        )?;

        let decoded = read_review_json_file::<ReviewConcernsFile>(file.path()).await;

        assert!(matches!(decoded, Err(ClientError::ReviewInputJson(_))));
        Ok(())
    }

    #[tokio::test]
    async fn review_json_file_rejects_content_beyond_the_frame_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let file = tempfile::NamedTempFile::new()?;
        fs::write(file.path(), vec![b'x'; MAX_REVIEW_JSON_INPUT_BYTES + 1])?;

        let decoded = read_review_json_file::<ReviewConcernsFile>(file.path()).await;

        assert!(matches!(decoded, Err(ClientError::ReviewInputExceedsFrame)));
        Ok(())
    }

    #[tokio::test]
    async fn review_json_file_bound_reserves_request_envelope_headroom()
    -> Result<(), Box<dyn std::error::Error>> {
        let file = tempfile::NamedTempFile::new()?;
        fs::write(file.path(), vec![b'x'; MAX_REVIEW_JSON_INPUT_BYTES])?;

        let decoded = read_review_json_file::<ReviewConcernsFile>(file.path()).await;

        const {
            assert!(MAX_REVIEW_JSON_INPUT_BYTES < signalbox_process_protocol::MAX_FRAME_BYTES);
        }
        assert!(!matches!(
            decoded,
            Err(ClientError::ReviewInputExceedsFrame)
        ));
        Ok(())
    }

    #[test]
    fn concern_acknowledgement_correlates_the_submitted_outcome() {
        assert!(review_concern_state_is_coherent(
            ReviewConcernTerminalOutcome::Succeeded,
            ReviewOrchestrationState::AwaitingJudgment,
        ));
        assert!(!review_concern_state_is_coherent(
            ReviewConcernTerminalOutcome::Failed,
            ReviewOrchestrationState::AwaitingJudgment,
        ));
    }

    #[test]
    fn successful_last_concern_can_close_an_incomplete_fanout() {
        assert!(review_concern_state_is_coherent(
            ReviewConcernTerminalOutcome::Succeeded,
            ReviewOrchestrationState::FanoutIncomplete,
        ));
    }

    #[test]
    fn judgment_acknowledgements_correlate_the_submitted_facts() {
        assert!(review_judgment_plan_state_is_coherent(
            true,
            ReviewOrchestrationState::AwaitingRepair,
        ));
        assert!(!review_judgment_effect_state_is_coherent(
            ReviewJudgmentEffectTerminalOutcome::Blocked,
            ReviewOrchestrationState::AwaitingRepair,
        ));
    }

    #[test]
    fn repair_acknowledgement_correlates_the_blocked_barrier() {
        assert!(review_repair_state_is_coherent(
            true,
            ReviewOrchestrationState::RepairIncomplete,
        ));
        assert!(!review_repair_state_is_coherent(
            true,
            ReviewOrchestrationState::AwaitingPublication,
        ));
    }

    #[test]
    fn publication_acknowledgement_correlates_the_complete_inventory() {
        assert!(review_publication_state_is_coherent(
            true,
            ReviewOrchestrationState::Complete,
        ));
        assert!(!review_publication_state_is_coherent(
            false,
            ReviewOrchestrationState::Complete,
        ));
    }

    #[test]
    fn review_pass_completion_response_requires_the_exact_terminal_state() {
        assert!(review_pass_completion_is_coherent(
            ReviewPassTerminalOutcome::Succeeded,
            ReviewPassLifecycle::Succeeded,
        ));
        assert!(!review_pass_completion_is_coherent(
            ReviewPassTerminalOutcome::Succeeded,
            ReviewPassLifecycle::Failed,
        ));
    }

    #[test]
    fn review_finding_event_response_requires_the_derived_status() {
        let event = ReviewFindingEvent::Fixed {};

        assert_eq!(
            review_finding_event_status(&event),
            ReviewFindingStatus::Fixed
        );
    }

    #[test]
    fn coherent_review_run_response_is_accepted() {
        let pass = review_pass_snapshot();
        let run = review_run_snapshot(Some(pass.pass_id));

        assert!(super::review_run_response_is_coherent(&run, Some(&pass)));
    }

    #[test]
    fn review_run_response_rejects_a_missing_recorded_pass() {
        let recorded_pass = review_pass_snapshot();
        let run = review_run_snapshot(Some(recorded_pass.pass_id));

        assert!(!super::review_run_response_is_coherent(&run, None));
    }

    #[test]
    fn review_run_response_rejects_cross_wired_pass_ancestry() {
        const FOREIGN_TARGET_IDENTITY: u128 = 4;

        let mut pass = review_pass_snapshot();
        let run = review_run_snapshot(Some(pass.pass_id));
        pass.target_id = CanonicalUuid::from_uuid(Uuid::from_u128(FOREIGN_TARGET_IDENTITY));

        assert!(!super::review_run_response_is_coherent(&run, Some(&pass)));
    }

    #[test]
    fn empty_standard_input_is_rejected() {
        assert!(read_input(&mut Cursor::new(Vec::<u8>::new())).is_err());
    }

    #[test]
    fn nul_in_standard_input_is_rejected() {
        assert!(read_input(&mut Cursor::new(b"before\0after".to_vec())).is_err());
    }

    #[test]
    fn oversized_standard_input_is_rejected() {
        assert!(
            read_input(&mut Cursor::new(vec![
                b'a';
                MAX_INPUT_CONTENT_FRAME_BYTES + 1
            ]))
            .is_err()
        );
    }

    #[test]
    fn exact_limit_standard_input_is_accepted() {
        let exact = vec![b'a'; MAX_INPUT_CONTENT_FRAME_BYTES];
        assert_eq!(
            read_input(&mut Cursor::new(exact.clone()))
                .ok()
                .map(|value| value.into_bytes()),
            Some(exact)
        );
    }

    #[test]
    fn send_waits_while_automatic_model_call_recovery_owns_the_decision() {
        let state = TurnState::ActiveAwaitingModelCallRecovery {
            ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            recovery_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            automatic_reconciliation_attempts: CanonicalU64::new(0),
            operator_action_required: false,
        };

        assert!(matches!(terminal_snapshot_state(Some(&state)), Ok(None)));
    }

    #[test]
    fn send_fails_when_model_call_recovery_requires_operator_action() {
        let state = TurnState::ActiveAwaitingModelCallRecovery {
            ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            recovery_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            automatic_reconciliation_attempts: CanonicalU64::new(5),
            operator_action_required: true,
        };

        assert!(matches!(
            terminal_snapshot_state(Some(&state)),
            Err(ClientError::TurnRecoveryRequired)
        ));
    }

    #[test]
    fn send_waits_while_automatic_tool_recovery_owns_the_decision() {
        let state = TurnState::ActiveAwaitingToolRecovery {
            ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            recovery_tool_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            automatic_reconciliation_attempts: CanonicalU64::new(0),
            operator_action_required: false,
        };

        assert!(matches!(terminal_snapshot_state(Some(&state)), Ok(None)));
    }

    #[test]
    fn send_fails_when_tool_recovery_requires_operator_action() {
        let state = TurnState::ActiveAwaitingToolRecovery {
            ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            recovery_tool_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            automatic_reconciliation_attempts: CanonicalU64::new(5),
            operator_action_required: true,
        };

        assert!(matches!(
            terminal_snapshot_state(Some(&state)),
            Err(ClientError::TurnRecoveryRequired)
        ));
    }

    #[test]
    fn queued_send_waits_while_automatic_tool_recovery_owns_its_blocker() {
        let blocker = TurnState::ActiveAwaitingToolRecovery {
            ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            recovery_tool_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            automatic_reconciliation_attempts: CanonicalU64::new(0),
            operator_action_required: false,
        };

        assert!(matches!(blocker_recovery_snapshot_state(&blocker), Ok(())));
    }

    #[test]
    fn queued_send_fails_when_its_tool_recovery_blocker_requires_operator_action() {
        let blocker = TurnState::ActiveAwaitingToolRecovery {
            ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            recovery_tool_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            automatic_reconciliation_attempts: CanonicalU64::new(5),
            operator_action_required: true,
        };

        assert!(matches!(
            blocker_recovery_snapshot_state(&blocker),
            Err(ClientError::TurnRecoveryRequired)
        ));
    }

    #[test]
    fn send_fails_explicitly_when_runner_recovery_is_required() {
        let state = TurnState::ActiveAwaitingRunnerRecovery {
            runner_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            placement_revision: signalbox_process_protocol::PositiveCanonicalU64::try_new(2)
                .expect("the fixture revision is positive"),
            tool_attempt_id: None,
        };
        let error = terminal_snapshot_state(Some(&state))
            .expect_err("runner recovery cannot be completed by the terminal");

        assert!(matches!(&error, ClientError::RunnerRecoveryRequired));
        assert_eq!(
            error.to_string(),
            "the submitted turn awaits lost-runner replacement or stop_turn before abandonment"
        );
    }

    #[test]
    fn queued_send_fails_explicitly_when_its_active_blocker_awaits_runner_recovery() {
        let blocker = TurnState::ActiveAwaitingRunnerRecovery {
            runner_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            placement_revision: signalbox_process_protocol::PositiveCanonicalU64::try_new(2)
                .expect("the fixture revision is positive"),
            tool_attempt_id: None,
        };

        assert!(matches!(
            blocker_recovery_snapshot_state(&blocker),
            Err(ClientError::RunnerRecoveryRequired)
        ));
    }

    #[test]
    fn send_classifies_cancelled_snapshot_truth() {
        let state = TurnState::Cancelled {
            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            terminal_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            terminal_model_call_id: None,
        };

        assert_eq!(
            terminal_snapshot_state(Some(&state))
                .expect("cancelled state is terminal protocol truth"),
            Some(TurnTerminal::Cancelled)
        );
    }

    #[test]
    fn send_classifies_reconciliation_required_snapshot_truth() {
        let state = TurnState::ReconciliationRequired {
            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            terminal_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            terminal_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
        };

        assert_eq!(
            terminal_snapshot_state(Some(&state))
                .expect("reconciliation state is terminal protocol truth"),
            Some(TurnTerminal::ReconciliationRequired)
        );
    }

    #[tokio::test]
    async fn queued_send_wait_uses_active_slot_not_acceptance_order_or_terminal_history()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let historical_terminal_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let current_blocker_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        let queued_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let follow_request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                follow_request.request(),
                &ClientRequest::FollowSession { session_id }
            );
            let snapshot = |version, request_id, cursor, blocker_state| -> io::Result<Vec<u8>> {
                let frame = |message| {
                    ServerFrame::try_new_for_version(version, request_id, message)
                        .map_err(io::Error::other)
                };
                let mut response =
                    encode_server_line(&frame(ServerMessage::TranscriptSnapshotStart {
                        session_id,
                        cursor: CanonicalU64::new(cursor),
                        runner: None,
                    })?)
                    .map_err(io::Error::other)?;
                response.extend_from_slice(
                    &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                        turn_id: historical_terminal_turn_id,
                        acceptance_position: CanonicalU64::new(1),
                        model_settings: None,
                        state: TurnState::ReconciliationRequired {
                            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
                            terminal_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(6)),
                            terminal_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(7)),
                        },
                    })?)
                    .map_err(io::Error::other)?,
                );
                response.extend_from_slice(
                    &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                        turn_id: queued_turn_id,
                        acceptance_position: CanonicalU64::new(3),
                        model_settings: None,
                        state: TurnState::Queued {
                            accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(10)),
                            content: UserInputContent::text(String::from("wait behind recovery")),
                        },
                    })?)
                    .map_err(io::Error::other)?,
                );
                response.extend_from_slice(
                    &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                        turn_id: current_blocker_turn_id,
                        acceptance_position: CanonicalU64::new(4),
                        model_settings: None,
                        state: blocker_state,
                    })?)
                    .map_err(io::Error::other)?,
                );
                response.extend_from_slice(
                    &encode_server_line(&frame(ServerMessage::TranscriptModelCallsEnd {
                        model_call_count: CanonicalU64::new(0),
                    })?)
                    .map_err(io::Error::other)?,
                );
                response.extend_from_slice(
                    &encode_server_line(&frame(ServerMessage::TranscriptSnapshotEnd {
                        session_id,
                        cursor: CanonicalU64::new(cursor),
                        turn_count: CanonicalU64::new(3),
                        entry_count: CanonicalU64::new(0),
                    })?)
                    .map_err(io::Error::other)?,
                );
                Ok(response)
            };
            let mut initial = snapshot(
                follow_request.version(),
                follow_request.request_id(),
                0,
                TurnState::ActiveRunning {
                    current_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(8)),
                    current_model_call: None,
                },
            )?;
            initial.extend_from_slice(
                &encode_server_line(
                    &ServerFrame::try_new_for_version(
                        follow_request.version(),
                        follow_request.request_id(),
                        ServerMessage::SessionEvent {
                            cursor: CanonicalU64::new(1),
                            session_id,
                            event: SessionEvent::ModelCallTransition {
                                turn_id: current_blocker_turn_id,
                                model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(9)),
                                state: ModelCallState::Terminal {
                                    disposition: ModelCallDisposition::Ambiguous,
                                },
                            },
                        },
                    )
                    .map_err(io::Error::other)?,
                )
                .map_err(io::Error::other)?,
            );
            writer.write_all(&initial).await?;

            let (refresh_stream, mut refresh_writer) = listener.accept().await?.0.into_split();
            let mut refresh_reader = BufReader::new(refresh_stream);
            let mut refresh_line = Vec::new();
            refresh_reader.read_until(b'\n', &mut refresh_line).await?;
            let refresh_request = decode_client_line(&refresh_line).map_err(io::Error::other)?;
            assert_eq!(
                refresh_request.request(),
                &ClientRequest::ReadTranscript { session_id }
            );
            let refreshed = snapshot(
                refresh_request.version(),
                refresh_request.request_id(),
                1,
                TurnState::ActiveAwaitingModelCallRecovery {
                    ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(8)),
                    recovery_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(9)),
                    automatic_reconciliation_attempts: CanonicalU64::new(0),
                    operator_action_required: false,
                },
            )?;
            refresh_writer.write_all(&refreshed).await?;

            let (exhausted_stream, mut exhausted_writer) = listener.accept().await?.0.into_split();
            let mut exhausted_reader = BufReader::new(exhausted_stream);
            let mut exhausted_line = Vec::new();
            exhausted_reader
                .read_until(b'\n', &mut exhausted_line)
                .await?;
            let exhausted_request =
                decode_client_line(&exhausted_line).map_err(io::Error::other)?;
            assert_eq!(
                exhausted_request.request(),
                &ClientRequest::ReadTranscript { session_id }
            );
            let exhausted = snapshot(
                exhausted_request.version(),
                exhausted_request.request_id(),
                1,
                TurnState::ActiveAwaitingModelCallRecovery {
                    ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(8)),
                    recovery_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(9)),
                    automatic_reconciliation_attempts: CanonicalU64::new(5),
                    operator_action_required: true,
                },
            )?;
            exhausted_writer.write_all(&exhausted).await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let result = await_turn_terminal(&mut client, session_id, queued_turn_id).await;

        assert!(matches!(result, Err(ClientError::TurnRecoveryRequired)));
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn selected_send_polls_after_an_automatic_recovery_transition()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let attempt_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        let model_call_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let server = tokio::spawn(async move {
            let snapshot = |version, request_id, cursor, state| -> io::Result<Vec<u8>> {
                let frame = |message| {
                    ServerFrame::try_new_for_version(version, request_id, message)
                        .map_err(io::Error::other)
                };
                let mut response =
                    encode_server_line(&frame(ServerMessage::TranscriptSnapshotStart {
                        session_id,
                        cursor: CanonicalU64::new(cursor),
                        runner: None,
                    })?)
                    .map_err(io::Error::other)?;
                response.extend_from_slice(
                    &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                        turn_id,
                        acceptance_position: CanonicalU64::new(1),
                        model_settings: None,
                        state,
                    })?)
                    .map_err(io::Error::other)?,
                );
                response.extend_from_slice(
                    &encode_server_line(&frame(ServerMessage::TranscriptModelCallsEnd {
                        model_call_count: CanonicalU64::new(0),
                    })?)
                    .map_err(io::Error::other)?,
                );
                response.extend_from_slice(
                    &encode_server_line(&frame(ServerMessage::TranscriptSnapshotEnd {
                        session_id,
                        cursor: CanonicalU64::new(cursor),
                        turn_count: CanonicalU64::new(1),
                        entry_count: CanonicalU64::new(0),
                    })?)
                    .map_err(io::Error::other)?,
                );
                Ok(response)
            };

            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let follow_request = decode_client_line(&line).map_err(io::Error::other)?;
            let mut initial = snapshot(
                follow_request.version(),
                follow_request.request_id(),
                0,
                TurnState::ActiveRunning {
                    current_attempt_id: attempt_id,
                    current_model_call: None,
                },
            )?;
            initial.extend_from_slice(
                &encode_server_line(
                    &ServerFrame::try_new_for_version(
                        follow_request.version(),
                        follow_request.request_id(),
                        ServerMessage::SessionEvent {
                            cursor: CanonicalU64::new(1),
                            session_id,
                            event: SessionEvent::ModelCallTransition {
                                turn_id,
                                model_call_id,
                                state: ModelCallState::Terminal {
                                    disposition: ModelCallDisposition::Ambiguous,
                                },
                            },
                        },
                    )
                    .map_err(io::Error::other)?,
                )
                .map_err(io::Error::other)?,
            );
            writer.write_all(&initial).await?;

            let (refresh_stream, mut refresh_writer) = listener.accept().await?.0.into_split();
            let mut refresh_reader = BufReader::new(refresh_stream);
            let mut refresh_line = Vec::new();
            refresh_reader.read_until(b'\n', &mut refresh_line).await?;
            let refresh_request = decode_client_line(&refresh_line).map_err(io::Error::other)?;
            refresh_writer
                .write_all(&snapshot(
                    refresh_request.version(),
                    refresh_request.request_id(),
                    1,
                    TurnState::ActiveAwaitingModelCallRecovery {
                        ended_attempt_id: attempt_id,
                        recovery_model_call_id: model_call_id,
                        automatic_reconciliation_attempts: CanonicalU64::new(0),
                        operator_action_required: false,
                    },
                )?)
                .await?;

            let (poll_stream, mut poll_writer) = listener.accept().await?.0.into_split();
            let mut poll_reader = BufReader::new(poll_stream);
            let mut poll_line = Vec::new();
            poll_reader.read_until(b'\n', &mut poll_line).await?;
            let poll_request = decode_client_line(&poll_line).map_err(io::Error::other)?;
            poll_writer
                .write_all(&snapshot(
                    poll_request.version(),
                    poll_request.request_id(),
                    1,
                    TurnState::ReconciliationRequired {
                        terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
                        terminal_attempt_id: attempt_id,
                        terminal_model_call_id: model_call_id,
                    },
                )?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let terminal = await_turn_terminal(&mut client, session_id, turn_id).await?;

        assert_eq!(terminal, TurnTerminal::ReconciliationRequired);
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn selected_send_polls_after_an_automatic_tool_recovery_transition()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let attempt_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        let model_call_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let tool_attempt_id = CanonicalUuid::from_uuid(Uuid::from_u128(6));
        let server = tokio::spawn(async move {
            let snapshot = |version, request_id, cursor, state| -> io::Result<Vec<u8>> {
                let frame = |message| {
                    ServerFrame::try_new_for_version(version, request_id, message)
                        .map_err(io::Error::other)
                };
                let mut response =
                    encode_server_line(&frame(ServerMessage::TranscriptSnapshotStart {
                        session_id,
                        cursor: CanonicalU64::new(cursor),
                        runner: None,
                    })?)
                    .map_err(io::Error::other)?;
                response.extend_from_slice(
                    &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                        turn_id,
                        acceptance_position: CanonicalU64::new(1),
                        model_settings: None,
                        state,
                    })?)
                    .map_err(io::Error::other)?,
                );
                response.extend_from_slice(
                    &encode_server_line(&frame(ServerMessage::TranscriptModelCallsEnd {
                        model_call_count: CanonicalU64::new(0),
                    })?)
                    .map_err(io::Error::other)?,
                );
                response.extend_from_slice(
                    &encode_server_line(&frame(ServerMessage::TranscriptSnapshotEnd {
                        session_id,
                        cursor: CanonicalU64::new(cursor),
                        turn_count: CanonicalU64::new(1),
                        entry_count: CanonicalU64::new(0),
                    })?)
                    .map_err(io::Error::other)?,
                );
                Ok(response)
            };

            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let follow_request = decode_client_line(&line).map_err(io::Error::other)?;
            let mut initial = snapshot(
                follow_request.version(),
                follow_request.request_id(),
                0,
                TurnState::ActiveRunning {
                    current_attempt_id: attempt_id,
                    current_model_call: None,
                },
            )?;
            initial.extend_from_slice(
                &encode_server_line(
                    &ServerFrame::try_new_for_version(
                        follow_request.version(),
                        follow_request.request_id(),
                        ServerMessage::SessionEvent {
                            cursor: CanonicalU64::new(1),
                            session_id,
                            event: SessionEvent::ToolBatchTransition {
                                turn_id,
                                model_call_id,
                                state: ToolBatchState::RecoveryRequired { tool_attempt_id },
                            },
                        },
                    )
                    .map_err(io::Error::other)?,
                )
                .map_err(io::Error::other)?,
            );
            writer.write_all(&initial).await?;

            let (refresh_stream, mut refresh_writer) = listener.accept().await?.0.into_split();
            let mut refresh_reader = BufReader::new(refresh_stream);
            let mut refresh_line = Vec::new();
            refresh_reader.read_until(b'\n', &mut refresh_line).await?;
            let refresh_request = decode_client_line(&refresh_line).map_err(io::Error::other)?;
            refresh_writer
                .write_all(&snapshot(
                    refresh_request.version(),
                    refresh_request.request_id(),
                    1,
                    TurnState::ActiveAwaitingToolRecovery {
                        ended_attempt_id: attempt_id,
                        recovery_tool_attempt_id: tool_attempt_id,
                        automatic_reconciliation_attempts: CanonicalU64::new(0),
                        operator_action_required: false,
                    },
                )?)
                .await?;

            let (poll_stream, mut poll_writer) = listener.accept().await?.0.into_split();
            let mut poll_reader = BufReader::new(poll_stream);
            let mut poll_line = Vec::new();
            poll_reader.read_until(b'\n', &mut poll_line).await?;
            let poll_request = decode_client_line(&poll_line).map_err(io::Error::other)?;
            poll_writer
                .write_all(&snapshot(
                    poll_request.version(),
                    poll_request.request_id(),
                    1,
                    TurnState::ToolReconciliationRequired {
                        terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
                        terminal_attempt_id: attempt_id,
                        terminal_tool_attempt_id: tool_attempt_id,
                    },
                )?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let terminal = await_turn_terminal(&mut client, session_id, turn_id).await?;

        assert_eq!(terminal, TurnTerminal::ReconciliationRequired);
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn send_wait_continues_after_a_superseded_runner_loss_event() -> Result<(), Box<dyn Error>>
    {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let attempt_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        let model_call_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let follow_request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                follow_request.request(),
                &ClientRequest::FollowSession { session_id }
            );
            let snapshot = |version, request_id, cursor| -> io::Result<Vec<u8>> {
                let frame = |message| {
                    ServerFrame::try_new_for_version(version, request_id, message)
                        .map_err(io::Error::other)
                };
                let mut response =
                    encode_server_line(&frame(ServerMessage::TranscriptSnapshotStart {
                        session_id,
                        cursor: CanonicalU64::new(cursor),
                        runner: None,
                    })?)
                    .map_err(io::Error::other)?;
                response.extend_from_slice(
                    &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                        turn_id,
                        acceptance_position: CanonicalU64::new(1),
                        model_settings: None,
                        state: TurnState::ActiveRunning {
                            current_attempt_id: attempt_id,
                            current_model_call: None,
                        },
                    })?)
                    .map_err(io::Error::other)?,
                );
                response.extend_from_slice(
                    &encode_server_line(&frame(ServerMessage::TranscriptModelCallsEnd {
                        model_call_count: CanonicalU64::new(0),
                    })?)
                    .map_err(io::Error::other)?,
                );
                response.extend_from_slice(
                    &encode_server_line(&frame(ServerMessage::TranscriptSnapshotEnd {
                        session_id,
                        cursor: CanonicalU64::new(cursor),
                        turn_count: CanonicalU64::new(1),
                        entry_count: CanonicalU64::new(0),
                    })?)
                    .map_err(io::Error::other)?,
                );
                Ok(response)
            };
            let mut initial = snapshot(follow_request.version(), follow_request.request_id(), 0)?;
            initial.extend_from_slice(
                &encode_server_line(
                    &ServerFrame::try_new_for_version(
                        follow_request.version(),
                        follow_request.request_id(),
                        ServerMessage::SessionEvent {
                            cursor: CanonicalU64::new(1),
                            session_id,
                            event: SessionEvent::RunnerStateTransition {
                                runner_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
                                placement_revision: RunnerPlacementRevision::try_new(1)
                                    .ok_or_else(|| io::Error::other("positive fixture revision"))?,
                                sandbox_profile: RunnerSandboxProfile::WorkspaceRestricted,
                                working_directory: None,
                                state: RunnerStateTransitionState::RunnerLost,
                            },
                        },
                    )
                    .map_err(io::Error::other)?,
                )
                .map_err(io::Error::other)?,
            );
            writer.write_all(&initial).await?;

            let (refresh_stream, mut refresh_writer) = listener.accept().await?.0.into_split();
            let mut refresh_reader = BufReader::new(refresh_stream);
            let mut refresh_line = Vec::new();
            refresh_reader.read_until(b'\n', &mut refresh_line).await?;
            let refresh_request = decode_client_line(&refresh_line).map_err(io::Error::other)?;
            assert_eq!(
                refresh_request.request(),
                &ClientRequest::ReadTranscript { session_id }
            );
            refresh_writer
                .write_all(&snapshot(
                    refresh_request.version(),
                    refresh_request.request_id(),
                    1,
                )?)
                .await?;
            writer
                .write_all(
                    &encode_server_line(
                        &ServerFrame::try_new_for_version(
                            follow_request.version(),
                            follow_request.request_id(),
                            ServerMessage::SessionEvent {
                                cursor: CanonicalU64::new(2),
                                session_id,
                                event: SessionEvent::TurnCompleted {
                                    turn_id,
                                    model_call_id,
                                    completion_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                                        6,
                                    )),
                                    terminal_frontier_id: CanonicalUuid::from_uuid(
                                        Uuid::from_u128(7),
                                    ),
                                },
                            },
                        )
                        .map_err(io::Error::other)?,
                    )
                    .map_err(io::Error::other)?,
                )
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let terminal = await_turn_terminal(&mut client, session_id, turn_id).await?;

        assert_eq!(terminal, TurnTerminal::Completed);
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn send_wait_ignores_streamed_text_until_the_durable_terminal_event()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let model_call_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::FollowSession { session_id }
            );
            let frame = |message| {
                ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                    .map_err(io::Error::other)
            };
            let mut response =
                encode_server_line(&frame(ServerMessage::TranscriptSnapshotStart {
                    session_id,
                    cursor: CanonicalU64::new(0),
                    runner: None,
                })?)
                .map_err(io::Error::other)?;
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                    turn_id,
                    acceptance_position: CanonicalU64::new(1),
                    model_settings: None,
                    state: TurnState::Queued {
                        accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                        content: UserInputContent::text(String::from("stream the reply")),
                    },
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptModelCallsEnd {
                    model_call_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptSnapshotEnd {
                    session_id,
                    cursor: CanonicalU64::new(0),
                    turn_count: CanonicalU64::new(1),
                    entry_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ProviderTextDelta {
                    session_id,
                    turn_id,
                    model_call_id,
                    part_index: CanonicalU64::new(0),
                    content: ContentFragment::try_new(String::from("already [redacted]"))
                        .map_err(io::Error::other)?,
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::SessionEvent {
                    cursor: CanonicalU64::new(1),
                    session_id,
                    event: SessionEvent::TurnCompleted {
                        turn_id,
                        model_call_id,
                        completion_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
                        terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(6)),
                    },
                })?)
                .map_err(io::Error::other)?,
            );
            writer.write_all(&response).await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let terminal = await_turn_terminal(&mut client, session_id, turn_id).await?;

        assert_eq!(terminal, TurnTerminal::Completed);
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn send_wait_rejects_streamed_text_for_another_session() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let other_session_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::FollowSession { session_id }
            );
            let frame = |message| {
                ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                    .map_err(io::Error::other)
            };
            let mut response =
                encode_server_line(&frame(ServerMessage::TranscriptSnapshotStart {
                    session_id,
                    cursor: CanonicalU64::new(0),
                    runner: None,
                })?)
                .map_err(io::Error::other)?;
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                    turn_id,
                    acceptance_position: CanonicalU64::new(1),
                    model_settings: None,
                    state: TurnState::Queued {
                        accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
                        content: UserInputContent::text(String::from("stream the reply")),
                    },
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptModelCallsEnd {
                    model_call_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptSnapshotEnd {
                    session_id,
                    cursor: CanonicalU64::new(0),
                    turn_count: CanonicalU64::new(1),
                    entry_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ProviderTextDelta {
                    session_id: other_session_id,
                    turn_id,
                    model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
                    part_index: CanonicalU64::new(0),
                    content: ContentFragment::try_new(String::from("cross-wired text"))
                        .map_err(io::Error::other)?,
                })?)
                .map_err(io::Error::other)?,
            );
            writer.write_all(&response).await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let result = await_turn_terminal(&mut client, session_id, turn_id).await;

        assert!(matches!(
            result,
            Err(ClientError::Protocol(
                "follow returned an unexpected response"
            ))
        ));
        server.await??;
        Ok(())
    }

    #[test]
    fn send_classifies_cancelled_event_for_its_turn() {
        let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let event = SessionEvent::TurnCancelled {
            turn_id: selected_turn,
            cancellation_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
        };

        assert_eq!(
            terminal_event_state(&event, selected_turn),
            Some(TurnTerminal::Cancelled)
        );
    }

    #[test]
    fn send_classifies_reconciliation_required_event_for_its_turn() {
        let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let event = SessionEvent::TurnReconciliationRequired {
            turn_id: selected_turn,
            model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
        };

        assert_eq!(
            terminal_event_state(&event, selected_turn),
            Some(TurnTerminal::ReconciliationRequired)
        );
        assert!(session_recovery_transition(&event));
    }

    #[test]
    fn cli_socket_path_must_be_absolute() {
        assert!(matches!(
            socket_path(Some(PathBuf::from("relative.sock")), None),
            Err(ClientError::Input(
                "the local process socket path must be absolute"
            ))
        ));
    }

    #[test]
    fn environment_socket_path_must_be_absolute() {
        assert!(matches!(
            socket_path(None, Some(OsString::from("relative.sock"))),
            Err(ClientError::Input(
                "the local process socket path must be absolute"
            ))
        ));
    }

    #[test]
    fn selected_turn_ambiguous_model_call_requests_recovery_reread() {
        let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let event = SessionEvent::ModelCallTransition {
            turn_id: selected_turn,
            model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            state: ModelCallState::Terminal {
                disposition: ModelCallDisposition::Ambiguous,
            },
        };

        assert!(model_call_recovery_transition(&event, selected_turn));
        assert!(session_recovery_transition(&event));
        assert!(!model_call_recovery_transition(
            &event,
            CanonicalUuid::from_uuid(Uuid::from_u128(3))
        ));
    }

    #[test]
    fn selected_turn_tool_recovery_requests_authoritative_reread() {
        let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let event = SessionEvent::ToolBatchTransition {
            turn_id: selected_turn,
            model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            state: ToolBatchState::RecoveryRequired {
                tool_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            },
        };

        assert!(tool_recovery_transition(&event, selected_turn));
        assert!(session_recovery_transition(&event));
        assert!(!tool_recovery_transition(
            &event,
            CanonicalUuid::from_uuid(Uuid::from_u128(4))
        ));
    }

    #[test]
    fn runner_loss_requests_authoritative_turn_reread() {
        let event = SessionEvent::RunnerStateTransition {
            runner_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            placement_revision: RunnerPlacementRevision::try_new(2)
                .expect("the fixture placement revision is positive"),
            sandbox_profile: RunnerSandboxProfile::WorkspaceRestricted,
            working_directory: None,
            state: RunnerStateTransitionState::RunnerLost,
        };

        assert!(selected_turn_recovery_transition(
            &event,
            CanonicalUuid::from_uuid(Uuid::from_u128(3))
        ));
    }

    #[test]
    fn queued_send_stops_on_current_pre_pin_runner_loss_from_snapshot() {
        let projection = runner_projection(1, RunnerProjectionState::RunnerLostBeforePin, None);
        let result = queued_turn_runner_recovery(Some(&projection));

        assert!(matches!(result, Err(ClientError::RunnerRecoveryRequired)));
    }

    #[test]
    fn selected_send_stops_when_its_authoritative_turn_is_still_queued_on_runner_loss() {
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let projection = runner_projection(1, RunnerProjectionState::RunnerLost, None);
        let mut snapshot = TranscriptSnapshot::from_messages_with_runner(
            1,
            Some(projection),
            [ServerMessage::TranscriptTurn {
                turn_id,
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Queued {
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                    content: UserInputContent::text(String::from("queued selected input")),
                },
            }],
        )
        .expect("the queued selected-turn fixture spools");
        let result = queued_turn_recovery(&mut snapshot, turn_id);

        assert!(matches!(result, Err(ClientError::RunnerRecoveryRequired)));
    }

    #[test]
    fn queued_send_ignores_pre_pin_runner_loss_superseded_in_snapshot() {
        let projection = runner_projection(
            2,
            RunnerProjectionState::Pinned,
            Some(RunnerConnectionHealth::Connected),
        );
        let result = queued_turn_runner_recovery(Some(&projection));

        assert!(result.is_ok());
    }

    #[test]
    fn queued_send_stops_on_current_pinned_runner_loss() {
        let projection = runner_projection(1, RunnerProjectionState::RunnerLost, None);
        let result = queued_turn_runner_recovery(Some(&projection));

        assert!(matches!(result, Err(ClientError::RunnerRecoveryRequired)));
    }

    /// INV-044: orderly terminal runner shutdown blocks queued activation
    /// before placement reconciliation catches up.
    #[test]
    fn queued_send_stops_on_current_runner_shutdown() {
        let projection = runner_projection(
            1,
            RunnerProjectionState::Pinned,
            Some(RunnerConnectionHealth::Shutdown),
        );
        let result = queued_turn_runner_recovery(Some(&projection));

        assert!(matches!(result, Err(ClientError::RunnerRecoveryRequired)));
    }

    /// INV-044: terminal runner connection loss blocks queued activation
    /// before placement reconciliation catches up.
    #[test]
    fn queued_send_stops_on_current_runner_connection_loss() {
        let projection = runner_projection(
            1,
            RunnerProjectionState::Pinned,
            Some(RunnerConnectionHealth::Lost),
        );
        let result = queued_turn_runner_recovery(Some(&projection));

        assert!(matches!(result, Err(ClientError::RunnerRecoveryRequired)));
    }

    #[test]
    fn tool_batch_events_select_their_exact_material() {
        let turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let call = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let frontier = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        assert_eq!(
            terminal_snapshot_selection(
                &SessionEvent::ToolBatchTransition {
                    turn_id: turn,
                    model_call_id: call,
                    state: ToolBatchState::Proposed {
                        frontier_id: frontier,
                    },
                },
                followed_session()
            ),
            Some(SnapshotSelection::ToolBatchProposed {
                turn_id: turn,
                model_call_id: call,
            })
        );
        assert_eq!(
            terminal_snapshot_selection(
                &SessionEvent::ToolBatchTransition {
                    turn_id: turn,
                    model_call_id: call,
                    state: ToolBatchState::ResultsProjected {
                        frontier_id: frontier,
                    },
                },
                followed_session()
            ),
            Some(SnapshotSelection::ToolBatchResults {
                turn_id: turn,
                model_call_id: call,
            })
        );
    }

    #[test]
    fn refused_terminal_event_requests_no_side_reread() {
        assert!(
            terminal_snapshot_selection(
                &SessionEvent::TurnRefused {
                    turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
                    model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
                    terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                },
                followed_session()
            )
            .is_none()
        );
    }

    #[test]
    fn cancellation_event_selects_its_exact_marker_for_reread() {
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));

        assert!(matches!(
            terminal_snapshot_selection(&SessionEvent::TurnCancelled {
                turn_id,
                cancellation_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
                terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            }, followed_session()),
            Some(SnapshotSelection::Cancelled {
                turn_id: selected,
                terminal_entry_id,
            }) if selected == turn_id && terminal_entry_id == CanonicalUuid::from_uuid(Uuid::from_u128(2))
        ));
    }

    #[test]
    fn reconciliation_event_selects_no_semantic_material_for_reread() {
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));

        assert!(
            terminal_snapshot_selection(
                &SessionEvent::TurnReconciliationRequired {
                    turn_id,
                    model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
                    terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                },
                followed_session()
            )
            .is_none()
        );
    }

    #[test]
    fn tool_reconciliation_event_selects_terminal_tool_results() {
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let tool_attempt_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let terminal_frontier_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));

        assert_eq!(
            terminal_snapshot_selection(
                &SessionEvent::TurnToolReconciliationRequired {
                    turn_id,
                    tool_attempt_id,
                    terminal_frontier_id,
                },
                followed_session()
            ),
            Some(SnapshotSelection::ToolReconciliation {
                turn_id,
                tool_attempt_id,
                terminal_frontier_id,
            })
        );
    }

    #[test]
    fn child_addressed_cascade_disposition_requests_an_authoritative_reread() {
        let event = SessionEvent::ChildLifecycleDisposition {
            spawning_request_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            child_session_id: followed_session(),
            outcome: DelegationOutcome::Stopped,
            reason: DelegationReason::ParentStopped,
            provenance: DelegationProvenance::ParentGoalCommand {
                parent_session_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
                goal_generation: CanonicalU64::new(1),
                command_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            },
        };

        assert!(child_lifecycle_terminalization(&event, followed_session()));
        assert_eq!(
            terminal_snapshot_selection(&event, followed_session()),
            Some(SnapshotSelection::All)
        );
    }

    #[test]
    fn parent_addressed_cascade_disposition_requests_no_reread() {
        let event = SessionEvent::ChildLifecycleDisposition {
            spawning_request_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            child_session_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
            outcome: DelegationOutcome::Stopped,
            reason: DelegationReason::ParentStopped,
            provenance: DelegationProvenance::ParentGoalCommand {
                parent_session_id: followed_session(),
                goal_generation: CanonicalU64::new(1),
                command_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            },
        };

        assert!(!child_lifecycle_terminalization(&event, followed_session()));
        assert_eq!(
            terminal_snapshot_selection(&event, followed_session()),
            None
        );
    }

    #[tokio::test]
    async fn invalid_send_input_fails_before_a_missing_socket_is_opened() {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let exit = run(
            [
                OsString::from("--socket"),
                OsString::from("/does/not/exist"),
                OsString::from("send"),
                OsString::from("00000000-0000-0000-0000-000000000001"),
            ],
            None,
            &mut input,
            &mut output,
            &mut error,
        )
        .await;
        assert_eq!(exit, ExitCode::FAILURE);
        assert!(String::from_utf8_lossy(&error).contains("must not be empty"));
    }

    #[tokio::test]
    async fn missing_import_source_fails_before_a_missing_socket_is_opened() {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let exit = run(
            [
                OsString::from("--socket"),
                OsString::from("/does/not/exist/hub.sock"),
                OsString::from("import"),
                OsString::from("--format"),
                OsString::from("claude-code"),
                OsString::from("/does/not/exist/session.jsonl"),
            ],
            None,
            &mut input,
            &mut output,
            &mut error,
        )
        .await;

        assert_eq!(exit, ExitCode::FAILURE);
        assert!(output.is_empty());
        assert!(
            String::from_utf8_lossy(&error)
                .contains("conversation import source file could not be read")
        );
    }

    #[tokio::test]
    async fn import_reader_rejects_source_beyond_its_single_frame_bound()
    -> Result<(), Box<dyn Error>> {
        let source_file = tempfile::tempfile()?;
        let source_size = MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES + 1;
        source_file.set_len(u64::try_from(source_size)?)?;
        let source_file = tokio::fs::File::from_std(source_file);

        let error = read_import_file(source_file).await.unwrap_err();

        assert!(matches!(error, ClientError::SourceExceedsFrame));
        Ok(())
    }

    #[test]
    fn import_transport_selects_single_shot_only_when_the_exact_frame_fits()
    -> Result<(), Box<dyn Error>> {
        let small_source = b"{}\n";
        let oversized_source = vec![b'x'; MAX_FRAME_BYTES];
        let request_id = RequestId::try_new(1)?;

        assert!(source_fits_single_shot_import(
            ConversationImportFormat::CodexRolloutJsonlV1,
            small_source,
            request_id,
        )?);
        assert!(!source_fits_single_shot_import(
            ConversationImportFormat::CodexRolloutJsonlV1,
            &oversized_source,
            request_id,
        )?);
        Ok(())
    }

    #[test]
    fn chunked_import_reads_at_most_one_byte_past_the_declared_size() {
        let declared_size_bytes = CanonicalU64::new(1);

        assert_eq!(
            conversation_import_chunk_read_limit(declared_size_bytes, 0),
            2
        );
        assert_eq!(
            conversation_import_chunk_read_limit(declared_size_bytes, 1),
            1
        );
        assert_eq!(
            conversation_import_chunk_read_limit(declared_size_bytes, 2),
            0
        );
        assert_eq!(
            conversation_import_chunk_read_limit(CanonicalU64::new(u64::MAX), u64::MAX),
            0
        );
    }

    #[track_caller]
    fn assert_append_request(frame: &ClientFrame, expected_chunk: &[u8]) {
        assert_eq!(
            frame.request(),
            &ClientRequest::AppendConversationImport {
                chunk: ConversationImportSource::new(expected_chunk.to_vec()),
            }
        );
    }

    #[tokio::test]
    async fn large_file_import_streams_exact_bounded_assembly_and_commits()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let source = vec![b'x'; MAX_FRAME_BYTES / 4 * 3 + 1];
        let source_path = directory.path().join("source.jsonl");
        fs::write(&source_path, &source)?;
        let source_file = tokio::fs::File::open(source_path).await?;
        let expected_source = source;
        let imported_conversation_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();

            reader.read_until(b'\n', &mut line).await?;
            assert!(line.len() <= MAX_FRAME_BYTES);
            let begin = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                begin.request(),
                &ClientRequest::BeginConversationImport {
                    format: ConversationImportFormat::CodexRolloutJsonlV1,
                    declared_size_bytes: CanonicalU64::new(
                        u64::try_from(expected_source.len()).map_err(io::Error::other)?,
                    ),
                }
            );
            let begun = ServerFrame::try_new_for_version(
                begin.version(),
                begin.request_id(),
                ServerMessage::ConversationImportBegun {
                    declared_size_bytes: CanonicalU64::new(
                        u64::try_from(expected_source.len()).map_err(io::Error::other)?,
                    ),
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&begun).map_err(io::Error::other)?)
                .await?;

            line.clear();
            reader.read_until(b'\n', &mut line).await?;
            assert!(line.len() <= MAX_FRAME_BYTES);
            let first_append = decode_client_line(&line).map_err(io::Error::other)?;
            assert_append_request(
                &first_append,
                &expected_source[..MAX_CONVERSATION_IMPORT_CHUNK_BYTES],
            );
            let first_appended = ServerFrame::try_new_for_version(
                first_append.version(),
                first_append.request_id(),
                ServerMessage::ConversationImportAppended {
                    assembled_size_bytes: CanonicalU64::new(
                        u64::try_from(MAX_CONVERSATION_IMPORT_CHUNK_BYTES)
                            .map_err(io::Error::other)?,
                    ),
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&first_appended).map_err(io::Error::other)?)
                .await?;

            line.clear();
            reader.read_until(b'\n', &mut line).await?;
            assert!(line.len() <= MAX_FRAME_BYTES);
            let second_append = decode_client_line(&line).map_err(io::Error::other)?;
            assert_append_request(
                &second_append,
                &expected_source[MAX_CONVERSATION_IMPORT_CHUNK_BYTES..],
            );
            let second_appended = ServerFrame::try_new_for_version(
                second_append.version(),
                second_append.request_id(),
                ServerMessage::ConversationImportAppended {
                    assembled_size_bytes: CanonicalU64::new(
                        u64::try_from(expected_source.len()).map_err(io::Error::other)?,
                    ),
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&second_appended).map_err(io::Error::other)?)
                .await?;

            line.clear();
            reader.read_until(b'\n', &mut line).await?;
            assert!(line.len() <= MAX_FRAME_BYTES);
            let commit = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                commit.request(),
                &ClientRequest::CommitConversationImport {}
            );
            let inserted = ServerFrame::try_new_for_version(
                commit.version(),
                commit.request_id(),
                ServerMessage::ConversationImportInserted {
                    imported_conversation_id,
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&inserted).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });
        let mut client = ProcessClient::new(socket);

        let outcome = import_conversation_file(
            &mut client,
            ConversationImportFormat::CodexRolloutJsonlV1,
            source_file,
        )
        .await?;

        assert_eq!(
            outcome,
            ConversationImportOutcome::Inserted(imported_conversation_id)
        );
        server.await??;
        Ok(())
    }

    /// S28 / INV-038: a directory replaced after enumeration cannot redirect
    /// a queued candidate read through a symbolic link.
    #[tokio::test]
    async fn s28_inv038_scan_refuses_directory_symlink_replacement() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let queued_directory = root.path().join("queued");
        let retained_directory = root.path().join("retained");
        fs::create_dir(&queued_directory)?;
        fs::write(queued_directory.join("conversation.jsonl"), b"inside")?;
        fs::write(outside.path().join("conversation.jsonl"), b"outside")?;
        let scan = collect_import_paths(root.path())?;
        let candidate = scan
            .paths
            .first()
            .ok_or("fixture must select one candidate")?;
        let relative = candidate.relative.clone();
        fs::rename(&queued_directory, retained_directory)?;
        symlink(outside.path(), &queued_directory)?;

        let opened = open_scanned_import_source(&scan.root, &relative);

        assert!(matches!(opened, Err(ClientError::SourceFile(_))));
        Ok(())
    }

    /// S34: an unreadable `--system-prompt-file` names the prompt file, not
    /// the unrelated conversation-import source, in both its typed variant and
    /// its rendered diagnostic.
    #[tokio::test]
    async fn s34_missing_system_prompt_file_reports_a_prompt_file_failure()
    -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let absent = root.path().join("prompt.txt");

        let failure = read_system_prompt_file(&absent)
            .await
            .expect_err("an absent prompt file must fail");

        assert!(matches!(failure, ClientError::SystemPromptFile(_)));
        assert_eq!(
            failure.to_string(),
            "the system prompt file could not be read"
        );
        Ok(())
    }

    /// S28 / INV-038: a regular candidate replaced after enumeration by a
    /// FIFO is rejected without waiting for a writer.
    #[cfg(not(target_vendor = "apple"))]
    #[tokio::test]
    async fn s28_inv038_scan_refuses_fifo_replacement_without_blocking()
    -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let candidate_path = root.path().join("conversation.jsonl");
        fs::write(&candidate_path, b"inside")?;
        let scan = collect_import_paths(root.path())?;
        let candidate = scan
            .paths
            .first()
            .ok_or("fixture must select one candidate")?;
        let relative = candidate.relative.clone();
        fs::remove_file(&candidate_path)?;
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &candidate_path,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;

        let opened = open_scanned_import_source(&scan.root, &relative);

        assert!(matches!(opened, Err(ClientError::SourceFile(_))));
        Ok(())
    }

    #[test]
    fn blob_upload_missing_source_retains_path_and_os_failure() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let absent = root.path().join("absent.bin");

        let Err(failure) = open_blob_source(&absent) else {
            panic!("an absent blob source must fail")
        };
        let ClientError::BlobSourceFile { path, source } = failure else {
            panic!("an absent blob source must retain its path and OS failure")
        };

        assert_eq!(path, absent);
        assert_eq!(source.kind(), io::ErrorKind::NotFound);
        Ok(())
    }

    #[test]
    fn blob_upload_source_failure_display_names_path_and_cause() {
        let path = PathBuf::from("fixture.bin");
        let failure = ClientError::blob_source_file(
            &path,
            io::Error::new(io::ErrorKind::PermissionDenied, "fixture denied"),
        );

        expect_test::expect![[r#"
            the blob upload source file 'fixture.bin' could not be read: fixture denied"#]]
        .assert_eq(&failure.to_string());
    }

    /// INV-060: opening a FIFO as an upload source is nonblocking and rejects
    /// the descriptor before hashing.
    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn inv060_blob_upload_rejects_fifo_source_without_blocking() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("blob.fifo");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &source,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;

        let opened = open_blob_source(&source);

        let Err(ClientError::BlobSourceFile { path, .. }) = opened else {
            panic!("a FIFO source must retain its rejected path")
        };
        assert_eq!(path, source);
        Ok(())
    }

    /// INV-060: a regular seekable source is nonempty when its hash pass reads
    /// bytes even if its advisory metadata length is zero.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn inv060_blob_upload_counts_bytes_from_the_hash_pass() -> Result<(), Box<dyn Error>> {
        let path = Path::new("/proc/version");
        let mut source = open_blob_source(path)?;
        let metadata_length = source.file.metadata().await?.len();

        let (_digest, observed_length) = hash_blob_source(&mut source.file, path).await?;

        assert_eq!(metadata_length, 0);
        assert!(observed_length.value() > 0);
        Ok(())
    }

    async fn reply_to_ambiguous_blob_upload(
        listener: &UnixListener,
        bytes: &[u8],
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
        code: ErrorCode,
    ) -> Result<(), io::Error> {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();

        reader.read_until(b'\n', &mut line).await?;
        let begin = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            begin.request(),
            &ClientRequest::BeginBlobUpload {
                expected_digest: digest,
                expected_length_bytes: byte_length,
            }
        );
        let begun = ServerFrame::try_new_for_version(
            begin.version(),
            begin.request_id(),
            ServerMessage::BlobUploadBegun {
                expected_digest: digest,
                expected_length_bytes: byte_length,
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&begun).map_err(io::Error::other)?)
            .await?;

        line.clear();
        reader.read_until(b'\n', &mut line).await?;
        let append = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            append.request(),
            &ClientRequest::AppendBlobUpload {
                chunk: BlobChunk::new(bytes.to_vec()),
            }
        );
        let appended = ServerFrame::try_new_for_version(
            append.version(),
            append.request_id(),
            ServerMessage::BlobUploadAppended {
                assembled_length_bytes: byte_length,
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&appended).map_err(io::Error::other)?)
            .await?;

        line.clear();
        reader.read_until(b'\n', &mut line).await?;
        let commit = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(commit.request(), &ClientRequest::CommitBlobUpload {});
        let ambiguous = ServerFrame::try_new_for_version(
            commit.version(),
            commit.request_id(),
            ServerMessage::Error {
                code,
                message: String::from("blob publication outcome is ambiguous"),
                detail: ErrorDetail::none(),
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&ambiguous).map_err(io::Error::other)?)
            .await
    }

    async fn reply_to_restarted_blob_upload(
        listener: &UnixListener,
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
    ) -> Result<(), io::Error> {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let begin = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            begin.request(),
            &ClientRequest::BeginBlobUpload {
                expected_digest: digest,
                expected_length_bytes: byte_length,
            }
        );
        let present = ServerFrame::try_new_for_version(
            begin.version(),
            begin.request_id(),
            ServerMessage::BlobUploadAlreadyPresent {
                digest,
                byte_length,
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&present).map_err(io::Error::other)?)
            .await?;
        line.clear();
        assert_eq!(reader.read_until(b'\n', &mut line).await?, 0);
        Ok(())
    }

    async fn assert_ambiguous_blob_upload_restarts(code: ErrorCode) -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let source_path = directory.path().join("blob.bin");
        let bytes = b"ambiguously published bytes";
        fs::write(&source_path, bytes)?;
        let digest = CanonicalBlobDigest::from_digest(signalbox_domain::BlobDigest::digest(bytes));
        let byte_length = CanonicalU64::new(u64::try_from(bytes.len())?);
        let listener = UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            reply_to_ambiguous_blob_upload(&listener, bytes, digest, byte_length, code).await?;
            reply_to_restarted_blob_upload(&listener, digest, byte_length).await
        });
        let mut client = ProcessClient::new(socket);
        let source = open_blob_source(&source_path)?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);

        upload_blob(&mut client, &mut output, source).await?;

        expect_test::expect![[r#"
            already_present digest=sha256:0e6161e59e9ca2ce9118def8a479a2a9696dc6352eca107b5a580069df4db7e2 byte_length=27
        "#]].assert_eq(&String::from_utf8(stdout)?);
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    /// INV-060: an ambiguous catalog commit restarts the complete high-level
    /// upload instead of retrying commit alone.
    #[tokio::test]
    async fn inv060_blob_upload_restarts_after_ambiguous_catalog_commit()
    -> Result<(), Box<dyn Error>> {
        assert_ambiguous_blob_upload_restarts(ErrorCode::CommitAmbiguous).await
    }

    /// INV-060: an ambiguous remote publication restarts the complete
    /// high-level upload instead of retrying commit alone.
    #[tokio::test]
    async fn inv060_blob_upload_restarts_after_ambiguous_publication() -> Result<(), Box<dyn Error>>
    {
        assert_ambiguous_blob_upload_restarts(ErrorCode::PublicationAmbiguous).await
    }

    /// INV-060: an already-present receipt succeeds only after re-reading the
    /// same descriptor and proving its identity is unchanged.
    #[tokio::test]
    async fn inv060_blob_upload_revalidates_source_before_deduplication()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let source_path = directory.path().join("blob.bin");
        let original = b"catalogued source bytes";
        let replacement = b"rewritten source bytes";
        fs::write(&source_path, original)?;
        let digest =
            CanonicalBlobDigest::from_digest(signalbox_domain::BlobDigest::digest(original));
        let byte_length = CanonicalU64::new(u64::try_from(original.len())?);
        let listener = UnixListener::bind(&socket)?;
        let rewritten_path = source_path.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let begin = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                begin.request(),
                &ClientRequest::BeginBlobUpload {
                    expected_digest: digest,
                    expected_length_bytes: byte_length,
                }
            );
            fs::write(rewritten_path, replacement)?;
            let present = ServerFrame::try_new_for_version(
                begin.version(),
                begin.request_id(),
                ServerMessage::BlobUploadAlreadyPresent {
                    digest,
                    byte_length,
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&present).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });
        let mut client = ProcessClient::new(socket);
        let source = open_blob_source(&source_path)?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);

        let failure = upload_blob(&mut client, &mut output, source)
            .await
            .expect_err("a rewritten source must not accept deduplication");

        assert!(matches!(
            failure,
            ClientError::Input("blob source changed after it was hashed")
        ));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    /// INV-060: the terminal client prehashes one descriptor, streams bounded
    /// chunks in order, validates every echo, and reports the committed identity.
    #[tokio::test]
    async fn inv060_blob_upload_streams_the_exact_lifecycle() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let source_path = directory.path().join("blob.bin");
        let first_chunk = vec![b'a'; MAX_BLOB_CHUNK_BYTES];
        let final_chunk = b"terminal blob tail";
        let mut bytes = first_chunk.clone();
        bytes.extend_from_slice(final_chunk);
        fs::write(&source_path, &bytes)?;
        let digest = CanonicalBlobDigest::from_digest(signalbox_domain::BlobDigest::digest(&bytes));
        let byte_length = CanonicalU64::new(u64::try_from(bytes.len())?);
        let first_length = CanonicalU64::new(u64::try_from(first_chunk.len())?);
        let listener = UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();

            reader.read_until(b'\n', &mut line).await?;
            let begin = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                begin.request(),
                &ClientRequest::BeginBlobUpload {
                    expected_digest: digest,
                    expected_length_bytes: byte_length,
                }
            );
            let begun = ServerFrame::try_new_for_version(
                begin.version(),
                begin.request_id(),
                ServerMessage::BlobUploadBegun {
                    expected_digest: digest,
                    expected_length_bytes: byte_length,
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&begun).map_err(io::Error::other)?)
                .await?;

            line.clear();
            reader.read_until(b'\n', &mut line).await?;
            let append = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                append.request(),
                &ClientRequest::AppendBlobUpload {
                    chunk: BlobChunk::new(first_chunk),
                }
            );
            let appended = ServerFrame::try_new_for_version(
                append.version(),
                append.request_id(),
                ServerMessage::BlobUploadAppended {
                    assembled_length_bytes: first_length,
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&appended).map_err(io::Error::other)?)
                .await?;

            line.clear();
            reader.read_until(b'\n', &mut line).await?;
            let append = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                append.request(),
                &ClientRequest::AppendBlobUpload {
                    chunk: BlobChunk::new(final_chunk.to_vec()),
                }
            );
            let appended = ServerFrame::try_new_for_version(
                append.version(),
                append.request_id(),
                ServerMessage::BlobUploadAppended {
                    assembled_length_bytes: byte_length,
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&appended).map_err(io::Error::other)?)
                .await?;

            line.clear();
            reader.read_until(b'\n', &mut line).await?;
            let commit = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(commit.request(), &ClientRequest::CommitBlobUpload {});
            let committed = ServerFrame::try_new_for_version(
                commit.version(),
                commit.request_id(),
                ServerMessage::BlobUploadCommitted {
                    digest,
                    byte_length,
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&committed).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });
        let mut client = ProcessClient::new(socket);
        let file = tokio::fs::File::open(&source_path).await?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);

        upload_blob(
            &mut client,
            &mut output,
            PreparedBlobSource {
                path: source_path,
                file,
            },
        )
        .await?;

        expect_test::expect![[r#"
            committed digest=sha256:dc8dba98d0eeeb8521413d99301f4b1efe3a2eab5e514460aabbd3e9b9d5684e byte_length=4194322
        "#]].assert_eq(&String::from_utf8(stdout)?);
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    /// INV-060: terminal metadata validates echoed identity and prints the
    /// bounded catalog facts returned by the daemon.
    #[tokio::test]
    async fn inv060_blob_metadata_preserves_exact_wire_facts() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
        let byte_length = CanonicalU64::new(9);
        let replica_count = CanonicalU64::new(1);
        let listener = UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let metadata = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                metadata.request(),
                &ClientRequest::ReadBlobMetadata { digest }
            );
            let metadata_response = ServerFrame::try_new_for_version(
                metadata.version(),
                metadata.request_id(),
                ServerMessage::BlobMetadata {
                    digest,
                    byte_length,
                    replica_count,
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&metadata_response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });
        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);

        read_blob_metadata(&mut client, &mut output, digest).await?;

        assert_eq!(
            String::from_utf8(stdout)?,
            format!(
                "digest={digest} byte_length={} replica_count={}\n",
                byte_length.value(),
                replica_count.value()
            )
        );
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    /// INV-060: a terminal range read validates echoed identity and offset and
    /// returns only the exact requested bytes for file delivery.
    #[tokio::test]
    async fn inv060_blob_read_returns_only_the_exact_range() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
        let offset_bytes = CanonicalU64::new(7);
        let bytes = vec![0, 255];
        let length_bytes = CanonicalU64::new(u64::try_from(bytes.len())?);
        let listener = UnixListener::bind(&socket)?;
        let expected_bytes = bytes.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let range = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                range.request(),
                &ClientRequest::ReadBlobChunk {
                    digest,
                    offset_bytes,
                    length_bytes,
                }
            );
            let response = ServerFrame::try_new_for_version(
                range.version(),
                range.request_id(),
                ServerMessage::BlobChunkRead {
                    digest,
                    offset_bytes,
                    bytes: BlobChunk::new(expected_bytes),
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });
        let mut client = ProcessClient::new(socket);

        let observed = read_blob_chunk(&mut client, digest, offset_bytes, length_bytes).await?;

        assert_eq!(observed, bytes);
        server.await??;
        Ok(())
    }

    /// INV-060: terminal range delivery creates one file containing exactly the
    /// bounded bytes returned by the daemon.
    #[tokio::test]
    async fn inv060_blob_output_file_contains_exact_bytes() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("range.bin");
        let bytes = b"exact range bytes";

        write_blob_output(&output, bytes).await?;

        assert_eq!(fs::read(&output)?, bytes);
        Ok(())
    }

    #[tokio::test]
    async fn blob_output_file_uses_private_mode() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("range.bin");

        write_blob_output(&output, b"range").await?;

        assert_eq!(fs::metadata(&output)?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }

    #[tokio::test]
    async fn blob_output_refuses_to_replace_an_existing_file() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("existing.bin");
        let existing = b"existing bytes";
        fs::write(&output, existing)?;

        let failure = write_blob_output(&output, b"replacement")
            .await
            .expect_err("blob range delivery must not replace an existing file");

        let ClientError::BlobOutputFile { path, source } = failure else {
            panic!("an output collision must retain its path and OS failure")
        };
        assert_eq!(path, output);
        assert_eq!(source.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path)?, existing);
        Ok(())
    }

    #[tokio::test]
    async fn search_rejects_a_page_that_exceeds_its_requested_bound() -> Result<(), Box<dyn Error>>
    {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::ListSessionMetadata {
                    required_tags: Vec::new(),
                    title_contains: None,
                    include_archived: false,
                    page_size: CanonicalU64::new(1),
                    after_session_id: None,
                }
            );
            let frame = |message| {
                ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                    .map_err(io::Error::other)
            };
            let summary = |seed| ServerMessage::SessionMetadataSummary {
                session_id: CanonicalUuid::from_uuid(Uuid::from_u128(seed)),
                defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(9)),
                },
                dangerous_tool_auto_approval: false,
                title: None,
                tags: Vec::new(),
                archived: false,
                last_writer: None,
            };
            let mut response = Vec::new();
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::SessionMetadataPageStart {})?)
                    .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(summary(1))?).map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(summary(2))?).map_err(io::Error::other)?,
            );
            writer.write_all(&response).await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let result = search(
            &mut client,
            &mut output,
            SessionMetadataPageRequest {
                required_tags: Vec::new(),
                title_contains: None,
                include_archived: false,
                page_size: CanonicalU64::new(1),
                after_session_id: None,
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(ClientError::Protocol(
                "session metadata page exceeded its requested bound"
            ))
        ));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    /// S28: the inspection read is the client's source of selectable
    /// positions, so a gap in the emitted sequence is rejected before any row
    /// can suggest a position the daemon did not emit.
    #[tokio::test]
    async fn imported_rejects_noncontiguous_positions_before_writing_rows()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let imported_conversation_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::ReadImportedConversation {
                    imported_conversation_id,
                }
            );
            let frame = |message| {
                ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                    .map_err(io::Error::other)
            };
            let entry = |position| ServerMessage::ImportedConversationEntry {
                position: CanonicalU64::new(position),
                imported_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::from(
                    100 - position,
                ))),
                source_speaker: ImportedSourceSpeaker::NotAttested {},
                content_kind: ImportedContentKind::SourceEvent,
                text_preview: None,
            };
            let mut response = Vec::new();
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ImportedConversationStart {
                    imported_conversation_id,
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(entry(1))?).map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(entry(3))?).map_err(io::Error::other)?,
            );
            writer.write_all(&response).await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let result = imported(&mut client, &mut output, imported_conversation_id).await;

        assert!(matches!(
            result,
            Err(ClientError::Protocol(
                "imported entry positions were not the contiguous sequence from one"
            ))
        ));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    /// S28: an imported conversation's normalized entry sequence is nonempty,
    /// so an empty inventory contradicts the record the daemon claims to be
    /// reading. The shared reader fails closed on it rather than printing a
    /// conversation with no selectable position.
    #[tokio::test]
    async fn imported_rejects_an_empty_entry_inventory() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let imported_conversation_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::ReadImportedConversation {
                    imported_conversation_id,
                }
            );
            let frame = |message| {
                ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                    .map_err(io::Error::other)
            };
            let mut response = Vec::new();
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ImportedConversationStart {
                    imported_conversation_id,
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ImportedConversationEnd {
                    imported_conversation_id,
                    entry_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            writer.write_all(&response).await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let result = imported(&mut client, &mut output, imported_conversation_id).await;

        assert!(matches!(
            result,
            Err(ClientError::Protocol(
                "imported conversation reported no entries"
            ))
        ));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    /// S28: `latest` resolves against the imported conversation's own declared
    /// entry count and reaches the wire as that concrete ordinal, so the
    /// durable command an exact replay reconstructs is unchanged.
    #[tokio::test]
    async fn continue_resolves_latest_to_a_concrete_wire_position() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let imported_conversation_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let command_id = CommandId::try_from_uuid(Uuid::from_u128(2))?;
        let selection_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::ReadImportedConversation {
                    imported_conversation_id,
                }
            );
            let frame = |message| {
                ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                    .map_err(io::Error::other)
            };
            let entry = |position| ServerMessage::ImportedConversationEntry {
                position: CanonicalU64::new(position),
                imported_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::from(
                    100 - position,
                ))),
                source_speaker: ImportedSourceSpeaker::NotAttested {},
                content_kind: ImportedContentKind::SourceEvent,
                text_preview: None,
            };
            let mut response = Vec::new();
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ImportedConversationStart {
                    imported_conversation_id,
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(entry(1))?).map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(entry(2))?).map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ImportedConversationEnd {
                    imported_conversation_id,
                    entry_count: CanonicalU64::new(2),
                })?)
                .map_err(io::Error::other)?,
            );
            writer.write_all(&response).await?;

            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::CreateSessionFromImportedFrontier {
                    command_id,
                    imported_conversation_id,
                    through_position: CanonicalU64::new(2),
                    relationship: ImportedSessionRelationship::Resume,
                    initial_model_selection: ModelSelection::Direct { selection_id },
                    model_settings: ModelSettingsOverlay::inherit_all(),
                }
            );
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::SessionCreated {
                    session_id,
                    model_settings: provider_default_model_settings(),
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        continue_imported(
            &mut client,
            &mut output,
            imported_conversation_id,
            ThroughPositionArgument::Latest,
            ImportedSessionRelationship::Resume,
            ModelSelection::Direct { selection_id },
            Some(command_id),
        )
        .await?;

        assert_eq!(String::from_utf8(stdout)?, format!("{session_id}\n"));
        assert_eq!(String::from_utf8(stderr)?, "through_position=2\n");
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn conversations_rejects_a_page_that_exceeds_its_requested_bound()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::ListConversations {
                    title_contains: None,
                    origin: ConversationOriginFilter::All,
                    include_archived: false,
                    page_size: CanonicalU64::new(1),
                    after: None,
                }
            );
            let frame = |message| {
                ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                    .map_err(io::Error::other)
            };
            let summary = |seed| ServerMessage::ConversationSummary {
                conversation: ConversationSummary::NativeSession {
                    session_id: CanonicalUuid::from_uuid(Uuid::from_u128(seed)),
                    title: None,
                    archived: false,
                    defaults_version: CanonicalU64::new(1),
                },
            };
            let mut response = Vec::new();
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ConversationPageStart {})?)
                    .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(summary(1))?).map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(summary(2))?).map_err(io::Error::other)?,
            );
            writer.write_all(&response).await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let result = conversations(
            &mut client,
            &mut output,
            ConversationsPageRequest {
                title_contains: None,
                origin: ConversationOriginFilter::All,
                include_archived: false,
                page_size: CanonicalU64::new(1),
                after: None,
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(ClientError::Protocol(
                "conversation page exceeded its requested bound"
            ))
        ));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn conversations_rejects_summaries_out_of_unified_cursor_order()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            let frame = |message| {
                ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                    .map_err(io::Error::other)
            };
            let mut response = Vec::new();
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ConversationPageStart {})?)
                    .map_err(io::Error::other)?,
            );
            // The imported row shares identity value 1 with the native row
            // that follows it, so the pair inverts the native-before-imported
            // tiebreak of the unified order.
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ConversationSummary {
                    conversation: ConversationSummary::ImportedConversation {
                        imported_conversation_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
                        title: None,
                        entry_count: CanonicalU64::new(1),
                        source_format:
                            signalbox_process_protocol::ImportedConversationSourceFormat::CodexRolloutJsonlV1,
                    },
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ConversationSummary {
                    conversation: ConversationSummary::NativeSession {
                        session_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
                        title: None,
                        archived: false,
                        defaults_version: CanonicalU64::new(1),
                    },
                })?)
                .map_err(io::Error::other)?,
            );
            writer.write_all(&response).await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let result = conversations(
            &mut client,
            &mut output,
            ConversationsPageRequest {
                title_contains: None,
                origin: ConversationOriginFilter::All,
                include_archived: false,
                page_size: CanonicalU64::new(50),
                after: None,
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(ClientError::Protocol(
                "conversation summaries were not strictly ordered"
            ))
        ));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn review_records_an_empty_complete_finding_inventory() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let findings_file = directory.path().join("findings.json");
        fs::write(&findings_file, br#"{"findings":[]}"#)?;
        let listener = UnixListener::bind(&socket)?;
        let command_id = CommandId::try_from_uuid(Uuid::from_u128(1))?;
        let run_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let pass_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let output_frontier_id = CanonicalUuid::from_uuid(Uuid::from_u128(5));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::RecordReviewFindings {
                    command_id,
                    run_id,
                    pass_id,
                    turn_id,
                    output_frontier_id,
                    findings: Vec::new(),
                }
            );
            let frame = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::ReviewFindingsRecorded {
                    run_id,
                    pass_id,
                    finding_count: CanonicalU64::new(0),
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&frame).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        review(
            &mut client,
            &mut output,
            ReviewCommand::RecordFindings {
                command_id: Some(command_id),
                run_id,
                pass_id,
                turn_id,
                output_frontier_id,
                findings_file,
            },
            Some(ClientDeploymentLimits::unbounded()),
        )
        .await?;

        let expected_stdout = format!("run={run_id} pass={pass_id} findings=0 recorded\n");
        assert_eq!(String::from_utf8(stdout)?, expected_stdout);
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn review_reserves_an_external_publication_link() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let command_id = CommandId::try_from_uuid(Uuid::from_u128(1))?;
        let finding_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let external_link_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::ReserveReviewExternalLink {
                    command_id,
                    external_link_id,
                    finding_id,
                    provider: String::from("example-host"),
                    object_kind: ReviewExternalObjectKind::ReviewComment,
                }
            );
            let frame = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::ReviewExternalLinkReserved { external_link_id },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&frame).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        review(
            &mut client,
            &mut output,
            ReviewCommand::ReserveExternalLink {
                command_id: Some(command_id),
                external_link_id,
                finding_id,
                provider: String::from("example-host"),
                object_kind: ReviewExternalObjectKind::ReviewComment,
            },
            None,
        )
        .await?;

        assert_eq!(
            String::from_utf8(stdout)?,
            format!("external_link={external_link_id} reserved\n")
        );
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn review_attaches_an_external_publication_link() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let command_id = CommandId::try_from_uuid(Uuid::from_u128(1))?;
        let external_link_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let run_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        let pass_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(5));
        let output_frontier_id = CanonicalUuid::from_uuid(Uuid::from_u128(6));
        let event_ordinal = CanonicalU64::new(1);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::AttachReviewExternalLink {
                    command_id,
                    external_link_id,
                    run_id,
                    pass_id,
                    turn_id,
                    output_frontier_id,
                    external_object: String::from("provider-object-7"),
                    event_ordinal,
                }
            );
            let frame = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::ReviewExternalLinkAttached {
                    external_link_id,
                    external_object: String::from("provider-object-7"),
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&frame).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        review(
            &mut client,
            &mut output,
            ReviewCommand::AttachExternalLink {
                command_id: Some(command_id),
                external_link_id,
                run_id,
                pass_id,
                turn_id,
                output_frontier_id,
                external_object: String::from("provider-object-7"),
                event_ordinal,
            },
            None,
        )
        .await?;

        assert_eq!(
            String::from_utf8(stdout)?,
            format!("external_link={external_link_id} attached\n")
        );
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn review_list_rejects_terminal_count_before_writing_items() -> Result<(), Box<dyn Error>>
    {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let run_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let finding_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::ListReviewFindings { run_id }
            );
            let frame = |message| {
                ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                    .map_err(io::Error::other)
            };
            let finding = ReviewFindingSnapshot {
                target_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                run_id,
                producing_pass_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
                finding: ReviewFindingInput {
                    finding_id,
                    file_path: String::from("src/review.rs"),
                    line_start: Some(CanonicalU64::new(11)),
                    line_end: Some(CanonicalU64::new(14)),
                    diff_side: None,
                    title: String::from("Retain the exact edge"),
                    body: String::from("The terminal count must authenticate the list."),
                    severity: ReviewSeverity::High,
                    is_real_confidence: CanonicalU64::new(9_000),
                    severity_label_confidence: CanonicalU64::new(8_500),
                    category: String::from("correctness"),
                    recommended_fix: None,
                },
                status: ReviewFindingStatus::Open,
                event_count: CanonicalU64::new(0),
            };
            let mut response = Vec::new();
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ReviewFindingsStart { run_id })?)
                    .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ReviewFindingItem { finding })?)
                    .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::ReviewFindingsEnd {
                    finding_count: CanonicalU64::new(2),
                })?)
                .map_err(io::Error::other)?,
            );
            writer.write_all(&response).await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let error = review(
            &mut client,
            &mut output,
            ReviewCommand::ListFindings { run_id },
            Some(ClientDeploymentLimits::unbounded()),
        )
        .await
        .expect_err("the mismatched terminal count must reject the list");

        assert_eq!(
            error.to_string(),
            "the server violated the process protocol: review finding list sequence or count was invalid"
        );
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn review_list_rejects_an_over_bound_inventory_before_writing_items()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let run_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::ListReviewFindings { run_id }
            );
            let response =
                over_bound_review_findings_response(&request, run_id, REVIEW_FINDING_LIMIT_FIXTURE)
                    .map_err(io::Error::other)?;
            writer.write_all(&response).await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let error = review(
            &mut client,
            &mut output,
            ReviewCommand::ListFindings { run_id },
            Some(ClientDeploymentLimits {
                max_review_findings_per_run: Some(REVIEW_FINDING_LIMIT_FIXTURE),
                ..ClientDeploymentLimits::unbounded()
            }),
        )
        .await
        .expect_err("the over-bound finding inventory must be rejected");

        assert_eq!(
            error.to_string(),
            "the server violated the process protocol: review finding list exceeded its admitted bound"
        );
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn create_connection_failure_is_definitely_uncommitted() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let mut client = ProcessClient::new(directory.path().join("missing.sock"));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);

        let result = create(
            &mut client,
            &mut output,
            ModelSelection::Direct {
                selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            },
            Some(CommandId::try_from_uuid(Uuid::from_u128(2))?),
            None,
            super::SessionPlacement::Pathless {},
        )
        .await;

        assert!(matches!(result, Err(ClientError::Io(_))));
        Ok(())
    }

    #[tokio::test]
    async fn create_rejects_settings_for_another_direct_model() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let requested_selection_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let returned_selection_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let command_id = CommandId::try_from_uuid(Uuid::from_u128(3))?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::CreateSession {
                    command_id,
                    initial_model_selection: ModelSelection::Direct {
                        selection_id: requested_selection_id,
                    },
                    model_settings: ModelSettingsOverlay::inherit_all(),
                    system_prompt: SystemPromptMember::present(None),
                    placement: SessionPlacement::Pathless {},
                    lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
                }
            );
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::SessionCreated {
                    session_id,
                    model_settings: session_reasoning_model_settings(returned_selection_id),
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let error = create(
            &mut client,
            &mut output,
            ModelSelection::Direct {
                selection_id: requested_selection_id,
            },
            Some(command_id),
            None,
            SessionPlacement::Pathless {},
        )
        .await
        .expect_err("creation must reject settings validated for another direct model");

        assert!(error.is_ambiguous_mutation());
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn model_replacement_preserves_the_session_settings_layer() -> Result<(), Box<dyn Error>>
    {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let prior_selection_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let installed_selection_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        let command_id = CommandId::try_from_uuid(Uuid::from_u128(4))?;
        let expected_session_settings = session_reasoning_model_settings(prior_selection_id)
            .precedence
            .session;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let read = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                read.request(),
                &ClientRequest::ReadSessionDefaults {
                    session_id,
                    defaults_version: None,
                }
            );
            let read_response = ServerFrame::try_new_for_version(
                read.version(),
                read.request_id(),
                ServerMessage::SessionDefaults {
                    session_id,
                    defaults_version: CanonicalU64::new(1),
                    model_selection: ModelSelection::Direct {
                        selection_id: prior_selection_id,
                    },
                    model_settings: session_reasoning_model_settings(prior_selection_id),
                    dangerous_tool_auto_approval: false,
                    system_prompt: None,
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&read_response).map_err(io::Error::other)?)
                .await?;

            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let replace = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                replace.request(),
                &ClientRequest::ReplaceSessionDefaults {
                    command_id,
                    session_id,
                    expected_defaults_version: CanonicalU64::new(1),
                    model_selection: ModelSelection::Direct {
                        selection_id: installed_selection_id,
                    },
                    model_settings: expected_session_settings,
                    dangerous_tool_auto_approval: false,
                    system_prompt: SystemPromptMember::present(None),
                }
            );
            let replace_response = ServerFrame::try_new_for_version(
                replace.version(),
                replace.request_id(),
                ServerMessage::SessionDefaultsReplaced {
                    session_id,
                    defaults_version: CanonicalU64::new(2),
                    model_selection: ModelSelection::Direct {
                        selection_id: installed_selection_id,
                    },
                    model_settings: session_reasoning_model_settings(installed_selection_id),
                    dangerous_tool_auto_approval: false,
                    system_prompt: SystemPromptMember::present(None),
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&replace_response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        replace_session_model(
            &mut client,
            &mut output,
            session_id,
            ModelSelection::Direct {
                selection_id: installed_selection_id,
            },
            Some(command_id),
            None,
            None,
            ModelSystemPromptChoice::Keep,
        )
        .await?;

        assert_eq!(
            String::from_utf8(stderr)?,
            "defaults_version=1\ndangerous_tool_auto_approval=disabled\n"
        );
        assert_eq!(
            String::from_utf8(stdout)?,
            format!("session={session_id} defaults_version=2 model={installed_selection_id}\n")
        );
        server.await??;
        Ok(())
    }

    #[test]
    fn model_replacement_rejects_a_receipt_with_another_session_settings_layer() {
        let prior_selection_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let expected_session_settings = session_reasoning_model_settings(prior_selection_id)
            .precedence
            .session;
        let returned = provider_default_model_settings();

        assert!(!replacement_receipt_settings_match(
            expected_session_settings,
            &returned
        ));
    }

    #[tokio::test]
    async fn submit_connection_failure_is_definitely_uncommitted() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let mut client = ProcessClient::new(directory.path().join("missing.sock"));

        let result = submit_input(
            &mut client,
            CommandId::try_from_uuid(Uuid::from_u128(1))?,
            CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            InputContent::new(String::from("queued content")),
            Some(CanonicalU64::new(1)),
            None,
        )
        .await;

        assert!(matches!(result, Err(ClientError::Io(_))));
        Ok(())
    }

    #[tokio::test]
    async fn submit_input_releases_its_connection_after_acceptance() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let command_id = CommandId::try_from_uuid(Uuid::from_u128(4))?;
        let content = InputContent::new(String::from("queued content"));
        let expected_content = UserInputContent::text(content.clone().into_string());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::SubmitInput {
                    command_id,
                    session_id,
                    content: expected_content,
                    expected_defaults_version: Some(CanonicalU64::new(1)),
                    model_settings: ModelSettingsOverlay::inherit_all(),
                    delivery: None,
                }
            );
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::InputSubmitted {
                    session_id,
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                    acceptance_position: CanonicalU64::new(1),
                    turn_id,
                    model_settings: provider_default_model_settings(),
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;

            let mut byte = [0_u8; 1];
            let read = timeout(Duration::from_secs(1), reader.read(&mut byte))
                .await
                .map_err(io::Error::other)??;
            assert_eq!(read, 0);
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let submitted_turn = submit_input(
            &mut client,
            command_id,
            session_id,
            content,
            Some(CanonicalU64::new(1)),
            None,
        )
        .await?;
        assert_eq!(submitted_turn, SubmitInputReceipt::Turn { turn_id });
        server.await??;
        Ok(())
    }

    /// INV-033: the current client sends the configuration-free request and returns its typed accepted-input/source-turn receipt.
    #[tokio::test]
    async fn inv033_current_client_uses_the_exact_steering_exchange() -> Result<(), Box<dyn Error>>
    {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let source_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let accepted_input_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(request.version(), ProtocolVersion::One);
            assert_eq!(
                request.request(),
                &ClientRequest::SubmitInput {
                    command_id: CommandId::try_from_uuid(Uuid::from_u128(4))
                        .map_err(io::Error::other)?,
                    session_id,
                    content: UserInputContent::text(String::from("steering content")),
                    expected_defaults_version: None,
                    model_settings: ModelSettingsOverlay::inherit_all(),
                    delivery: Some(InputDelivery::Steer {
                        expected_active_turn_id: source_turn_id,
                    }),
                }
            );
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::SteeringSubmitted {
                    session_id,
                    accepted_input_id,
                    acceptance_position: CanonicalU64::new(2),
                    source_turn_id,
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let receipt = submit_input(
            &mut client,
            CommandId::try_from_uuid(Uuid::from_u128(4))?,
            session_id,
            InputContent::new(String::from("steering content")),
            None,
            Some(InputDelivery::Steer {
                expected_active_turn_id: source_turn_id,
            }),
        )
        .await?;
        assert_eq!(
            receipt,
            SubmitInputReceipt::Steering {
                accepted_input_id,
                acceptance_position: 2,
                source_turn_id,
            }
        );
        server.await??;
        Ok(())
    }

    /// INV-033: the reconciliation verb names the exact parked turn on the
    /// wire and returns the accepted successor turn.
    #[tokio::test]
    async fn reconcile_turn_names_the_parked_turn_and_returns_its_successor()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let parked_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let successor_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(5));
        let command_id = CommandId::try_from_uuid(Uuid::from_u128(4))?;
        let defaults_version = CanonicalU64::new(1);
        let content = InputContent::new(String::from("continue after reconciliation"));
        let expected_content = UserInputContent::text(content.clone().into_string());
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::ReconcileTurn {
                    command_id,
                    session_id,
                    expected_active_turn_id: parked_turn_id,
                    content: expected_content,
                    expected_defaults_version: defaults_version,
                    model_settings: ModelSettingsOverlay::inherit_all(),
                }
            );
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::InputSubmitted {
                    session_id,
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                    acceptance_position: CanonicalU64::new(2),
                    turn_id: successor_turn_id,
                    model_settings: provider_default_model_settings(),
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let accepted_successor = reconcile_turn(
            &mut client,
            command_id,
            session_id,
            parked_turn_id,
            content,
            defaults_version,
        )
        .await?;
        assert_eq!(accepted_successor, successor_turn_id);
        server.await??;
        Ok(())
    }

    /// INV-033: the stop verb names the exact expected active turn on the
    /// wire and returns the accepted successor turn.
    #[tokio::test]
    async fn inv033_stop_turn_names_the_active_turn_and_returns_its_successor()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let active_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let successor_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(5));
        let command_id = CommandId::try_from_uuid(Uuid::from_u128(4))?;
        let content = InputContent::new(String::from("continue after the stop"));
        let defaults_version = CanonicalU64::new(1);
        let selected_scope = DescendantTerminationScope::ParentAndDescendants;
        let expected_request = ClientRequest::StopTurn {
            command_id,
            session_id,
            expected_active_turn_id: active_turn_id,
            content: UserInputContent::text(content.clone().into_string()),
            expected_defaults_version: defaults_version,
            descendant_scope: selected_scope,
            model_settings: ModelSettingsOverlay::inherit_all(),
        };
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(request.request(), &expected_request);
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::InputSubmitted {
                    session_id,
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                    acceptance_position: CanonicalU64::new(2),
                    turn_id: successor_turn_id,
                    model_settings: provider_default_model_settings(),
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let accepted_successor = stop_turn(
            &mut client,
            command_id,
            session_id,
            active_turn_id,
            content,
            defaults_version,
            selected_scope,
        )
        .await?;
        assert_eq!(accepted_successor, successor_turn_id);
        server.await??;
        Ok(())
    }

    /// INV-033: a decision verb sends the exact closed decision and validates
    /// that the receipt echoes the same request and decision.
    #[tokio::test]
    async fn inv033_decide_validates_the_exact_recorded_receipt() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let tool_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert!(matches!(
                request.request(),
                ClientRequest::DecideToolRequest {
                    session_id: requested_session,
                    tool_request_id: requested_tool,
                    decision: ToolDecision::Deny { reason },
                    ..
                } if *requested_session == session_id
                    && *requested_tool == tool_request_id
                    && reason == "writes outside the workspace"
            ));
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::ToolRequestDecided {
                    tool_request_id,
                    decision: ToolDecision::Deny {
                        reason: String::from("writes outside the workspace"),
                    },
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let mut client = ProcessClient::new(socket);
        decide(
            &mut client,
            &mut output,
            session_id,
            tool_request_id,
            Some(CommandId::try_from_uuid(Uuid::from_u128(4))?),
            ToolDecision::Deny {
                reason: String::from("writes outside the workspace"),
            },
        )
        .await?;
        server.await??;
        assert_eq!(
            String::from_utf8(stdout)?,
            format!("tool_request={tool_request_id} decision=deny\n")
        );
        assert_eq!(String::from_utf8(stderr)?, "");
        Ok(())
    }

    /// INV-033: a receipt naming a different request or decision is a
    /// protocol violation, never silently accepted.
    #[tokio::test]
    async fn inv033_decide_rejects_a_receipt_for_a_different_decision() -> Result<(), Box<dyn Error>>
    {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let tool_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::ToolRequestDecided {
                    tool_request_id,
                    decision: ToolDecision::Approve {},
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let mut client = ProcessClient::new(socket);
        let result = decide(
            &mut client,
            &mut output,
            session_id,
            tool_request_id,
            Some(CommandId::try_from_uuid(Uuid::from_u128(4))?),
            ToolDecision::Deny {
                reason: String::from("writes outside the workspace"),
            },
        )
        .await;
        assert!(matches!(result, Err(ClientError::AmbiguousMutation)));
        server.await??;
        Ok(())
    }

    /// INV-033: `decide` accepts only its own receipt. A `tool_denial_overridden`
    /// receipt names a distinct command — it proves a one-shot override was
    /// recorded for a future re-proposal, never that this pending request was
    /// decided — so naming the same request cannot make it stand in for one.
    #[tokio::test]
    async fn inv033_decide_rejects_a_denial_override_receipt() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let tool_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::ToolDenialOverridden { tool_request_id },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let mut client = ProcessClient::new(socket);
        let result = decide(
            &mut client,
            &mut output,
            session_id,
            tool_request_id,
            Some(CommandId::try_from_uuid(Uuid::from_u128(4))?),
            ToolDecision::Approve {},
        )
        .await;
        assert!(matches!(result, Err(ClientError::AmbiguousMutation)));
        server.await??;
        assert_eq!(String::from_utf8(stdout)?, "");
        Ok(())
    }

    const DELEGATION_SESSION: &str = "00000000-0000-0000-0000-000000000001";
    const DELEGATION_TURN: &str = "00000000-0000-0000-0000-000000000002";
    const DELEGATION_SPAWN_REQUEST: &str = "00000000-0000-0000-0000-000000000003";
    const DELEGATION_CHILD: &str = "00000000-0000-0000-0000-000000000004";
    const DELEGATION_AWAIT_REQUEST: &str = "00000000-0000-0000-0000-000000000005";
    const DELEGATION_CHILD_TURN: &str = "00000000-0000-0000-0000-000000000006";
    const DELEGATION_MESSAGE_REQUEST: &str = "00000000-0000-0000-0000-000000000007";
    const DELEGATION_MESSAGE: &str = "00000000-0000-0000-0000-000000000008";
    const DELEGATION_BACKGROUND_AWAIT_REQUEST: &str = "00000000-0000-0000-0000-000000000009";
    const DELEGATION_FOREIGN_PARENT: &str = "00000000-0000-0000-0000-00000000000a";

    struct DelegationVerbResult {
        exit: ExitCode,
        stdout: String,
        stderr: String,
    }

    async fn run_delegation_verb(
        command: &[&str],
        expected: ClientRequest,
        response: ServerMessage,
    ) -> Result<DelegationVerbResult, Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let server =
            tokio::spawn(
                async move { accept_request_and_reply(&listener, &expected, response).await },
            );
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run(
            client_arguments(&socket, command),
            None,
            &mut input,
            &mut stdout,
            &mut stderr,
        )
        .await;
        server.await??;
        Ok(DelegationVerbResult {
            exit,
            stdout: String::from_utf8(stdout)?,
            stderr: String::from_utf8(stderr)?,
        })
    }

    #[tokio::test]
    async fn delegation_spawn_encodes_request_and_renders_receipt() -> Result<(), Box<dyn Error>> {
        let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
        let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
        let request_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SPAWN_REQUEST)?);
        let child_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_CHILD)?);
        let relationship = DelegationPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let result = run_delegation_verb(
            &[
                "session",
                "spawn",
                DELEGATION_SESSION,
                DELEGATION_TURN,
                DELEGATION_SPAWN_REQUEST,
                "--task",
                "inspect logs",
                "--bound",
                "--on-parent-stopped",
                "stop",
                "--on-parent-cancelled",
                "cancel",
            ],
            ClientRequest::SpawnSession {
                session_id,
                turn_id,
                tool_request_id: request_id,
                task: String::from("inspect logs"),
                relationship,
            },
            ServerMessage::SessionSpawned {
                tool_request_id: request_id,
                child_session_id: child_id,
                relationship,
            },
        )
        .await?;

        assert_eq!(result.exit, ExitCode::SUCCESS);
        assert_eq!(
            result.stdout,
            format!(
                "spawn_request={request_id} child_session={child_id} relationship=bound on_parent_stopped=stop on_parent_cancelled=cancel\n"
            )
        );
        assert_eq!(result.stderr, "");
        Ok(())
    }

    #[tokio::test]
    async fn delegation_spawn_rejects_the_parent_as_its_own_child() -> Result<(), Box<dyn Error>> {
        let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
        let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
        let request_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SPAWN_REQUEST)?);
        let relationship = DelegationPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let result = run_delegation_verb(
            &[
                "session",
                "spawn",
                DELEGATION_SESSION,
                DELEGATION_TURN,
                DELEGATION_SPAWN_REQUEST,
                "--task",
                "inspect logs",
                "--bound",
                "--on-parent-stopped",
                "stop",
                "--on-parent-cancelled",
                "cancel",
            ],
            ClientRequest::SpawnSession {
                session_id,
                turn_id,
                tool_request_id: request_id,
                task: String::from("inspect logs"),
                relationship,
            },
            ServerMessage::SessionSpawned {
                tool_request_id: request_id,
                child_session_id: session_id,
                relationship,
            },
        )
        .await?;

        assert_eq!(result.exit, ExitCode::FAILURE);
        assert_eq!(result.stdout, "");
        assert!(!result.stderr.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn delegation_foreground_await_encodes_request_and_renders_result()
    -> Result<(), Box<dyn Error>> {
        let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
        let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
        let request_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_AWAIT_REQUEST)?);
        let spawn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SPAWN_REQUEST)?);
        let child_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_CHILD)?);
        let child_turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_CHILD_TURN)?);
        let result = run_delegation_verb(
            &[
                "session",
                "await",
                DELEGATION_SESSION,
                DELEGATION_TURN,
                DELEGATION_AWAIT_REQUEST,
                DELEGATION_CHILD,
                "--mode",
                "foreground",
            ],
            ClientRequest::AwaitSession {
                session_id,
                turn_id,
                tool_request_id: request_id,
                child_session_id: child_id,
                mode: DelegationWaitMode::Foreground,
            },
            ServerMessage::ChildResult {
                await_request_id: request_id,
                spawning_request_id: spawn_id,
                child_session_id: child_id,
                outcome: DelegationOutcome::Returned,
                content: Some(String::from("done\nnow")),
                reason: DelegationReason::ChildCompleted,
                provenance: DelegationProvenance::ChildTurn {
                    child_session_id: child_id,
                    child_turn_id,
                },
            },
        )
        .await?;

        assert_eq!(result.exit, ExitCode::SUCCESS);
        assert_eq!(
            result.stdout,
            format!(
                "await_request={request_id} spawning_request={spawn_id} child_session={child_id} delivery=foreground outcome=returned reason=child_completed provenance=child_turn:{child_id}:{child_turn_id} content=done\\u{{a}}now\n"
            )
        );
        assert_eq!(result.stderr, "");
        Ok(())
    }

    #[tokio::test]
    async fn delegation_foreground_await_rejects_another_parent_provenance()
    -> Result<(), Box<dyn Error>> {
        let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
        let foreign_parent = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_FOREIGN_PARENT)?);
        let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
        let request_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_AWAIT_REQUEST)?);
        let spawn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SPAWN_REQUEST)?);
        let child_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_CHILD)?);
        let command_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_MESSAGE)?);
        let result = run_delegation_verb(
            &[
                "session",
                "await",
                DELEGATION_SESSION,
                DELEGATION_TURN,
                DELEGATION_AWAIT_REQUEST,
                DELEGATION_CHILD,
                "--mode",
                "foreground",
            ],
            ClientRequest::AwaitSession {
                session_id,
                turn_id,
                tool_request_id: request_id,
                child_session_id: child_id,
                mode: DelegationWaitMode::Foreground,
            },
            ServerMessage::ChildResult {
                await_request_id: request_id,
                spawning_request_id: spawn_id,
                child_session_id: child_id,
                outcome: DelegationOutcome::Stopped,
                content: None,
                reason: DelegationReason::ParentStopped,
                provenance: DelegationProvenance::ParentGoalCommand {
                    parent_session_id: foreign_parent,
                    goal_generation: CanonicalU64::new(1),
                    command_id,
                    descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                },
            },
        )
        .await?;

        assert_eq!(result.exit, ExitCode::FAILURE);
        assert_eq!(result.stdout, "");
        assert!(!result.stderr.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn delegation_background_await_encodes_request_and_renders_registration()
    -> Result<(), Box<dyn Error>> {
        let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
        let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
        let request_id =
            CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_BACKGROUND_AWAIT_REQUEST)?);
        let child_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_CHILD)?);
        let result = run_delegation_verb(
            &[
                "session",
                "await",
                DELEGATION_SESSION,
                DELEGATION_TURN,
                DELEGATION_BACKGROUND_AWAIT_REQUEST,
                DELEGATION_CHILD,
                "--mode",
                "background",
            ],
            ClientRequest::AwaitSession {
                session_id,
                turn_id,
                tool_request_id: request_id,
                child_session_id: child_id,
                mode: DelegationWaitMode::Background,
            },
            ServerMessage::SessionAwaitRegistered {
                tool_request_id: request_id,
                child_session_id: child_id,
                mode: DelegationWaitMode::Background,
            },
        )
        .await?;

        assert_eq!(result.exit, ExitCode::SUCCESS);
        assert_eq!(
            result.stdout,
            format!("await_request={request_id} child_session={child_id} mode=background\n")
        );
        assert_eq!(result.stderr, "");
        Ok(())
    }

    #[tokio::test]
    async fn delegation_message_encodes_request_and_renders_receipt() -> Result<(), Box<dyn Error>>
    {
        let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
        let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
        let request_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_MESSAGE_REQUEST)?);
        let child_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_CHILD)?);
        let message_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_MESSAGE)?);
        let result = run_delegation_verb(
            &[
                "session",
                "message",
                DELEGATION_SESSION,
                DELEGATION_TURN,
                DELEGATION_MESSAGE_REQUEST,
                DELEGATION_CHILD,
                "--content",
                "status ready",
            ],
            ClientRequest::SendSessionMessage {
                session_id,
                turn_id,
                tool_request_id: request_id,
                peer_session_id: child_id,
                content: String::from("status ready"),
            },
            ServerMessage::SessionMessageSent {
                tool_request_id: request_id,
                message_id,
                direction: DelegationMessageDirection::ParentToChild,
                ordinal: CanonicalU64::new(2),
                delivery_sequence: CanonicalU64::new(1),
            },
        )
        .await?;

        assert_eq!(result.exit, ExitCode::SUCCESS);
        assert_eq!(
            result.stdout,
            format!(
                "message_request={request_id} peer_session={child_id} message={message_id} direction=parent_to_child ordinal=2 delivery_sequence=1\n"
            )
        );
        assert_eq!(result.stderr, "");
        Ok(())
    }

    #[tokio::test]
    async fn delegation_background_await_rejects_self_relationship() -> Result<(), Box<dyn Error>> {
        let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
        let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
        let request_id =
            CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_BACKGROUND_AWAIT_REQUEST)?);
        let result = run_delegation_verb(
            &[
                "session",
                "await",
                DELEGATION_SESSION,
                DELEGATION_TURN,
                DELEGATION_BACKGROUND_AWAIT_REQUEST,
                DELEGATION_SESSION,
                "--mode",
                "background",
            ],
            ClientRequest::AwaitSession {
                session_id,
                turn_id,
                tool_request_id: request_id,
                child_session_id: session_id,
                mode: DelegationWaitMode::Background,
            },
            ServerMessage::SessionAwaitRegistered {
                tool_request_id: request_id,
                child_session_id: session_id,
                mode: DelegationWaitMode::Background,
            },
        )
        .await?;

        assert_eq!(result.exit, ExitCode::FAILURE);
        assert_eq!(result.stdout, "");
        assert!(!result.stderr.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn delegation_foreground_await_rejects_self_relationship() -> Result<(), Box<dyn Error>> {
        let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
        let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
        let request_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_AWAIT_REQUEST)?);
        let spawn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SPAWN_REQUEST)?);
        let child_turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_CHILD_TURN)?);
        let result = run_delegation_verb(
            &[
                "session",
                "await",
                DELEGATION_SESSION,
                DELEGATION_TURN,
                DELEGATION_AWAIT_REQUEST,
                DELEGATION_SESSION,
                "--mode",
                "foreground",
            ],
            ClientRequest::AwaitSession {
                session_id,
                turn_id,
                tool_request_id: request_id,
                child_session_id: session_id,
                mode: DelegationWaitMode::Foreground,
            },
            ServerMessage::ChildResult {
                await_request_id: request_id,
                spawning_request_id: spawn_id,
                child_session_id: session_id,
                outcome: DelegationOutcome::Returned,
                content: Some(String::from("done")),
                reason: DelegationReason::ChildCompleted,
                provenance: DelegationProvenance::ChildTurn {
                    child_session_id: session_id,
                    child_turn_id,
                },
            },
        )
        .await?;

        assert_eq!(result.exit, ExitCode::FAILURE);
        assert_eq!(result.stdout, "");
        assert!(!result.stderr.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn delegation_message_rejects_self_peer() -> Result<(), Box<dyn Error>> {
        let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
        let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
        let request_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_MESSAGE_REQUEST)?);
        let message_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_MESSAGE)?);
        let result = run_delegation_verb(
            &[
                "session",
                "message",
                DELEGATION_SESSION,
                DELEGATION_TURN,
                DELEGATION_MESSAGE_REQUEST,
                DELEGATION_SESSION,
                "--content",
                "status ready",
            ],
            ClientRequest::SendSessionMessage {
                session_id,
                turn_id,
                tool_request_id: request_id,
                peer_session_id: session_id,
                content: String::from("status ready"),
            },
            ServerMessage::SessionMessageSent {
                tool_request_id: request_id,
                message_id,
                direction: DelegationMessageDirection::ParentToChild,
                ordinal: CanonicalU64::new(2),
                delivery_sequence: CanonicalU64::new(1),
            },
        )
        .await?;

        assert_eq!(result.exit, ExitCode::FAILURE);
        assert_eq!(result.stdout, "");
        assert!(!result.stderr.is_empty());
        Ok(())
    }

    fn over_bound_review_findings_response(
        request: &ClientFrame,
        run_id: CanonicalUuid,
        maximum: u64,
    ) -> Result<Vec<u8>, FrameEncodeError> {
        const FIRST_FINDING_IDENTITY: u128 = 10;

        let frame = |message| {
            ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
        };
        let mut response =
            encode_server_line(&frame(ServerMessage::ReviewFindingsStart { run_id })?)?;
        for offset in 0..=maximum {
            let finding_id = CanonicalUuid::from_uuid(Uuid::from_u128(
                FIRST_FINDING_IDENTITY + u128::from(offset),
            ));
            let finding = ReviewFindingSnapshot {
                target_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                run_id,
                producing_pass_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
                finding: ReviewFindingInput {
                    finding_id,
                    file_path: String::from("src/review.rs"),
                    line_start: None,
                    line_end: None,
                    diff_side: None,
                    title: String::from("Bound the list"),
                    body: String::from("The client must reject an over-bound inventory."),
                    severity: ReviewSeverity::High,
                    is_real_confidence: CanonicalU64::new(9_000),
                    severity_label_confidence: CanonicalU64::new(8_500),
                    category: String::from("availability"),
                    recommended_fix: None,
                },
                status: ReviewFindingStatus::Open,
                event_count: CanonicalU64::new(0),
            };
            response.extend_from_slice(&encode_server_line(&frame(
                ServerMessage::ReviewFindingItem { finding },
            )?)?);
        }
        response.extend_from_slice(&encode_server_line(&frame(
            ServerMessage::ReviewFindingsEnd {
                finding_count: CanonicalU64::new(maximum + 1),
            },
        )?)?);
        Ok(response)
    }

    const REVIEW_FINDING_LIMIT_FIXTURE: u64 = 3;

    fn review_run_snapshot(pass_id: Option<CanonicalUuid>) -> ReviewRunSnapshot {
        ReviewRunSnapshot {
            target_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            run_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            workflow: ReviewWorkflow::ReadOnlyReview,
            policy_version: CanonicalU64::new(1),
            minimum_judge_confidence: CanonicalU64::new(8_000),
            minimum_publication_confidence: CanonicalU64::new(9_000),
            state: ReviewRunLifecycle::Queued,
            pass_id,
        }
    }

    fn review_pass_snapshot() -> ReviewPassSnapshot {
        ReviewPassSnapshot {
            pass_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            run_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            target_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            kind: ReviewPassKind::ReadOnlyReview,
            session_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
            accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(6)),
            origin_turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(7)),
            state: ReviewPassLifecycle::Queued,
            turn_id: None,
            output_frontier_id: None,
        }
    }
}
