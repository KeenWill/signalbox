//! First text-only model-call execution orchestration.
//!
//! docs/spec/model-call-execution.md owns the staged transaction and
//! provider-effect order. The application keeps persistence, provider
//! capability preparation, send authorization, provider interaction, and
//! terminal observation distinct.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    future::Future,
    num::NonZeroU64,
    sync::{Arc, Weak},
    time::Duration,
};

/// Maximum projected frontier content and representation overhead retained for one call.
pub const MAX_RETAINED_FRONTIER_CONTENT_BYTES: usize = 256 * 1024 * 1024;

// Worst-case compact JSON for maximum checked metadata, u64 length, and digest.
const MAX_RENDERED_ATTACHMENT_STUB_BYTES: usize = 2_304;

use signalbox_domain::{
    AcceptedInputId, AmbiguousModelCallTurnIdentities, AssistantResponsePart, AssistantText,
    AttachmentKind, AuthorizedModelCall, AvailabilitySuccessorModelCallTurn, BlobDigest,
    CompletedModelCallIdentities, ContextCompactionRange, ContextFrontierId,
    ContextFrontierProjection, ContextFrontierProjectionFailure,
    CorrelatedModelCallTerminalObservation, CredentialPoolExhaustedModelCallTurn,
    DangerousToolAutoApproval, DelegationContent, DelegationMessageId, DelegationOutcome,
    DelegationWaitMode, DirectModelSelection, FailedModelCallTurn, FailedModelCallTurnIdentities,
    ImportedSourceAttestation, ImportedSpeaker, ImportedText, ImportedTranscriptContent,
    ImportedTranscriptEntryId, InitialToolApproval, ModelCallId, ModelCallTerminalIdentities,
    ModelCallTerminalObservation, ModelCallTerminalOutcome,
    PhysicalCancellationModelCallTurnIdentities, PreparedModelCallRequest, ProviderCompactionBlock,
    RecordedUserOverride, RefusedModelCallTurnIdentities, SemanticTranscriptEntryId,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryRef,
    SessionConfigurationDefaultsVersion, SessionId, SessionSystemPrompt,
    StopRequestedModelCallTurn, StoppedToolResponsePartIdentity,
    StoppedToolRoundModelCallIdentities, ToolApprovalDecision, ToolAttemptEnd, ToolDenialReason,
    ToolExecutionError, ToolRequest, ToolRequestId, ToolResponsePartIdentity, ToolResultContent,
    ToolRoundModelCallIdentities, TurnAttemptId, TurnId, UserContent, UserContentPart,
};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::{
    ClassifyOperatorFailure, NoToolCatalog, OperatorFailureClass, ResolvedToolConversationEntry,
    ToolCatalog, ToolDefinition, tool_loop::initial_tool_approval,
};

mod content;
pub use content::{
    ModelAttachmentStub, ModelCallCredentialReference, ModelConversationMessage,
    ModelToolResultContent, ModelUserContent, ModelUserContentPart, ProviderReasoningProvenance,
};
use content::{SerializedAttachmentEnvelope, SerializedAttachmentStub};

mod provider;
pub use provider::{
    AttemptDispatchGate, InProcessAttemptDispatchGate, InProcessAttemptDispatchPermit,
    ModelCallExecutionIdGenerator, ModelCallProvider, UuidV7ModelCallExecutionIdGenerator,
};

mod outcome;
pub use outcome::{ModelCallExecutionError, ModelCallExecutionOutcome};

mod scripted;
pub use scripted::{
    ScriptedModelCallCapability, ScriptedModelCallError, ScriptedModelCallProvider,
    ScriptedModelCallStep,
};

mod render;
#[cfg(test)]
use render::render_frontier_messages;
use render::render_frontier_messages_with_placements;
pub use render::{
    ModelFrontierRenderingError, projected_frontier_content_bytes, render_model_user_content,
};

mod prepared;
pub use prepared::PreparedModelOperation;

mod ports;
use ports::RetainedModelCallExecutionStateKind;
pub use ports::{
    AttachmentPreparationFailure, AuthorizeModelCallOutcome, AuthorizeModelCallTransaction,
    AvailabilitySuccessorOutcome, CommitModelCallObservationTransaction,
    CredentialPoolExhaustedOutcome, FailPreparedModelCallTransaction, ModelCallAuthorizationReread,
    ModelCallCapabilityPreparation, ModelCallInputTokenCount, ModelCallInputTokenCounter,
    ModelCallObservationCommitOutcome, ModelCallTerminalIdentityCandidates,
    PrepareModelCallOutcome, PrepareModelCallTransaction, PreparedModelCallFailureCause,
    RetainedModelCallExecutionState, RetainedModelCallObservationStatus,
    RetainedPreparedFailureStatus,
};

mod report;
use report::{
    TurnTerminalOutcome, automatic_tool_round_count, report_model_call_terminalization,
    report_turn_terminalization,
};

mod service;
pub use service::ModelCallExecutionService;

#[cfg(test)]
mod tests;
