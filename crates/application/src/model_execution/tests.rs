use std::error::Error;
use std::{
    collections::VecDeque,
    io::{self, Write},
    num::NonZeroU64,
    sync::{Arc, Mutex as StdMutex},
};

use expect_test::expect;
use signalbox_domain::{
    AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputSchedulingReconstitutionInput, AcceptedInputStartingLineage,
    AcceptedInputTurnActivationIdentities, AcceptedInputTurnSchedulingRecord,
    AcceptedInputTurnSchedulingRecordState, ActiveTurnSchedulingReconstitutionInput, Actor,
    DecideToolRequest, DeliveryRequest, DirectModelSelection, DurableCommandId,
    FrozenModelSelection, ImportedMessageContentAbsence, ModelCallDisposition,
    ModelCallExecutionReconstitutionInput, ModelCallOriginContent, ModelCallReconstitutionInput,
    ModelCallReconstitutionState, ModelSelectionOverride, ModelSelectionRequest,
    ModelTargetCatalog, ModelTargetDefinition, NormalizedToolArguments,
    PerInputConfigurationChoices, PinnedProviderTargetReconstitutionInput, ProviderModelIdentity,
    ResolvedContextFrontierReconstitutionInput, ResolvedProviderTarget,
    SemanticTranscriptEntryReconstitutionInput, SessionAcceptanceTailEntryReconstitutionInput,
    SessionAcceptanceTailReconstitutionInput, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionInputPosition, SessionReconstitutionInput, SubmitInput,
    SubmitInputAppliedTurnOriginReconstitutionInput, SubmitInputDirectTurnOriginConstructionInput,
    SubmitInputReconstitutionInput, SubmitInputTurnOriginReconstitutionInput,
    ToolApprovalResolutionReconstitutionInput, ToolAttemptReconstitutionInput,
    ToolAttemptReconstitutionState, ToolDispatchGeneration, ToolEffectClass, ToolName,
    ToolPermissionDefault, ToolRequestOrdinal, ToolRequestReconstitutionInput, ToolResultText,
    TranscriptAncestry,
};
use uuid::Uuid;

use super::*;

mod support;
use support::{
    AttachmentFailureProvider, BoundaryBlockingProvider, CapturedTelemetry, FakeAuthorization,
    FakeError, FakeFailure, FakeObservation, FakePrepare, FixedIds, NoSendAuthorization,
    ScriptedFailure, UnusedAuthorization, UnusedFailure, UnusedObservation, UnusedProvider,
    counted_frontier_bytes, credential_reference, current_turn_tool_rounds, failed_turn_fixture,
    guarded_proposal, guarded_tool_approvals, identity, model_tool_request,
    one_current_batch_with_inherited_tool_history, prepared_fixture, ready,
    ready_with_tool_evidence, recorded_guarded_override, rendered_content_bytes, rendered_text,
    tool_response, tool_round_saturated_fixture, tool_round_saturated_fixture_with_assistant_text,
};

mod execution;
mod frontier_accounting;
mod preparation;
mod rendering;
mod tool_approval;
