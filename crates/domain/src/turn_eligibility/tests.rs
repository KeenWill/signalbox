//! Turn scheduling eligibility tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use expect_test::expect;
use signalbox_expect_table::table;

use super::*;
use crate::{
    AcceptedInputDisposition, AssistantText, AttemptEnd, CreateSessionFromImportedFrontier,
    CurrentTurnAttemptState, DescendantTerminationScope, FrozenModelSelection,
    ImportedConversation, ImportedConversationFormat, ImportedRawRecordPosition,
    ImportedRawSourceRecord, ImportedRecordEntryPosition, ImportedSessionReconstitutionInput,
    ImportedSessionRelationship, ImportedSourceAttestation, ImportedSourceMetadata,
    ImportedStructuredObjectMember, ImportedStructuredValue, ImportedText,
    ImportedTranscriptContent, ImportedTranscriptEntryInput, ImportedTranscriptPosition,
    ModelCallReconstitutionInput, ModelCallReconstitutionState, ModelSelectionOverride,
    ModelSelectionRequest, NormalizedToolArguments, PerInputConfigurationChoices,
    ResolvedProviderTarget, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionCreationCause, SessionCreationProvenance, SessionPlacement, SessionPlacementVersion,
    SessionReconstitutionInput, ToolApprovalDecision, ToolApprovalResolutionReconstitutionInput,
    ToolAttemptEnd, ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState,
    ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionInput, ToolDispatchGeneration,
    ToolEffectClass, ToolExecutionError, ToolExecutionErrorKind, ToolName, ToolRequestOrdinal,
    ToolRequestReconstitutionInput, ToolResultContent, ToolResultText, VersionedSessionPlacement,
    test_support::{
        accepted_input_id, command_id, context_frontier_id, delegation_message_id, direct,
        imported_conversation_id, imported_transcript_entry_id, model_call_id,
        provider_model_identity, semantic_transcript_entry_id, session_id, tool_attempt_id,
        tool_request_id, transcript_frontier, turn_attempt_id, turn_id,
    },
};

use super::acceptance_tail::{
    ActiveAcceptanceTailReconstitutionEvidence, reconstitute_active_acceptance_tail,
};
use super::correlation::{
    scheduling_record_is_terminal, tool_reconciliation_attempt_end_matches, validate_start,
};
use super::projection::{ActiveExecutingToolBatchCorrelation, active_execution_steering_inputs};
use super::reconstitute::promote_external_interrupt_chains;
use crate::{
    AcceptedInputId, AcceptedInputLifecycle, AcceptedInputQueueOrder, AcceptedInputQueueOrderError,
    AcceptedInputStartingLineage, ActiveTurnPhase, AppliedInterruptCommandResult,
    CancellationStopDisposition, ContextFrontierId, DelegationContent, DeliveryRequest,
    InitialSemanticTranscriptEntryPayload, ModelCallDisposition, OriginConfiguration,
    PendingSteeringReclassificationIdentity, ReconstitutedImportedSession,
    ResolvedContextFrontierReconstitutionInput, ResolvedContextFrontierSnapshot,
    SemanticTranscriptEntry, SemanticTranscriptEntryId, SemanticTranscriptEntryPayload,
    SemanticTranscriptEntryReconstitutionInput, SemanticTranscriptEntryRef, Session,
    SessionInputPosition, ToolApprovalResolution, ToolRequestId, TranscriptAncestry, TurnAttemptId,
    TurnConfigurationProvenance, TurnDisposition, TurnId, UnstoppedAttemptDisposition,
};
use std::{collections::BTreeMap, collections::BTreeSet, num::NonZeroU64};

mod acceptance_tail;
mod activation_and_failure;
mod continuation_calls;
mod delegated_activation;
mod eligibility_rejections;
mod external_interrupts;
mod fixtures;
mod frontier_lineage;
mod origin_and_delivery;
mod record_rejections;
mod snapshot_rejections;
mod steering;
mod terminal_frontier;
mod terminal_provenance;
mod tool_rounds;
