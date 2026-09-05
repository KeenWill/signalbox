//! Atomic PostgreSQL persistence and replay for durable input acceptance.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::{
    error::Error,
    fmt,
    num::{NonZeroU32, NonZeroU64},
};

use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_application::{SubmitInputIdGenerator, SubmitInputOutcome, SubmitInputTransaction};
use signalbox_domain::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputQueuePriority, AcceptedInputSchedulingProjection,
    AcceptedInputSchedulingReconstitutionFailure, AcceptedInputSchedulingReconstitutionInput,
    AcceptedInputStartingLineage, AcceptedInputTurnSchedulingRecord,
    AcceptedInputTurnSchedulingRecordState, AcceptedInputTurnSchedulingStatus,
    ActiveTurnSchedulingReconstitutionInput, Actor, AppliedInterruptCommandResult, AssistantText,
    AttachmentDisplayFilename, AttachmentKind, AutomaticReconciliationAuthority, BlobDigest,
    CancellationStopDisposition, CancelledModelCallTurnIdentities,
    CancelledTurnExecutionReconstitutionInput, CommandPrincipal,
    ConsumedSteeringReconstitutionInput, ContextCompactionId,
    ContextCompactionModelCallReconstitutionInput, ContextCompactionModelCallState,
    ContextCompactionRange, ContextCompactionReconstitutionInput, ContextCompactionTokenUsage,
    ContextFrontierId, ContextFrontierProjection, ContinuationRoundReconstitutionInput,
    DelegatedTurnSchedulingFact, DelegatedTurnSchedulingState, DelegationContent,
    DelegationMessageId, DelegationOutcome, DelegationOutcomeKind, DelegationOutcomeReason,
    DelegationProvenanceReconstitutionInput, DelegationWaitMode, DeliveryRequest,
    DescendantTerminationScope, DirectModelSelection, DurableCommandId,
    FailedTurnExecutionReconstitutionInput, FrozenAliasDefinition, FrozenModelSelection,
    GoalEventOrdinal, GoalGeneration, GoalTurnOriginConstructionInput, GoalTurnSource,
    IssuedOperationRef, ModelAlias, ModelCallDisposition, ModelCallId, ModelCallInterruptOutcome,
    ModelCallReconstitutionInput, ModelCallReconstitutionState, ModelCallTerminalOutcome,
    ModelCapabilityCatalog, ModelSelectionOverride, ModelSelectionRequest,
    NonAcceptedTurnPredecessorReconstitutionInput, NonEmptyUnicodeTextFailure, OriginConfiguration,
    OriginConfigurationReconstitutionInput, OriginModelSettingsError, ParentTerminationKind,
    PerInputConfigurationChoices, PinnedProviderTargetReconstitutionInput, PreparedSubmitInput,
    ProviderCompactionBlock, ProviderModelIdentity, ReconstitutedSubmitInput,
    ResolvedContextFrontierReconstitutionInput, ResolvedContextFrontierSnapshot,
    ResolvedProviderTarget, RunnerGeneration, RunnerId, SemanticTranscriptEntryId,
    SemanticTranscriptEntryPayload as InitialSemanticTranscriptEntryPayload,
    SemanticTranscriptEntryReconstitutionInput, SemanticTranscriptEntryRef, Session,
    SessionAcceptanceTailEntryReconstitutionInput, SessionAcceptanceTailReconstitutionInput,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionId,
    SessionInputPosition, SteeringBinding, SteeringContinuationRoundReconstitutionInput,
    SteeringReclassificationReason, SubmitInput,
    SubmitInputAppliedPendingSteeringReconstitutionInput, SubmitInputAppliedResult,
    SubmitInputAppliedTurnOriginReconstitutionInput,
    SubmitInputAutomaticReconciliationConstructionInput,
    SubmitInputDirectTurnOriginConstructionInput,
    SubmitInputInterruptedModelCallReconciliationConstructionInput,
    SubmitInputInterruptedToolReconciliationConstructionInput, SubmitInputPreparationFailure,
    SubmitInputReclassifiedTurnOriginConstructionInput, SubmitInputReconstitutionFailure,
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
    SubmitInputTurnOriginReconstitutionInput, TerminalAttemptEndReconstitutionInput, ToolAttemptId,
    ToolRequestId, TranscriptAncestry, TurnAttemptId, TurnId, UnstoppedAttemptDisposition,
    UnsupportedModelSetting, UserContent, UserContentPart,
};
use sqlx::{FromRow, PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    command_registry::{
        self, CommandKind, RegistryCorruption, RegistryInspectionError, SUBMIT_INPUT_KIND,
    },
    mapping::{
        ActiveTurnPhaseStorageKind, PositiveOrdinalMappingError, accepted_input_id_from_uuid,
        accepted_input_id_to_uuid, active_turn_phase_from_str,
        dangerous_tool_auto_approval_from_str, dangerous_tool_auto_approval_to_str,
        defaults_version_from_numeric, defaults_version_to_numeric, durable_command_id_from_uuid,
        durable_command_id_to_uuid, input_position_from_numeric, input_position_to_numeric,
        model_change_adjustments_from_json, model_settings_from_json,
        model_settings_overlay_from_json, model_settings_overlay_to_json,
        positive_u64_from_numeric, session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid,
        turn_id_to_uuid,
    },
    model_execution::{
        ModelCallCorruption, ModelCallRepositoryError,
        attach_interrupt_reclassification_candidates,
        attach_interrupt_reclassification_candidates_for_activated,
        attach_interrupt_reclassification_candidates_for_active,
        attach_recovery_interrupt_reclassification_candidates,
        attach_recovery_interrupt_reclassification_candidates_for_activated, load_call_snapshot,
        load_delegated_runner_recovery_for_interrupt, lock_delegated_child_endpoint_sessions,
        persist_stop_requested, persist_terminal_outcome, persist_tool_reconciliation_required,
        require_live_execution_for_restart,
    },
    model_settings_resolution,
    outbox::{self, OutboxEvent},
    session::{SessionCorruption, SessionRepositoryError, load_session_from_connection},
    tool_loop::{
        deny_awaiting_approvals_for_interrupt, load_active_batch_from_connection,
        load_continuation_round_evidence, load_optional_foreground_delegation_outcome,
        load_recovery_batch_by_attempt, load_runner_recovery_batch_without_attempt,
        load_runner_recovery_cancellation_batch, load_runner_recovery_source_snapshot,
        load_steering_continuation_round_evidence, load_terminal_result_attempts,
        load_terminal_result_denials, persist_ended_attempt,
    },
};

const STORAGE_VERSION: i16 = 3;
const APPLIED: &str = "applied";
const REJECTED: &str = "rejected";

#[derive(FromRow)]
struct StoredSchedulingInventoryCounts {
    queue_count: i64,
    lifecycle_count: i64,
}

pub(crate) type StoredTurnOriginKey = (Uuid, Uuid);

struct StoredTurnOriginLink {
    provenance: StoredTurnOriginProvenance,
    kind: StoredTurnOriginKind,
    accepted_input: AcceptedInputId,
    queue_order: AcceptedInputQueueOrder,
}

enum StoredTurnOriginProvenance {
    Submit(DurableCommandId),
    Goal {
        generation: GoalGeneration,
        source: GoalTurnSource,
        content: UserContent,
    },
}

#[derive(Clone, Copy)]
enum StoredTurnOriginKind {
    Direct {
        predecessor: Option<StoredTurnOriginKey>,
    },
    Reclassified {
        source: StoredTurnOriginKey,
        source_disposition: StoredTerminalTurnDisposition,
    },
}

impl StoredTurnOriginKind {
    const fn dependency(self) -> Option<StoredTurnOriginKey> {
        match self {
            Self::Direct { predecessor } => predecessor,
            Self::Reclassified { source, .. } => Some(source),
        }
    }
}

#[derive(Clone, Copy)]
enum StoredTerminalTurnDisposition {
    Completed,
    Refused,
    Failed,
    Cancelled {
        interrupt_command: DurableCommandId,
    },
    ReconciliationRequired {
        authority: StoredAutomaticReconciliationAuthority,
        ambiguous_operation: IssuedOperationRef,
    },
}

#[derive(Clone, Copy)]
enum StoredAutomaticReconciliationAuthority {
    AppliedInterrupt(DurableCommandId),
    AutomaticRecovery(NonZeroU32),
}

impl StoredTerminalTurnDisposition {
    const fn unstopped_domain(self) -> Option<signalbox_domain::TurnDisposition> {
        match self {
            Self::Completed => Some(signalbox_domain::TurnDisposition::Completed),
            Self::Refused => Some(signalbox_domain::TurnDisposition::Refused),
            Self::Failed => Some(signalbox_domain::TurnDisposition::Failed),
            Self::Cancelled { .. } | Self::ReconciliationRequired { .. } => None,
        }
    }
}

fn turn_origin_dependency_order(
    relationships: impl IntoIterator<Item = (StoredTurnOriginKey, Option<StoredTurnOriginKey>)>,
) -> Option<Vec<StoredTurnOriginKey>> {
    let mut ready = VecDeque::new();
    let mut dependents: BTreeMap<StoredTurnOriginKey, Vec<StoredTurnOriginKey>> = BTreeMap::new();
    let mut relationship_count = 0;
    for (turn, predecessor) in relationships {
        relationship_count += 1;
        if let Some(predecessor) = predecessor {
            dependents.entry(predecessor).or_default().push(turn);
        } else {
            ready.push_back(turn);
        }
    }

    let mut ordered = Vec::with_capacity(relationship_count);
    while let Some(turn) = ready.pop_front() {
        ordered.push(turn);
        if let Some(newly_ready) = dependents.remove(&turn) {
            ready.extend(newly_ready);
        }
    }
    (ordered.len() == relationship_count).then_some(ordered)
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn turn_origin_dependency_order_handles_reverse_key_chains() {
        let session = Uuid::from_u128(1);
        let chain = (1..=512)
            .rev()
            .map(|turn| (session, Uuid::from_u128(turn)))
            .collect::<Vec<_>>();
        let relationships = chain
            .iter()
            .enumerate()
            .map(|(index, turn)| (*turn, index.checked_sub(1).map(|prior| chain[prior])))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            turn_origin_dependency_order(
                relationships
                    .iter()
                    .map(|(turn, predecessor)| (*turn, *predecessor)),
            ),
            Some(chain),
        );
    }

    #[test]
    fn turn_origin_dependency_order_rejects_cycles() {
        let session = Uuid::from_u128(1);
        let first = (session, Uuid::from_u128(1));
        let second = (session, Uuid::from_u128(2));

        assert_eq!(
            turn_origin_dependency_order([(first, Some(second)), (second, Some(first))]),
            None,
        );
    }

    #[test]
    fn lost_commit_response_is_typed_as_ambiguous() {
        let error = SubmitInputRepositoryError::from_commit_failure(sqlx::Error::Io(
            io::Error::new(io::ErrorKind::ConnectionReset, "commit response was lost"),
        ));

        assert!(matches!(
            error,
            SubmitInputRepositoryError::CommitAmbiguous(_)
        ));
    }

    #[test]
    fn imported_conversation_database_failure_remains_retryable() {
        let error = map_imported_scheduling_error(
            crate::create_session_from_imported_frontier::ImportedSessionRepositoryError::ImportedConversation(
                crate::conversation_import::ImportedConversationRepositoryError::Database(
                    sqlx::Error::PoolTimedOut,
                ),
            ),
        );

        assert!(matches!(
            error,
            SubmitInputRepositoryError::Database(sqlx::Error::PoolTimedOut)
        ));
    }

    #[test]
    fn delegation_child_result_decoder_restores_exact_typed_outcome() {
        let child = Uuid::from_u128(0xd101);
        let turn = Uuid::from_u128(0xd102);
        let content = DelegationContent::try_new(String::from("checked result"))
            .expect("fixture content is valid");
        let outcome = DelegationOutcome::reconstitute(
            decode_delegation_outcome_kind("result_returned")
                .expect("fixture outcome kind is supported"),
            Some(content.clone()),
            decode_delegation_outcome_reason("child_completed")
                .expect("fixture reason is supported"),
            decode_delegation_provenance("child_turn", child, Some(turn), None, None)
                .expect("fixture provenance is complete"),
        )
        .expect("fixture outcome is internally consistent");

        assert_eq!(outcome.kind(), DelegationOutcomeKind::ResultReturned);
        assert_eq!(outcome.content(), Some(&content));
        assert_eq!(
            outcome.reconstitution_provenance(),
            DelegationProvenanceReconstitutionInput::ChildTurn {
                session: session_id_from_uuid(child),
                turn: turn_id_from_uuid(turn),
            }
        );
    }

    #[test]
    fn delegation_parent_result_decoder_restores_command_provenance() {
        let parent = Uuid::from_u128(0xd111);
        let turn = Uuid::from_u128(0xd112);
        let command = Uuid::from_u128(0xd113);
        let outcome = DelegationOutcome::reconstitute(
            decode_delegation_outcome_kind("continue_running")
                .expect("fixture outcome kind is supported"),
            None,
            decode_delegation_outcome_reason("parent_stopped_parent_and_descendants")
                .expect("fixture reason is supported"),
            decode_delegation_provenance(
                "parent_turn_command",
                parent,
                Some(turn),
                None,
                Some(command),
            )
            .expect("fixture provenance is complete"),
        )
        .expect("fixture outcome is internally consistent");

        assert_eq!(outcome.kind(), DelegationOutcomeKind::ContinueRunning);
        assert_eq!(outcome.content(), None);
        assert_eq!(
            outcome.reconstitution_provenance(),
            DelegationProvenanceReconstitutionInput::ParentTurnCommand {
                session: session_id_from_uuid(parent),
                turn: turn_id_from_uuid(turn),
                command: durable_command_id_from_uuid(command)
                    .expect("fixture command identity is valid"),
            }
        );
    }

    #[test]
    fn delegation_result_decoder_rejects_incomplete_parent_provenance() {
        let error = decode_delegation_provenance(
            "parent_goal_command",
            Uuid::from_u128(0xd121),
            None,
            None,
            Some(Uuid::from_u128(0xd122)),
        )
        .expect_err("parent goal provenance requires its generation");

        assert_eq!(
            error,
            SubmitInputCorruption::Inconsistent("delegation result provenance")
        );
    }

    #[test]
    fn unsupported_model_setting_remains_a_caller_facing_repository_error() {
        let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x51));
        let unsupported = UnsupportedModelSetting::FastMode { selection };

        let error =
            map_model_settings_resolution_error(OriginModelSettingsError::Unsupported(unsupported));

        assert_unsupported_model_setting(error, &unsupported);
    }

    #[track_caller]
    fn assert_unsupported_model_setting(
        error: SubmitInputRepositoryError,
        expected: &UnsupportedModelSetting,
    ) {
        let SubmitInputRepositoryError::UnsupportedModelSetting(actual) = error else {
            panic!("expected unsupported model setting, got {error}");
        };
        assert_eq!(&actual, expected);
    }
}

/// The committed outcome of handling one canonical input submission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubmitInputHandlingOutcome {
    /// First handling or equal replay returns the complete recorded result.
    Recorded(SubmitInputResult),
    /// The identifier already names another kind or structural payload.
    ConflictingReuse {
        /// The user-global identifier whose earlier meaning is retained.
        command_id: DurableCommandId,
    },
}

/// A durable shape that cannot reconstruct one complete input handling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubmitInputCorruption {
    /// One required row or field is absent.
    Missing(&'static str),
    /// A closed discriminator or representation version is unsupported.
    Unsupported {
        /// The record field that could not be decoded.
        field: &'static str,
        /// The durable spelling that was observed.
        value: String,
    },
    /// Typed records or variant-specific fields disagree.
    Inconsistent(&'static str),
    /// A stored positive ordinal cannot construct the domain value.
    InvalidOrdinal {
        /// The ordinal-bearing field.
        field: &'static str,
        /// Why its numeric representation is invalid.
        reason: PositiveOrdinalMappingError,
    },
    /// Exact stored text cannot construct baseline user content.
    InvalidContent {
        /// The content-bearing field.
        field: &'static str,
        /// Why the exact stored text is outside the baseline.
        failure: NonEmptyUnicodeTextFailure,
    },
    /// The current session projection required for first handling is invalid.
    CurrentSession(SessionCorruption),
    /// Checked stored values fail domain-owned receipt correlation.
    Domain(SubmitInputReconstitutionFailure),
    /// Complete scheduling facts fail domain-owned aggregate reconstruction.
    Scheduling(AcceptedInputSchedulingReconstitutionFailure),
}

impl fmt::Display for SubmitInputCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing durable SubmitInput {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported SubmitInput {field}: {value}")
            }
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent SubmitInput {relationship}")
            }
            Self::InvalidOrdinal { field, reason } => {
                write!(formatter, "invalid SubmitInput {field}: {reason}")
            }
            Self::InvalidContent { field, failure } => {
                write!(formatter, "invalid SubmitInput {field}: {failure:?}")
            }
            Self::CurrentSession(error) => {
                write!(formatter, "SubmitInput current Session is invalid: {error}")
            }
            Self::Domain(failure) => {
                write!(
                    formatter,
                    "SubmitInput domain reconstitution failed: {failure:?}"
                )
            }
            Self::Scheduling(failure) => {
                write!(
                    formatter,
                    "SubmitInput scheduling reconstitution failed: {failure:?}"
                )
            }
        }
    }
}

impl Error for SubmitInputCorruption {}

/// A database failure, wrong purpose-specific load, or integrity failure.
#[derive(Debug)]
pub enum SubmitInputRepositoryError {
    /// PostgreSQL failed before any commit could have succeeded.
    Database(sqlx::Error),
    /// PostgreSQL obscured whether the requested commit succeeded.
    CommitAmbiguous(sqlx::Error),
    /// A purpose-specific load named a valid command of another admitted kind.
    DifferentCommandKind {
        /// The user-global identifier that names another kind.
        command_id: DurableCommandId,
    },
    /// A generated accepted-input candidate reused the active turn's origin.
    AcceptedInputIdentityCollision {
        /// The unclaimed durable command.
        command_id: DurableCommandId,
        /// The authoritative active turn.
        active_turn: TurnId,
        /// The colliding accepted-input candidate and active origin.
        accepted_input: AcceptedInputId,
    },
    /// A caller-owned explicit setting is unsupported by the selected model.
    UnsupportedModelSetting(UnsupportedModelSetting),
    /// Durable records cannot reconstruct the requested domain value.
    Corruption(SubmitInputCorruption),
    /// The active turn's model-execution aggregate could not apply or persist
    /// the correlated stop transition.
    ModelExecution(Box<ModelCallRepositoryError>),
}

impl fmt::Display for SubmitInputRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "SubmitInput database failure: {error}"),
            Self::CommitAmbiguous(error) => {
                write!(
                    formatter,
                    "SubmitInput commit outcome is ambiguous: {error}"
                )
            }
            Self::DifferentCommandKind { command_id } => {
                write!(
                    formatter,
                    "durable command {command_id:?} does not name SubmitInput"
                )
            }
            Self::AcceptedInputIdentityCollision {
                command_id,
                active_turn,
                accepted_input,
            } => write!(
                formatter,
                "SubmitInput command {command_id:?} proposed accepted input {accepted_input:?}, which is already the origin of active turn {active_turn:?}"
            ),
            Self::UnsupportedModelSetting(error) => error.fmt(formatter),
            Self::Corruption(error) => error.fmt(formatter),
            Self::ModelExecution(error) => {
                write!(formatter, "SubmitInput model execution failed: {error}")
            }
        }
    }
}

impl Error for SubmitInputRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::DifferentCommandKind { .. } | Self::AcceptedInputIdentityCollision { .. } => None,
            Self::UnsupportedModelSetting(error) => Some(error),
            Self::Corruption(error) => Some(error),
            Self::ModelExecution(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for SubmitInputRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<SubmitInputCorruption> for SubmitInputRepositoryError {
    fn from(error: SubmitInputCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl From<ModelCallRepositoryError> for SubmitInputRepositoryError {
    fn from(error: ModelCallRepositoryError) -> Self {
        Self::ModelExecution(Box::new(error))
    }
}

impl SubmitInputRepositoryError {
    fn from_commit_failure(error: sqlx::Error) -> Self {
        if crate::commit_failure_is_ambiguous(&error) {
            Self::CommitAmbiguous(error)
        } else {
            Self::Database(error)
        }
    }
}

enum TransactionDecision {
    Commit(SubmitInputHandlingOutcome),
    Rollback(SubmitInputHandlingOutcome),
}

struct PreparedAgainstLockedState {
    prepared: PreparedSubmitInput,
    scheduling: Option<AcceptedInputSchedulingProjection>,
    settles_closure: bool,
}

/// PostgreSQL implementation of atomic durable input acceptance.
#[derive(Clone, Debug)]
pub struct SubmitInputRepository {
    pool: PgPool,
    model_capabilities: Option<ModelCapabilityCatalog>,
    attachment_maximum_bytes: Option<u64>,
}

impl SubmitInputRepository {
    /// Uses the supplied pool for atomic handling and fail-closed loads.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            model_capabilities: None,
            attachment_maximum_bytes: None,
        }
    }

    /// Uses the supplied pool and deployment capability catalog for
    /// settings-aware input preparation.
    pub fn with_model_capabilities(
        pool: PgPool,
        model_capabilities: ModelCapabilityCatalog,
    ) -> Self {
        Self {
            pool,
            model_capabilities: Some(model_capabilities),
            attachment_maximum_bytes: None,
        }
    }

    /// Installs the deployment ceiling used for claim-first attachment
    /// catalog admission.
    #[must_use]
    pub const fn with_attachment_maximum_bytes(mut self, maximum_bytes: u64) -> Self {
        self.attachment_maximum_bytes = Some(maximum_bytes);
        self
    }

    /// Handles an unseen command or resolves its immutable recorded meaning.
    ///
    /// Registry inspection or claim is always first. An unseen command then
    /// locks the session and its current-defaults pointer before reading
    /// state, serializes position assignment on the session row, and commits
    /// the typed terminal result with all applied effects.
    pub async fn handle_with_candidates<NextTurn, NextToolCancellation>(
        &self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        cancellation_identities: CancelledModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
        next_tool_cancellation: NextToolCancellation,
    ) -> Result<SubmitInputHandlingOutcome, SubmitInputRepositoryError>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        NextToolCancellation: FnMut(
                &[signalbox_domain::ToolRequestId],
            ) -> (
                Vec<signalbox_domain::SemanticTranscriptEntryId>,
                signalbox_domain::ContextFrontierId,
            ) + Send,
    {
        self.handle_with_candidates_alias_resolver(
            command,
            accepted_input,
            turn,
            cancellation_identities,
            next_reclassified_turn,
            next_tool_cancellation,
            |_| None,
        )
        .await
    }

    /// Handles one command with deployment model-alias resolution.
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_with_candidates_alias_resolver<NextTurn, NextToolCancellation>(
        &self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        cancellation_identities: CancelledModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
        next_tool_cancellation: NextToolCancellation,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    ) -> Result<SubmitInputHandlingOutcome, SubmitInputRepositoryError>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        NextToolCancellation: FnMut(
                &[signalbox_domain::ToolRequestId],
            ) -> (
                Vec<signalbox_domain::SemanticTranscriptEntryId>,
                signalbox_domain::ContextFrontierId,
            ) + Send,
    {
        let principal = CommandPrincipal::for_actor(command.actor());
        let unreachable_closure_decision = command.command_id();
        let unreachable_closure_attempt =
            TurnAttemptId::from_uuid(command.command_id().into_uuid());
        self.handle_with_candidates_alias_resolver_as(
            command,
            principal,
            ParentTerminationKind::Cancelled,
            accepted_input,
            turn,
            cancellation_identities,
            next_reclassified_turn,
            next_tool_cancellation,
            || unreachable_closure_decision,
            || unreachable_closure_attempt,
            select_definition,
        )
        .await
    }

    /// Handles one command with an authenticated envelope principal and
    /// deployment model-alias resolution.
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_with_candidates_alias_resolver_as<
        NextTurn,
        NextToolCancellation,
        NextClosureDecision,
        NextClosureAttempt,
    >(
        &self,
        command: SubmitInput,
        principal: CommandPrincipal,
        cascade_root_kind: ParentTerminationKind,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        cancellation_identities: CancelledModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
        next_tool_cancellation: NextToolCancellation,
        next_closure_decision: NextClosureDecision,
        next_closure_attempt: NextClosureAttempt,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    ) -> Result<SubmitInputHandlingOutcome, SubmitInputRepositoryError>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        NextToolCancellation: FnMut(
                &[signalbox_domain::ToolRequestId],
            ) -> (
                Vec<signalbox_domain::SemanticTranscriptEntryId>,
                signalbox_domain::ContextFrontierId,
            ) + Send,
        NextClosureDecision: FnMut() -> DurableCommandId + Send,
        NextClosureAttempt: FnMut() -> TurnAttemptId + Send,
    {
        let mut transaction = self.pool.begin().await?;
        let decision = Box::pin(handle_in_transaction(
            &mut transaction,
            command,
            principal,
            cascade_root_kind,
            accepted_input,
            turn,
            cancellation_identities,
            next_reclassified_turn,
            next_tool_cancellation,
            next_closure_decision,
            next_closure_attempt,
            select_definition,
            self.model_capabilities.as_ref(),
            self.attachment_maximum_bytes,
        ))
        .await;

        match decision {
            Ok(TransactionDecision::Commit(outcome)) => {
                transaction
                    .commit()
                    .await
                    .map_err(SubmitInputRepositoryError::from_commit_failure)?;
                Ok(outcome)
            }
            Ok(TransactionDecision::Rollback(outcome)) => {
                transaction.rollback().await?;
                Ok(outcome)
            }
            Err(error) => {
                if let Err(rollback_error) = transaction.rollback().await {
                    return Err(rollback_error.into());
                }
                Err(error)
            }
        }
    }

    /// Loads one complete handling, or `None` only for an unseen identifier.
    pub async fn load(
        &self,
        command_id: DurableCommandId,
    ) -> Result<Option<ReconstitutedSubmitInput>, SubmitInputRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        match inspect_registry(&mut connection, command_id).await? {
            None => Ok(None),
            Some(CommandKind::SubmitInput) => {
                load_from_connection(&mut connection, command_id).await
            }
            Some(
                CommandKind::CreateSession
                | CommandKind::CreateSessionFromImportedFrontier
                | CommandKind::ReplaceSessionDefaults
                | CommandKind::ReplaceSessionMetadata
                | CommandKind::DecideToolRequest
                | CommandKind::OverrideDeniedToolRequest
                | CommandKind::ReviewWorkflow
                | CommandKind::ReviewOrchestration
                | CommandKind::CompactSession
                | CommandKind::Goal
                | CommandKind::UpdateSessionPlacement
                | CommandKind::RegisterWorkspace
                | CommandKind::MintGitRemote
                | CommandKind::WithdrawGitRemote
                | CommandKind::SessionLifecycle,
            ) => Err(Self::wrong_kind(command_id)),
        }
    }

    fn wrong_kind(command_id: DurableCommandId) -> SubmitInputRepositoryError {
        SubmitInputRepositoryError::DifferentCommandKind { command_id }
    }
}

impl SubmitInputTransaction for SubmitInputRepository {
    type Error = SubmitInputRepositoryError;

    async fn handle<NextTurn, NextToolCancellation, NextClosureDecision, NextClosureAttempt>(
        &mut self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        cancellation_identities: CancelledModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
        next_tool_cancellation: NextToolCancellation,
        _next_closure_decision: NextClosureDecision,
        _next_closure_attempt: NextClosureAttempt,
    ) -> Result<SubmitInputOutcome, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        NextToolCancellation: FnMut(
                &[signalbox_domain::ToolRequestId],
            ) -> (
                Vec<signalbox_domain::SemanticTranscriptEntryId>,
                signalbox_domain::ContextFrontierId,
            ) + Send,
        NextClosureDecision: FnMut() -> DurableCommandId + Send,
        NextClosureAttempt: FnMut() -> TurnAttemptId + Send,
    {
        let outcome = SubmitInputRepository::handle_with_candidates(
            self,
            command,
            accepted_input,
            turn,
            cancellation_identities,
            next_reclassified_turn,
            next_tool_cancellation,
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

#[allow(clippy::too_many_arguments)]
async fn handle_in_transaction<
    NextTurn,
    NextToolCancellation,
    NextClosureDecision,
    NextClosureAttempt,
>(
    connection: &mut PgConnection,
    command: SubmitInput,
    principal: CommandPrincipal,
    cascade_root_kind: ParentTerminationKind,
    accepted_input: AcceptedInputId,
    turn: Option<TurnId>,
    cancellation_identities: CancelledModelCallTurnIdentities,
    mut next_reclassified_turn: NextTurn,
    mut next_tool_cancellation: NextToolCancellation,
    mut next_closure_decision: NextClosureDecision,
    mut next_closure_attempt: NextClosureAttempt,
    select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    model_capabilities: Option<&ModelCapabilityCatalog>,
    attachment_maximum_bytes: Option<u64>,
) -> Result<TransactionDecision, SubmitInputRepositoryError>
where
    NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    NextToolCancellation: FnMut(
            &[signalbox_domain::ToolRequestId],
        ) -> (
            Vec<signalbox_domain::SemanticTranscriptEntryId>,
            signalbox_domain::ContextFrontierId,
        ) + Send,
    NextClosureDecision: FnMut() -> DurableCommandId + Send,
    NextClosureAttempt: FnMut() -> TurnAttemptId + Send,
{
    let command_id = command.command_id();
    match inspect_registry(connection, command_id).await? {
        Some(CommandKind::SubmitInput) => {
            return Ok(TransactionDecision::Rollback(existing_outcome(
                &command,
                require_recorded(connection, command_id).await?,
            )));
        }
        Some(
            CommandKind::CreateSession
            | CommandKind::CreateSessionFromImportedFrontier
            | CommandKind::ReplaceSessionDefaults
            | CommandKind::ReplaceSessionMetadata
            | CommandKind::DecideToolRequest
            | CommandKind::OverrideDeniedToolRequest
            | CommandKind::ReviewWorkflow
            | CommandKind::ReviewOrchestration
            | CommandKind::CompactSession
            | CommandKind::Goal
            | CommandKind::UpdateSessionPlacement
            | CommandKind::RegisterWorkspace
            | CommandKind::MintGitRemote
            | CommandKind::WithdrawGitRemote
            | CommandKind::SessionLifecycle,
        ) => {
            return Ok(TransactionDecision::Rollback(
                SubmitInputHandlingOutcome::ConflictingReuse { command_id },
            ));
        }
        None => {}
    }

    let issuer = crate::command_registry::issuer_columns(principal);
    let claimed = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at,
             issuer_kind, issuer_module)
         VALUES ($1, $2, $3, transaction_timestamp(), $4, $5)
         ON CONFLICT DO NOTHING",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .bind(SUBMIT_INPUT_KIND)
    .bind(STORAGE_VERSION)
    .bind(issuer.0)
    .bind(issuer.1)
    .execute(&mut *connection)
    .await?
    .rows_affected()
        == 1;

    if !claimed {
        return match inspect_registry(connection, command_id).await? {
            Some(CommandKind::SubmitInput) => Ok(TransactionDecision::Rollback(existing_outcome(
                &command,
                require_recorded(connection, command_id).await?,
            ))),
            Some(
                CommandKind::CreateSession
                | CommandKind::CreateSessionFromImportedFrontier
                | CommandKind::ReplaceSessionDefaults
                | CommandKind::ReplaceSessionMetadata
                | CommandKind::DecideToolRequest
                | CommandKind::OverrideDeniedToolRequest
                | CommandKind::ReviewWorkflow
                | CommandKind::ReviewOrchestration
                | CommandKind::CompactSession
                | CommandKind::Goal
                | CommandKind::UpdateSessionPlacement
                | CommandKind::RegisterWorkspace
                | CommandKind::MintGitRemote
                | CommandKind::WithdrawGitRemote
                | CommandKind::SessionLifecycle,
            ) => Ok(TransactionDecision::Rollback(
                SubmitInputHandlingOutcome::ConflictingReuse { command_id },
            )),
            None => Err(SubmitInputCorruption::Inconsistent("winner claim disappeared").into()),
        };
    }

    if let Some(prepared) =
        prepare_attachment_authority_rejection(connection, &command, attachment_maximum_bytes)
            .await?
    {
        let recorded = prepared.result().clone();
        insert_prepared_command(connection, &prepared).await?;
        settle_injection_receipt(connection, &prepared).await?;
        return Ok(TransactionDecision::Commit(
            SubmitInputHandlingOutcome::Recorded(recorded),
        ));
    }

    let frontier_command = command.clone();
    if attachment_maximum_bytes.is_some() {
        sqlx::query("SAVEPOINT submit_input_attachment_frontier")
            .execute(&mut *connection)
            .await?;
    }

    if matches!(
        command.delivery(),
        DeliveryRequest::Interrupt {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        }
    ) {
        sqlx::query(crate::lock_inventory::DELEGATION_TERMINATION_SESSION_FRONTIER)
            .bind(session_id_to_uuid(command.session()))
            .bind(parent_termination_kind_to_str(cascade_root_kind))
            .execute(&mut *connection)
            .await?;
    }

    lock_delegated_child_endpoint_sessions(connection, command.session()).await?;
    let PreparedAgainstLockedState {
        prepared,
        scheduling,
        settles_closure,
    } = prepare_against_locked_state(
        connection,
        command,
        principal,
        accepted_input,
        turn,
        &mut next_closure_decision,
        &mut next_closure_attempt,
        select_definition,
        model_capabilities,
    )
    .await?;
    if settles_closure && matches!(prepared.result(), SubmitInputResult::Rejected(_)) {
        return Ok(TransactionDecision::Rollback(
            SubmitInputHandlingOutcome::Recorded(prepared.result().clone()),
        ));
    }
    let prior_queued_inputs = scheduling
        .as_ref()
        .map(|scheduling| {
            scheduling
                .turns()
                .filter(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Queued)
                .map(|turn| turn.accepted_input().id())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let recorded = prepared.result().clone();
    let interrupt = match prepared.result() {
        SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) => {
            origin.applied_interrupt().copied()
        }
        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
        | SubmitInputResult::Rejected(_) => None,
    };
    insert_prepared_command(connection, &prepared).await?;
    sqlx::query("SELECT materialize_session_delegation_termination_cascade($1, $2)")
        .bind(durable_command_id_to_uuid(command_id))
        .bind(parent_termination_kind_to_str(cascade_root_kind))
        .execute(&mut *connection)
        .await?;
    let interrupt_outcome = if let Some(interrupt) = interrupt {
        let runner_recovery_source_snapshot = load_runner_recovery_source_snapshot(
            connection,
            interrupt.session(),
            interrupt.proof().predecessor(),
        )
        .await
        .map_err(map_tool_loop_error)?;
        let active_tool_batch = if runner_recovery_source_snapshot.is_none() {
            load_active_batch_from_connection(
                connection,
                interrupt.session(),
                interrupt.proof().predecessor(),
            )
            .await
            .map_err(map_tool_loop_error)?
        } else {
            None
        };
        let executing_tool_batch = active_tool_batch.clone().filter(|batch| {
            matches!(
                batch.phase(),
                signalbox_domain::ToolBatchPhase::Executing { .. }
                    | signalbox_domain::ToolBatchPhase::AwaitingChild { .. }
            )
        });
        if let Some(mut batch) = executing_tool_batch {
            if let Some(current) =
                batch
                    .requests()
                    .iter()
                    .find_map(|request| match batch.attempt(request.id()) {
                        Some(signalbox_domain::ReconstitutedToolAttempt::Current(current)) => {
                            Some(current.clone())
                        }
                        Some(signalbox_domain::ReconstitutedToolAttempt::Ended(_)) | None => None,
                    })
            {
                if current.state() == signalbox_domain::CurrentToolAttemptState::InFlight {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "in-flight tool attempt escaped the dispatch gate",
                    )
                    .into());
                }
                let ended = match current.classify_crash_loss() {
                    signalbox_domain::ToolAttemptCrashOutcome::KnownFailed(ended) => ended,
                    signalbox_domain::ToolAttemptCrashOutcome::Ambiguous(_) => {
                        return Err(SubmitInputCorruption::Inconsistent(
                            "prepared tool attempt classified ambiguous",
                        )
                        .into());
                    }
                };
                persist_ended_attempt(connection, &ended)
                    .await
                    .map_err(map_tool_loop_error)?;
                batch = load_active_batch_from_connection(
                    connection,
                    interrupt.session(),
                    batch.turn(),
                )
                .await
                .map_err(map_tool_loop_error)?
                .ok_or(SubmitInputCorruption::Missing("closed tool batch"))?;
            }
            let request_ids = batch
                .requests()
                .iter()
                .map(signalbox_domain::ToolRequest::id)
                .collect::<Vec<_>>();
            let (result_entries, result_frontier) = next_tool_cancellation(&request_ids);
            let child_wait =
                batch
                    .requests()
                    .iter()
                    .find_map(|request| match batch.attempt(request.id()) {
                        Some(signalbox_domain::ReconstitutedToolAttempt::Ended(attempt)) => {
                            match attempt.end() {
                                signalbox_domain::ToolAttemptEnd::AwaitingChild {
                                    spawning_request,
                                    child,
                                } => Some((request.id(), *spawning_request, *child)),
                                _ => None,
                            }
                        }
                        _ => None,
                    });
            let projection = match child_wait {
                Some((awaiting_request, spawning_request, child)) => batch
                    .prepare_delegation_cancellation_projection(
                        result_entries,
                        result_frontier,
                        load_optional_foreground_delegation_outcome(
                            connection,
                            interrupt.session(),
                            awaiting_request,
                            spawning_request,
                            child,
                        )
                        .await
                        .map_err(map_tool_loop_error)?,
                    ),
                None => batch.prepare_cancellation_projection(result_entries, result_frontier),
            }
            .map_err(|_| {
                SubmitInputCorruption::Inconsistent(
                    "executing tool batch cannot project cancellation",
                )
            })?;
            // The scheduling projection is built from `queued_input_origin`, so
            // it carries an active turn only for an accepted-input origin. A
            // delegation-origin active turn is absent from it and must be
            // reconstituted through the delegated live-turn loader, exactly as
            // the recovery arm below decides.
            let projected_active_turn = scheduling
                .as_ref()
                .and_then(AcceptedInputSchedulingProjection::active_turn_execution);
            if let Some(active_turn) = projected_active_turn {
                let Some(scheduling) = scheduling else {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "tool interrupt scheduling projection",
                    )
                    .into());
                };
                let identities = attach_interrupt_reclassification_candidates_for_active(
                    cancellation_identities,
                    &active_turn,
                    &mut next_reclassified_turn,
                )
                .map_err(|_| {
                    SubmitInputCorruption::Inconsistent(
                        "tool interrupt reclassification candidates",
                    )
                })?;
                Some(ModelCallInterruptOutcome::Cancelled(
                    scheduling
                        .apply_interrupt_to_tool_batch(batch, projection, interrupt, identities)
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "applied interrupt cannot close executing tool batch",
                            )
                        })?,
                ))
            } else {
                let execution =
                    require_live_execution_for_restart(connection, interrupt.session()).await?;
                let identities = attach_interrupt_reclassification_candidates(
                    cancellation_identities,
                    &execution,
                    &mut next_reclassified_turn,
                )?;
                Some(ModelCallInterruptOutcome::Cancelled(
                    execution
                        .apply_interrupt_to_tool_batch(interrupt, projection, identities)
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "applied interrupt cannot close executing tool batch",
                            )
                        })?,
                ))
            }
        } else {
            let recovery_operation = scheduling
                .as_ref()
                .and_then(AcceptedInputSchedulingProjection::active_turn_execution)
                .and_then(|active| match active.phase() {
                    signalbox_domain::ActiveTurnPhase::AwaitingRecoveryDecision {
                        ambiguous_operations,
                        applied_interrupt: None,
                    } if ambiguous_operations.operation_count() == 1 => {
                        ambiguous_operations.iter().next()
                    }
                    signalbox_domain::ActiveTurnPhase::Running { .. }
                    | signalbox_domain::ActiveTurnPhase::AwaitingApproval { .. }
                    | signalbox_domain::ActiveTurnPhase::AwaitingChild { .. }
                    | signalbox_domain::ActiveTurnPhase::AwaitingRecoveryDecision { .. }
                    | signalbox_domain::ActiveTurnPhase::AwaitingRunnerRecovery { .. } => None,
                });
            if let Some(IssuedOperationRef::ToolAttempt(recovery_attempt)) = recovery_operation {
                let scheduling = scheduling.ok_or(SubmitInputCorruption::Inconsistent(
                    "applied interrupt lacks active scheduling state",
                ))?;
                let batch = load_recovery_batch_by_attempt(
                    connection,
                    interrupt.session(),
                    interrupt.proof().predecessor(),
                    recovery_attempt,
                )
                .await
                .map_err(map_tool_loop_error)?;
                let wait = batch
                    .awaiting_recovery()
                    .ok_or(SubmitInputCorruption::Inconsistent(
                        "tool recovery wait evidence",
                    ))?;
                let tool_attempt = batch
                    .requests()
                    .iter()
                    .find_map(|request| match batch.attempt(request.id()) {
                        Some(signalbox_domain::ReconstitutedToolAttempt::Ended(attempt))
                            if attempt.attempt() == recovery_attempt =>
                        {
                            Some(attempt.clone())
                        }
                        Some(signalbox_domain::ReconstitutedToolAttempt::Current(_))
                        | Some(signalbox_domain::ReconstitutedToolAttempt::Ended(_))
                        | None => None,
                    })
                    .ok_or(SubmitInputCorruption::Inconsistent(
                        "ambiguous tool attempt evidence",
                    ))?;
                let request_ids = batch
                    .requests()
                    .iter()
                    .map(signalbox_domain::ToolRequest::id)
                    .collect::<Vec<_>>();
                let (result_entries, result_frontier) = next_tool_cancellation(&request_ids);
                let result_projection = batch
                    .prepare_reconciliation_projection(result_entries, result_frontier)
                    .map_err(|_| {
                        SubmitInputCorruption::Inconsistent(
                            "tool recovery batch cannot materialize terminal results",
                        )
                    })?;
                let active_turn = scheduling.active_turn_execution().ok_or(
                    SubmitInputCorruption::Inconsistent(
                        "applied interrupt lacks active turn execution",
                    ),
                )?;
                let identities = attach_recovery_interrupt_reclassification_candidates(
                    signalbox_domain::AmbiguousModelCallTurnIdentities::new(result_frontier),
                    &active_turn,
                    &mut next_reclassified_turn,
                )?;
                Some(ModelCallInterruptOutcome::ToolReconciliationRequired(
                    scheduling
                        .apply_interrupt_to_tool_recovery(
                            wait,
                            tool_attempt,
                            result_projection,
                            interrupt,
                            identities,
                        )
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "applied interrupt does not match tool recovery wait",
                            )
                        })?,
                ))
            } else if matches!(recovery_operation, Some(IssuedOperationRef::ModelCall(_))) {
                let scheduling = scheduling.ok_or(SubmitInputCorruption::Inconsistent(
                    "applied interrupt lacks active scheduling state",
                ))?;
                let active_turn = scheduling.active_turn_execution().ok_or(
                    SubmitInputCorruption::Inconsistent(
                        "applied interrupt lacks active turn execution",
                    ),
                )?;
                let identities = attach_recovery_interrupt_reclassification_candidates(
                    cancellation_identities.into_ambiguous(),
                    &active_turn,
                    &mut next_reclassified_turn,
                )?;
                Some(ModelCallInterruptOutcome::ReconciliationRequired(
                    scheduling
                        .apply_interrupt_to_model_call_recovery(interrupt, identities)
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "applied interrupt does not match model-call recovery wait",
                            )
                        })?,
                ))
            } else if scheduling
                .as_ref()
                .and_then(AcceptedInputSchedulingProjection::active_turn_execution)
                .is_some_and(|active| {
                    matches!(
                        active.phase(),
                        signalbox_domain::ActiveTurnPhase::AwaitingRunnerRecovery { .. }
                    )
                })
            {
                let scheduling = scheduling.ok_or(SubmitInputCorruption::Inconsistent(
                    "applied interrupt lacks runner recovery scheduling state",
                ))?;
                let active_turn = scheduling.active_turn_execution().ok_or(
                    SubmitInputCorruption::Inconsistent(
                        "applied interrupt lacks runner recovery active turn",
                    ),
                )?;
                let source_snapshot = runner_recovery_source_snapshot.clone().ok_or(
                    SubmitInputCorruption::Missing("runner recovery source frontier"),
                )?;
                let source_frontier = source_snapshot.frontier().snapshot();
                let command = interrupt.proof().command();
                let yielded_attempt = load_runner_recovery_yielded_attempt(
                    connection,
                    interrupt.session(),
                    interrupt.proof().predecessor(),
                )
                .await?;
                let interrupted_tool_attempt = match active_turn.phase() {
                    signalbox_domain::ActiveTurnPhase::AwaitingRunnerRecovery {
                        optional_tool_attempt,
                        ..
                    } => *optional_tool_attempt,
                    _ => None,
                };
                let outcome = if let Some(recovery_attempt) = interrupted_tool_attempt {
                    let preserves_ambiguity = terminalize_retryable_runner_recovery_attempt(
                        connection,
                        interrupt.session(),
                        interrupt.proof().predecessor(),
                        recovery_attempt,
                    )
                    .await?;
                    let batch = if preserves_ambiguity {
                        load_recovery_batch_by_attempt(
                            connection,
                            interrupt.session(),
                            interrupt.proof().predecessor(),
                            recovery_attempt,
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                    } else {
                        load_runner_recovery_cancellation_batch(
                            connection,
                            interrupt.session(),
                            interrupt.proof().predecessor(),
                            yielded_attempt,
                            Some(recovery_attempt),
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                        .ok_or(SubmitInputCorruption::Missing(
                            "runner retryable tool recovery batch",
                        ))?
                    };
                    let request_ids = batch
                        .requests()
                        .iter()
                        .map(signalbox_domain::ToolRequest::id)
                        .collect::<Vec<_>>();
                    let (result_entries, result_frontier) = next_tool_cancellation(&request_ids);
                    if !preserves_ambiguity {
                        let result_projection = batch
                            .prepare_cancellation_projection(result_entries, result_frontier)
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "runner retryable recovery batch cannot close",
                                )
                            })?;
                        let identities = attach_interrupt_reclassification_candidates_for_active(
                            cancellation_identities,
                            &active_turn,
                            &mut next_reclassified_turn,
                        )
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "runner retryable recovery interrupt candidates",
                            )
                        })?;
                        ModelCallInterruptOutcome::Cancelled(
                            scheduling
                                .apply_interrupt_to_retryable_runner_tool_recovery(
                                    batch,
                                    result_projection,
                                    interrupt,
                                    identities,
                                )
                                .map_err(|_| {
                                    SubmitInputCorruption::Inconsistent(
                                        "applied interrupt does not match retryable runner wait",
                                    )
                                })?,
                        )
                    } else {
                        let wait = batch.awaiting_recovery().ok_or(
                            SubmitInputCorruption::Inconsistent(
                                "runner tool recovery wait evidence",
                            ),
                        )?;
                        let tool_attempt = batch
                            .requests()
                            .iter()
                            .find_map(|request| match batch.attempt(request.id()) {
                                Some(signalbox_domain::ReconstitutedToolAttempt::Ended(
                                    attempt,
                                )) if attempt.attempt() == recovery_attempt => {
                                    Some(attempt.clone())
                                }
                                _ => None,
                            })
                            .ok_or(SubmitInputCorruption::Inconsistent(
                                "runner ambiguous tool attempt evidence",
                            ))?;
                        let result_projection = batch
                            .prepare_reconciliation_projection(result_entries, result_frontier)
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "runner recovery batch cannot preserve ambiguity",
                                )
                            })?;
                        let identities = attach_recovery_interrupt_reclassification_candidates(
                            signalbox_domain::AmbiguousModelCallTurnIdentities::new(
                                result_frontier,
                            ),
                            &active_turn,
                            &mut next_reclassified_turn,
                        )?;
                        ModelCallInterruptOutcome::ToolReconciliationRequired(
                        scheduling
                            .apply_interrupt_to_runner_tool_recovery(
                                wait,
                                tool_attempt,
                                yielded_attempt,
                                result_projection,
                                interrupt,
                                identities,
                            )
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "applied interrupt does not match runner tool recovery wait",
                                )
                            })?,
                    )
                    }
                } else {
                    let result_projection = match load_runner_recovery_batch_without_attempt(
                        connection,
                        interrupt.session(),
                        interrupt.proof().predecessor(),
                        yielded_attempt,
                    )
                    .await
                    .map_err(map_tool_loop_error)?
                    {
                        Some(batch) => {
                            let request_ids = batch
                                .requests()
                                .iter()
                                .map(signalbox_domain::ToolRequest::id)
                                .collect::<Vec<_>>();
                            let (result_entries, result_frontier) =
                                next_tool_cancellation(&request_ids);
                            Some(
                                batch
                                    .prepare_cancellation_projection(
                                        result_entries,
                                        result_frontier,
                                    )
                                    .map_err(|_| {
                                        SubmitInputCorruption::Inconsistent(
                                            "runner recovery batch cannot close",
                                        )
                                    })?,
                            )
                        }
                        None => None,
                    };
                    let identities = attach_interrupt_reclassification_candidates_for_active(
                        cancellation_identities,
                        &active_turn,
                        &mut next_reclassified_turn,
                    )
                    .map_err(|_| {
                        SubmitInputCorruption::Inconsistent(
                            "runner recovery interrupt reclassification candidates",
                        )
                    })?;
                    ModelCallInterruptOutcome::Cancelled(
                        scheduling
                            .apply_interrupt_to_runner_recovery(
                                source_snapshot,
                                result_projection,
                                interrupt,
                                identities,
                            )
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "applied interrupt does not match runner recovery wait",
                                )
                            })?,
                    )
                };
                persist_runner_recovery_interrupt_effect(
                    connection,
                    command,
                    interrupt.session(),
                    interrupt.proof().predecessor(),
                    source_frontier,
                )
                .await?;
                Some(outcome)
            } else if let Some((active_turn, starting_snapshot)) =
                load_delegated_runner_recovery_for_interrupt(connection, interrupt.session())
                    .await?
            {
                let source_snapshot = runner_recovery_source_snapshot.ok_or(
                    SubmitInputCorruption::Missing("delegated runner recovery source frontier"),
                )?;
                let source_frontier = source_snapshot.frontier().snapshot();
                let command = interrupt.proof().command();
                let yielded_attempt = load_runner_recovery_yielded_attempt(
                    connection,
                    interrupt.session(),
                    interrupt.proof().predecessor(),
                )
                .await?;
                let interrupted_tool_attempt = match active_turn.phase() {
                    signalbox_domain::ActiveTurnPhase::AwaitingRunnerRecovery {
                        optional_tool_attempt,
                        ..
                    } => *optional_tool_attempt,
                    _ => None,
                };
                let outcome = if let Some(recovery_attempt) = interrupted_tool_attempt {
                    let preserves_ambiguity = terminalize_retryable_runner_recovery_attempt(
                        connection,
                        interrupt.session(),
                        interrupt.proof().predecessor(),
                        recovery_attempt,
                    )
                    .await?;
                    let batch = if preserves_ambiguity {
                        load_recovery_batch_by_attempt(
                            connection,
                            interrupt.session(),
                            interrupt.proof().predecessor(),
                            recovery_attempt,
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                    } else {
                        load_runner_recovery_cancellation_batch(
                            connection,
                            interrupt.session(),
                            interrupt.proof().predecessor(),
                            yielded_attempt,
                            Some(recovery_attempt),
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                        .ok_or(SubmitInputCorruption::Missing(
                            "delegated runner retryable recovery batch",
                        ))?
                    };
                    let request_ids = batch
                        .requests()
                        .iter()
                        .map(signalbox_domain::ToolRequest::id)
                        .collect::<Vec<_>>();
                    let (result_entries, result_frontier) = next_tool_cancellation(&request_ids);
                    if !preserves_ambiguity {
                        let result_projection = batch
                            .prepare_cancellation_projection(result_entries, result_frontier)
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "delegated retryable runner batch cannot close",
                                )
                            })?;
                        let identities =
                            attach_interrupt_reclassification_candidates_for_activated(
                                cancellation_identities,
                                &active_turn,
                                &mut next_reclassified_turn,
                            )
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "delegated retryable runner interrupt candidates",
                                )
                            })?;
                        ModelCallInterruptOutcome::Cancelled(
                            active_turn
                                .apply_interrupt_to_retryable_runner_tool_recovery(
                                    starting_snapshot,
                                    batch,
                                    result_projection,
                                    interrupt,
                                    identities,
                                )
                                .map_err(|_| {
                                    SubmitInputCorruption::Inconsistent(
                                        "delegated interrupt does not match retryable runner wait",
                                    )
                                })?,
                        )
                    } else {
                        let wait = batch.awaiting_recovery().ok_or(
                            SubmitInputCorruption::Inconsistent(
                                "delegated runner tool recovery wait",
                            ),
                        )?;
                        let tool_attempt = batch
                            .requests()
                            .iter()
                            .find_map(|request| match batch.attempt(request.id()) {
                                Some(signalbox_domain::ReconstitutedToolAttempt::Ended(
                                    attempt,
                                )) if attempt.attempt() == recovery_attempt => {
                                    Some(attempt.clone())
                                }
                                _ => None,
                            })
                            .ok_or(SubmitInputCorruption::Inconsistent(
                                "delegated runner ambiguous tool attempt",
                            ))?;
                        let result_projection = batch
                            .prepare_reconciliation_projection(result_entries, result_frontier)
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "delegated runner recovery batch cannot preserve ambiguity",
                                )
                            })?;
                        let identities =
                            attach_recovery_interrupt_reclassification_candidates_for_activated(
                                signalbox_domain::AmbiguousModelCallTurnIdentities::new(
                                    result_frontier,
                                ),
                                &active_turn,
                                &mut next_reclassified_turn,
                            )?;
                        ModelCallInterruptOutcome::ToolReconciliationRequired(
                            active_turn
                                .apply_interrupt_to_runner_tool_recovery(
                                    wait,
                                    tool_attempt,
                                    yielded_attempt,
                                    result_projection,
                                    interrupt,
                                    identities,
                                )
                                .map_err(|_| {
                                    SubmitInputCorruption::Inconsistent(
                                        "delegated interrupt does not match runner tool recovery",
                                    )
                                })?,
                        )
                    }
                } else {
                    let result_projection = match load_runner_recovery_batch_without_attempt(
                        connection,
                        interrupt.session(),
                        interrupt.proof().predecessor(),
                        yielded_attempt,
                    )
                    .await
                    .map_err(map_tool_loop_error)?
                    {
                        Some(batch) => {
                            let request_ids = batch
                                .requests()
                                .iter()
                                .map(signalbox_domain::ToolRequest::id)
                                .collect::<Vec<_>>();
                            let (result_entries, result_frontier) =
                                next_tool_cancellation(&request_ids);
                            Some(
                                batch
                                    .prepare_cancellation_projection(
                                        result_entries,
                                        result_frontier,
                                    )
                                    .map_err(|_| {
                                        SubmitInputCorruption::Inconsistent(
                                            "delegated runner recovery batch cannot close",
                                        )
                                    })?,
                            )
                        }
                        None => None,
                    };
                    let identities = attach_interrupt_reclassification_candidates_for_activated(
                        cancellation_identities,
                        &active_turn,
                        &mut next_reclassified_turn,
                    )
                    .map_err(|_| {
                        SubmitInputCorruption::Inconsistent(
                            "delegated runner recovery interrupt reclassification candidates",
                        )
                    })?;
                    ModelCallInterruptOutcome::Cancelled(
                        active_turn
                            .apply_interrupt_to_runner_recovery(
                                starting_snapshot,
                                source_snapshot,
                                result_projection,
                                interrupt,
                                identities,
                            )
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "applied interrupt does not match delegated runner recovery wait",
                                )
                            })?,
                    )
                };
                persist_runner_recovery_interrupt_effect(
                    connection,
                    command,
                    interrupt.session(),
                    interrupt.proof().predecessor(),
                    source_frontier,
                )
                .await?;
                Some(outcome)
            } else {
                let execution =
                    require_live_execution_for_restart(connection, interrupt.session()).await?;
                let identities = attach_interrupt_reclassification_candidates(
                    cancellation_identities,
                    &execution,
                    &mut next_reclassified_turn,
                )?;
                Some(
                    execution
                        .apply_interrupt(interrupt, identities)
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "applied interrupt does not match active model execution",
                            )
                        })?,
                )
            }
        }
    } else {
        None
    };
    insert_prepared_effects(connection, prepared).await?;
    match interrupt_outcome {
        Some(ModelCallInterruptOutcome::Cancelled(cancelled)) => {
            persist_terminal_outcome(
                connection,
                &ModelCallTerminalOutcome::Cancelled(cancelled),
                None,
            )
            .await?;
        }
        Some(ModelCallInterruptOutcome::CancellationRequested(stopped)) => {
            persist_stop_requested(connection, &stopped).await?;
        }
        Some(ModelCallInterruptOutcome::ReconciliationRequired(reconciliation)) => {
            let session = reconciliation.session();
            let turn = reconciliation.turn();
            persist_terminal_outcome(
                connection,
                &ModelCallTerminalOutcome::ReconciliationRequired(reconciliation),
                None,
            )
            .await?;
            supersede_automatic_reconciliation(connection, session, turn).await?;
        }
        Some(ModelCallInterruptOutcome::ToolReconciliationRequired(reconciliation)) => {
            persist_tool_reconciliation_required(connection, &reconciliation).await?;
            supersede_automatic_reconciliation(
                connection,
                reconciliation.session(),
                reconciliation.turn(),
            )
            .await?;
        }
        None => {}
    }
    if let Some(maximum_bytes) = attachment_maximum_bytes {
        if matches!(recorded, SubmitInputResult::Applied(_))
            && session_has_attachment_parts(connection, frontier_command.session()).await?
            && Box::pin(prospective_attachment_frontier_exceeds_bound(
                connection,
                frontier_command.session(),
                &prior_queued_inputs,
                &recorded,
                maximum_bytes,
            ))
            .await?
        {
            sqlx::query("ROLLBACK TO SAVEPOINT submit_input_attachment_frontier")
                .execute(&mut *connection)
                .await?;
            let rejected = frontier_command.prepare_attachment_byte_budget_exceeded(maximum_bytes);
            let recorded = rejected.result().clone();
            insert_prepared_command(connection, &rejected).await?;
            insert_prepared_effects(connection, rejected).await?;
            sqlx::query("RELEASE SAVEPOINT submit_input_attachment_frontier")
                .execute(&mut *connection)
                .await?;
            return Ok(TransactionDecision::Commit(
                SubmitInputHandlingOutcome::Recorded(recorded),
            ));
        }
        sqlx::query("RELEASE SAVEPOINT submit_input_attachment_frontier")
            .execute(&mut *connection)
            .await?;
    }
    Ok(TransactionDecision::Commit(
        SubmitInputHandlingOutcome::Recorded(recorded),
    ))
}

async fn supersede_automatic_reconciliation(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<(), SubmitInputRepositoryError> {
    sqlx::query(
        "UPDATE automatic_reconciliation_attempt AS attempt
            SET outcome_kind = 'superseded', finished_at = statement_timestamp()
           FROM automatic_reconciliation AS recovery
          WHERE recovery.turn_id = $1
            AND recovery.session_id = $2
            AND recovery.state_kind = 'attempting'
            AND attempt.turn_id = recovery.turn_id
            AND attempt.attempt_ordinal = recovery.attempt_count
            AND attempt.outcome_kind = 'attempting'",
    )
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE automatic_reconciliation
            SET state_kind = 'superseded', exhausted_at = NULL
          WHERE turn_id = $1
            AND session_id = $2
            AND state_kind IN ('scheduled', 'attempting', 'exhausted')",
    )
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .execute(connection)
    .await?;
    Ok(())
}

async fn prospective_attachment_frontier_exceeds_bound(
    connection: &mut PgConnection,
    session: SessionId,
    prior_queued_inputs: &[AcceptedInputId],
    result: &SubmitInputResult,
    maximum_bytes: u64,
) -> Result<bool, SubmitInputRepositoryError> {
    let current = match load_session_from_connection(connection, session).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            return Err(SubmitInputCorruption::Inconsistent(
                "provisionally applied input session disappeared",
            )
            .into());
        }
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(SubmitInputCorruption::CurrentSession(error).into());
        }
    };
    let delegated_parked_frontier =
        load_delegated_parked_attachment_frontier(connection, session).await?;
    let supplemental_semantic_frontiers = delegated_parked_frontier
        .as_ref()
        .map(|frontier| vec![frontier.snapshot.frontier().snapshot()])
        .unwrap_or_default();
    let scheduling = load_scheduling_projection_with_semantic_frontiers(
        connection,
        current,
        &supplemental_semantic_frontiers,
    )
    .await?;
    let live_execution = match require_live_execution_for_restart(connection, session).await {
        Err(ModelCallRepositoryError::Corruption(ModelCallCorruption::Unsupported {
            field: "delegated turn attempt state",
            value,
        })) if value == "stop_requested" => Err(ModelCallRepositoryError::NoLiveExecution),
        // A turn executing a tool batch keeps the `running` phase against the
        // continuation attempt while the call that produced the batch is
        // already terminal. Model-execution reconstitution then sees the
        // turn's retained provider pin with no current call and reports
        // `PinnedTargetUnexpected`, which is exactly the statement that this
        // turn has no live model call to read a frontier from. The scheduling
        // projection records the batch's yielded frontier, so route the state
        // through the same no-live-execution path the parked phases use rather
        // than rolling back an otherwise-applied submission.
        Err(ModelCallRepositoryError::Corruption(ModelCallCorruption::Execution(
            signalbox_domain::ModelCallExecutionReconstitutionFailure::PinnedTargetUnexpected,
        ))) => Err(ModelCallRepositoryError::NoLiveExecution),
        result => result,
    };
    let (base_origins, check_base) = match live_execution {
        Ok(execution) => {
            let mut distinct = BTreeSet::new();
            let complete_entries = execution.frontier_entries().cloned().collect::<Vec<_>>();
            let projection = ContextFrontierProjection::from_complete_entries(&complete_entries)
                .map_err(|_| {
                    SubmitInputCorruption::Inconsistent("prospective attachment context projection")
                })?;
            let entries_by_reference = complete_entries
                .iter()
                .map(|entry| (entry.reference(), entry))
                .collect::<BTreeMap<_, _>>();
            let mut projected_entries = Vec::new();
            for reference in projection.ordered_entries() {
                let Some(entry) = entries_by_reference.get(&reference) else {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "prospective attachment projected entry",
                    )
                    .into());
                };
                projected_entries.push(*entry);
            }
            let mut origins = projected_entries
                .into_iter()
                .filter_map(|entry| match entry.payload() {
                    InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                        accepted_input,
                    }
                    | InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                        accepted_input,
                        ..
                    } => distinct.insert(*accepted_input).then_some(*accepted_input),
                    _ => None,
                })
                .collect::<Vec<_>>();
            origins.extend(
                execution
                    .active_turn()
                    .pending_steering()
                    .iter()
                    .map(|pending| pending.accepted_input())
                    .filter(|accepted_input| distinct.insert(*accepted_input)),
            );
            (
                origins,
                matches!(
                    result,
                    SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
                ),
            )
        }
        Err(ModelCallRepositoryError::NoLiveExecution) => {
            if let Some(active) = scheduling.active_turn_execution() {
                let mut distinct = BTreeSet::new();
                let mut origins =
                    if matches!(
                        active.phase(),
                        signalbox_domain::ActiveTurnPhase::AwaitingRunnerRecovery { .. }
                    ) {
                        let snapshot = load_runner_recovery_source_snapshot(
                            connection,
                            session,
                            active.turn(),
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                        .ok_or(SubmitInputCorruption::Inconsistent(
                            "runner recovery prospective attachment frontier missing",
                        ))?;
                        let complete_entries = snapshot
                            .ordered_entries()
                            .map(|reference| scheduling.semantic_entry(reference).cloned())
                            .collect::<Option<Vec<_>>>()
                            .ok_or(SubmitInputCorruption::Inconsistent(
                                "runner recovery prospective attachment frontier entry missing",
                            ))?;
                        let projection = ContextFrontierProjection::from_complete_entries(
                            &complete_entries,
                        )
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "runner recovery prospective attachment frontier projection",
                            )
                        })?;
                        let entries_by_reference = complete_entries
                            .iter()
                            .map(|entry| (entry.reference(), entry))
                            .collect::<BTreeMap<_, _>>();
                        projection
                            .ordered_entries()
                            .filter_map(|reference| {
                                match entries_by_reference[&reference].payload() {
                                InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                                    accepted_input,
                                }
                                | InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                                    accepted_input,
                                    ..
                                } => distinct.insert(*accepted_input).then_some(*accepted_input),
                                InitialSemanticTranscriptEntryPayload::TurnFailed { .. }
                                | InitialSemanticTranscriptEntryPayload::DelegatedTask { .. }
                                | InitialSemanticTranscriptEntryPayload::DelegationMessage { .. }
                                | InitialSemanticTranscriptEntryPayload::DelegationResult { .. }
                                | InitialSemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
                                | InitialSemanticTranscriptEntryPayload::ContextSummary { .. }
                                | InitialSemanticTranscriptEntryPayload::TurnCancelled { .. }
                                | InitialSemanticTranscriptEntryPayload::AssistantText { .. }
                                | InitialSemanticTranscriptEntryPayload::ProviderCompaction { .. }
                                | InitialSemanticTranscriptEntryPayload::AssistantToolUse { .. }
                                | InitialSemanticTranscriptEntryPayload::ToolExecutionResult { .. }
                                | InitialSemanticTranscriptEntryPayload::ToolDenied { .. }
                                | InitialSemanticTranscriptEntryPayload::ToolClosed { .. }
                                | InitialSemanticTranscriptEntryPayload::TurnCompleted { .. }
                                | InitialSemanticTranscriptEntryPayload::Imported { .. } => None,
                            }
                            })
                            .collect::<Vec<_>>()
                    } else {
                        scheduling.active_rendered_frontier_origins().ok_or(
                            SubmitInputCorruption::Inconsistent(
                                "active prospective attachment frontier missing",
                            ),
                        )?
                    };
                distinct.extend(origins.iter().copied());
                origins.extend(
                    active
                        .pending_steering()
                        .iter()
                        .map(|pending| pending.accepted_input())
                        .filter(|accepted_input| distinct.insert(*accepted_input)),
                );
                (
                    origins,
                    matches!(
                        result,
                        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
                    ),
                )
            } else if let Some(frontier) = delegated_parked_frontier.as_ref() {
                let origins = delegated_parked_attachment_frontier_origins(
                    connection,
                    session,
                    &scheduling,
                    frontier,
                )
                .await?;
                (
                    origins,
                    matches!(
                        result,
                        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
                    ),
                )
            } else {
                (
                    scheduling
                        .earliest_queued_rendered_base_origins()
                        .transpose()
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "prospective attachment context projection",
                            )
                        })?
                        .unwrap_or_default(),
                    false,
                )
            }
        }
        Err(error) => return Err(error.into()),
    };
    let queued_inputs = scheduling
        .turns()
        .filter(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Queued)
        .map(|turn| turn.accepted_input().id())
        .collect::<Vec<_>>();
    let queued_reset_origins = scheduling
        .turns()
        .filter(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Queued)
        .enumerate()
        .filter_map(|(index, turn)| {
            scheduling
                .external_predecessor_rendered_base_origins(turn.turn())
                .map(|origins| (index, origins))
        })
        .collect::<BTreeMap<_, _>>();
    let first_changed_queue = if check_base {
        0
    } else {
        prior_queued_inputs
            .iter()
            .zip(&queued_inputs)
            .take_while(|(before, after)| before == after)
            .count()
    };
    if !check_base && first_changed_queue == queued_inputs.len() {
        return Err(SubmitInputCorruption::Inconsistent(
            "applied turn-origin input left the prospective queue unchanged",
        )
        .into());
    }

    let all_origins = base_origins
        .iter()
        .chain(&queued_inputs)
        .chain(queued_reset_origins.values().flatten())
        .map(|accepted_input| accepted_input.into_uuid())
        .collect::<Vec<_>>();
    if !accepted_inputs_have_attachment_parts(connection, &all_origins).await? {
        return Ok(false);
    }
    let rows = sqlx::query(
        "SELECT part.accepted_input_id, part.blob_digest, blob.byte_length
           FROM accepted_input_content_part AS part
           JOIN blob ON blob.digest = part.blob_digest
          WHERE part.accepted_input_id = ANY($1)
            AND part.part_kind = 'attachment'
          ORDER BY part.accepted_input_id, part.position",
    )
    .bind(&all_origins)
    .fetch_all(&mut *connection)
    .await?;
    let mut attachments = BTreeMap::<AcceptedInputId, Vec<(BlobDigest, u64)>>::new();
    for row in rows {
        let accepted_input =
            accepted_input_id_from_uuid(row.try_get::<Uuid, _>("accepted_input_id")?);
        let digest = BlobDigest::from_bytes(
            row.try_get::<Vec<u8>, _>("blob_digest")?
                .try_into()
                .map_err(|_| {
                    SubmitInputCorruption::Inconsistent("prospective attachment digest")
                })?,
        );
        let length = positive_u64_from_numeric(row.try_get("byte_length")?).map_err(|_| {
            SubmitInputCorruption::Inconsistent("prospective attachment byte length")
        })?;
        attachments
            .entry(accepted_input)
            .or_default()
            .push((digest, length));
    }

    let mut digests = BTreeMap::<BlobDigest, u64>::new();
    let mut total = 0_u64;
    for accepted_input in &base_origins {
        add_prospective_attachment_lengths(
            &mut digests,
            &mut total,
            attachments
                .get(accepted_input)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        )?;
    }
    if check_base && total > maximum_bytes {
        return Ok(true);
    }
    for (index, accepted_input) in queued_inputs.iter().enumerate() {
        if let Some(reset_origins) = queued_reset_origins.get(&index) {
            digests.clear();
            total = 0;
            for reset_origin in reset_origins {
                add_prospective_attachment_lengths(
                    &mut digests,
                    &mut total,
                    attachments
                        .get(reset_origin)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                )?;
            }
        }
        add_prospective_attachment_lengths(
            &mut digests,
            &mut total,
            attachments
                .get(accepted_input)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        )?;
        if index >= first_changed_queue && total > maximum_bytes {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn session_has_attachment_parts(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<bool, SubmitInputRepositoryError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM accepted_input AS accepted
               JOIN accepted_input_content_part AS part
                 ON part.accepted_input_id = accepted.accepted_input_id
              WHERE accepted.session_id = $1
                AND part.part_kind = 'attachment'
         )",
    )
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut *connection)
    .await?)
}

async fn accepted_inputs_have_attachment_parts(
    connection: &mut PgConnection,
    accepted_inputs: &[Uuid],
) -> Result<bool, SubmitInputRepositoryError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM accepted_input_content_part
              WHERE accepted_input_id = ANY($1)
                AND part_kind = 'attachment'
         )",
    )
    .bind(accepted_inputs)
    .fetch_one(&mut *connection)
    .await?)
}

struct DelegatedParkedAttachmentFrontier {
    turn: TurnId,
    snapshot: ResolvedContextFrontierSnapshot,
}

async fn load_delegated_parked_attachment_frontier(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<DelegatedParkedAttachmentFrontier>, SubmitInputRepositoryError> {
    let row = sqlx::query(
        "SELECT turn_id, active_phase_kind, recovery_model_call_id,
                (
                    SELECT call.model_call_id
                      FROM model_call AS call
                     WHERE call.session_id = turn_lifecycle.session_id
                       AND call.turn_id = turn_lifecycle.turn_id
                       AND call.state_kind = 'cancellation_requested'
                ) AS stop_requested_model_call_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND origin_kind = 'delegation'
            AND state_kind = 'active'
            AND NOT delegation_runtime_terminal
            AND goal_turn_is_runtime_relevant(session_id, turn_id)",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let turn = turn_id_from_uuid(required(&row, "turn_id")?);
    let phase_spelling: String = required(&row, "active_phase_kind")?;
    let phase = active_turn_phase_from_str(&phase_spelling).ok_or({
        SubmitInputCorruption::Unsupported {
            field: "delegated parked active phase",
            value: phase_spelling,
        }
    })?;
    let snapshot = match phase {
        ActiveTurnPhaseStorageKind::AwaitingToolApproval
        | ActiveTurnPhaseStorageKind::AwaitingChild
        | ActiveTurnPhaseStorageKind::AwaitingToolRecovery => {
            load_active_batch_from_connection(connection, session, turn)
                .await
                .map_err(map_tool_loop_error)?
                .map(|batch| batch.yielded_snapshot().clone())
                .ok_or(SubmitInputCorruption::Inconsistent(
                    "delegated parked tool frontier missing",
                ))?
        }
        ActiveTurnPhaseStorageKind::AwaitingModelCallRecovery => {
            let recovery_call: Uuid = required(&row, "recovery_model_call_id")?;
            let frontier = sqlx::query_scalar::<_, Uuid>(
                "SELECT context_frontier_id
                   FROM model_call
                  WHERE model_call_id = $1
                    AND session_id = $2
                    AND turn_id = $3",
            )
            .bind(recovery_call)
            .bind(session_id_to_uuid(session))
            .bind(turn_id_to_uuid(turn))
            .fetch_optional(&mut *connection)
            .await?
            .ok_or(SubmitInputCorruption::Inconsistent(
                "delegated model recovery frontier missing",
            ))?;
            load_call_snapshot(connection, session, ContextFrontierId::from_uuid(frontier))
                .await
                .map_err(|error| SubmitInputRepositoryError::ModelExecution(Box::new(error)))?
                .reconstitute()
                .ok_or(SubmitInputCorruption::Inconsistent(
                    "delegated model recovery snapshot invalid",
                ))?
        }
        ActiveTurnPhaseStorageKind::AwaitingRunnerRecovery => {
            load_runner_recovery_source_snapshot(connection, session, turn)
                .await
                .map_err(map_tool_loop_error)?
                .ok_or(SubmitInputCorruption::Inconsistent(
                    "delegated runner recovery prospective attachment frontier missing",
                ))?
        }
        ActiveTurnPhaseStorageKind::Running => {
            let stop_requested_call: Option<Uuid> = row.try_get("stop_requested_model_call_id")?;
            let Some(stop_requested_call) = stop_requested_call else {
                // A turn executing a tool batch keeps the `running` phase
                // while the call that produced the batch is already terminal,
                // so this is the delegated spelling of the state the
                // accepted-input path reads through
                // `active_rendered_frontier_origins`. The batch's yielded
                // frontier is the delegated turn's retained context; without
                // it, accounting would fall back to the earliest queued base
                // and omit both that frontier and its pending steering.
                // A delegated turn with neither a cancellation-requested call
                // nor an active batch has a live model call, which the
                // live-execution path reads instead.
                return Ok(load_active_batch_from_connection(connection, session, turn)
                    .await
                    .map_err(map_tool_loop_error)?
                    .map(|batch| DelegatedParkedAttachmentFrontier {
                        turn,
                        snapshot: batch.yielded_snapshot().clone(),
                    }));
            };
            let frontier = sqlx::query_scalar::<_, Uuid>(
                "SELECT context_frontier_id
                   FROM model_call
                  WHERE model_call_id = $1
                    AND session_id = $2
                    AND turn_id = $3
                    AND state_kind = 'cancellation_requested'",
            )
            .bind(stop_requested_call)
            .bind(session_id_to_uuid(session))
            .bind(turn_id_to_uuid(turn))
            .fetch_optional(&mut *connection)
            .await?
            .ok_or(SubmitInputCorruption::Inconsistent(
                "delegated stop-requested frontier missing",
            ))?;
            load_call_snapshot(connection, session, ContextFrontierId::from_uuid(frontier))
                .await
                .map_err(|error| SubmitInputRepositoryError::ModelExecution(Box::new(error)))?
                .reconstitute()
                .ok_or(SubmitInputCorruption::Inconsistent(
                    "delegated stop-requested snapshot invalid",
                ))?
        }
    };
    Ok(Some(DelegatedParkedAttachmentFrontier { turn, snapshot }))
}

async fn delegated_parked_attachment_frontier_origins(
    connection: &mut PgConnection,
    session: SessionId,
    scheduling: &AcceptedInputSchedulingProjection,
    frontier: &DelegatedParkedAttachmentFrontier,
) -> Result<Vec<AcceptedInputId>, SubmitInputRepositoryError> {
    let complete_entries = frontier
        .snapshot
        .ordered_entries()
        .map(|reference| scheduling.semantic_entry(reference).cloned())
        .collect::<Option<Vec<_>>>()
        .ok_or(SubmitInputCorruption::Inconsistent(
            "delegated parked prospective attachment frontier entry missing",
        ))?;
    let projection =
        ContextFrontierProjection::from_complete_entries(&complete_entries).map_err(|_| {
            SubmitInputCorruption::Inconsistent(
                "delegated parked prospective attachment frontier projection",
            )
        })?;
    let entries_by_reference = complete_entries
        .iter()
        .map(|entry| (entry.reference(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut distinct = BTreeSet::new();
    let mut origins = projection
        .ordered_entries()
        .filter_map(
            |reference| match entries_by_reference[&reference].payload() {
                InitialSemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
                | InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input,
                    ..
                } => distinct.insert(*accepted_input).then_some(*accepted_input),
                InitialSemanticTranscriptEntryPayload::TurnFailed { .. }
                | InitialSemanticTranscriptEntryPayload::DelegatedTask { .. }
                | InitialSemanticTranscriptEntryPayload::DelegationMessage { .. }
                | InitialSemanticTranscriptEntryPayload::DelegationResult { .. }
                | InitialSemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
                | InitialSemanticTranscriptEntryPayload::ContextSummary { .. }
                | InitialSemanticTranscriptEntryPayload::TurnCancelled { .. }
                | InitialSemanticTranscriptEntryPayload::AssistantText { .. }
                | InitialSemanticTranscriptEntryPayload::ProviderCompaction { .. }
                | InitialSemanticTranscriptEntryPayload::AssistantToolUse { .. }
                | InitialSemanticTranscriptEntryPayload::ToolExecutionResult { .. }
                | InitialSemanticTranscriptEntryPayload::ToolDenied { .. }
                | InitialSemanticTranscriptEntryPayload::ToolClosed { .. }
                | InitialSemanticTranscriptEntryPayload::TurnCompleted { .. }
                | InitialSemanticTranscriptEntryPayload::Imported { .. } => None,
            },
        )
        .collect::<Vec<_>>();
    let pending = sqlx::query_scalar::<_, Uuid>(
        "SELECT accepted_input_id
           FROM accepted_input
          WHERE session_id = $1
            AND disposition_kind = 'pending_steering'
            AND expected_active_turn_id = $2
          ORDER BY acceptance_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(frontier.turn))
    .fetch_all(&mut *connection)
    .await?;
    origins.extend(
        pending
            .into_iter()
            .map(accepted_input_id_from_uuid)
            .filter(|accepted_input| distinct.insert(*accepted_input)),
    );
    Ok(origins)
}

fn add_prospective_attachment_lengths(
    digests: &mut BTreeMap<BlobDigest, u64>,
    total: &mut u64,
    attachments: &[(BlobDigest, u64)],
) -> Result<(), SubmitInputRepositoryError> {
    for (digest, length) in attachments {
        match digests.get(digest) {
            Some(recorded) if recorded != length => {
                return Err(SubmitInputCorruption::Inconsistent(
                    "prospective attachment length disagreement",
                )
                .into());
            }
            Some(_) => {}
            None => {
                *total = total
                    .checked_add(*length)
                    .ok_or(SubmitInputCorruption::Inconsistent(
                        "prospective attachment byte length sum",
                    ))?;
                digests.insert(*digest, *length);
            }
        }
    }
    Ok(())
}

async fn prepare_attachment_authority_rejection(
    connection: &mut PgConnection,
    command: &SubmitInput,
    maximum_bytes: Option<u64>,
) -> Result<Option<PreparedSubmitInput>, SubmitInputRepositoryError> {
    let digests = command
        .content()
        .parts()
        .iter()
        .filter_map(|part| match part {
            UserContentPart::Attachment { digest, .. } => Some(*digest),
            UserContentPart::Text { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    if digests.is_empty() {
        return Ok(None);
    }

    let mut total_bytes = 0_u64;
    for digest in digests {
        let row = sqlx::query(
            "SELECT blob.byte_length,
                    EXISTS (
                        SELECT 1
                          FROM blob_replica
                         WHERE blob_replica.digest = blob.digest
                    ) AS has_verified_replica
               FROM blob
              WHERE blob.digest = $1",
        )
        .bind(digest.as_bytes().as_slice())
        .fetch_optional(&mut *connection)
        .await?;
        let Some(row) = row else {
            return Ok(Some(
                command.clone().prepare_attachment_blob_not_found(digest),
            ));
        };
        let has_verified_replica: bool = required(&row, "has_verified_replica")?;
        if !has_verified_replica {
            return Ok(Some(
                command.clone().prepare_attachment_blob_not_found(digest),
            ));
        }
        let byte_length = positive_u64_from_numeric(required(&row, "byte_length")?)
            .map_err(|_| SubmitInputCorruption::Inconsistent("attachment blob byte length"))?;
        let Some(next_total) = total_bytes.checked_add(byte_length) else {
            let maximum_bytes = maximum_bytes.ok_or(SubmitInputCorruption::Inconsistent(
                "attachment authority maximum is unavailable",
            ))?;
            return Ok(Some(
                command
                    .clone()
                    .prepare_attachment_byte_budget_exceeded(maximum_bytes),
            ));
        };
        total_bytes = next_total;
    }

    let maximum_bytes = maximum_bytes.ok_or(SubmitInputCorruption::Inconsistent(
        "attachment authority maximum is unavailable",
    ))?;
    if total_bytes > maximum_bytes {
        return Ok(Some(
            command
                .clone()
                .prepare_attachment_byte_budget_exceeded(maximum_bytes),
        ));
    }
    Ok(None)
}

async fn terminalize_retryable_runner_recovery_attempt(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    attempt: ToolAttemptId,
) -> Result<bool, SubmitInputRepositoryError> {
    let row = sqlx::query(crate::lock_inventory::SUBMIT_INPUT_RUNNER_RECOVERY_ATTEMPT)
        .bind(session_id_to_uuid(session))
        .bind(turn_id_to_uuid(turn))
        .bind(attempt.into_uuid())
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(SubmitInputCorruption::Missing(
            "runner recovery interrupted attempt authority",
        ))?;
    let attempt_state: String = required(&row, "state_kind")?;
    let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
    if attempt_state == "terminal" && terminal_disposition.as_deref() == Some("ambiguous") {
        return Ok(true);
    }
    let lease_effect: String = required(&row, "lease_effect_class")?;
    let lease_state: String = required(&row, "lease_state_kind")?;
    if attempt_state != "in_flight"
        || !matches!(
            (lease_state.as_str(), lease_effect.as_str()),
            ("lost_unclaimed", "pure" | "idempotent" | "side_effecting")
                | (
                    "lost_execution_possible" | "lost_claimed",
                    "pure" | "idempotent"
                )
        )
    {
        return Err(SubmitInputCorruption::Inconsistent(
            "runner recovery interrupted attempt stop state",
        )
        .into());
    }
    let preserves_ambiguity = lease_state != "lost_unclaimed" && lease_effect == "idempotent";
    let terminal_disposition = if preserves_ambiguity {
        "ambiguous"
    } else {
        "known_failed"
    };
    let error_kind = if preserves_ambiguity {
        None
    } else {
        Some("crash_lost")
    };
    let changed = sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal', terminal_disposition_kind = $1,
                error_kind = $2
          WHERE attempt_id = $3 AND session_id = $4 AND turn_id = $5
            AND state_kind = 'in_flight'",
    )
    .bind(terminal_disposition)
    .bind(error_kind)
    .bind(attempt.into_uuid())
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    if changed != 1 {
        return Err(SubmitInputCorruption::Inconsistent(
            "runner recovery interrupted attempt retirement",
        )
        .into());
    }
    Ok(preserves_ambiguity)
}

async fn persist_runner_recovery_interrupt_effect(
    connection: &mut PgConnection,
    command: DurableCommandId,
    session: SessionId,
    turn: TurnId,
    source_frontier: ContextFrontierId,
) -> Result<(), SubmitInputRepositoryError> {
    let rows = sqlx::query(
        "INSERT INTO turn_runner_recovery_interrupt_effect
            (command_id, session_id, turn_id, placement_event_ordinal,
             runner_id, placement_revision, yielded_turn_attempt_id,
             interrupted_tool_attempt_id, source_frontier_id)
         SELECT $1, lifecycle.session_id, lifecycle.turn_id,
                head.event_ordinal, lifecycle.runner_recovery_runner_id,
                lifecycle.runner_recovery_placement_revision,
                yielded_attempt.turn_attempt_id,
                lifecycle.runner_recovery_tool_attempt_id, $4
           FROM turn_lifecycle AS lifecycle
           JOIN runner_current_session_placement AS head
             ON head.session_id = lifecycle.session_id
           JOIN runner_session_placement_record AS placement
             ON placement.session_id = head.session_id
            AND placement.event_ordinal = head.event_ordinal
           JOIN turn_attempt AS yielded_attempt
             ON yielded_attempt.turn_id = lifecycle.turn_id
            AND yielded_attempt.session_id = lifecycle.session_id
            AND yielded_attempt.state_kind = 'ended'
            AND yielded_attempt.end_variant = 'without_stop'
            AND yielded_attempt.end_disposition = 'yielded_to_durable_wait'
            AND yielded_attempt.interrupt_command_id IS NULL
            AND yielded_attempt.interrupt_predecessor_turn_id IS NULL
            AND NOT EXISTS (
                SELECT 1
                  FROM turn_attempt AS continuation
                 WHERE continuation.continued_from_attempt_id =
                        yielded_attempt.turn_attempt_id
            )
          WHERE lifecycle.session_id = $2
            AND lifecycle.turn_id = $3
            AND lifecycle.state_kind = 'active'
            AND lifecycle.active_phase_kind = 'awaiting_runner_recovery'
            AND placement.state_kind IN ('runner_lost', 'runner_lost_before_pin')
            AND placement.lost_runner_id = lifecycle.runner_recovery_runner_id
            AND placement.placement_revision =
                lifecycle.runner_recovery_placement_revision
            AND placement.interrupted_tool_attempt_id IS NOT DISTINCT FROM
                lifecycle.runner_recovery_tool_attempt_id
            AND (
                (
                    lifecycle.active_tool_round_call_id IS NULL
                    AND $4 = lifecycle.starting_frontier_id
                )
                OR EXISTS (
                    SELECT 1
                      FROM tool_round AS round
                     WHERE round.producing_model_call_id =
                            lifecycle.active_tool_round_call_id
                       AND round.turn_id = lifecycle.turn_id
                       AND round.session_id = lifecycle.session_id
                       AND round.boundary_kind = 'continuing'
                       AND round.boundary_frontier_id = $4
                )
            )",
    )
    .bind(durable_command_id_to_uuid(command))
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(source_frontier.into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    if rows != 1 {
        return Err(SubmitInputCorruption::Inconsistent("runner recovery interrupt effect").into());
    }
    Ok(())
}

async fn load_runner_recovery_yielded_attempt(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<TurnAttemptId, SubmitInputRepositoryError> {
    let attempt = sqlx::query_scalar::<_, Uuid>(
        "SELECT attempt.turn_attempt_id
           FROM turn_lifecycle AS lifecycle
           JOIN turn_attempt AS attempt
             ON attempt.turn_id = lifecycle.turn_id
            AND attempt.session_id = lifecycle.session_id
            AND attempt.state_kind = 'ended'
            AND attempt.end_variant = 'without_stop'
            AND attempt.end_disposition = 'yielded_to_durable_wait'
            AND attempt.interrupt_command_id IS NULL
            AND attempt.interrupt_predecessor_turn_id IS NULL
            AND NOT EXISTS (
                SELECT 1
                  FROM turn_attempt AS continuation
                 WHERE continuation.continued_from_attempt_id =
                        attempt.turn_attempt_id
            )
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2
            AND lifecycle.state_kind = 'active'
            AND lifecycle.active_phase_kind = 'awaiting_runner_recovery'",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(SubmitInputCorruption::Missing(
        "runner recovery yielded attempt",
    ))?;
    Ok(TurnAttemptId::from_uuid(attempt))
}

/// Persists the initial input for a freshly inserted session in the caller's
/// transaction. Repository-watch dispatch uses this narrow bridge so the
/// session, its first queued turn, and the dispatch audit become visible at
/// one commit boundary.
///
/// The core identities a fresh initial input mints: the accepted input, its
/// queued turn, and the cancellation entry and frontier the turn would need.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FreshInitialInput {
    /// The accepted input.
    pub accepted_input: AcceptedInputId,
    /// The queued turn.
    pub turn: TurnId,
    /// The reserved cancellation entry.
    pub cancellation_entry: SemanticTranscriptEntryId,
    /// The reserved cancellation frontier.
    pub cancellation_frontier: ContextFrontierId,
}

/// A freshly inserted session has no active turn, so submit preparation cannot
/// apply an interrupt. The reclassification and tool-cancellation callbacks
/// are therefore unreachable and use the reserved identities as placeholders.
/// The four identities are drawn from the submit slice's application-owned
/// generator under the lock (docs/spec/session-lifecycle.md).
pub(crate) async fn insert_fresh_initial_input(
    connection: &mut PgConnection,
    command: SubmitInput,
    principal: CommandPrincipal,
    ids: &mut impl SubmitInputIdGenerator,
    select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
) -> Result<FreshInitialInput, SubmitInputRepositoryError> {
    let minted = FreshInitialInput {
        accepted_input: ids.next_accepted_input_id(),
        turn: ids.next_turn_id(),
        cancellation_entry: ids.next_semantic_entry_id(),
        cancellation_frontier: ids.next_context_frontier_id(),
    };
    let FreshInitialInput {
        accepted_input,
        turn,
        cancellation_entry,
        cancellation_frontier,
    } = minted;
    let unreachable_closure_decision = command.command_id();
    let unreachable_closure_attempt = TurnAttemptId::from_uuid(command.command_id().into_uuid());
    let outcome = handle_in_transaction(
        connection,
        command,
        principal,
        ParentTerminationKind::Cancelled,
        accepted_input,
        Some(turn),
        CancelledModelCallTurnIdentities::new(cancellation_entry, cancellation_frontier),
        |_| turn,
        |_| (Vec::new(), cancellation_frontier),
        || unreachable_closure_decision,
        || unreachable_closure_attempt,
        select_definition,
        None,
        None,
    )
    .await?;
    match outcome {
        TransactionDecision::Commit(SubmitInputHandlingOutcome::Recorded(
            SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(result)),
        )) if result.accepted_input() == accepted_input && result.turn() == turn => Ok(minted),
        TransactionDecision::Commit(_)
        | TransactionDecision::Rollback(SubmitInputHandlingOutcome::Recorded(_))
        | TransactionDecision::Rollback(SubmitInputHandlingOutcome::ConflictingReuse { .. }) => {
            Err(SubmitInputCorruption::Inconsistent(
                "fresh session initial input did not create its reserved turn",
            )
            .into())
        }
    }
}

async fn require_recorded(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<ReconstitutedSubmitInput, SubmitInputRepositoryError> {
    load_from_connection(connection, command_id)
        .await?
        .ok_or_else(|| SubmitInputCorruption::Inconsistent("registry entry disappeared").into())
}

pub(crate) async fn require_recorded_batch(
    connection: &mut PgConnection,
    command_ids: &[DurableCommandId],
) -> Result<BTreeMap<DurableCommandId, ReconstitutedSubmitInput>, SubmitInputRepositoryError> {
    let requested = command_ids
        .iter()
        .copied()
        .map(|command_id| (durable_command_id_to_uuid(command_id), command_id))
        .collect::<BTreeMap<_, _>>();
    let requested_uuids = requested.keys().copied().collect::<Vec<_>>();
    let rows = load_complete_rows(connection, &requested_uuids).await?;
    let mut rows_by_command = BTreeMap::new();
    let mut related_turns = BTreeSet::new();
    for row in rows {
        let command_uuid: Uuid = required(&row, "registry_command_id")?;
        if !requested.contains_key(&command_uuid) {
            return Err(
                SubmitInputCorruption::Inconsistent("unexpected batched command identity").into(),
            );
        }
        if non_accepted_predecessor(&row)?.is_none()
            && let Some(related_turn) = related_turn_origin_key(&row)?
        {
            related_turns.insert(related_turn);
        }
        if rows_by_command.insert(command_uuid, row).is_some() {
            return Err(
                SubmitInputCorruption::Inconsistent("duplicate batched command row").into(),
            );
        }
    }
    if rows_by_command.len() != requested.len() {
        return Err(SubmitInputCorruption::Missing("batched origin command").into());
    }

    let related_origins = load_turn_origin_graph(connection, &related_turns).await?;
    let mut recorded = BTreeMap::new();
    for (command_uuid, command_id) in requested {
        let row = rows_by_command
            .remove(&command_uuid)
            .ok_or(SubmitInputCorruption::Missing("batched origin command"))?;
        let non_accepted_predecessor = non_accepted_predecessor(&row)?;
        let related_turn_origin = if non_accepted_predecessor.is_some() {
            None
        } else {
            related_turn_origin_key(&row)?
                .map(|key| {
                    related_origins
                        .get(&key)
                        .cloned()
                        .ok_or(SubmitInputCorruption::Missing("related turn origin"))
                })
                .transpose()?
        };
        let existing_interrupt = load_existing_interrupt(connection, &row).await?;
        let reconstructed = decode_complete(
            row,
            command_id,
            related_turn_origin,
            non_accepted_predecessor,
            existing_interrupt,
        )?;
        if recorded.insert(command_id, reconstructed).is_some() {
            return Err(
                SubmitInputCorruption::Inconsistent("duplicate batched command row").into(),
            );
        }
    }
    Ok(recorded)
}

fn existing_outcome(
    command: &SubmitInput,
    recorded: ReconstitutedSubmitInput,
) -> SubmitInputHandlingOutcome {
    if command == recorded.command() {
        SubmitInputHandlingOutcome::Recorded(recorded.result().clone())
    } else {
        SubmitInputHandlingOutcome::ConflictingReuse {
            command_id: command.command_id(),
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the locked preparation keeps command inputs and deferred identity effects explicit"
)]
async fn prepare_against_locked_state<NextClosureDecision, NextClosureAttempt>(
    connection: &mut PgConnection,
    command: SubmitInput,
    principal: CommandPrincipal,
    accepted_input: AcceptedInputId,
    turn: Option<TurnId>,
    next_closure_decision: &mut NextClosureDecision,
    next_closure_attempt: &mut NextClosureAttempt,
    select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    model_capabilities: Option<&ModelCapabilityCatalog>,
) -> Result<PreparedAgainstLockedState, SubmitInputRepositoryError>
where
    NextClosureDecision: FnMut() -> DurableCommandId + Send,
    NextClosureAttempt: FnMut() -> TurnAttemptId + Send,
{
    // Lock-mode constraint: these session-row locks must use the no-key-update
    // mode, not PostgreSQL's strongest row-lock mode. Submit orders the session row before the
    // scheduler row and current-defaults pointer row, while a concurrent
    // defaults replacement holds the pointer row (its compare-and-set) when its
    // `session_defaults_version` insert requests `FOR KEY SHARE` on this
    // session row through the non-deferrable session foreign key.
    // The stronger mode conflicts with `FOR KEY SHARE` and closes that lock-order
    // cycle into a deadlock (40P01); `FOR NO KEY UPDATE` does not conflict
    // with referential-integrity `KEY SHARE` locks while remaining
    // self-exclusive, so per-session position assignment stays serialized.
    // A delegated child can terminalize while processing input. Such a
    // terminalization later locks the parent endpoint, so delegated input must
    // join peer-message ordering before it acquires the child scheduler.
    let parent = sqlx::query_scalar::<_, Uuid>(
        "SELECT parent_session_id
           FROM session_delegation
          WHERE child_session_id = $1",
    )
    .bind(session_id_to_uuid(command.session()))
    .fetch_optional(&mut *connection)
    .await?
    .map(session_id_from_uuid);
    let (first, second) = parent
        .map(|parent| crate::lock_inventory::ordered_session_pair(command.session(), parent))
        .unwrap_or((command.session(), command.session()));
    let first_exists = sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::SUBMIT_INPUT_SESSION)
        .bind(session_id_to_uuid(first))
        .fetch_optional(&mut *connection)
        .await?
        .is_some();
    let second_exists = if second == first {
        first_exists
    } else {
        sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::SUBMIT_INPUT_SESSION)
            .bind(session_id_to_uuid(second))
            .fetch_optional(&mut *connection)
            .await?
            .is_some()
    };
    let session_exists = if command.session() == first {
        first_exists
    } else {
        second_exists
    };
    if !session_exists {
        return Ok(PreparedAgainstLockedState {
            prepared: command.prepare_session_not_found(),
            scheduling: None,
            settles_closure: false,
        });
    }

    let scheduler_exists =
        sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::SUBMIT_INPUT_SCHEDULER)
            .bind(session_id_to_uuid(command.session()))
            .fetch_optional(&mut *connection)
            .await?
            .is_some();
    if !scheduler_exists {
        return Err(
            SubmitInputCorruption::CurrentSession(SessionCorruption::Missing("scheduler row"))
                .into(),
        );
    }
    let pending_terminal = sqlx::query_scalar::<_, bool>(
        "SELECT pending_terminal_outcome_kind IS NOT NULL
           FROM session_lifecycle
          WHERE session_id = $1",
    )
    .bind(session_id_to_uuid(command.session()))
    .fetch_one(&mut *connection)
    .await?;
    let settles_closure =
        pending_terminal && settles_committed_closure(connection, &command, principal).await?;
    if pending_terminal && !settles_closure {
        return Err(
            SubmitInputCorruption::Inconsistent("session has a pending terminal handoff").into(),
        );
    }
    if settles_closure
        && let DeliveryRequest::Interrupt {
            expected_active_turn,
            ..
        } = command.delivery()
    {
        deny_awaiting_approvals_for_interrupt(
            connection,
            command.session(),
            expected_active_turn,
            next_closure_decision,
            next_closure_attempt,
        )
        .await
        .map_err(map_tool_loop_error)?;
    }

    let pointer_exists =
        sqlx::query_scalar::<_, Decimal>(crate::lock_inventory::SUBMIT_INPUT_DEFAULTS)
            .bind(session_id_to_uuid(command.session()))
            .fetch_optional(&mut *connection)
            .await?
            .is_some();
    if !pointer_exists {
        return Err(
            SubmitInputCorruption::CurrentSession(SessionCorruption::Missing(
                "current defaults pointer",
            ))
            .into(),
        );
    }

    let session = match load_session_from_connection(connection, command.session()).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            return Err(SubmitInputCorruption::Inconsistent("locked session disappeared").into());
        }
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(SubmitInputCorruption::CurrentSession(error).into());
        }
    };

    let scheduling = load_scheduling_projection(connection, session.clone()).await?;
    let active_turn_id = scheduling.active_turn().map(|active| active.turn());
    let prepared = if active_turn_id.is_some() {
        match model_capabilities {
            Some(capabilities) => command.prepare_with_active_turn_with_model_settings(
                &scheduling,
                accepted_input,
                turn,
                select_definition,
                capabilities,
            ),
            None => command.prepare_with_active_turn(
                &scheduling,
                accepted_input,
                turn,
                select_definition,
            ),
        }
    } else {
        let delegated_active = sqlx::query(
            "SELECT lifecycle.turn_id, lifecycle.active_phase_kind,
                    attempt.interrupt_command_id
               FROM turn_lifecycle AS lifecycle
               LEFT JOIN turn_attempt AS attempt
                 ON attempt.turn_attempt_id = lifecycle.current_attempt_id
                AND attempt.turn_id = lifecycle.turn_id
                AND attempt.session_id = lifecycle.session_id
              WHERE lifecycle.session_id = $1
                AND lifecycle.origin_kind = 'delegation'
                AND lifecycle.state_kind = 'active'
                AND NOT lifecycle.delegation_runtime_terminal",
        )
        .bind(session_id_to_uuid(command.session()))
        .fetch_optional(&mut *connection)
        .await?;
        let previous_position = sqlx::query_scalar::<_, Option<Decimal>>(
            "SELECT max(accepted_position)
               FROM (
                    SELECT acceptance_position AS accepted_position
                      FROM accepted_input
                     WHERE session_id = $1
                    UNION ALL
                    SELECT acceptance_position AS accepted_position
                      FROM turn_lifecycle
                     WHERE session_id = $1
               ) AS session_positions",
        )
        .bind(session_id_to_uuid(command.session()))
        .fetch_one(&mut *connection)
        .await?
        .map(|value| {
            input_position_from_numeric(value).map_err(|reason| {
                SubmitInputRepositoryError::Corruption(SubmitInputCorruption::InvalidOrdinal {
                    field: "previous acceptance_position",
                    reason,
                })
            })
        })
        .transpose()?;
        match delegated_active {
            Some(active) => {
                let active_turn = turn_id_from_uuid(required(&active, "turn_id")?);
                let phase: String = required(&active, "active_phase_kind")?;
                let awaiting_approval = match phase.as_str() {
                    "running"
                    | "awaiting_child"
                    | "awaiting_model_call_recovery"
                    | "awaiting_tool_recovery"
                    | "awaiting_runner_recovery" => false,
                    "awaiting_tool_approval" => true,
                    value => {
                        return Err(SubmitInputCorruption::Unsupported {
                            field: "delegated active phase",
                            value: value.to_owned(),
                        }
                        .into());
                    }
                };
                let existing_interrupt = active
                    .try_get::<Option<Uuid>, _>("interrupt_command_id")?
                    .map(durable_command_id_from_uuid)
                    .transpose()
                    .map_err(|_| {
                        SubmitInputCorruption::Inconsistent("delegated active interrupt command")
                    })?;
                command.prepare_with_delegated_active_turn(
                    &session,
                    active_turn,
                    previous_position,
                    existing_interrupt,
                    awaiting_approval,
                    accepted_input,
                    turn,
                    select_definition,
                )
            }
            // No delegated active turn: the no-active-turn path is the one that
            // freezes configuration, so settings capability resolution applies
            // here. `prepare_with_delegated_active_turn` resolves settings
            // internally and takes no capability catalog.
            None => match model_capabilities {
                Some(capabilities) => command.prepare_when_no_active_turn_with_model_settings(
                    &session,
                    accepted_input,
                    turn,
                    previous_position,
                    select_definition,
                    capabilities,
                ),
                None => command.prepare_when_no_active_turn(
                    &session,
                    accepted_input,
                    turn,
                    previous_position,
                    select_definition,
                ),
            },
        }
    };

    prepared
        .map(|prepared| PreparedAgainstLockedState {
            prepared,
            scheduling: Some(scheduling),
            settles_closure,
        })
        .map_err(|error| match error.failure() {
            SubmitInputPreparationFailure::SessionMismatch { .. } => {
                SubmitInputCorruption::Inconsistent("current session ownership").into()
            }
            SubmitInputPreparationFailure::TurnCandidateMismatch => {
                SubmitInputCorruption::Inconsistent("delivery turn candidate").into()
            }
            SubmitInputPreparationFailure::AcceptedInputCandidateReusesActiveOrigin {
                active_turn,
                accepted_input,
            } => SubmitInputRepositoryError::AcceptedInputIdentityCollision {
                command_id: error.command().command_id(),
                active_turn,
                accepted_input,
            },
            SubmitInputPreparationFailure::ActiveTurnProjectionMissing => {
                SubmitInputCorruption::Inconsistent("selected active scheduling state").into()
            }
            SubmitInputPreparationFailure::InterruptQueueOrderInvalid => {
                SubmitInputCorruption::Inconsistent("interrupt queue order").into()
            }
            SubmitInputPreparationFailure::ModelSettingsResolution(error) => {
                map_model_settings_resolution_error(error)
            }
        })
}

/// Whether this is the core-issued interrupt a committed closure owes its own
/// live turn. The closure recorded that turn, and the interrupt
/// terminalizing it is how the handoff settles, so a pending handoff admits
/// exactly it.
async fn settles_committed_closure(
    connection: &mut PgConnection,
    command: &SubmitInput,
    principal: CommandPrincipal,
) -> Result<bool, SubmitInputRepositoryError> {
    if principal != CommandPrincipal::Core {
        return Ok(false);
    }
    let DeliveryRequest::Interrupt {
        expected_active_turn,
        ..
    } = command.delivery()
    else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1
               FROM session_lifecycle_command
              WHERE session_id = $1
                AND applied_effect_kind = 'closure_pending'
                AND live_turn_id = $2)",
    )
    .bind(session_id_to_uuid(command.session()))
    .bind(turn_id_to_uuid(expected_active_turn))
    .fetch_one(&mut *connection)
    .await?)
}

fn map_model_settings_resolution_error(
    error: OriginModelSettingsError,
) -> SubmitInputRepositoryError {
    match error {
        OriginModelSettingsError::Unsupported(error) => {
            SubmitInputRepositoryError::UnsupportedModelSetting(error)
        }
        OriginModelSettingsError::UnknownAlias(_)
        | OriginModelSettingsError::MissingCapabilities { .. } => {
            SubmitInputCorruption::Inconsistent("model settings resolution").into()
        }
    }
}

pub(crate) async fn load_scheduling_projection(
    connection: &mut PgConnection,
    session: Session,
) -> Result<AcceptedInputSchedulingProjection, SubmitInputRepositoryError> {
    load_scheduling_projection_with_semantic_frontiers(connection, session, &[]).await
}

async fn load_scheduling_projection_with_semantic_frontiers(
    connection: &mut PgConnection,
    session: Session,
    supplemental_semantic_frontiers: &[ContextFrontierId],
) -> Result<AcceptedInputSchedulingProjection, SubmitInputRepositoryError> {
    let session_id = session.id();
    let imported_session = if matches!(
        session.creation_provenance().ancestry(),
        TranscriptAncestry::ImportedConversation { .. }
    ) {
        Some(
            crate::create_session_from_imported_frontier::load_complete_current(
                connection, &session,
            )
            .await
            .map_err(map_imported_scheduling_error)?,
        )
    } else {
        None
    };
    let inventory = sqlx::query_as::<_, StoredSchedulingInventoryCounts>(
        "SELECT
            (SELECT count(*)
               FROM queued_input_origin
              WHERE session_id = $1
                AND goal_turn_is_runtime_relevant(
                    session_id, turn_id
                )) AS queue_count,
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1
                AND origin_kind = 'accepted_input'
                AND goal_turn_is_runtime_relevant(
                    session_id, turn_id
                )) AS lifecycle_count",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_one(&mut *connection)
    .await?;
    if inventory.queue_count != inventory.lifecycle_count {
        return Err(
            SubmitInputCorruption::Inconsistent("complete scheduling turn inventory").into(),
        );
    }

    let rows = sqlx::query(
        "SELECT
            queued.turn_id AS queued_turn_id,
            queued.accepted_input_id AS queued_accepted_input_id,
            queued.session_id AS queued_session_id,
            queued.acceptance_position AS queued_position,
            queued.priority_kind,
            queued.interrupt_predecessor_turn_id,
            accepted.accepting_command_id,
            goal.goal_generation,
            accepted.accepted_input_id,
            accepted.session_id AS accepted_session_id,
            accepted.disposition_kind,
            accepted.origin_turn_id,
            accepted.expected_active_turn_id AS accepted_source_turn_id,
            queued.source_configuration_turn_id,
            (
                queued.defaults_version IS NULL
                AND queued.requested_model_kind IS NULL
                AND queued.requested_direct_model_selection_id IS NULL
                AND queued.requested_model_alias_id IS NULL
                AND queued.frozen_model_kind IS NULL
                AND queued.frozen_direct_model_selection_id IS NULL
                AND queued.frozen_model_alias_id IS NULL
                AND queued.frozen_alias_selected_direct_id IS NULL
                AND queued.model_parameters IS NULL
                AND queued.known_provider_failure_retry IS NULL
                AND queued.model_fallback IS NULL
                AND queued.dangerous_tool_auto_approval IS NULL
            ) AS queued_configuration_values_absent,
            queued.defaults_version AS queued_defaults_version,
            queued.requested_model_kind,
            queued.requested_direct_model_selection_id,
            queued.requested_model_alias_id,
            queued.frozen_model_kind,
            queued.frozen_direct_model_selection_id,
            queued.frozen_model_alias_id,
            queued.frozen_alias_selected_direct_id,
            queued.model_parameters,
            queued.known_provider_failure_retry,
            queued.model_fallback,
            queued.dangerous_tool_auto_approval AS queued_tool_auto_approval,
            goal_defaults.session_id AS goal_defaults_session_id,
            goal_defaults.version AS goal_defaults_version,
            goal_defaults.model_selection_kind AS goal_defaults_model_kind,
            goal_defaults.direct_model_selection_id AS goal_defaults_direct_id,
            goal_defaults.model_alias_id AS goal_defaults_alias_id,
            goal_defaults.dangerous_tool_auto_approval AS goal_defaults_tool_auto_approval,
            goal_defaults.model_settings AS goal_defaults_model_settings,
            turn.turn_id AS lifecycle_turn_id,
            turn.session_id AS lifecycle_session_id,
            turn.state_kind AS lifecycle_state_kind,
            turn.start_lineage_kind,
            turn.immediate_predecessor_turn_id,
            turn.starting_frontier_id,
            turn.terminal_frontier_id,
            turn.active_phase_kind,
            turn.current_attempt_id,
            turn.pinned_provider_model_identity_id,
            turn.recovery_model_call_id,
            turn.active_tool_round_call_id,
            turn.approval_tool_request_id,
            turn.recovery_tool_attempt_id,
            turn.runner_recovery_runner_id,
            turn.runner_recovery_placement_revision,
            turn.runner_recovery_tool_attempt_id,
            turn.child_wait_request_id,
            turn.model_identity_boundary_required,
            turn.terminal_attempt_id,
            turn.terminal_model_call_id,
            turn.terminal_tool_attempt_id,
            turn.terminal_disposition_kind,
            automatic_reconciliation.model_call_id
                AS automatic_reconciliation_model_call_id,
            automatic_reconciliation.tool_attempt_id
                AS automatic_reconciliation_tool_attempt_id,
            automatic_reconciliation.state_kind
                AS automatic_reconciliation_state_kind,
            automatic_reconciliation.attempt_count
                AS automatic_reconciliation_attempt_count,
            (
                SELECT call.model_call_id
                  FROM model_call AS call
                 WHERE call.turn_id = turn.turn_id
                   AND call.session_id = turn.session_id
                   AND call.state_kind = 'cancellation_requested'
            ) AS stop_requested_model_call_id,
            attempt.turn_attempt_id,
            attempt.turn_id AS attempt_turn_id,
            attempt.session_id AS attempt_session_id,
            attempt.continued_from_attempt_id,
            attempt.state_kind AS attempt_state_kind,
            attempt.interrupt_command_id,
            attempt.interrupt_predecessor_turn_id AS attempt_interrupt_predecessor_turn_id,
            attempt.end_variant,
            attempt.end_disposition,
            runner_recovery_effect.command_id AS runner_recovery_interrupt_command_id,
            runner_recovery_effect.yielded_turn_attempt_id
                AS runner_recovery_yielded_attempt_id,
            runner_recovery_effect.interrupted_tool_attempt_id
                AS runner_recovery_interrupted_tool_attempt_id
         FROM queued_input_origin AS queued
         LEFT JOIN accepted_input AS accepted
           ON accepted.accepted_input_id = queued.accepted_input_id
         LEFT JOIN goal_turn AS goal
           ON goal.accepted_input_id = accepted.accepted_input_id
          AND goal.session_id = queued.session_id
          AND goal.turn_id = queued.turn_id
         LEFT JOIN session_defaults_version AS goal_defaults
           ON goal_defaults.session_id = queued.session_id
          AND goal_defaults.version = queued.defaults_version
         LEFT JOIN turn_lifecycle AS turn
           ON turn.turn_id = queued.turn_id
         LEFT JOIN turn_attempt AS attempt
           ON attempt.turn_attempt_id = COALESCE(
                turn.current_attempt_id,
                turn.terminal_attempt_id
              )
         LEFT JOIN automatic_reconciliation AS automatic_reconciliation
           ON automatic_reconciliation.turn_id = turn.turn_id
          AND automatic_reconciliation.session_id = turn.session_id
         LEFT JOIN turn_runner_recovery_interrupt_effect AS runner_recovery_effect
           ON runner_recovery_effect.turn_id = turn.turn_id
          AND runner_recovery_effect.session_id = turn.session_id
        WHERE queued.session_id = $1
          AND goal_turn_is_runtime_relevant(
                queued.session_id, queued.turn_id
          )
        ORDER BY queued.acceptance_position",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    let mut accepting_commands = Vec::with_capacity(rows.len());
    for row in &rows {
        if let Some(command_uuid) = row.try_get::<Option<Uuid>, _>("accepting_command_id")? {
            accepting_commands.push(
                durable_command_id_from_uuid(command_uuid).map_err(|_| {
                    SubmitInputCorruption::Inconsistent("accepting command identity")
                })?,
            );
        }
    }
    let recorded_commands = require_recorded_batch(connection, &accepting_commands).await?;

    let mut turns = Vec::with_capacity(rows.len());
    let mut turn_configurations = BTreeMap::<TurnId, OriginConfiguration>::new();
    let mut pinned_target_identities = BTreeMap::new();
    let mut required_frontiers = BTreeSet::new();
    let mut required_model_calls = BTreeSet::new();
    let mut named_continuation_gate_calls = BTreeSet::new();
    for row in rows {
        let queued_turn = turn_id_from_uuid(required(&row, "queued_turn_id")?);
        let queued_accepted =
            accepted_input_id_from_uuid(required(&row, "queued_accepted_input_id")?);
        let queued_session = session_id_from_uuid(required(&row, "queued_session_id")?);
        let queued_position = decode_position(&row, "queued_position")?;
        let queued_order = match required::<String>(&row, "priority_kind")?.as_str() {
            "ordinary" => {
                if row
                    .try_get::<Option<Uuid>, _>("interrupt_predecessor_turn_id")?
                    .is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("ordinary queue priority").into(),
                    );
                }
                AcceptedInputQueueOrder::ordinary(queued_position)
            }
            "interrupt_immediately_after" => {
                let predecessor =
                    turn_id_from_uuid(required(&row, "interrupt_predecessor_turn_id")?);
                AcceptedInputQueueOrder::interrupt_immediately_after(queued_position, predecessor)
            }
            value => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "queue priority kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };

        let accepted_input = accepted_input_id_from_uuid(required(&row, "accepted_input_id")?);
        let accepted_session = session_id_from_uuid(required(&row, "accepted_session_id")?);
        let disposition_kind: String = required(&row, "disposition_kind")?;
        let origin_turn = turn_id_from_uuid(required(&row, "origin_turn_id")?);
        let accepted_source_turn: Option<Uuid> = row.try_get("accepted_source_turn_id")?;

        let lifecycle_turn = turn_id_from_uuid(required(&row, "lifecycle_turn_id")?);
        let lifecycle_session = session_id_from_uuid(required(&row, "lifecycle_session_id")?);
        if queued_accepted != accepted_input
            || queued_turn != origin_turn
            || lifecycle_turn != queued_turn
        {
            return Err(SubmitInputCorruption::Inconsistent(
                "scheduling turn identity correlation",
            )
            .into());
        }

        let accepting_command: Option<Uuid> = row.try_get("accepting_command_id")?;
        let goal_generation: Option<Decimal> = row.try_get("goal_generation")?;
        // A command and a goal generation are not exclusive. A dispatched
        // work turn is bound to the generation it runs under while keeping the
        // submit command that accepted its tagged context, so the command is
        // what reconstitutes the input and the generation rides alongside it.
        let (accepted_lifecycle, origin_delivery, origin_configuration, binding) =
            if let Some(accepting_command) = accepting_command {
                let accepting_command =
                    durable_command_id_from_uuid(accepting_command).map_err(|_| {
                        SubmitInputCorruption::Inconsistent("accepting command identity")
                    })?;
                let recorded = recorded_commands
                    .get(&accepting_command)
                    .ok_or(SubmitInputCorruption::Missing("batched origin receipt"))?;
                match (disposition_kind.as_str(), recorded.result()) {
                    (
                        "origin_of",
                        SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)),
                    ) if applied.accepted_input() == accepted_input
                        && applied.session() == accepted_session
                        && applied.turn() == queued_turn
                        && accepted_source_turn
                            == accepted_origin_source_turn(recorded.command().delivery())
                                .map(TurnId::into_uuid) =>
                    {
                        (
                            AcceptedInputLifecycle::new(
                                accepted_input,
                                AcceptedInputDisposition::OriginOf(origin_turn),
                            ),
                            recorded.command().delivery(),
                            applied.origin_configuration().clone(),
                            None,
                        )
                    }
                    (
                        "reclassified_as_turn_origin",
                        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(
                            applied,
                        )),
                    ) if applied.accepted_input() == accepted_input
                        && applied.session() == accepted_session
                        && applied.binding().source_turn().into_uuid()
                            == accepted_source_turn.ok_or(SubmitInputCorruption::Missing(
                                "reclassified source turn",
                            ))? =>
                    {
                        let source_turn = applied.binding().source_turn();
                        let source_configuration =
                            turn_configurations.get(&source_turn).cloned().ok_or(
                                SubmitInputCorruption::Missing("reclassified source configuration"),
                            )?;
                        (
                            AcceptedInputLifecycle::new(
                                accepted_input,
                                AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                                    turn: origin_turn,
                                    reason:
                                        SteeringReclassificationReason::NoSafePointBeforeTerminal,
                                },
                            ),
                            recorded.command().delivery(),
                            source_configuration,
                            Some(applied.binding()),
                        )
                    }
                    ("origin_of" | "reclassified_as_turn_origin", _) => {
                        return Err(SubmitInputCorruption::Inconsistent(
                            "scheduling origin command result",
                        )
                        .into());
                    }
                    (value, _) => {
                        return Err(SubmitInputCorruption::Unsupported {
                            field: "scheduling accepted-input disposition_kind",
                            value: value.to_owned(),
                        }
                        .into());
                    }
                }
            } else {
                if goal_generation.is_none()
                    || disposition_kind != "origin_of"
                    || accepted_source_turn.is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("scheduling goal turn shape").into(),
                    );
                }
                let origin_configuration = decode_goal_origin_configuration(&row, queued_session)?;
                let origin_delivery = DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: PerInputConfigurationChoices::new(
                        origin_configuration.session_defaults_version(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                };
                (
                    AcceptedInputLifecycle::new(
                        accepted_input,
                        AcceptedInputDisposition::OriginOf(origin_turn),
                    ),
                    origin_delivery,
                    origin_configuration,
                    None,
                )
            };
        match binding {
            Some(binding) => require_stored_inherited_configuration(&row, binding.source_turn())?,
            None => require_stored_origin_configuration(&row, &origin_configuration)?,
        }
        if turn_configurations
            .insert(queued_turn, origin_configuration.clone())
            .is_some()
        {
            return Err(
                SubmitInputCorruption::Inconsistent("duplicate scheduling configuration").into(),
            );
        }

        let state_kind: String = required(&row, "lifecycle_state_kind")?;
        let lineage_kind: Option<String> = row.try_get("start_lineage_kind")?;
        let predecessor: Option<Uuid> = row.try_get("immediate_predecessor_turn_id")?;
        let starting_frontier: Option<Uuid> = row.try_get("starting_frontier_id")?;
        let terminal_frontier: Option<Uuid> = row.try_get("terminal_frontier_id")?;
        let active_phase: Option<String> = row.try_get("active_phase_kind")?;
        let current_attempt: Option<Uuid> = row.try_get("current_attempt_id")?;
        let pinned_target: Option<Uuid> = row.try_get("pinned_provider_model_identity_id")?;
        let model_identity_boundary_required: bool =
            required(&row, "model_identity_boundary_required")?;
        let recovery_model_call: Option<Uuid> = row.try_get("recovery_model_call_id")?;
        let active_tool_round: Option<Uuid> = row.try_get("active_tool_round_call_id")?;
        let approval_tool_request: Option<Uuid> = row.try_get("approval_tool_request_id")?;
        let recovery_tool_attempt: Option<Uuid> = row.try_get("recovery_tool_attempt_id")?;
        let runner_recovery_runner: Option<Uuid> = row.try_get("runner_recovery_runner_id")?;
        let runner_recovery_revision: Option<Decimal> =
            row.try_get("runner_recovery_placement_revision")?;
        let runner_recovery_tool_attempt: Option<Uuid> =
            row.try_get("runner_recovery_tool_attempt_id")?;
        let child_wait_request: Option<Uuid> = row.try_get("child_wait_request_id")?;
        let terminal_attempt: Option<Uuid> = row.try_get("terminal_attempt_id")?;
        let terminal_model_call: Option<Uuid> = row.try_get("terminal_model_call_id")?;
        let terminal_tool_attempt: Option<Uuid> = row.try_get("terminal_tool_attempt_id")?;
        let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
        let automatic_reconciliation_model_call: Option<Uuid> =
            row.try_get("automatic_reconciliation_model_call_id")?;
        let automatic_reconciliation_tool_attempt: Option<Uuid> =
            row.try_get("automatic_reconciliation_tool_attempt_id")?;
        let automatic_reconciliation_state: Option<String> =
            row.try_get("automatic_reconciliation_state_kind")?;
        let automatic_reconciliation_attempt_count: Option<i32> =
            row.try_get("automatic_reconciliation_attempt_count")?;
        if active_phase.as_deref() != Some("awaiting_runner_recovery")
            && (runner_recovery_runner.is_some()
                || runner_recovery_revision.is_some()
                || runner_recovery_tool_attempt.is_some())
        {
            return Err(
                SubmitInputCorruption::Inconsistent("runner recovery lifecycle payload").into(),
            );
        }
        let automatic_reconciliation_present = automatic_reconciliation_model_call.is_some()
            || automatic_reconciliation_tool_attempt.is_some()
            || automatic_reconciliation_state.is_some()
            || automatic_reconciliation_attempt_count.is_some();
        if automatic_reconciliation_present
            && !(state_kind == "active"
                && matches!(
                    active_phase.as_deref(),
                    Some("awaiting_model_call_recovery" | "awaiting_tool_recovery")
                )
                || state_kind == "terminal"
                    && terminal_disposition.as_deref() == Some("reconciliation_required"))
        {
            return Err(SubmitInputCorruption::Inconsistent(
                "automatic model-call reconciliation lifecycle phase",
            )
            .into());
        }
        let state = match state_kind.as_str() {
            "queued" => {
                if lineage_kind.is_some()
                    || predecessor.is_some()
                    || starting_frontier.is_some()
                    || terminal_frontier.is_some()
                    || active_phase.is_some()
                    || current_attempt.is_some()
                    || recovery_model_call.is_some()
                    || active_tool_round.is_some()
                    || approval_tool_request.is_some()
                    || recovery_tool_attempt.is_some()
                    || runner_recovery_runner.is_some()
                    || runner_recovery_revision.is_some()
                    || runner_recovery_tool_attempt.is_some()
                    || child_wait_request.is_some()
                    || terminal_attempt.is_some()
                    || terminal_model_call.is_some()
                    || terminal_tool_attempt.is_some()
                    || terminal_disposition.is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("queued scheduling lifecycle").into(),
                    );
                }
                AcceptedInputTurnSchedulingRecordState::Queued
            }
            "active" => {
                if terminal_frontier.is_some()
                    || terminal_attempt.is_some()
                    || terminal_model_call.is_some()
                    || terminal_tool_attempt.is_some()
                    || terminal_disposition.is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("active scheduling lifecycle").into(),
                    );
                }
                let phase = match active_phase.as_deref() {
                    Some("awaiting_runner_recovery")
                        if current_attempt.is_none()
                            && recovery_model_call.is_none()
                            && approval_tool_request.is_none()
                            && recovery_tool_attempt.is_none()
                            && child_wait_request.is_none() =>
                    {
                        let runner = runner_recovery_runner
                            .ok_or(SubmitInputCorruption::Missing("runner_recovery_runner_id"))?;
                        let revision = runner_recovery_revision.ok_or(
                            SubmitInputCorruption::Missing("runner_recovery_placement_revision"),
                        )?;
                        let revision = positive_u64_from_numeric(revision)
                            .ok()
                            .and_then(RunnerGeneration::try_from_u64)
                            .ok_or(SubmitInputCorruption::Inconsistent(
                                "runner recovery placement revision",
                            ))?;
                        let source_snapshot = load_runner_recovery_source_snapshot(
                            connection,
                            lifecycle_session,
                            lifecycle_turn,
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                        .ok_or(SubmitInputCorruption::Inconsistent(
                            "runner recovery source snapshot missing",
                        ))?;
                        required_frontiers
                            .insert(source_snapshot.frontier().snapshot().into_uuid());
                        ActiveTurnSchedulingReconstitutionInput::awaiting_runner_recovery(
                            lifecycle_turn,
                            RunnerId::from_uuid(runner),
                            revision,
                            runner_recovery_tool_attempt.map(ToolAttemptId::from_uuid),
                            Some(source_snapshot.frontier().snapshot()),
                        )
                    }
                    Some("running") if recovery_model_call.is_none() => {
                        if approval_tool_request.is_some() || recovery_tool_attempt.is_some() {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "running tool phase references",
                            )
                            .into());
                        }
                        let attempt_id = TurnAttemptId::from_uuid(
                            current_attempt
                                .ok_or(SubmitInputCorruption::Missing("current_attempt_id"))?,
                        );
                        require_current_attempt_row(
                            &row,
                            lifecycle_session,
                            lifecycle_turn,
                            attempt_id,
                        )?;
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if end_variant.is_some() || end_disposition.is_some() {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "active live attempt end",
                            )
                            .into());
                        }
                        let mut phase = match attempt_state.as_str() {
                            "prepared" => ActiveTurnSchedulingReconstitutionInput::prepared(
                                lifecycle_turn,
                                attempt_id,
                            ),
                            "running" => ActiveTurnSchedulingReconstitutionInput::running(
                                lifecycle_turn,
                                attempt_id,
                            ),
                            "stop_requested" => {
                                let call = required::<Uuid>(&row, "stop_requested_model_call_id")?;
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                required_model_calls.insert(call);
                                ActiveTurnSchedulingReconstitutionInput::stop_requested(
                                    lifecycle_turn,
                                    attempt_id,
                                    ModelCallId::from_uuid(call),
                                    interrupt,
                                )
                            }
                            value => {
                                return Err(SubmitInputCorruption::Unsupported {
                                    field: "active attempt state_kind",
                                    value: value.to_owned(),
                                }
                                .into());
                            }
                        };
                        if let Some(round_call) = active_tool_round {
                            let batch = load_active_batch_from_connection(
                                connection,
                                lifecycle_session,
                                lifecycle_turn,
                            )
                            .await
                            .map_err(map_tool_loop_error)?
                            .ok_or(SubmitInputCorruption::Missing("active tool batch"))?;
                            if batch.producing_call().into_uuid() != round_call
                                || !matches!(
                                    batch.phase(),
                                    signalbox_domain::ToolBatchPhase::Executing {
                                        turn_attempt
                                    } if turn_attempt == attempt_id
                                )
                            {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "running tool batch",
                                )
                                .into());
                            }
                            required_frontiers
                                .insert(batch.yielded_snapshot().frontier().snapshot().into_uuid());
                            required_model_calls.insert(round_call);
                            phase = phase.with_executing_tool_batch(&batch);
                        }
                        phase
                    }
                    Some("awaiting_model_call_recovery")
                        if active_tool_round.is_none()
                            && approval_tool_request.is_none()
                            && recovery_tool_attempt.is_none() =>
                    {
                        let recovery_call = recovery_model_call
                            .ok_or(SubmitInputCorruption::Missing("recovery_model_call_id"))?;
                        let attempt_id = TurnAttemptId::from_uuid(
                            current_attempt
                                .ok_or(SubmitInputCorruption::Missing("current_attempt_id"))?,
                        );
                        require_current_attempt_row(
                            &row,
                            lifecycle_session,
                            lifecycle_turn,
                            attempt_id,
                        )?;
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if attempt_state != "ended" {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "model-call recovery attempt end",
                            )
                            .into());
                        }
                        required_model_calls.insert(recovery_call);
                        named_continuation_gate_calls.insert(recovery_call);
                        match (end_variant.as_deref(), end_disposition.as_deref()) {
                            (Some("without_stop"), Some("ambiguous")) => ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery(
                                lifecycle_turn,
                                attempt_id,
                                ModelCallId::from_uuid(recovery_call),
                            ),
                            (Some("without_stop"), Some("lost")) => ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_restart(
                                lifecycle_turn,
                                attempt_id,
                                ModelCallId::from_uuid(recovery_call),
                            ),
                            (Some("after_cancellation"), Some("ambiguous")) => {
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_cancellation(
                                    lifecycle_turn,
                                    attempt_id,
                                    ModelCallId::from_uuid(recovery_call),
                                    interrupt,
                                )
                            }
                            (Some("after_cancellation"), Some("lost")) => {
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_cancellation_restart(
                                    lifecycle_turn,
                                    attempt_id,
                                    ModelCallId::from_uuid(recovery_call),
                                    interrupt,
                                )
                            }
                            (None, _) | (_, None) => {
                                return Err(SubmitInputCorruption::Missing(
                                    "model-call recovery attempt end",
                                )
                                .into());
                            }
                            (Some(value), Some(_)) => {
                                return Err(SubmitInputCorruption::Unsupported {
                                    field: "model-call recovery attempt end_variant",
                                    value: value.to_owned(),
                                }
                                .into());
                            }
                        }
                    }
                    Some("awaiting_tool_approval")
                        if recovery_model_call.is_none()
                            && recovery_tool_attempt.is_none()
                            && current_attempt.is_none() =>
                    {
                        let round_call = active_tool_round
                            .ok_or(SubmitInputCorruption::Missing("active_tool_round_call_id"))?;
                        let request = approval_tool_request
                            .ok_or(SubmitInputCorruption::Missing("approval_tool_request_id"))?;
                        if row.try_get::<Option<Uuid>, _>("turn_attempt_id")?.is_some() {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "approval wait attempt",
                            )
                            .into());
                        }
                        let batch = load_active_batch_from_connection(
                            connection,
                            lifecycle_session,
                            lifecycle_turn,
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                        .ok_or(SubmitInputCorruption::Missing("active tool batch"))?;
                        batch
                            .awaiting_approval()
                            .filter(|wait| wait.request().into_uuid() == request)
                            .ok_or(SubmitInputCorruption::Inconsistent(
                                "tool approval wait evidence",
                            ))?;
                        required_frontiers
                            .insert(batch.yielded_snapshot().frontier().snapshot().into_uuid());
                        required_model_calls.insert(round_call);
                        ActiveTurnSchedulingReconstitutionInput::awaiting_approval(
                            lifecycle_turn,
                            &batch,
                        )
                        .ok_or(SubmitInputCorruption::Inconsistent(
                            "tool approval batch evidence",
                        ))?
                    }
                    Some("awaiting_tool_recovery")
                        if recovery_model_call.is_none() && approval_tool_request.is_none() =>
                    {
                        let round_call = active_tool_round
                            .ok_or(SubmitInputCorruption::Missing("active_tool_round_call_id"))?;
                        let recovery_attempt = recovery_tool_attempt
                            .ok_or(SubmitInputCorruption::Missing("recovery_tool_attempt_id"))?;
                        let attempt_id = TurnAttemptId::from_uuid(
                            current_attempt
                                .ok_or(SubmitInputCorruption::Missing("current_attempt_id"))?,
                        );
                        require_current_attempt_row(
                            &row,
                            lifecycle_session,
                            lifecycle_turn,
                            attempt_id,
                        )?;
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if attempt_state != "ended" {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "tool recovery turn-attempt end",
                            )
                            .into());
                        }
                        let batch = load_active_batch_from_connection(
                            connection,
                            lifecycle_session,
                            lifecycle_turn,
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                        .ok_or(SubmitInputCorruption::Missing("active tool batch"))?;
                        let wait = batch
                            .awaiting_recovery()
                            .filter(|wait| wait.attempt().into_uuid() == recovery_attempt)
                            .ok_or(SubmitInputCorruption::Inconsistent(
                                "tool recovery wait evidence",
                            ))?;
                        required_model_calls.insert(round_call);
                        required_frontiers.insert(wait.yielded_frontier().into_uuid());
                        match (end_variant.as_deref(), end_disposition.as_deref()) {
                            (Some("without_stop"), Some("ambiguous")) => {
                                ActiveTurnSchedulingReconstitutionInput::awaiting_tool_recovery(
                                    lifecycle_turn,
                                    attempt_id,
                                    wait,
                                )
                            }
                            (Some("without_stop"), Some("lost")) => {
                                ActiveTurnSchedulingReconstitutionInput::awaiting_tool_recovery_after_restart(
                                    lifecycle_turn,
                                    attempt_id,
                                    wait,
                                )
                            }
                            (Some("after_cancellation"), Some("ambiguous")) => {
                                ActiveTurnSchedulingReconstitutionInput::awaiting_tool_recovery_after_cancellation(
                                    lifecycle_turn,
                                    attempt_id,
                                    wait,
                                    require_applied_interrupt_from_attempt(
                                        &row,
                                        lifecycle_turn,
                                        &recorded_commands,
                                    )?,
                                )
                            }
                            (Some("after_cancellation"), Some("lost")) => {
                                ActiveTurnSchedulingReconstitutionInput::awaiting_tool_recovery_after_cancellation_restart(
                                    lifecycle_turn,
                                    attempt_id,
                                    wait,
                                    require_applied_interrupt_from_attempt(
                                        &row,
                                        lifecycle_turn,
                                        &recorded_commands,
                                    )?,
                                )
                            }
                            _ => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "tool recovery attempt end",
                                )
                                .into());
                            }
                        }
                    }
                    Some("awaiting_child")
                        if current_attempt.is_none()
                            && recovery_model_call.is_none()
                            && approval_tool_request.is_none()
                            && recovery_tool_attempt.is_none() =>
                    {
                        let round_call = active_tool_round
                            .ok_or(SubmitInputCorruption::Missing("active_tool_round_call_id"))?;
                        let awaiting_request = child_wait_request
                            .ok_or(SubmitInputCorruption::Missing("child_wait_request_id"))?;
                        let batch = load_active_batch_from_connection(
                            connection,
                            lifecycle_session,
                            lifecycle_turn,
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                        .ok_or(SubmitInputCorruption::Missing("active tool batch"))?;
                        if batch.producing_call().into_uuid() != round_call
                            || !matches!(
                                batch.phase(),
                                signalbox_domain::ToolBatchPhase::AwaitingChild { request, .. }
                                    if request.into_uuid() == awaiting_request
                            )
                        {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "foreground child wait evidence",
                            )
                            .into());
                        }
                        required_frontiers
                            .insert(batch.yielded_snapshot().frontier().snapshot().into_uuid());
                        required_model_calls.insert(round_call);
                        ActiveTurnSchedulingReconstitutionInput::awaiting_child(
                            lifecycle_turn,
                            &batch,
                        )
                        .ok_or(SubmitInputCorruption::Inconsistent(
                            "foreground child wait batch evidence",
                        ))?
                    }
                    Some(value) => {
                        return Err(SubmitInputCorruption::Unsupported {
                            field: "active phase kind",
                            value: value.to_owned(),
                        }
                        .into());
                    }
                    None => {
                        return Err(SubmitInputCorruption::Missing("active_phase_kind").into());
                    }
                };
                let starting_frontier = starting_frontier
                    .ok_or(SubmitInputCorruption::Missing("starting_frontier_id"))?;
                required_frontiers.insert(starting_frontier);
                AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: decode_starting_lineage(lineage_kind, predecessor)?,
                    starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                    phase,
                }
            }
            "terminal" => {
                if active_phase.is_some()
                    || current_attempt.is_some()
                    || recovery_model_call.is_some()
                    || active_tool_round.is_some()
                    || approval_tool_request.is_some()
                    || recovery_tool_attempt.is_some()
                    || child_wait_request.is_some()
                {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "terminal scheduling lifecycle",
                    )
                    .into());
                }
                let starting_frontier = starting_frontier
                    .ok_or(SubmitInputCorruption::Missing("starting_frontier_id"))?;
                let terminal_frontier = terminal_frontier
                    .ok_or(SubmitInputCorruption::Missing("terminal_frontier_id"))?;
                required_frontiers.insert(starting_frontier);
                required_frontiers.insert(terminal_frontier);
                let starting_lineage = decode_starting_lineage(lineage_kind, predecessor)?;
                if terminal_tool_attempt.is_some()
                    && terminal_disposition.as_deref() != Some("reconciliation_required")
                {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "terminal tool attempt disposition",
                    )
                    .into());
                }
                match terminal_disposition.as_deref() {
                    Some("failed") => {
                        let terminal_execution = match (terminal_attempt, terminal_model_call) {
                            (None, None) => None,
                            (Some(terminal_attempt), terminal_call) => {
                                let stored_attempt_id =
                                    TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?);
                                let attempt_turn =
                                    turn_id_from_uuid(required(&row, "attempt_turn_id")?);
                                let attempt_session =
                                    session_id_from_uuid(required(&row, "attempt_session_id")?);
                                let attempt_state: String = required(&row, "attempt_state_kind")?;
                                let end_variant: Option<String> = row.try_get("end_variant")?;
                                let end_disposition: Option<String> =
                                    row.try_get("end_disposition")?;
                                if stored_attempt_id.into_uuid() != terminal_attempt
                                    || attempt_turn != lifecycle_turn
                                    || attempt_session != lifecycle_session
                                    || attempt_state != "ended"
                                {
                                    return Err(SubmitInputCorruption::Inconsistent(
                                        "failed terminal attempt",
                                    )
                                    .into());
                                }
                                let ended_call = terminal_call.map(|call| {
                                    required_model_calls.insert(call);
                                    ModelCallId::from_uuid(call)
                                });
                                // A failed turn's terminal frontier can close
                                // a tool round whether the stored provenance
                                // names no call (a round loss or a
                                // denial-only closure) or the round's
                                // continuation call (a provider failure or a
                                // lost prepared call at the continuation
                                // boundary), so the result evidence loads for
                                // both shapes.
                                let failed_terminal_frontier =
                                    ContextFrontierId::from_uuid(terminal_frontier);
                                let terminal_tool_attempts = load_terminal_result_attempts(
                                    connection,
                                    lifecycle_session,
                                    lifecycle_turn,
                                    failed_terminal_frontier,
                                )
                                .await
                                .map_err(map_tool_loop_error)?;
                                let terminal_tool_denials = load_terminal_result_denials(
                                    connection,
                                    lifecycle_session,
                                    lifecycle_turn,
                                    failed_terminal_frontier,
                                )
                                .await
                                .map_err(map_tool_loop_error)?;
                                let execution = match (
                                    end_variant.as_deref(),
                                    end_disposition.as_deref(),
                                    ended_call,
                                ) {
                                    (Some("without_stop"), Some("known_failure"), Some(call)) => {
                                        FailedTurnExecutionReconstitutionInput::with_call(
                                            lifecycle_turn,
                                            stored_attempt_id,
                                            UnstoppedAttemptDisposition::KnownFailure,
                                            call,
                                        )
                                    }
                                    (Some("without_stop"), Some("known_failure"), None) => {
                                        FailedTurnExecutionReconstitutionInput::attempt_only(
                                            lifecycle_turn,
                                            stored_attempt_id,
                                            UnstoppedAttemptDisposition::KnownFailure,
                                        )
                                    }
                                    (Some("without_stop"), Some("lost"), Some(call)) => {
                                        FailedTurnExecutionReconstitutionInput::with_call(
                                            lifecycle_turn,
                                            stored_attempt_id,
                                            UnstoppedAttemptDisposition::Lost,
                                            call,
                                        )
                                    }
                                    (Some("without_stop"), Some("lost"), None) => {
                                        FailedTurnExecutionReconstitutionInput::attempt_only(
                                            lifecycle_turn,
                                            stored_attempt_id,
                                            UnstoppedAttemptDisposition::Lost,
                                        )
                                    }
                                    (
                                        Some("after_cancellation"),
                                        Some("known_failure"),
                                        ended_call,
                                    ) => {
                                        let interrupt = require_applied_interrupt_from_attempt(
                                            &row,
                                            lifecycle_turn,
                                            &recorded_commands,
                                        )?;
                                        match ended_call {
                                            Some(call) => FailedTurnExecutionReconstitutionInput::with_call_after_cancellation(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                CancellationStopDisposition::KnownFailure,
                                                interrupt,
                                                call,
                                            ),
                                            None => FailedTurnExecutionReconstitutionInput::attempt_only_after_cancellation(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                CancellationStopDisposition::KnownFailure,
                                                interrupt,
                                            ),
                                        }
                                    }
                                    (Some("after_cancellation"), Some("lost"), ended_call) => {
                                        let interrupt = require_applied_interrupt_from_attempt(
                                            &row,
                                            lifecycle_turn,
                                            &recorded_commands,
                                        )?;
                                        match ended_call {
                                            Some(call) => FailedTurnExecutionReconstitutionInput::with_call_after_cancellation(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                CancellationStopDisposition::Lost,
                                                interrupt,
                                                call,
                                            ),
                                            None => FailedTurnExecutionReconstitutionInput::attempt_only_after_cancellation(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                CancellationStopDisposition::Lost,
                                                interrupt,
                                            ),
                                        }
                                    }
                                    _ => {
                                        return Err(SubmitInputCorruption::Inconsistent(
                                            "failed terminal attempt disposition",
                                        )
                                        .into());
                                    }
                                };
                                Some(
                                    execution
                                        .with_terminal_tool_attempts(terminal_tool_attempts)
                                        .with_terminal_tool_denials(terminal_tool_denials),
                                )
                            }
                            (None, Some(_)) => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "failed terminal call without attempt",
                                )
                                .into());
                            }
                        };
                        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                            starting_lineage,
                            starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                            terminal_execution,
                            terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                        }
                    }
                    Some("cancelled") => {
                        let terminal_attempt = terminal_attempt
                            .ok_or(SubmitInputCorruption::Missing("terminal_attempt_id"))?;
                        let stored_attempt_id =
                            TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?);
                        let attempt_turn = turn_id_from_uuid(required(&row, "attempt_turn_id")?);
                        let attempt_session =
                            session_id_from_uuid(required(&row, "attempt_session_id")?);
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if stored_attempt_id.into_uuid() != terminal_attempt
                            || attempt_turn != lifecycle_turn
                            || attempt_session != lifecycle_session
                            || attempt_state != "ended"
                        {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "cancelled terminal attempt",
                            )
                            .into());
                        }
                        let (attempt_end, interrupt) = match (
                            end_variant.as_deref(),
                            end_disposition.as_deref(),
                        ) {
                            (Some("after_cancellation"), Some("cancelled")) => {
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                (
                                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                                        CancellationStopDisposition::Cancelled,
                                        interrupt,
                                    ),
                                    interrupt,
                                )
                            }
                            (Some("without_stop"), Some("yielded_to_durable_wait")) => {
                                let interrupted_tool_attempt =
                                    row.try_get("runner_recovery_interrupted_tool_attempt_id")?;
                                let interrupt = require_applied_runner_recovery_interrupt(
                                    &row,
                                    lifecycle_turn,
                                    stored_attempt_id,
                                    interrupted_tool_attempt,
                                    &recorded_commands,
                                )?;
                                (
                                    TerminalAttemptEndReconstitutionInput::yielded_to_runner_recovery(
                                        interrupt,
                                    ),
                                    interrupt,
                                )
                            }
                            _ => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "cancelled terminal attempt disposition",
                                )
                                .into());
                            }
                        };
                        let ended_call = terminal_model_call.map(ModelCallId::from_uuid);
                        if let Some(call) = terminal_model_call {
                            required_model_calls.insert(call);
                        }
                        // A cancelled turn's terminal frontier can close a tool
                        // round whether the stored provenance names no call (a
                        // batch interrupt) or the round's completed producing
                        // call (a stop racing a tool-using response), so the
                        // result evidence loads for both shapes.
                        let cancelled_terminal_frontier =
                            ContextFrontierId::from_uuid(terminal_frontier);
                        let terminal_tool_attempts = load_terminal_result_attempts(
                            connection,
                            lifecycle_session,
                            lifecycle_turn,
                            cancelled_terminal_frontier,
                        )
                        .await
                        .map_err(map_tool_loop_error)?;
                        let terminal_tool_denials = load_terminal_result_denials(
                            connection,
                            lifecycle_session,
                            lifecycle_turn,
                            cancelled_terminal_frontier,
                        )
                        .await
                        .map_err(map_tool_loop_error)?;
                        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                            starting_lineage,
                            starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                                lifecycle_turn,
                                stored_attempt_id,
                                attempt_end,
                                ended_call,
                                interrupt,
                            )
                            .with_terminal_tool_attempts(terminal_tool_attempts)
                            .with_terminal_tool_denials(terminal_tool_denials),
                            terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                        }
                    }
                    Some("reconciliation_required") => {
                        let terminal_attempt = terminal_attempt
                            .ok_or(SubmitInputCorruption::Missing("terminal_attempt_id"))?;
                        let stored_attempt_id =
                            TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?);
                        let attempt_turn = turn_id_from_uuid(required(&row, "attempt_turn_id")?);
                        let attempt_session =
                            session_id_from_uuid(required(&row, "attempt_session_id")?);
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if stored_attempt_id.into_uuid() != terminal_attempt
                            || attempt_turn != lifecycle_turn
                            || attempt_session != lifecycle_session
                            || attempt_state != "ended"
                        {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "reconciliation terminal attempt",
                            )
                            .into());
                        }
                        let automatic_authority = match (
                            automatic_reconciliation_model_call,
                            automatic_reconciliation_tool_attempt,
                            automatic_reconciliation_state.as_deref(),
                            automatic_reconciliation_attempt_count,
                        ) {
                            (None, None, None, None) => None,
                            (model_call, tool_attempt, Some("reconciled"), Some(attempts))
                                if model_call == terminal_model_call
                                    && tool_attempt == terminal_tool_attempt
                                    && model_call.is_some() != tool_attempt.is_some() =>
                            {
                                let attempt = u32::try_from(attempts)
                                    .ok()
                                    .and_then(NonZeroU32::new)
                                    .filter(|attempt| attempt.get() <= 5)
                                    .ok_or(SubmitInputCorruption::Inconsistent(
                                        "automatic model-call reconciliation attempt",
                                    ))?;
                                Some(AutomaticReconciliationAuthority::AutomaticRecovery {
                                    attempt,
                                })
                            }
                            (model_call, tool_attempt, Some("superseded"), Some(attempts))
                                if model_call == terminal_model_call
                                    && tool_attempt == terminal_tool_attempt
                                    && model_call.is_some() != tool_attempt.is_some()
                                    && u32::try_from(attempts)
                                        .is_ok_and(|attempts| attempts <= 5) =>
                            {
                                None
                            }
                            _ => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "automatic model-call reconciliation authority",
                                )
                                .into());
                            }
                        };
                        let (authority, reconciling_attempt_end) = match (
                            end_variant.as_deref(),
                            end_disposition.as_deref(),
                        ) {
                            (Some("without_stop"), Some(disposition @ ("ambiguous" | "lost"))) => {
                                let interrupt =
                                    applied_interrupt_for_turn(lifecycle_turn, &recorded_commands)?;
                                let authority = match (interrupt, automatic_authority) {
                                    (Some(interrupt), _) => {
                                        AutomaticReconciliationAuthority::AppliedInterrupt(
                                            interrupt,
                                        )
                                    }
                                    (None, Some(authority)) => authority,
                                    (None, None) => {
                                        return Err(SubmitInputCorruption::Missing(
                                            "model-call reconciliation authority",
                                        )
                                        .into());
                                    }
                                };
                                let attempt_end = if disposition == "ambiguous" {
                                    TerminalAttemptEndReconstitutionInput::without_stop(
                                        UnstoppedAttemptDisposition::Ambiguous,
                                    )
                                } else {
                                    TerminalAttemptEndReconstitutionInput::without_stop(
                                        UnstoppedAttemptDisposition::Lost,
                                    )
                                };
                                (authority, attempt_end)
                            }
                            (Some("after_cancellation"), Some("ambiguous")) => {
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                (
                                    AutomaticReconciliationAuthority::AppliedInterrupt(interrupt),
                                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                                        CancellationStopDisposition::Ambiguous,
                                        interrupt,
                                    ),
                                )
                            }
                            (Some("after_cancellation"), Some("lost")) => {
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                (
                                    AutomaticReconciliationAuthority::AppliedInterrupt(interrupt),
                                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                                        CancellationStopDisposition::Lost,
                                        interrupt,
                                    ),
                                )
                            }
                            (Some("without_stop"), Some("yielded_to_durable_wait")) => {
                                if automatic_authority.is_some() {
                                    return Err(SubmitInputCorruption::Inconsistent(
                                        "automatic authority with runner recovery attempt",
                                    )
                                    .into());
                                }
                                let interrupt = require_applied_runner_recovery_interrupt(
                                    &row,
                                    lifecycle_turn,
                                    stored_attempt_id,
                                    terminal_tool_attempt,
                                    &recorded_commands,
                                )?;
                                (
                                    AutomaticReconciliationAuthority::AppliedInterrupt(interrupt),
                                    TerminalAttemptEndReconstitutionInput::yielded_to_runner_recovery(
                                        interrupt,
                                    ),
                                )
                            }
                            _ => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "reconciliation terminal attempt disposition",
                                )
                                .into());
                            }
                        };
                        match (terminal_model_call, terminal_tool_attempt) {
                            (Some(terminal_call), None) => {
                                required_model_calls.insert(terminal_call);
                                named_continuation_gate_calls.insert(terminal_call);
                                AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
                                    starting_lineage,
                                    starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                                    reconciling_attempt: stored_attempt_id,
                                    reconciling_attempt_end,
                                    ambiguous_call: ModelCallId::from_uuid(terminal_call),
                                    authority,
                                    terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                                }
                            }
                            (None, Some(terminal_tool_attempt)) => {
                                let batch = load_recovery_batch_by_attempt(
                                    connection,
                                    lifecycle_session,
                                    lifecycle_turn,
                                    signalbox_domain::ToolAttemptId::from_uuid(
                                        terminal_tool_attempt,
                                    ),
                                )
                                .await
                                .map_err(map_tool_loop_error)?;
                                let ambiguous_tool = batch.awaiting_recovery().ok_or(
                                    SubmitInputCorruption::Inconsistent(
                                        "terminal tool recovery evidence",
                                    ),
                                )?;
                                if ambiguous_tool.issuing_attempt() != stored_attempt_id {
                                    return Err(SubmitInputCorruption::Inconsistent(
                                        "terminal tool recovery turn attempt",
                                    )
                                    .into());
                                }
                                required_model_calls
                                    .insert(ambiguous_tool.producing_call().into_uuid());
                                required_frontiers
                                    .insert(ambiguous_tool.yielded_frontier().into_uuid());
                                AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
                                    starting_lineage,
                                    starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                                    reconciling_attempt: stored_attempt_id,
                                    reconciling_attempt_end,
                                    tool_batch: batch,
                                    authority,
                                    terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                                }
                            }
                            (Some(_), Some(_)) | (None, None) => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "reconciliation terminal operation",
                                )
                                .into());
                            }
                        }
                    }
                    Some("completed" | "refused") => {
                        let terminal_attempt = terminal_attempt
                            .ok_or(SubmitInputCorruption::Missing("terminal_attempt_id"))?;
                        let terminal_call = terminal_model_call
                            .ok_or(SubmitInputCorruption::Missing("terminal_model_call_id"))?;
                        let stored_attempt_id =
                            TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?);
                        let attempt_turn = turn_id_from_uuid(required(&row, "attempt_turn_id")?);
                        let attempt_session =
                            session_id_from_uuid(required(&row, "attempt_session_id")?);
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if stored_attempt_id.into_uuid() != terminal_attempt
                            || attempt_turn != lifecycle_turn
                            || attempt_session != lifecycle_session
                            || attempt_state != "ended"
                        {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "terminal model-call attempt",
                            )
                            .into());
                        }
                        required_model_calls.insert(terminal_call);
                        match terminal_disposition.as_deref() {
                            Some("completed") => {
                                let completing_attempt_end = match (
                                    end_variant.as_deref(),
                                    end_disposition.as_deref(),
                                ) {
                                    (Some("without_stop"), Some("turn_completed")) => {
                                        TerminalAttemptEndReconstitutionInput::without_stop(
                                            UnstoppedAttemptDisposition::TurnCompleted,
                                        )
                                    }
                                    (Some("without_stop"), Some("lost")) => {
                                        TerminalAttemptEndReconstitutionInput::without_stop(
                                            UnstoppedAttemptDisposition::Lost,
                                        )
                                    }
                                    (Some("after_cancellation"), Some("turn_completed")) => {
                                        TerminalAttemptEndReconstitutionInput::after_cancellation(
                                            CancellationStopDisposition::TurnCompleted,
                                            require_applied_interrupt_from_attempt(
                                                &row,
                                                lifecycle_turn,
                                                &recorded_commands,
                                            )?,
                                        )
                                    }
                                    (Some("after_cancellation"), Some("lost")) => {
                                        TerminalAttemptEndReconstitutionInput::after_cancellation(
                                            CancellationStopDisposition::Lost,
                                            require_applied_interrupt_from_attempt(
                                                &row,
                                                lifecycle_turn,
                                                &recorded_commands,
                                            )?,
                                        )
                                    }
                                    _ => {
                                        return Err(SubmitInputCorruption::Inconsistent(
                                            "terminal model-call disposition",
                                        )
                                        .into());
                                    }
                                };
                                AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                                    starting_lineage,
                                    starting_frontier: ContextFrontierId::from_uuid(
                                        starting_frontier,
                                    ),
                                    completing_attempt: stored_attempt_id,
                                    completing_attempt_end,
                                    completing_call: ModelCallId::from_uuid(terminal_call),
                                    terminal_frontier: ContextFrontierId::from_uuid(
                                        terminal_frontier,
                                    ),
                                }
                            }
                            Some("refused") => {
                                let refusing_attempt_end = match (
                                    end_variant.as_deref(),
                                    end_disposition.as_deref(),
                                ) {
                                    (Some("without_stop"), Some("turn_refused")) => {
                                        TerminalAttemptEndReconstitutionInput::without_stop(
                                            UnstoppedAttemptDisposition::TurnRefused,
                                        )
                                    }
                                    (Some("without_stop"), Some("lost")) => {
                                        TerminalAttemptEndReconstitutionInput::without_stop(
                                            UnstoppedAttemptDisposition::Lost,
                                        )
                                    }
                                    (Some("after_cancellation"), Some("turn_refused")) => {
                                        TerminalAttemptEndReconstitutionInput::after_cancellation(
                                            CancellationStopDisposition::TurnRefused,
                                            require_applied_interrupt_from_attempt(
                                                &row,
                                                lifecycle_turn,
                                                &recorded_commands,
                                            )?,
                                        )
                                    }
                                    (Some("after_cancellation"), Some("lost")) => {
                                        TerminalAttemptEndReconstitutionInput::after_cancellation(
                                            CancellationStopDisposition::Lost,
                                            require_applied_interrupt_from_attempt(
                                                &row,
                                                lifecycle_turn,
                                                &recorded_commands,
                                            )?,
                                        )
                                    }
                                    _ => {
                                        return Err(SubmitInputCorruption::Inconsistent(
                                            "terminal model-call disposition",
                                        )
                                        .into());
                                    }
                                };
                                named_continuation_gate_calls.insert(terminal_call);
                                AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                                    starting_lineage,
                                    starting_frontier: ContextFrontierId::from_uuid(
                                        starting_frontier,
                                    ),
                                    refusing_attempt: stored_attempt_id,
                                    refusing_attempt_end,
                                    refusing_call: ModelCallId::from_uuid(terminal_call),
                                    terminal_frontier: ContextFrontierId::from_uuid(
                                        terminal_frontier,
                                    ),
                                }
                            }
                            _ => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "terminal model-call disposition",
                                )
                                .into());
                            }
                        }
                    }
                    Some(value) => {
                        return Err(SubmitInputCorruption::Unsupported {
                            field: "terminal disposition kind",
                            value: value.to_owned(),
                        }
                        .into());
                    }
                    None => {
                        return Err(
                            SubmitInputCorruption::Missing("terminal_disposition_kind").into()
                        );
                    }
                }
            }
            value => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "turn lifecycle state_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };

        if let Some(identity) = pinned_target
            && pinned_target_identities
                .insert(queued_turn, identity)
                .is_some()
        {
            return Err(SubmitInputCorruption::Inconsistent("duplicate turn target pin").into());
        }

        let mut record = match binding {
            Some(binding) => AcceptedInputTurnSchedulingRecord::reclassified(
                lifecycle_session,
                lifecycle_turn,
                accepted_session,
                accepted_lifecycle,
                queued_session,
                queued_turn,
                AcceptedInputQueueOrder::ordinary(queued_position),
                origin_delivery,
                binding,
                origin_configuration,
                state,
            ),
            None => AcceptedInputTurnSchedulingRecord::new(
                lifecycle_session,
                lifecycle_turn,
                accepted_session,
                accepted_lifecycle,
                queued_session,
                queued_turn,
                queued_order,
                origin_delivery,
                origin_configuration,
                state,
            ),
        };
        if !model_identity_boundary_required {
            record = record.without_legacy_model_identity_boundary();
        }
        turns.push(record);
    }

    let active_acceptance_tail =
        load_active_acceptance_tail(connection, session_id, &turns).await?;

    let consumed_steering_rows = sqlx::query(
        "SELECT accepted.session_id, accepted.accepted_input_id,
                accepted.acceptance_position, accepted.expected_active_turn_id,
                accepted.consuming_model_call_id
           FROM accepted_input AS accepted
           JOIN turn_lifecycle AS source
             ON source.turn_id = accepted.expected_active_turn_id
            AND source.session_id = accepted.session_id
            AND source.origin_kind = 'accepted_input'
          WHERE accepted.session_id = $1
            AND accepted.disposition_kind = 'consumed_as_steering'
          ORDER BY accepted.acceptance_position",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    let mut consumed_steering = Vec::with_capacity(consumed_steering_rows.len());
    let mut consumed_counts_by_call = BTreeMap::<Uuid, u64>::new();
    for row in consumed_steering_rows {
        let call = ModelCallId::from_uuid(required(&row, "consuming_model_call_id")?);
        required_model_calls.insert(call.into_uuid());
        *consumed_counts_by_call.entry(call.into_uuid()).or_default() += 1;
        consumed_steering.push(ConsumedSteeringReconstitutionInput::new(
            session_id_from_uuid(required(&row, "session_id")?),
            AcceptedInputLifecycle::new(
                accepted_input_id_from_uuid(required(&row, "accepted_input_id")?),
                AcceptedInputDisposition::ConsumedAsSteering { call },
            ),
            decode_position(&row, "acceptance_position")?,
            turn_id_from_uuid(required(&row, "expected_active_turn_id")?),
        ));
    }
    let delegated_consumed_steering_rows = sqlx::query(
        "SELECT accepted.session_id, accepted.accepted_input_id,
                accepted.acceptance_position, accepted.expected_active_turn_id,
                accepted.consuming_model_call_id
           FROM accepted_input AS accepted
           JOIN turn_lifecycle AS source
             ON source.turn_id = accepted.expected_active_turn_id
            AND source.session_id = accepted.session_id
            AND source.origin_kind = 'delegation'
          WHERE accepted.session_id = $1
            AND accepted.disposition_kind = 'consumed_as_steering'
          ORDER BY accepted.acceptance_position",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    let mut delegated_consumed_steering =
        Vec::with_capacity(delegated_consumed_steering_rows.len());
    for row in delegated_consumed_steering_rows {
        let call = ModelCallId::from_uuid(required(&row, "consuming_model_call_id")?);
        delegated_consumed_steering.push(ConsumedSteeringReconstitutionInput::new(
            session_id_from_uuid(required(&row, "session_id")?),
            AcceptedInputLifecycle::new(
                accepted_input_id_from_uuid(required(&row, "accepted_input_id")?),
                AcceptedInputDisposition::ConsumedAsSteering { call },
            ),
            decode_position(&row, "acceptance_position")?,
            turn_id_from_uuid(required(&row, "expected_active_turn_id")?),
        ));
    }

    let assistant_model_calls = sqlx::query_scalar::<_, Uuid>(
        "SELECT DISTINCT producing_model_call_id
           FROM semantic_transcript_entry
          WHERE source_session_id = $1
            AND payload_kind IN ('assistant_text', 'provider_compaction', 'assistant_tool_use')
          ORDER BY producing_model_call_id",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    required_model_calls.extend(assistant_model_calls);

    let required_model_call_ids = required_model_calls.iter().copied().collect::<Vec<_>>();
    let model_call_rows = sqlx::query(
        "SELECT
            call.model_call_id,
            call.turn_id,
            call.session_id,
            call.turn_attempt_id,
            call.selection_kind,
            call.direct_model_selection_id,
            call.frozen_model_alias_id,
            call.frozen_alias_selected_direct_id,
            call.resolved_provider_model_identity_id,
            call.context_frontier_id,
            call.state_kind,
            call.terminal_disposition_kind,
            manifest.turn_instruction_manifest_id,
            manifest.boundary_kind AS instruction_manifest_boundary_kind,
            manifest.eligibility_hash_algorithm
                AS instruction_eligibility_hash_algorithm,
            manifest.eligibility_hash AS instruction_eligibility_hash,
            manifest.admitted_set_hash_algorithm
                AS instruction_admitted_set_hash_algorithm,
            manifest.admitted_set_hash AS instruction_admitted_set_hash,
            manifest.manifest_hash_algorithm
                AS instruction_manifest_hash_algorithm,
            manifest.manifest_hash AS instruction_manifest_hash,
            discovery.scan_complete AS instruction_discovery_complete,
            lifecycle.origin_kind AS turn_origin_kind,
            lifecycle.pinned_provider_model_identity_id,
            (attempt.continued_from_attempt_id IS NOT NULL)
                AS continues_prior_attempt
           FROM model_call AS call
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = call.turn_attempt_id
            AND attempt.turn_id = call.turn_id
            AND attempt.session_id = call.session_id
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.turn_id = call.turn_id
            AND lifecycle.session_id = call.session_id
      LEFT JOIN turn_instruction_manifest AS manifest
             ON manifest.turn_instruction_manifest_id = call.turn_instruction_manifest_id
            AND manifest.session_id = call.session_id
            AND manifest.turn_id = call.turn_id
      LEFT JOIN instruction_discovery AS discovery
             ON discovery.instruction_discovery_id = manifest.instruction_discovery_id
          WHERE call.session_id = $1
            AND call.model_call_id = ANY($2)
          ORDER BY call.model_call_id",
    )
    .bind(session_id_to_uuid(session_id))
    .bind(&required_model_call_ids)
    .fetch_all(&mut *connection)
    .await?;
    /// One steering-consuming call whose round evidence must be looked up.
    struct ConsumingCallFacts {
        call: Uuid,
        turn: TurnId,
        call_frontier: Uuid,
        consumed_count: u64,
    }
    /// One gate-named steering-free call whose round evidence must be looked
    /// up.
    struct NamedGateCallFacts {
        call: Uuid,
        turn: TurnId,
        call_frontier: Uuid,
    }
    let mut model_calls = Vec::with_capacity(model_call_rows.len());
    let mut pinned_targets = Vec::with_capacity(model_call_rows.len());
    let mut loaded_pinned_turns = BTreeSet::new();
    let mut loaded_model_calls = BTreeSet::new();
    let mut delegated_turns = BTreeSet::new();
    let mut consuming_call_facts = Vec::new();
    let mut named_gate_call_facts = Vec::new();
    for row in model_call_rows {
        let call_uuid: Uuid = required(&row, "model_call_id")?;
        if !loaded_model_calls.insert(call_uuid) {
            return Err(SubmitInputCorruption::Inconsistent("duplicate model call").into());
        }
        let frontier_uuid: Uuid = required(&row, "context_frontier_id")?;
        let turn_uuid: Uuid = required(&row, "turn_id")?;
        let turn = turn_id_from_uuid(turn_uuid);
        crate::model_execution::authenticate_model_call_instruction_manifest(
            &row, session_id, turn,
        )?;
        let turn_origin_kind: String = required(&row, "turn_origin_kind")?;
        if turn_origin_kind == "delegation" {
            delegated_turns.insert(turn);
        }
        let pinned_identity = match pinned_target_identities.get(&turn).copied() {
            Some(identity) => identity,
            None if turn_origin_kind == "delegation" => {
                required(&row, "pinned_provider_model_identity_id")?
            }
            None => {
                return Err(SubmitInputCorruption::Missing("model call turn target pin").into());
            }
        };
        if loaded_pinned_turns.insert(turn) {
            pinned_targets.push(PinnedProviderTargetReconstitutionInput::new(
                turn,
                ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(pinned_identity)),
            ));
        }
        required_frontiers.insert(frontier_uuid);
        let state_kind: String = required(&row, "state_kind")?;
        let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
        let state = match (state_kind.as_str(), terminal_disposition.as_deref()) {
            ("prepared", None) => ModelCallReconstitutionState::Prepared,
            ("in_flight", None) => ModelCallReconstitutionState::InFlight,
            ("cancellation_requested", None) => ModelCallReconstitutionState::CancellationRequested,
            ("terminal", Some(disposition)) => {
                ModelCallReconstitutionState::Terminal(decode_model_call_disposition(disposition)?)
            }
            ("prepared" | "in_flight" | "cancellation_requested" | "terminal", _) => {
                return Err(SubmitInputCorruption::Inconsistent("model call state payload").into());
            }
            (value, _) => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "model call state_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        // Only a continuation-chain attempt's call can carry a continuation
        // round window, so first-round consumers skip the evidence lookup.
        let continues_prior_attempt: bool = required(&row, "continues_prior_attempt")?;
        if let Some(consumed_count) = consumed_counts_by_call.get(&call_uuid).copied()
            && continues_prior_attempt
        {
            consuming_call_facts.push(ConsumingCallFacts {
                call: call_uuid,
                turn,
                call_frontier: frontier_uuid,
                consumed_count,
            });
        }
        if named_continuation_gate_calls.contains(&call_uuid) && continues_prior_attempt {
            named_gate_call_facts.push(NamedGateCallFacts {
                call: call_uuid,
                turn,
                call_frontier: frontier_uuid,
            });
        }
        model_calls.push(ModelCallReconstitutionInput::new(
            ModelCallId::from_uuid(call_uuid),
            turn,
            TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?),
            decode_frozen_model(
                required(&row, "selection_kind")?,
                row.try_get("direct_model_selection_id")?,
                row.try_get("frozen_model_alias_id")?,
                row.try_get("frozen_alias_selected_direct_id")?,
            )?,
            ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(required(
                &row,
                "resolved_provider_model_identity_id",
            )?)),
            ContextFrontierId::from_uuid(frontier_uuid),
            state,
        ));
    }
    if loaded_model_calls != required_model_calls {
        return Err(SubmitInputCorruption::Missing("scheduling model call").into());
    }

    // A steering-consuming call prepared at a tool-round continuation
    // boundary reconstitutes only together with its round's complete result
    // evidence; a call prepared against its turn's starting frontier has no
    // continuing round window and carries none.
    let mut steering_continuation_rounds = Vec::new();
    for facts in consuming_call_facts {
        let Some((round_tool_attempts, round_tool_denials)) =
            load_steering_continuation_round_evidence(
                connection,
                session_id,
                facts.turn,
                ContextFrontierId::from_uuid(facts.call_frontier),
                facts.consumed_count,
            )
            .await
            .map_err(map_tool_loop_error)?
        else {
            continue;
        };
        steering_continuation_rounds.push(SteeringContinuationRoundReconstitutionInput::new(
            ModelCallId::from_uuid(facts.call),
            round_tool_attempts,
            round_tool_denials,
        ));
    }

    // A steering-free continuation call named by a terminal or recovery gate
    // reconstitutes only together with its round's complete result evidence;
    // a named call prepared against its turn's starting frontier has no
    // continuing round window and carries none.
    let mut continuation_rounds = Vec::new();
    for facts in named_gate_call_facts {
        let Some((round_tool_attempts, round_tool_denials)) = load_continuation_round_evidence(
            connection,
            session_id,
            facts.turn,
            ContextFrontierId::from_uuid(facts.call_frontier),
        )
        .await
        .map_err(map_tool_loop_error)?
        else {
            continue;
        };
        continuation_rounds.push(ContinuationRoundReconstitutionInput::new(
            ModelCallId::from_uuid(facts.call),
            round_tool_attempts,
            round_tool_denials,
        ));
    }

    let compaction_rows = sqlx::query(
        "SELECT
            call.model_call_id,
            call.source_frontier_id AS call_source_frontier_id,
            call.direct_model_selection_id,
            call.resolved_provider_model_identity_id,
            call.state_kind,
            call.terminal_disposition_kind,
            call.input_tokens,
            call.output_tokens,
            call.cache_creation_input_tokens,
            call.cache_read_input_tokens,
            compaction.context_compaction_id,
            compaction.predecessor_compaction_id,
            compaction.source_frontier_id AS compaction_source_frontier_id,
            compaction.result_frontier_id,
            compaction.producing_call_id,
            compaction.first_source_session_id,
            compaction.first_entry_id,
            compaction.through_source_session_id,
            compaction.through_entry_id,
            compaction.summary_entry_id
         FROM context_compaction_model_call AS call
         LEFT JOIN context_compaction AS compaction
           ON compaction.producing_call_id = call.model_call_id
          AND compaction.session_id = call.session_id
        WHERE call.session_id = $1
        ORDER BY call.model_call_id",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    let mut compaction_calls = Vec::with_capacity(compaction_rows.len());
    let mut compactions = Vec::with_capacity(compaction_rows.len());
    for row in compaction_rows {
        let call_id = ModelCallId::from_uuid(required(&row, "model_call_id")?);
        let call_source_frontier_uuid: Uuid = required(&row, "call_source_frontier_id")?;
        required_frontiers.insert(call_source_frontier_uuid);
        let state_kind: String = required(&row, "state_kind")?;
        let disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
        let state = match (state_kind.as_str(), disposition.as_deref()) {
            ("prepared", None) => ContextCompactionModelCallState::Prepared,
            ("in_flight", None) => ContextCompactionModelCallState::InFlight,
            ("terminal", Some(value)) => {
                ContextCompactionModelCallState::Terminal(decode_model_call_disposition(value)?)
            }
            ("prepared" | "in_flight" | "terminal", _) => {
                return Err(SubmitInputCorruption::Inconsistent(
                    "compaction model call state payload",
                )
                .into());
            }
            (value, _) => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "compaction model call state_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        let usage = ContextCompactionTokenUsage::unreported()
            .with_input_tokens(decode_optional_token_count(&row, "input_tokens")?)
            .with_output_tokens(decode_optional_token_count(&row, "output_tokens")?)
            .with_cache_creation_input_tokens(decode_optional_token_count(
                &row,
                "cache_creation_input_tokens",
            )?)
            .with_cache_read_input_tokens(decode_optional_token_count(
                &row,
                "cache_read_input_tokens",
            )?);
        compaction_calls.push(ContextCompactionModelCallReconstitutionInput::new(
            call_id,
            session_id,
            DirectModelSelection::from_uuid(required(&row, "direct_model_selection_id")?),
            ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(required(
                &row,
                "resolved_provider_model_identity_id",
            )?)),
            ContextFrontierId::from_uuid(call_source_frontier_uuid),
            state,
            usage,
        ));
        let Some(compaction_uuid) = row.try_get::<Option<Uuid>, _>("context_compaction_id")? else {
            continue;
        };
        let compaction_id = ContextCompactionId::from_uuid(compaction_uuid);
        let producing_call = ModelCallId::from_uuid(required(&row, "producing_call_id")?);
        let source_frontier_uuid: Uuid = required(&row, "compaction_source_frontier_id")?;
        let result_frontier_uuid: Uuid = required(&row, "result_frontier_id")?;
        required_frontiers.insert(source_frontier_uuid);
        required_frontiers.insert(result_frontier_uuid);
        let first = SemanticTranscriptEntryRef::from_source(
            session_id_from_uuid(required(&row, "first_source_session_id")?),
            SemanticTranscriptEntryId::from_uuid(required(&row, "first_entry_id")?),
        );
        let through = SemanticTranscriptEntryRef::from_source(
            session_id_from_uuid(required(&row, "through_source_session_id")?),
            SemanticTranscriptEntryId::from_uuid(required(&row, "through_entry_id")?),
        );
        let predecessor: Option<Uuid> = row.try_get("predecessor_compaction_id")?;
        compactions.push(ContextCompactionReconstitutionInput::new(
            compaction_id,
            session_id,
            predecessor.map(ContextCompactionId::from_uuid),
            ContextFrontierId::from_uuid(source_frontier_uuid),
            ContextFrontierId::from_uuid(result_frontier_uuid),
            producing_call,
            ContextCompactionRange::inclusive(first, through),
            SemanticTranscriptEntryId::from_uuid(required(&row, "summary_entry_id")?),
        ));
    }

    #[derive(sqlx::FromRow)]
    struct PrecedingNonAcceptedTerminalRow {
        turn_id: Uuid,
        successor_turn_id: Uuid,
        terminal_frontier_id: Uuid,
        direct_selection_id: Uuid,
    }
    let preceding_non_accepted_terminals = sqlx::query_as::<_, PrecedingNonAcceptedTerminalRow>(
        "SELECT terminal.turn_id AS turn_id,
                queued.turn_id AS successor_turn_id,
                turn_lifecycle_effective_terminal_frontier(
                    terminal.session_id, terminal.turn_id
                ) AS terminal_frontier_id,
                effective.direct_selection_id AS direct_selection_id
           FROM queued_input_origin AS queued
           JOIN turn_lifecycle AS terminal
             ON terminal.session_id = $1
            AND terminal.turn_id = CASE queued.priority_kind
                    WHEN 'interrupt_immediately_after'
                        THEN queued.interrupt_predecessor_turn_id
                    WHEN 'ordinary'
                        THEN accepted_input_turn_queue_predecessor(
                            queued.session_id, queued.turn_id
                        )
                END
           JOIN LATERAL turn_origin_effective_model_configuration(
                terminal.turn_id, terminal.session_id
           ) AS effective ON true
          WHERE queued.session_id = $1
            AND goal_turn_is_runtime_relevant(
                    queued.session_id, queued.turn_id
                )
            AND terminal.origin_kind = 'delegation'
            AND (
                queued.priority_kind = 'interrupt_immediately_after'
                OR EXISTS (
                    SELECT 1
                      FROM session_delegation_initial_task AS initial_task
                     WHERE initial_task.child_session_id = terminal.session_id
                       AND initial_task.turn_id = terminal.turn_id
                )
                OR EXISTS (
                    SELECT 1
                      FROM session_delegation_wake_turn_origin AS wake
                     WHERE wake.recipient_session_id = terminal.session_id
                       AND wake.turn_id = terminal.turn_id
                )
            )
            AND (
                terminal.state_kind = 'terminal'
                OR terminal.delegation_runtime_terminal
            )
            AND turn_lifecycle_effective_terminal_frontier(
                    terminal.session_id, terminal.turn_id
                ) IS NOT NULL
          ORDER BY queued.acceptance_position",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    for preceding in &preceding_non_accepted_terminals {
        delegated_turns.insert(turn_id_from_uuid(preceding.turn_id));
        required_frontiers.insert(preceding.terminal_frontier_id);
    }

    let semantic_delegated_turns = sqlx::query_scalar::<_, Uuid>(
        "SELECT DISTINCT subject.turn_id
           FROM (
                SELECT CASE entry.payload_kind
                         WHEN 'model_identity_changed'
                           THEN entry.model_identity_turn_id
                         WHEN 'turn_completed' THEN entry.completed_turn_id
                         WHEN 'turn_failed' THEN entry.failed_turn_id
                         WHEN 'turn_cancelled' THEN entry.cancelled_turn_id
                       END AS turn_id
                  FROM semantic_transcript_entry AS entry
                 WHERE entry.source_session_id = $1
                   AND entry.payload_kind IN (
                        'model_identity_changed',
                        'turn_completed',
                        'turn_failed',
                        'turn_cancelled'
                   )
           ) AS subject
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.session_id = $1
            AND lifecycle.turn_id = subject.turn_id
            AND lifecycle.origin_kind = 'delegation'
            AND (
                lifecycle.state_kind = 'terminal'
                OR lifecycle.delegation_runtime_terminal
            )
          ORDER BY subject.turn_id",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    delegated_turns.extend(semantic_delegated_turns.into_iter().map(turn_id_from_uuid));

    let delegated_turn_ids = delegated_turns
        .iter()
        .map(|turn| turn.into_uuid())
        .collect::<Vec<_>>();
    let delegated_turn_rows = sqlx::query(
        "SELECT lifecycle.turn_id,
                effective.defaults_version,
                effective.direct_selection_id,
                lifecycle.state_kind,
                lifecycle.terminal_disposition_kind,
                lifecycle.delegation_runtime_terminal
           FROM turn_lifecycle AS lifecycle
           JOIN LATERAL turn_origin_effective_model_configuration(
                lifecycle.turn_id, lifecycle.session_id
           ) AS effective ON true
          WHERE lifecycle.session_id = $1
            AND lifecycle.origin_kind = 'delegation'
            AND lifecycle.turn_id = ANY($2)
          ORDER BY lifecycle.turn_id",
    )
    .bind(session_id_to_uuid(session_id))
    .bind(&delegated_turn_ids)
    .fetch_all(&mut *connection)
    .await?;
    let mut delegated_turn_facts = Vec::with_capacity(delegated_turn_rows.len());
    for row in delegated_turn_rows {
        let turn = turn_id_from_uuid(required(&row, "turn_id")?);
        let defaults_version =
            defaults_version_from_numeric(required(&row, "defaults_version")?)
                .map_err(|_| SubmitInputCorruption::Inconsistent("delegated defaults version"))?;
        let selected = DirectModelSelection::from_uuid(required(&row, "direct_selection_id")?);
        let state_kind: String = required(&row, "state_kind")?;
        let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
        let runtime_terminal: bool = required(&row, "delegation_runtime_terminal")?;
        let state = decode_delegated_turn_scheduling_state(
            &state_kind,
            terminal_disposition.as_deref(),
            runtime_terminal,
        )?;
        delegated_turn_facts.push(DelegatedTurnSchedulingFact::new(
            turn,
            defaults_version,
            selected,
            state,
        ));
    }
    if delegated_turn_facts.len() != delegated_turn_ids.len() {
        return Err(SubmitInputCorruption::Missing("delegated turn scheduling fact").into());
    }

    let scheduling_frontier_ids = required_frontiers.iter().copied().collect::<Vec<_>>();
    required_frontiers.extend(
        supplemental_semantic_frontiers
            .iter()
            .map(|frontier| frontier.into_uuid()),
    );
    let required_frontier_ids = required_frontiers.iter().copied().collect::<Vec<_>>();
    let frontier_rows = sqlx::query(
        "WITH RECURSIVE frontier_ids (context_frontier_id) AS (
            SELECT required.context_frontier_id
              FROM UNNEST($2::uuid[]) AS required(context_frontier_id)
            UNION
            SELECT frontier.prefix_context_frontier_id
              FROM frontier_ids
              JOIN context_frontier AS frontier
                ON frontier.owning_session_id = $1
               AND frontier.context_frontier_id =
                       frontier_ids.context_frontier_id
             WHERE frontier.prefix_context_frontier_id IS NOT NULL
        )
        SELECT
            frontier.context_frontier_id,
            frontier.prefix_context_frontier_id,
            frontier.member_count,
            delta.member_position,
            delta.source_session_id,
            delta.semantic_entry_id
          FROM frontier_ids
          JOIN context_frontier AS frontier
            ON frontier.owning_session_id = $1
           AND frontier.context_frontier_id =
                   frontier_ids.context_frontier_id
          LEFT JOIN context_frontier_delta AS delta
            ON delta.owning_session_id = frontier.owning_session_id
           AND delta.context_frontier_id =
                   frontier.context_frontier_id
         ORDER BY frontier.context_frontier_id, delta.member_position",
    )
    .bind(session_id_to_uuid(session_id))
    .bind(&required_frontier_ids)
    .fetch_all(&mut *connection)
    .await?;
    struct StoredFrontierDelta {
        prefix: Option<Uuid>,
        declared_count: Decimal,
        members: Vec<(Decimal, SessionId, SemanticTranscriptEntryId)>,
    }
    let mut stored_frontiers = BTreeMap::<Uuid, StoredFrontierDelta>::new();
    let mut required_semantic_entries = BTreeSet::new();
    for row in frontier_rows {
        let frontier: Uuid = required(&row, "context_frontier_id")?;
        let prefix: Option<Uuid> = row.try_get("prefix_context_frontier_id")?;
        let declared_count: Decimal = required(&row, "member_count")?;
        let position: Option<Decimal> = row.try_get("member_position")?;
        let source_session: Option<Uuid> = row.try_get("source_session_id")?;
        let semantic_entry: Option<Uuid> = row.try_get("semantic_entry_id")?;
        let stored = stored_frontiers
            .entry(frontier)
            .or_insert_with(|| StoredFrontierDelta {
                prefix,
                declared_count,
                members: Vec::new(),
            });
        if stored.prefix != prefix || stored.declared_count != declared_count {
            return Err(
                SubmitInputCorruption::Inconsistent("context frontier repeated header").into(),
            );
        }
        match (position, source_session, semantic_entry) {
            (Some(position), Some(source_session), Some(semantic_entry)) => {
                required_semantic_entries.insert((source_session, semantic_entry));
                stored.members.push((
                    position,
                    session_id_from_uuid(source_session),
                    SemanticTranscriptEntryId::from_uuid(semantic_entry),
                ));
            }
            (None, None, None) => {}
            _ => {
                return Err(
                    SubmitInputCorruption::Inconsistent("context frontier delta row").into(),
                );
            }
        }
    }

    let semantic_source_sessions = required_semantic_entries
        .iter()
        .map(|(source_session, _)| *source_session)
        .collect::<Vec<_>>();
    let semantic_entry_ids = required_semantic_entries
        .iter()
        .map(|(_, semantic_entry)| *semantic_entry)
        .collect::<Vec<_>>();
    let semantic_rows = sqlx::query(
        "SELECT
            entry.source_session_id,
            entry.semantic_entry_id,
            entry.payload_kind,
            entry.origin_accepted_input_id,
            entry.steering_source_turn_id,
            entry.failed_turn_id,
            entry.cancelled_turn_id,
            entry.assistant_text_value,
            entry.producing_model_call_id,
            entry.assistant_tool_request_id,
            entry.tool_result_request_id,
            entry.tool_result_attempt_id,
            entry.completed_turn_id,
            entry.imported_conversation_id,
            entry.imported_transcript_entry_id,
            entry.assistant_response_part_ordinal,
            entry.model_identity_turn_id,
            entry.model_identity_defaults_version,
            entry.model_identity_direct_selection_id,
            entry.context_summary_value,
            entry.context_summary_producing_call_id,
            entry.context_summary_first_source_session_id,
            entry.context_summary_first_entry_id,
            entry.context_summary_through_source_session_id,
            entry.context_summary_through_entry_id,
            entry.delegated_task_spawning_tool_request_id,
            entry.delegation_message_id,
            entry.delegation_result_awaiting_tool_request_id,
            entry.delegation_result_spawning_tool_request_id,
            delegated_task.task_content AS delegated_task_content,
            task_relation.parent_session_id AS delegated_task_parent_session_id,
            task_relation.parent_turn_id AS delegated_task_parent_turn_id,
            delegated_message.spawning_tool_request_id AS delegation_message_spawning_request_id,
            message_delivery.recipient_session_id AS delegation_message_recipient_session_id,
            message_delivery.delivery_sequence AS delegation_message_delivery_sequence,
            delegated_message.content_text AS delegation_message_content,
            CASE delegated_message.direction
                WHEN 'parent_to_child' THEN message_relation.parent_session_id
                WHEN 'child_to_parent' THEN message_relation.child_session_id
            END AS delegation_message_sender_session_id,
            delegated_wait.child_session_id AS delegation_result_child_session_id,
            delegated_wait.wait_mode AS delegation_result_wait_mode,
            result_delivery.delivery_sequence AS delegation_result_delivery_sequence,
            delegated_result.outcome_kind AS delegation_result_outcome_kind,
            delegated_result.content_text AS delegation_result_content,
            result_event.reason_kind AS delegation_result_reason_kind,
            result_event.provenance_kind AS delegation_result_provenance_kind,
            result_event.provenance_session_id AS delegation_result_provenance_session_id,
            result_event.provenance_turn_id AS delegation_result_provenance_turn_id,
            result_event.provenance_goal_generation AS delegation_result_provenance_goal_generation,
            result_event.provenance_command_id AS delegation_result_provenance_command_id
         FROM semantic_transcript_entry AS entry
         LEFT JOIN session_delegation_initial_task AS delegated_task
           ON delegated_task.spawning_tool_request_id =
                  entry.delegated_task_spawning_tool_request_id
          AND entry.payload_kind = 'delegated_task'
          AND delegated_task.child_session_id = entry.source_session_id
          AND delegated_task.semantic_entry_id = entry.semantic_entry_id
         LEFT JOIN session_delegation AS task_relation
           ON task_relation.spawning_tool_request_id =
                  delegated_task.spawning_tool_request_id
          AND entry.payload_kind = 'delegated_task'
         LEFT JOIN session_message_delivery AS message_delivery
           ON message_delivery.message_id = entry.delegation_message_id
          AND entry.payload_kind = 'delegation_message'
          AND message_delivery.recipient_session_id = entry.source_session_id
         LEFT JOIN session_message AS delegated_message
           ON delegated_message.message_id = message_delivery.message_id
          AND entry.payload_kind = 'delegation_message'
          AND delegated_message.spawning_tool_request_id =
                  message_delivery.spawning_tool_request_id
         LEFT JOIN session_delegation AS message_relation
           ON message_relation.spawning_tool_request_id =
                  delegated_message.spawning_tool_request_id
          AND entry.payload_kind = 'delegation_message'
         LEFT JOIN session_child_result_delivery AS result_delivery
           ON result_delivery.awaiting_tool_request_id =
                  entry.delegation_result_awaiting_tool_request_id
          AND entry.payload_kind = 'delegation_result'
          AND result_delivery.spawning_tool_request_id =
                  entry.delegation_result_spawning_tool_request_id
          AND result_delivery.parent_session_id = entry.source_session_id
         LEFT JOIN session_delegation_wait AS delegated_wait
           ON delegated_wait.awaiting_tool_request_id =
                  result_delivery.awaiting_tool_request_id
          AND entry.payload_kind = 'delegation_result'
          AND delegated_wait.spawning_tool_request_id =
                  result_delivery.spawning_tool_request_id
          AND delegated_wait.parent_session_id = result_delivery.parent_session_id
         LEFT JOIN session_child_result AS delegated_result
           ON delegated_result.spawning_tool_request_id =
                  result_delivery.spawning_tool_request_id
          AND entry.payload_kind = 'delegation_result'
         LEFT JOIN session_delegation_event AS result_event
           ON result_event.spawning_tool_request_id =
                  delegated_result.spawning_tool_request_id
          AND entry.payload_kind = 'delegation_result'
          AND result_event.event_ordinal = delegated_result.event_ordinal
          AND result_event.event_kind = delegated_result.event_kind
        WHERE entry.payload_kind <> 'imported_entry'
          AND (
            entry.source_session_id = $3
            OR (entry.source_session_id, entry.semantic_entry_id) IN (
            SELECT required.source_session_id, required.semantic_entry_id
              FROM UNNEST($1::uuid[], $2::uuid[])
                AS required(source_session_id, semantic_entry_id)
            )
        )
        ORDER BY entry.source_session_id, entry.semantic_entry_id",
    )
    .bind(&semantic_source_sessions)
    .bind(&semantic_entry_ids)
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    let mut semantic_entries = Vec::with_capacity(semantic_rows.len());
    let mut loaded_semantic_entries = imported_session
        .as_ref()
        .into_iter()
        .flat_map(|imported| imported.semantic_entries())
        .map(|entry| {
            (
                entry.source_session().into_uuid(),
                entry.identity().into_uuid(),
            )
        })
        .collect::<BTreeSet<_>>();
    for row in semantic_rows {
        let source_session_uuid: Uuid = required(&row, "source_session_id")?;
        let entry_uuid: Uuid = required(&row, "semantic_entry_id")?;
        if !loaded_semantic_entries.insert((source_session_uuid, entry_uuid)) {
            return Err(SubmitInputCorruption::Inconsistent("duplicate semantic entry").into());
        }
        let source_session = session_id_from_uuid(source_session_uuid);
        let entry = SemanticTranscriptEntryId::from_uuid(entry_uuid);
        let payload_kind: String = required(&row, "payload_kind")?;
        let origin: Option<Uuid> = row.try_get("origin_accepted_input_id")?;
        let steering_source_turn: Option<Uuid> = row.try_get("steering_source_turn_id")?;
        let failed_turn: Option<Uuid> = row.try_get("failed_turn_id")?;
        let cancelled_turn: Option<Uuid> = row.try_get("cancelled_turn_id")?;
        let assistant_text: Option<String> = row.try_get("assistant_text_value")?;
        let producing_call: Option<Uuid> = row.try_get("producing_model_call_id")?;
        let tool_request: Option<Uuid> = row.try_get("assistant_tool_request_id")?;
        let tool_result_request: Option<Uuid> = row.try_get("tool_result_request_id")?;
        let tool_result_attempt: Option<Uuid> = row.try_get("tool_result_attempt_id")?;
        let completed_turn: Option<Uuid> = row.try_get("completed_turn_id")?;
        let imported_conversation: Option<Uuid> = row.try_get("imported_conversation_id")?;
        let imported_transcript_entry: Option<Uuid> =
            row.try_get("imported_transcript_entry_id")?;
        let assistant_response_part_ordinal: Option<Decimal> =
            row.try_get("assistant_response_part_ordinal")?;
        let model_identity_turn: Option<Uuid> = row.try_get("model_identity_turn_id")?;
        let model_identity_defaults_version: Option<Decimal> =
            row.try_get("model_identity_defaults_version")?;
        let model_identity_direct_selection: Option<Uuid> =
            row.try_get("model_identity_direct_selection_id")?;
        let summary_value: Option<String> = row.try_get("context_summary_value")?;
        let summary_call: Option<Uuid> = row.try_get("context_summary_producing_call_id")?;
        let summary_first_session: Option<Uuid> =
            row.try_get("context_summary_first_source_session_id")?;
        let summary_first_entry: Option<Uuid> = row.try_get("context_summary_first_entry_id")?;
        let summary_through_session: Option<Uuid> =
            row.try_get("context_summary_through_source_session_id")?;
        let summary_through_entry: Option<Uuid> =
            row.try_get("context_summary_through_entry_id")?;
        let delegated_task_spawning_request: Option<Uuid> =
            row.try_get("delegated_task_spawning_tool_request_id")?;
        let delegated_task_content: Option<String> = row.try_get("delegated_task_content")?;
        let delegated_task_parent_session: Option<Uuid> =
            row.try_get("delegated_task_parent_session_id")?;
        let delegated_task_parent_turn: Option<Uuid> =
            row.try_get("delegated_task_parent_turn_id")?;
        let delegation_message: Option<Uuid> = row.try_get("delegation_message_id")?;
        let delegation_message_spawning_request: Option<Uuid> =
            row.try_get("delegation_message_spawning_request_id")?;
        let delegation_message_sender: Option<Uuid> =
            row.try_get("delegation_message_sender_session_id")?;
        let delegation_message_recipient: Option<Uuid> =
            row.try_get("delegation_message_recipient_session_id")?;
        let delegation_message_delivery_sequence: Option<Decimal> =
            row.try_get("delegation_message_delivery_sequence")?;
        let delegation_message_content: Option<String> =
            row.try_get("delegation_message_content")?;
        let delegation_result_awaiting_request: Option<Uuid> =
            row.try_get("delegation_result_awaiting_tool_request_id")?;
        let delegation_result_spawning_request: Option<Uuid> =
            row.try_get("delegation_result_spawning_tool_request_id")?;
        let delegation_result_child: Option<Uuid> =
            row.try_get("delegation_result_child_session_id")?;
        let delegation_result_wait_mode: Option<String> =
            row.try_get("delegation_result_wait_mode")?;
        let delegation_result_delivery_sequence: Option<Decimal> =
            row.try_get("delegation_result_delivery_sequence")?;
        let delegation_result_outcome: Option<String> =
            row.try_get("delegation_result_outcome_kind")?;
        let delegation_result_content: Option<String> = row.try_get("delegation_result_content")?;
        let delegation_result_reason: Option<String> =
            row.try_get("delegation_result_reason_kind")?;
        let delegation_result_provenance_kind: Option<String> =
            row.try_get("delegation_result_provenance_kind")?;
        let delegation_result_provenance_session: Option<Uuid> =
            row.try_get("delegation_result_provenance_session_id")?;
        let delegation_result_provenance_turn: Option<Uuid> =
            row.try_get("delegation_result_provenance_turn_id")?;
        let delegation_result_provenance_goal: Option<Decimal> =
            row.try_get("delegation_result_provenance_goal_generation")?;
        let delegation_result_provenance_command: Option<Uuid> =
            row.try_get("delegation_result_provenance_command_id")?;
        let legacy_payload_present = origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || cancelled_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || tool_result_attempt.is_some()
            || completed_turn.is_some()
            || imported_conversation.is_some()
            || imported_transcript_entry.is_some()
            || assistant_response_part_ordinal.is_some()
            || model_identity_turn.is_some()
            || model_identity_defaults_version.is_some()
            || model_identity_direct_selection.is_some()
            || summary_value.is_some()
            || summary_call.is_some()
            || summary_first_session.is_some()
            || summary_first_entry.is_some()
            || summary_through_session.is_some()
            || summary_through_entry.is_some();
        if payload_kind == "delegated_task" {
            let (Some(spawning_request), Some(parent_session), Some(parent_turn), Some(content)) = (
                delegated_task_spawning_request,
                delegated_task_parent_session,
                delegated_task_parent_turn,
                delegated_task_content,
            ) else {
                return Err(SubmitInputCorruption::Inconsistent("delegated task payload").into());
            };
            if legacy_payload_present
                || tool_result_request.is_some()
                || delegation_message.is_some()
                || delegation_result_awaiting_request.is_some()
                || delegation_result_spawning_request.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent("delegated task payload").into());
            }
            let content = delegation_content(content, "delegated_task_content")?;
            semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
                entry,
                source_session,
                InitialSemanticTranscriptEntryPayload::DelegatedTask {
                    spawning_request: ToolRequestId::from_uuid(spawning_request),
                    parent_session: session_id_from_uuid(parent_session),
                    parent_turn: turn_id_from_uuid(parent_turn),
                    content,
                },
            ));
            continue;
        }
        if payload_kind == "delegation_message" {
            let (
                Some(message),
                Some(spawning_request),
                Some(sender),
                Some(recipient),
                Some(delivery_sequence),
                Some(content),
            ) = (
                delegation_message,
                delegation_message_spawning_request,
                delegation_message_sender,
                delegation_message_recipient,
                delegation_message_delivery_sequence,
                delegation_message_content,
            )
            else {
                return Err(
                    SubmitInputCorruption::Inconsistent("delegation message payload").into(),
                );
            };
            if legacy_payload_present
                || tool_result_request.is_some()
                || delegated_task_spawning_request.is_some()
                || delegation_result_awaiting_request.is_some()
                || delegation_result_spawning_request.is_some()
                || recipient != source_session_uuid
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("delegation message payload").into(),
                );
            }
            let delivery_sequence = NonZeroU64::new(
                positive_u64_from_numeric(delivery_sequence)
                    .map_err(|_| SubmitInputCorruption::Inconsistent("delegation delivery"))?,
            )
            .ok_or(SubmitInputCorruption::Inconsistent("delegation delivery"))?;
            let content = delegation_content(content, "delegation_message_content")?;
            semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
                entry,
                source_session,
                InitialSemanticTranscriptEntryPayload::DelegationMessage {
                    spawning_request: ToolRequestId::from_uuid(spawning_request),
                    message: DelegationMessageId::from_uuid(message),
                    sender: session_id_from_uuid(sender),
                    recipient: source_session,
                    delivery_sequence,
                    content,
                },
            ));
            continue;
        }
        if payload_kind == "delegation_result" {
            let (
                Some(awaiting_request),
                Some(spawning_request),
                Some(child),
                Some(wait_mode),
                Some(outcome_kind),
                Some(reason),
                Some(provenance_kind),
                Some(provenance_session),
            ) = (
                delegation_result_awaiting_request,
                delegation_result_spawning_request,
                delegation_result_child,
                delegation_result_wait_mode.as_deref(),
                delegation_result_outcome.as_deref(),
                delegation_result_reason.as_deref(),
                delegation_result_provenance_kind.as_deref(),
                delegation_result_provenance_session,
            )
            else {
                return Err(
                    SubmitInputCorruption::Inconsistent("delegation result payload").into(),
                );
            };
            let mode = decode_delegation_wait_mode(wait_mode)?;
            let delivery_sequence = delegation_result_delivery_sequence
                .map(|value| {
                    positive_u64_from_numeric(value)
                        .ok()
                        .and_then(NonZeroU64::new)
                        .ok_or(SubmitInputCorruption::Inconsistent("delegation delivery"))
                })
                .transpose()?;
            if legacy_payload_present
                || delegated_task_spawning_request.is_some()
                || delegation_message.is_some()
                || (mode == DelegationWaitMode::Foreground
                    && (tool_result_request != Some(awaiting_request)
                        || delivery_sequence.is_some()))
                || (mode == DelegationWaitMode::Background
                    && (tool_result_request.is_some() || delivery_sequence.is_none()))
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("delegation result payload").into(),
                );
            }
            let content = delegation_result_content
                .map(|value| delegation_content(value, "delegation_result_content"))
                .transpose()?;
            let outcome = DelegationOutcome::reconstitute(
                decode_delegation_outcome_kind(outcome_kind)?,
                content,
                decode_delegation_outcome_reason(reason)?,
                decode_delegation_provenance(
                    provenance_kind,
                    provenance_session,
                    delegation_result_provenance_turn,
                    delegation_result_provenance_goal,
                    delegation_result_provenance_command,
                )?,
            )
            .ok_or(SubmitInputCorruption::Inconsistent(
                "delegation result outcome",
            ))?;
            semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
                entry,
                source_session,
                InitialSemanticTranscriptEntryPayload::DelegationResult {
                    awaiting_request: ToolRequestId::from_uuid(awaiting_request),
                    spawning_request: ToolRequestId::from_uuid(spawning_request),
                    child: session_id_from_uuid(child),
                    mode,
                    delivery_sequence,
                    outcome: Box::new(outcome),
                },
            ));
            continue;
        }
        if delegated_task_spawning_request.is_some()
            || delegation_message.is_some()
            || delegation_result_awaiting_request.is_some()
            || delegation_result_spawning_request.is_some()
        {
            return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
        }
        if payload_kind == "context_summary" {
            if origin.is_some()
                || steering_source_turn.is_some()
                || failed_turn.is_some()
                || cancelled_turn.is_some()
                || assistant_text.is_some()
                || producing_call.is_some()
                || tool_request.is_some()
                || tool_result_request.is_some()
                || tool_result_attempt.is_some()
                || completed_turn.is_some()
                || imported_conversation.is_some()
                || imported_transcript_entry.is_some()
                || assistant_response_part_ordinal.is_some()
                || model_identity_turn.is_some()
                || model_identity_defaults_version.is_some()
                || model_identity_direct_selection.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
            }
            let (
                Some(value),
                Some(call),
                Some(first_session),
                Some(first_entry),
                Some(through_session),
                Some(through_entry),
            ) = (
                summary_value,
                summary_call,
                summary_first_session,
                summary_first_entry,
                summary_through_session,
                summary_through_entry,
            )
            else {
                return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
            };
            let payload = InitialSemanticTranscriptEntryPayload::ContextSummary {
                producing_call: ModelCallId::from_uuid(call),
                summarized: ContextCompactionRange::inclusive(
                    SemanticTranscriptEntryRef::from_source(
                        session_id_from_uuid(first_session),
                        SemanticTranscriptEntryId::from_uuid(first_entry),
                    ),
                    SemanticTranscriptEntryRef::from_source(
                        session_id_from_uuid(through_session),
                        SemanticTranscriptEntryId::from_uuid(through_entry),
                    ),
                ),
                value: AssistantText::try_new(value).map_err(|error| {
                    SubmitInputCorruption::InvalidContent {
                        field: "context_summary_value",
                        failure: error.failure(),
                    }
                })?,
            };
            semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
                entry,
                source_session,
                payload,
            ));
            continue;
        }
        if summary_value.is_some()
            || summary_call.is_some()
            || summary_first_session.is_some()
            || summary_first_entry.is_some()
            || summary_through_session.is_some()
            || summary_through_entry.is_some()
        {
            return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
        }
        if payload_kind == "model_identity_changed" {
            if origin.is_some()
                || steering_source_turn.is_some()
                || failed_turn.is_some()
                || cancelled_turn.is_some()
                || assistant_text.is_some()
                || producing_call.is_some()
                || tool_request.is_some()
                || tool_result_request.is_some()
                || tool_result_attempt.is_some()
                || completed_turn.is_some()
                || imported_conversation.is_some()
                || imported_transcript_entry.is_some()
                || assistant_response_part_ordinal.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
            }
            let payload = match (
                model_identity_turn,
                model_identity_defaults_version,
                model_identity_direct_selection,
            ) {
                (Some(turn), Some(defaults_version), Some(selected)) => {
                    InitialSemanticTranscriptEntryPayload::ModelIdentityChanged {
                        turn: turn_id_from_uuid(turn),
                        defaults_version: defaults_version_from_numeric(defaults_version).map_err(
                            |_| {
                                SubmitInputCorruption::Inconsistent(
                                    "model identity defaults version",
                                )
                            },
                        )?,
                        selected: DirectModelSelection::from_uuid(selected),
                    }
                }
                _ => {
                    return Err(
                        SubmitInputCorruption::Inconsistent("semantic entry payload").into(),
                    );
                }
            };
            semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
                entry,
                source_session,
                payload,
            ));
            continue;
        }
        if model_identity_turn.is_some()
            || model_identity_defaults_version.is_some()
            || model_identity_direct_selection.is_some()
        {
            return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
        }
        let payload = match (
            payload_kind.as_str(),
            origin,
            steering_source_turn,
            failed_turn,
            cancelled_turn,
            assistant_text,
            producing_call,
            tool_request,
            tool_result_request,
            tool_result_attempt,
            completed_turn,
        ) {
            (
                "origin_accepted_input",
                Some(origin),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: accepted_input_id_from_uuid(origin),
            },
            (
                "steering_accepted_input",
                Some(accepted_input),
                Some(source_turn),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: accepted_input_id_from_uuid(accepted_input),
                source_turn: turn_id_from_uuid(source_turn),
            },
            ("turn_failed", None, None, Some(turn), None, None, None, None, None, None, None) => {
                InitialSemanticTranscriptEntryPayload::TurnFailed {
                    turn: turn_id_from_uuid(turn),
                }
            }
            (
                "turn_cancelled",
                None,
                None,
                None,
                Some(turn),
                None,
                None,
                None,
                None,
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: turn_id_from_uuid(turn),
            },
            (
                "assistant_text",
                None,
                None,
                None,
                None,
                Some(text),
                Some(call),
                None,
                None,
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::AssistantText {
                producing_call: ModelCallId::from_uuid(call),
                value: AssistantText::try_new(text).map_err(|error| {
                    SubmitInputCorruption::InvalidContent {
                        field: "assistant_text_value",
                        failure: error.failure(),
                    }
                })?,
            },
            (
                "provider_compaction",
                None,
                None,
                None,
                None,
                Some(block),
                Some(call),
                None,
                None,
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::ProviderCompaction {
                producing_call: ModelCallId::from_uuid(call),
                block: ProviderCompactionBlock::try_new(block).map_err(|_| {
                    SubmitInputCorruption::Inconsistent("provider compaction block")
                })?,
            },
            (
                "assistant_tool_use",
                None,
                None,
                None,
                None,
                None,
                Some(call),
                Some(request),
                None,
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: ModelCallId::from_uuid(call),
                request: ToolRequestId::from_uuid(request),
            },
            (
                "tool_execution_result",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(attempt),
                None,
            ) => InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: signalbox_domain::ToolAttemptId::from_uuid(attempt),
            },
            (
                "tool_denied",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(request),
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::ToolDenied {
                request: ToolRequestId::from_uuid(request),
            },
            (
                "tool_closed_by_turn_end",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(request),
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::ToolClosed {
                request: ToolRequestId::from_uuid(request),
            },
            (
                "turn_completed",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(turn),
            ) => InitialSemanticTranscriptEntryPayload::TurnCompleted {
                turn: turn_id_from_uuid(turn),
            },
            (
                "origin_accepted_input"
                | "steering_accepted_input"
                | "turn_failed"
                | "turn_cancelled"
                | "assistant_text"
                | "provider_compaction"
                | "assistant_tool_use"
                | "tool_execution_result"
                | "tool_denied"
                | "tool_closed_by_turn_end"
                | "turn_completed",
                _,
                _,
                _,
                _,
                _,
                _,
                _,
                _,
                _,
                _,
            ) => {
                return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
            }
            (value, _, _, _, _, _, _, _, _, _, _) => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "semantic entry payload_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
            entry,
            source_session,
            payload,
        ));
    }
    if !required_semantic_entries.is_subset(&loaded_semantic_entries) {
        return Err(SubmitInputCorruption::Missing("context frontier semantic entry").into());
    }

    if required_frontier_ids
        .iter()
        .any(|frontier| !stored_frontiers.contains_key(frontier))
    {
        return Err(SubmitInputCorruption::Missing("scheduling context frontier").into());
    }
    let mut children = BTreeMap::<Uuid, Vec<Uuid>>::new();
    let mut ready = VecDeque::new();
    for (frontier, stored) in &stored_frontiers {
        if let Some(prefix) = stored.prefix {
            if !stored_frontiers.contains_key(&prefix) {
                return Err(SubmitInputCorruption::Missing("context frontier prefix").into());
            }
            children.entry(prefix).or_default().push(*frontier);
        } else {
            ready.push_back(*frontier);
        }
    }
    let mut reconstructed = BTreeMap::<Uuid, ResolvedContextFrontierReconstitutionInput>::new();
    while let Some(frontier) = ready.pop_front() {
        let stored = &stored_frontiers[&frontier];
        let prefix = stored
            .prefix
            .map(|prefix| {
                reconstructed
                    .get(&prefix)
                    .cloned()
                    .ok_or(SubmitInputCorruption::Missing(
                        "reconstructed context frontier prefix",
                    ))
            })
            .transpose()?;
        let prefix_member_count = prefix.as_ref().map_or(0, |prefix| prefix.entry_count());
        let actual_count = prefix_member_count
            .checked_add(stored.members.len())
            .ok_or(SubmitInputCorruption::Inconsistent(
                "context frontier declared membership",
            ))?;
        let actual_count = u64::try_from(actual_count).map_err(|_| {
            SubmitInputCorruption::Inconsistent("context frontier declared membership")
        })?;
        if stored.declared_count != Decimal::from(actual_count) {
            return Err(SubmitInputCorruption::Inconsistent(
                "context frontier declared membership",
            )
            .into());
        }
        let mut members = Vec::with_capacity(stored.members.len());
        for (index, (position, source_session, semantic_entry)) in stored.members.iter().enumerate()
        {
            let expected_position =
                u64::try_from(prefix_member_count + index + 1).map_err(|_| {
                    SubmitInputCorruption::Inconsistent("context frontier contiguous membership")
                })?;
            if *position != Decimal::from(expected_position) {
                return Err(SubmitInputCorruption::Inconsistent(
                    "context frontier contiguous membership",
                )
                .into());
            }
            members.push(SemanticTranscriptEntryRef::from_source(
                *source_session,
                *semantic_entry,
            ));
        }
        let input = match prefix {
            None => ResolvedContextFrontierReconstitutionInput::new(
                session_id,
                ContextFrontierId::from_uuid(frontier),
                members,
            ),
            Some(prefix) => {
                prefix.derive_appending(ContextFrontierId::from_uuid(frontier), members)
            }
        };
        reconstructed.insert(frontier, input);
        if let Some(successors) = children.get(&frontier) {
            ready.extend(successors.iter().copied());
        }
    }
    if reconstructed.len() != stored_frontiers.len() {
        return Err(SubmitInputCorruption::Inconsistent("context frontier prefix cycle").into());
    }
    let snapshots = scheduling_frontier_ids
        .iter()
        .map(|frontier| {
            reconstructed
                .get(frontier)
                .cloned()
                .ok_or(SubmitInputCorruption::Missing(
                    "reconstructed scheduling context frontier",
                ))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut input = AcceptedInputSchedulingReconstitutionInput::new(
        session,
        turns,
        semantic_entries,
        snapshots,
        active_acceptance_tail,
    );
    if let Some(imported_session) = imported_session {
        input = input.with_imported_session(imported_session);
    }
    for preceding in preceding_non_accepted_terminals {
        input = input.with_preceding_non_accepted_terminal(
            session_id,
            TurnId::from_uuid(preceding.turn_id),
            TurnId::from_uuid(preceding.successor_turn_id),
            ContextFrontierId::from_uuid(preceding.terminal_frontier_id),
            DirectModelSelection::from_uuid(preceding.direct_selection_id),
        );
    }
    input
        .with_model_call_facts(pinned_targets, model_calls)
        .with_context_compaction_facts(compaction_calls, compactions)
        .with_consumed_steering_facts(consumed_steering)
        .with_delegated_consumed_steering_facts(delegated_consumed_steering)
        .with_delegated_turn_facts(delegated_turn_facts)
        .with_steering_continuation_rounds(steering_continuation_rounds)
        .with_continuation_rounds(continuation_rounds)
        .reconstitute()
        .map_err(|error| {
            let (_, failure) = error.into_parts();
            SubmitInputCorruption::Scheduling(failure).into()
        })
}

fn delegation_content(
    value: String,
    field: &'static str,
) -> Result<DelegationContent, SubmitInputCorruption> {
    DelegationContent::try_new(value).map_err(|_| SubmitInputCorruption::Inconsistent(field))
}

fn decode_delegated_turn_scheduling_state(
    state_kind: &str,
    terminal_disposition: Option<&str>,
    runtime_terminal: bool,
) -> Result<DelegatedTurnSchedulingState, SubmitInputCorruption> {
    match (state_kind, terminal_disposition, runtime_terminal) {
        ("active", None, false) => Ok(DelegatedTurnSchedulingState::Active),
        ("queued" | "active", None, true) => Ok(DelegatedTurnSchedulingState::RuntimeTerminal),
        ("terminal", Some("completed"), _) => Ok(DelegatedTurnSchedulingState::TerminalCompleted),
        ("terminal", Some("refused"), _) => Ok(DelegatedTurnSchedulingState::TerminalRefused),
        ("terminal", Some("failed"), _) => Ok(DelegatedTurnSchedulingState::TerminalFailed),
        ("terminal", Some("cancelled"), _) => Ok(DelegatedTurnSchedulingState::TerminalCancelled),
        ("terminal", Some("reconciliation_required"), _) => {
            Ok(DelegatedTurnSchedulingState::TerminalReconciliationRequired)
        }
        _ => Err(SubmitInputCorruption::Inconsistent(
            "delegated turn scheduling state",
        )),
    }
}

fn decode_delegation_wait_mode(value: &str) -> Result<DelegationWaitMode, SubmitInputCorruption> {
    match value {
        "foreground" => Ok(DelegationWaitMode::Foreground),
        "background" => Ok(DelegationWaitMode::Background),
        _ => Err(SubmitInputCorruption::Unsupported {
            field: "delegation wait mode",
            value: value.to_owned(),
        }),
    }
}

fn decode_delegation_outcome_kind(
    value: &str,
) -> Result<DelegationOutcomeKind, SubmitInputCorruption> {
    match value {
        "result_returned" => Ok(DelegationOutcomeKind::ResultReturned),
        "child_failed" => Ok(DelegationOutcomeKind::ChildFailed),
        "child_stopped" => Ok(DelegationOutcomeKind::ChildStopped),
        "child_cancelled" => Ok(DelegationOutcomeKind::ChildCancelled),
        "already_terminal" => Ok(DelegationOutcomeKind::AlreadyTerminal),
        "continue_running" => Ok(DelegationOutcomeKind::ContinueRunning),
        _ => Err(SubmitInputCorruption::Unsupported {
            field: "delegation outcome",
            value: value.to_owned(),
        }),
    }
}

fn decode_delegation_outcome_reason(
    value: &str,
) -> Result<DelegationOutcomeReason, SubmitInputCorruption> {
    match value {
        "child_completed" => Ok(DelegationOutcomeReason::ChildCompleted),
        "child_execution_failed" => Ok(DelegationOutcomeReason::ChildExecutionFailed),
        "child_result_unavailable" => Ok(DelegationOutcomeReason::ChildResultUnavailable),
        "child_cancelled" => Ok(DelegationOutcomeReason::ChildCancelled),
        "parent_stopped_parent_and_descendants" => Ok(DelegationOutcomeReason::ParentStopped {
            scope: DescendantTerminationScope::ParentAndDescendants,
        }),
        "parent_cancelled_parent_and_descendants" => Ok(DelegationOutcomeReason::ParentCancelled {
            scope: DescendantTerminationScope::ParentAndDescendants,
        }),
        _ => Err(SubmitInputCorruption::Unsupported {
            field: "delegation outcome reason",
            value: value.to_owned(),
        }),
    }
}

fn decode_delegation_provenance(
    kind: &str,
    session: Uuid,
    turn: Option<Uuid>,
    generation: Option<Decimal>,
    command: Option<Uuid>,
) -> Result<DelegationProvenanceReconstitutionInput, SubmitInputCorruption> {
    match (kind, turn, generation, command) {
        ("child_turn", Some(turn), None, None) => {
            Ok(DelegationProvenanceReconstitutionInput::ChildTurn {
                session: session_id_from_uuid(session),
                turn: turn_id_from_uuid(turn),
            })
        }
        ("parent_turn_command", Some(turn), None, Some(command)) => {
            Ok(DelegationProvenanceReconstitutionInput::ParentTurnCommand {
                session: session_id_from_uuid(session),
                turn: turn_id_from_uuid(turn),
                command: durable_command_id_from_uuid(command).map_err(|_| {
                    SubmitInputCorruption::Inconsistent("delegation provenance command")
                })?,
            })
        }
        ("parent_goal_command", None, Some(generation), Some(command)) => {
            let generation = positive_u64_from_numeric(generation)
                .ok()
                .and_then(NonZeroU64::new)
                .ok_or(SubmitInputCorruption::Inconsistent(
                    "delegation provenance generation",
                ))?;
            Ok(DelegationProvenanceReconstitutionInput::ParentGoalCommand {
                session: session_id_from_uuid(session),
                generation: GoalGeneration::new(generation),
                command: durable_command_id_from_uuid(command).map_err(|_| {
                    SubmitInputCorruption::Inconsistent("delegation provenance command")
                })?,
            })
        }
        ("parent_lifecycle_command", None, None, Some(command)) => Ok(
            DelegationProvenanceReconstitutionInput::ParentLifecycleCommand {
                session: session_id_from_uuid(session),
                command: durable_command_id_from_uuid(command).map_err(|_| {
                    SubmitInputCorruption::Inconsistent("delegation provenance command")
                })?,
            },
        ),
        _ => Err(SubmitInputCorruption::Inconsistent(
            "delegation result provenance",
        )),
    }
}

fn map_imported_scheduling_error(
    error: crate::create_session_from_imported_frontier::ImportedSessionRepositoryError,
) -> SubmitInputRepositoryError {
    use crate::create_session_from_imported_frontier::ImportedSessionRepositoryError;

    match error {
        ImportedSessionRepositoryError::Database(error) => {
            SubmitInputRepositoryError::Database(error)
        }
        ImportedSessionRepositoryError::ImportedConversation(
            crate::conversation_import::ImportedConversationRepositoryError::Database(error),
        ) => SubmitInputRepositoryError::Database(error),
        ImportedSessionRepositoryError::Corruption(_)
        | ImportedSessionRepositoryError::CommitAmbiguous(_)
        | ImportedSessionRepositoryError::DifferentCommandKind { .. }
        | ImportedSessionRepositoryError::Preparation(_)
        | ImportedSessionRepositoryError::IdentityCollision(_)
        | ImportedSessionRepositoryError::ImportedConversation(_) => {
            SubmitInputCorruption::Inconsistent("complete imported scheduling projection").into()
        }
    }
}

fn require_current_attempt_row(
    row: &PgRow,
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
) -> Result<(), SubmitInputRepositoryError> {
    let stored_attempt: Option<Uuid> = row.try_get("turn_attempt_id")?;
    let stored_turn: Option<Uuid> = row.try_get("attempt_turn_id")?;
    let stored_session: Option<Uuid> = row.try_get("attempt_session_id")?;
    if stored_attempt != Some(attempt.into_uuid())
        || stored_turn != Some(turn.into_uuid())
        || stored_session != Some(session.into_uuid())
    {
        return Err(SubmitInputCorruption::Inconsistent("active current attempt").into());
    }
    Ok(())
}

fn map_tool_loop_error(
    error: crate::tool_loop::ToolLoopRepositoryError,
) -> SubmitInputRepositoryError {
    match error {
        crate::tool_loop::ToolLoopRepositoryError::Database { source, .. } => source.into(),
        crate::tool_loop::ToolLoopRepositoryError::IdentityCollision
        | crate::tool_loop::ToolLoopRepositoryError::Corruption(_)
        | crate::tool_loop::ToolLoopRepositoryError::DifferentCommandKind
        | crate::tool_loop::ToolLoopRepositoryError::ConflictingCommandReuse
        | crate::tool_loop::ToolLoopRepositoryError::InvalidTransition(_) => {
            SubmitInputCorruption::Inconsistent("active tool batch").into()
        }
    }
}

pub(crate) fn require_applied_interrupt_from_attempt(
    row: &PgRow,
    owning_turn: TurnId,
    recorded_commands: &BTreeMap<DurableCommandId, ReconstitutedSubmitInput>,
) -> Result<AppliedInterruptCommandResult, SubmitInputRepositoryError> {
    let command = durable_command_id_from_uuid(required(row, "interrupt_command_id")?)
        .map_err(|_| SubmitInputCorruption::Inconsistent("interrupt command identity"))?;
    let predecessor = turn_id_from_uuid(required(row, "attempt_interrupt_predecessor_turn_id")?);
    if predecessor != owning_turn {
        return Err(SubmitInputCorruption::Inconsistent("attempt interrupt predecessor").into());
    }
    let receipt = recorded_commands
        .get(&command)
        .ok_or(SubmitInputCorruption::Missing("applied interrupt command"))?;
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) = receipt.result()
    else {
        return Err(
            SubmitInputCorruption::Inconsistent("interrupt command was not applied").into(),
        );
    };
    origin
        .applied_interrupt()
        .copied()
        .filter(|interrupt| {
            interrupt.proof().command() == command && interrupt.proof().predecessor() == owning_turn
        })
        .ok_or_else(|| SubmitInputCorruption::Inconsistent("attempt interrupt authority").into())
}

fn applied_interrupt_for_turn(
    owning_turn: TurnId,
    recorded_commands: &BTreeMap<DurableCommandId, ReconstitutedSubmitInput>,
) -> Result<Option<AppliedInterruptCommandResult>, SubmitInputRepositoryError> {
    let mut matches = recorded_commands.values().filter_map(|receipt| {
        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) =
            receipt.result()
        else {
            return None;
        };
        origin
            .applied_interrupt()
            .copied()
            .filter(|interrupt| interrupt.proof().predecessor() == owning_turn)
    });
    let interrupt = matches.next();
    if matches.next().is_some() {
        return Err(
            SubmitInputCorruption::Inconsistent("multiple applied interrupt commands").into(),
        );
    }
    Ok(interrupt)
}

fn require_applied_runner_recovery_interrupt(
    row: &PgRow,
    owning_turn: TurnId,
    yielded_attempt: TurnAttemptId,
    interrupted_tool_attempt: Option<Uuid>,
    recorded_commands: &BTreeMap<DurableCommandId, ReconstitutedSubmitInput>,
) -> Result<AppliedInterruptCommandResult, SubmitInputRepositoryError> {
    let command =
        durable_command_id_from_uuid(required(row, "runner_recovery_interrupt_command_id")?)
            .map_err(|_| SubmitInputCorruption::Inconsistent("runner recovery command identity"))?;
    let recorded_yielded_attempt: Uuid = required(row, "runner_recovery_yielded_attempt_id")?;
    let recorded_interrupted_attempt: Option<Uuid> =
        row.try_get("runner_recovery_interrupted_tool_attempt_id")?;
    if recorded_yielded_attempt != yielded_attempt.into_uuid()
        || recorded_interrupted_attempt != interrupted_tool_attempt
    {
        return Err(SubmitInputCorruption::Inconsistent("runner recovery interrupt effect").into());
    }
    let receipt = recorded_commands
        .get(&command)
        .ok_or(SubmitInputCorruption::Missing(
            "runner recovery interrupt command",
        ))?;
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) = receipt.result()
    else {
        return Err(SubmitInputCorruption::Inconsistent(
            "runner recovery interrupt was not applied",
        )
        .into());
    };
    origin
        .applied_interrupt()
        .copied()
        .filter(|interrupt| {
            interrupt.proof().command() == command && interrupt.proof().predecessor() == owning_turn
        })
        .ok_or_else(|| {
            SubmitInputCorruption::Inconsistent("runner recovery interrupt authority").into()
        })
}

fn accepted_origin_source_turn(delivery: DeliveryRequest) -> Option<TurnId> {
    match delivery {
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            ..
        }
        | DeliveryRequest::Interrupt {
            expected_active_turn,
            ..
        } => Some(expected_active_turn),
        DeliveryRequest::StartWhenNoActiveTurn { .. } | DeliveryRequest::NextSafePoint { .. } => {
            None
        }
    }
}

pub(crate) fn decode_goal_origin_configuration(
    row: &PgRow,
    expected_session: SessionId,
) -> Result<OriginConfiguration, SubmitInputRepositoryError> {
    let defaults_session = session_id_from_uuid(required(row, "goal_defaults_session_id")?);
    if defaults_session != expected_session {
        return Err(SubmitInputCorruption::Inconsistent("goal defaults session").into());
    }
    let queued_version = decode_defaults_version(row, "queued_defaults_version")?;
    let defaults_version = decode_defaults_version(row, "goal_defaults_version")?;
    if defaults_version != queued_version {
        return Err(SubmitInputCorruption::Inconsistent("goal defaults version").into());
    }
    let defaults = decode_defaults(
        required(row, "goal_defaults_model_kind")?,
        row.try_get("goal_defaults_direct_id")?,
        row.try_get("goal_defaults_alias_id")?,
        required(row, "goal_defaults_tool_auto_approval")?,
        required(row, "goal_defaults_model_settings")?,
        "goal defaults",
    )?;
    let requested = decode_model_selection(
        required(row, "requested_model_kind")?,
        row.try_get("requested_direct_model_selection_id")?,
        row.try_get("requested_model_alias_id")?,
        "goal requested model",
    )?;
    let frozen = decode_frozen_model(
        required(row, "frozen_model_kind")?,
        row.try_get("frozen_direct_model_selection_id")?,
        row.try_get("frozen_model_alias_id")?,
        row.try_get("frozen_alias_selected_direct_id")?,
    )?;
    OriginConfigurationReconstitutionInput::new(defaults_version, defaults, requested, frozen)
        .reconstitute()
        .ok_or_else(|| SubmitInputCorruption::Inconsistent("goal origin configuration").into())
}

fn require_stored_origin_configuration(
    row: &PgRow,
    expected: &OriginConfiguration,
) -> Result<(), SubmitInputRepositoryError> {
    let source: Option<Uuid> = row.try_get("source_configuration_turn_id")?;
    if source.is_some() {
        return Err(
            SubmitInputCorruption::Inconsistent("explicit configuration source reference").into(),
        );
    }
    require_spelling(row, "model_parameters", "provider_defaults")?;
    require_spelling(row, "known_provider_failure_retry", "disabled")?;
    require_spelling(row, "model_fallback", "disabled")?;
    let dangerous_tool_auto_approval = decode_dangerous_tool_auto_approval(
        row,
        "queued_tool_auto_approval",
        "scheduling origin configuration",
    )?;
    let defaults_version = decode_defaults_version(row, "queued_defaults_version")?;
    let requested = decode_model_selection(
        required(row, "requested_model_kind")?,
        row.try_get("requested_direct_model_selection_id")?,
        row.try_get("requested_model_alias_id")?,
        "scheduling requested model",
    )?;
    let frozen = decode_frozen_model(
        required(row, "frozen_model_kind")?,
        row.try_get("frozen_direct_model_selection_id")?,
        row.try_get("frozen_model_alias_id")?,
        row.try_get("frozen_alias_selected_direct_id")?,
    )?;
    if defaults_version != expected.session_defaults_version()
        || requested != expected.requested().model()
        || frozen != *expected.effective().model()
        || dangerous_tool_auto_approval != expected.effective().dangerous_tool_auto_approval()
    {
        return Err(SubmitInputCorruption::Inconsistent("scheduling origin configuration").into());
    }
    Ok(())
}

fn require_stored_inherited_configuration(
    row: &PgRow,
    expected_source: TurnId,
) -> Result<(), SubmitInputRepositoryError> {
    let source: Option<Uuid> = row.try_get("source_configuration_turn_id")?;
    let values_absent: bool = required(row, "queued_configuration_values_absent")?;
    if source != Some(expected_source.into_uuid()) || !values_absent {
        return Err(
            SubmitInputCorruption::Inconsistent("inherited configuration provenance").into(),
        );
    }
    Ok(())
}

enum StoredOriginRuntimeState {
    RuntimeRelevant,
    RetiredGoal,
}

fn decode_origin_runtime_state(
    value: &str,
) -> Result<StoredOriginRuntimeState, SubmitInputRepositoryError> {
    match value {
        "runtime_relevant" => Ok(StoredOriginRuntimeState::RuntimeRelevant),
        "retired_goal" => Ok(StoredOriginRuntimeState::RetiredGoal),
        value => Err(SubmitInputCorruption::Unsupported {
            field: "acceptance-tail origin runtime state",
            value: value.to_owned(),
        }
        .into()),
    }
}

async fn load_active_acceptance_tail(
    connection: &mut PgConnection,
    session: SessionId,
    turns: &[AcceptedInputTurnSchedulingRecord],
) -> Result<Option<SessionAcceptanceTailReconstitutionInput>, SubmitInputRepositoryError> {
    let Some(active) = turns.iter().find(|record| {
        matches!(
            record.state(),
            AcceptedInputTurnSchedulingRecordState::Active { .. }
        )
    }) else {
        return Ok(None);
    };

    let rows = sqlx::query(
        "SELECT
            accepted_input_id,
            session_id,
            acceptance_position,
            disposition_kind,
            origin_turn_id,
            consuming_model_call_id,
            delivery_kind,
            descendant_scope,
            expected_active_turn_id,
            expected_defaults_version,
            model_override_kind,
            replacement_model_kind,
            replacement_direct_model_selection_id,
            replacement_model_alias_id,
            model_settings_override,
            CASE
                WHEN goal_turn_is_runtime_relevant(
                    accepted.session_id, accepted.origin_turn_id
                ) THEN 'runtime_relevant'
                ELSE 'retired_goal'
            END AS origin_runtime_state
           FROM accepted_input AS accepted
          WHERE accepted.session_id = $1
            AND accepted.acceptance_position >= $2
          ORDER BY accepted.acceptance_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(input_position_to_numeric(
        active.order().acceptance_position(),
    ))
    .fetch_all(&mut *connection)
    .await?;

    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let accepted_input = accepted_input_id_from_uuid(required(&row, "accepted_input_id")?);
        let entry_session = session_id_from_uuid(required(&row, "session_id")?);
        let position = decode_position(&row, "acceptance_position")?;
        let expected_active_turn: Option<Uuid> = row.try_get("expected_active_turn_id")?;
        let delivery = decode_delivery(
            required(&row, "delivery_kind")?,
            row.try_get("descendant_scope")?,
            expected_active_turn,
            row.try_get("expected_defaults_version")?,
            row.try_get("model_override_kind")?,
            row.try_get("replacement_model_kind")?,
            row.try_get("replacement_direct_model_selection_id")?,
            row.try_get("replacement_model_alias_id")?,
            required(&row, "model_settings_override")?,
            "active acceptance-tail delivery",
        )?;
        let disposition_kind: String = required(&row, "disposition_kind")?;
        let origin_turn: Option<Uuid> = row.try_get("origin_turn_id")?;
        let consuming_call: Option<Uuid> = row.try_get("consuming_model_call_id")?;
        let disposition = match (
            disposition_kind.as_str(),
            origin_turn,
            consuming_call,
            delivery,
        ) {
            ("origin_of", Some(origin), None, _) => {
                AcceptedInputDisposition::OriginOf(turn_id_from_uuid(origin))
            }
            (
                "pending_steering",
                None,
                None,
                DeliveryRequest::NextSafePoint {
                    expected_active_turn,
                },
            ) => AcceptedInputDisposition::PendingSteering {
                binding: SteeringBinding::new(expected_active_turn),
            },
            (
                "reclassified_as_turn_origin",
                Some(origin),
                None,
                DeliveryRequest::NextSafePoint { .. },
            ) => AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: turn_id_from_uuid(origin),
                reason: SteeringReclassificationReason::NoSafePointBeforeTerminal,
            },
            ("consumed_as_steering", None, Some(call), DeliveryRequest::NextSafePoint { .. }) => {
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: ModelCallId::from_uuid(call),
                }
            }
            ("closed_not_delivered", None, None, DeliveryRequest::NextSafePoint { .. }) => {
                AcceptedInputDisposition::ClosedNotDelivered
            }
            (
                "origin_of"
                | "pending_steering"
                | "reclassified_as_turn_origin"
                | "consumed_as_steering"
                | "closed_not_delivered",
                _,
                _,
                _,
            ) => {
                return Err(SubmitInputCorruption::Inconsistent(
                    "active acceptance-tail disposition",
                )
                .into());
            }
            (value, _, _, _) => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "active acceptance-tail disposition_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        let lifecycle = AcceptedInputLifecycle::new(accepted_input, disposition);
        let runtime_state: String = required(&row, "origin_runtime_state")?;
        let entry = match decode_origin_runtime_state(&runtime_state)? {
            StoredOriginRuntimeState::RetiredGoal => {
                SessionAcceptanceTailEntryReconstitutionInput::retired_goal_origin(
                    entry_session,
                    lifecycle,
                    position,
                    delivery,
                )
            }
            StoredOriginRuntimeState::RuntimeRelevant => {
                SessionAcceptanceTailEntryReconstitutionInput::new(
                    entry_session,
                    lifecycle,
                    position,
                    delivery,
                )
            }
        };
        entries.push(entry);
    }

    let observed_last_position = entries
        .last()
        .map(SessionAcceptanceTailEntryReconstitutionInput::position)
        .ok_or(SubmitInputCorruption::Missing(
            "active acceptance-tail origin",
        ))?;
    Ok(Some(SessionAcceptanceTailReconstitutionInput::new(
        session,
        active.accepted_input().id(),
        observed_last_position,
        entries,
    )))
}

fn decode_starting_lineage(
    kind: Option<String>,
    predecessor: Option<Uuid>,
) -> Result<AcceptedInputStartingLineage, SubmitInputRepositoryError> {
    match (kind.as_deref(), predecessor) {
        (Some("first_in_session"), None) => Ok(AcceptedInputStartingLineage::FirstInSession),
        (Some("after"), Some(predecessor)) => Ok(AcceptedInputStartingLineage::After {
            immediate_predecessor: turn_id_from_uuid(predecessor),
        }),
        (Some("first_in_session" | "after"), _) | (None, _) => {
            Err(SubmitInputCorruption::Inconsistent("starting lineage").into())
        }
        (Some(value), _) => Err(SubmitInputCorruption::Unsupported {
            field: "start_lineage_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

async fn insert_prepared_command(
    connection: &mut PgConnection,
    prepared: &PreparedSubmitInput,
) -> Result<(), SubmitInputRepositoryError> {
    let command = prepared.command();
    let actor = encode_actor(command.actor());
    let delivery = encode_delivery(command.delivery());
    let result = encode_result(prepared.result(), command.delivery(), command.session());

    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             delivery_kind, descendant_scope,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             model_settings_override, result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_actual_active_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position,
             result_existing_interrupt_command_id,
             result_attachment_digest, result_attachment_maximum_bytes)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
             $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23,
             $24, $25, $26, $27, $28, $29, $30, $31)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(SUBMIT_INPUT_KIND)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(command.session()))
    .bind(actor.kind)
    .bind(actor.turn)
    .bind(actor.tool_request)
    .bind(delivery.kind)
    .bind(delivery.descendant_scope)
    .bind(delivery.expected_active_turn)
    .bind(delivery.expected_defaults_version)
    .bind(delivery.model_override_kind)
    .bind(delivery.replacement.kind)
    .bind(delivery.replacement.direct)
    .bind(delivery.replacement.alias)
    .bind(&delivery.model_settings)
    .bind(result.kind)
    .bind(result.rejection_kind)
    .bind(session_id_to_uuid(result.session))
    .bind(result.accepted_input)
    .bind(result.turn)
    .bind(result.actual_active_turn)
    .bind(result.expected_active_turn)
    .bind(result.expected_defaults_version)
    .bind(result.current_defaults_version)
    .bind(result.unknown_alias)
    .bind(result.selected_defaults_version)
    .bind(result.last_position)
    .bind(result.existing_interrupt_command)
    .bind(result.attachment_digest)
    .bind(result.attachment_maximum_bytes)
    .execute(&mut *connection)
    .await?;

    insert_command_content_parts(connection, command.command_id(), command.content()).await?;

    Ok(())
}

async fn insert_prepared_effects(
    connection: &mut PgConnection,
    prepared: PreparedSubmitInput,
) -> Result<(), SubmitInputRepositoryError> {
    let command = prepared.command();
    let delivery = encode_delivery(command.delivery());

    if let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
        prepared.result()
    {
        let origin = applied.origin_configuration();
        let requested = encode_selection(origin.requested().model());
        let frozen = encode_frozen_model(origin.effective().model());
        let position = applied.acceptance_position();
        let (priority_kind, interrupt_predecessor) = match applied.queue_order().priority() {
            AcceptedInputQueuePriority::Ordinary => ("ordinary", None),
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { predecessor } => (
                "interrupt_immediately_after",
                Some(turn_id_to_uuid(predecessor)),
            ),
        };

        sqlx::query(
            "INSERT INTO accepted_input
                (accepted_input_id, accepting_command_id, session_id,
                 delivery_kind,
                 descendant_scope,
                 expected_active_turn_id, expected_defaults_version,
                 model_override_kind, replacement_model_kind,
                 replacement_direct_model_selection_id, replacement_model_alias_id,
                 model_settings_override, acceptance_position, disposition_kind,
                 origin_turn_id)
             VALUES
                ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
                 $12, $13, $14, $15)",
        )
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(durable_command_id_to_uuid(command.command_id()))
        .bind(session_id_to_uuid(applied.session()))
        .bind(delivery.kind)
        .bind(delivery.descendant_scope)
        .bind(delivery.expected_active_turn)
        .bind(delivery.expected_defaults_version)
        .bind(delivery.model_override_kind)
        .bind(delivery.replacement.kind)
        .bind(delivery.replacement.direct)
        .bind(delivery.replacement.alias)
        .bind(&delivery.model_settings)
        .bind(input_position_to_numeric(position))
        .bind("origin_of")
        .bind(turn_id_to_uuid(applied.turn()))
        .execute(&mut *connection)
        .await?;

        mirror_accepted_content_parts(connection, applied.accepted_input()).await?;

        let settings_event =
            applied
                .model_settings_event()
                .ok_or(SubmitInputCorruption::Inconsistent(
                    "resolved model settings event",
                ))?;
        model_settings_resolution::persist(connection, applied.session(), &settings_event).await?;

        sqlx::query(
            "INSERT INTO queued_input_origin
                (turn_id, accepted_input_id, session_id, acceptance_position,
                 priority_kind, defaults_version,
                 interrupt_predecessor_turn_id,
                 requested_model_kind, requested_direct_model_selection_id,
                 requested_model_alias_id, frozen_model_kind,
                 frozen_direct_model_selection_id, frozen_model_alias_id,
                 frozen_alias_selected_direct_id, model_parameters,
                 known_provider_failure_retry, model_fallback,
                 dangerous_tool_auto_approval)
             VALUES
                ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                 $14, $15, $16, $17, $18)",
        )
        .bind(turn_id_to_uuid(applied.turn()))
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(session_id_to_uuid(applied.session()))
        .bind(input_position_to_numeric(position))
        .bind(priority_kind)
        .bind(defaults_version_to_numeric(
            origin.session_defaults_version(),
        ))
        .bind(interrupt_predecessor)
        .bind(requested.kind)
        .bind(requested.direct)
        .bind(requested.alias)
        .bind(frozen.kind)
        .bind(frozen.direct)
        .bind(frozen.alias)
        .bind(frozen.alias_selected)
        .bind("provider_defaults")
        .bind("disabled")
        .bind("disabled")
        .bind(dangerous_tool_auto_approval_to_str(
            origin.effective().dangerous_tool_auto_approval(),
        ))
        .execute(&mut *connection)
        .await?;

        sqlx::query(
            "INSERT INTO turn_lifecycle
                (turn_id, session_id, origin_accepted_input_id,
                 acceptance_position, state_kind)
             VALUES ($1, $2, $3, $4, 'queued')",
        )
        .bind(turn_id_to_uuid(applied.turn()))
        .bind(session_id_to_uuid(applied.session()))
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(input_position_to_numeric(position))
        .execute(&mut *connection)
        .await?;

        outbox::append(
            connection,
            OutboxEvent::InputAccepted {
                session: applied.session(),
                accepted_input: applied.accepted_input(),
                turn: applied.turn(),
                acceptance_position: position,
            },
        )
        .await?;
    }

    if let SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(applied)) =
        prepared.result()
    {
        sqlx::query(
            "INSERT INTO accepted_input
                (accepted_input_id, accepting_command_id, session_id,
                 delivery_kind,
                 descendant_scope,
                 expected_active_turn_id, expected_defaults_version,
                 model_override_kind, replacement_model_kind,
                 replacement_direct_model_selection_id, replacement_model_alias_id,
                 model_settings_override, acceptance_position, disposition_kind,
                 origin_turn_id)
             VALUES
                ($1, $2, $3, 'next_safe_point', NULL,
                 $4, NULL, NULL, NULL, NULL, NULL, $5, $6,
                 'pending_steering', NULL)",
        )
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(durable_command_id_to_uuid(command.command_id()))
        .bind(session_id_to_uuid(applied.session()))
        .bind(turn_id_to_uuid(applied.binding().source_turn()))
        .bind(model_settings_overlay_to_json(
            signalbox_domain::ModelSettingsOverlay::inherit_all(),
        ))
        .bind(input_position_to_numeric(applied.acceptance_position()))
        .execute(&mut *connection)
        .await?;

        mirror_accepted_content_parts(connection, applied.accepted_input()).await?;
    }

    settle_injection_receipt(connection, &prepared).await
}

/// Settles the command's injection receipt. Pending steering settles at
/// its boundary; a session that does not exist has no receipt to carry.
async fn settle_injection_receipt(
    connection: &mut PgConnection,
    prepared: &PreparedSubmitInput,
) -> Result<(), SubmitInputRepositoryError> {
    let command = prepared.command();
    let outcome = match prepared.result() {
        SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) => {
            outbox::InjectionOutcomeOutbox::Delivered {
                turn: Some(applied.turn()),
            }
        }
        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
        | SubmitInputResult::Rejected(SubmitInputRejectedResult::SessionNotFound { .. }) => {
            return Ok(());
        }
        SubmitInputResult::Rejected(rejected) => {
            if matches!(
                rejected,
                SubmitInputRejectedResult::AttachmentBlobNotFound { .. }
                    | SubmitInputRejectedResult::AttachmentByteBudgetExceeded { .. }
            ) && !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM session WHERE session_id = $1)",
            )
            .bind(session_id_to_uuid(command.session()))
            .fetch_one(&mut *connection)
            .await?
            {
                return Ok(());
            }
            let kind = encode_result(prepared.result(), command.delivery(), command.session())
                .rejection_kind
                .ok_or(SubmitInputCorruption::Inconsistent("rejection kind"))?;
            outbox::InjectionOutcomeOutbox::Rejected { kind }
        }
    };
    outbox::append(
        connection,
        OutboxEvent::InjectionSettled {
            session: command.session(),
            command: command.command_id(),
            outcome,
        },
    )
    .await?;
    Ok(())
}

async fn insert_command_content_parts(
    connection: &mut PgConnection,
    command: DurableCommandId,
    content: &UserContent,
) -> Result<(), SubmitInputRepositoryError> {
    for (position, part) in content.parts().iter().enumerate() {
        let encoded = encode_content_part(part);
        sqlx::query(
            "INSERT INTO submit_input_command_content_part
                (command_id, position, part_kind, text_value, blob_digest,
                 attachment_kind, declared_media_type, display_filename)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(durable_command_id_to_uuid(command))
        .bind(
            i16::try_from(position).map_err(|_| {
                SubmitInputCorruption::Inconsistent("command content part position")
            })?,
        )
        .bind(encoded.kind)
        .bind(encoded.text)
        .bind(encoded.digest)
        .bind(encoded.attachment_kind)
        .bind(encoded.media_type)
        .bind(encoded.filename)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

async fn mirror_accepted_content_parts(
    connection: &mut PgConnection,
    accepted_input: AcceptedInputId,
) -> Result<(), SubmitInputRepositoryError> {
    sqlx::query(
        "INSERT INTO accepted_input_content_part
            (accepted_input_id, position, part_kind, text_value, blob_digest,
             attachment_kind, declared_media_type, display_filename)
         SELECT accepted.accepted_input_id, part.position, part.part_kind,
                part.text_value, part.blob_digest, part.attachment_kind,
                part.declared_media_type, part.display_filename
           FROM accepted_input AS accepted
           JOIN submit_input_command_content_part AS part
             ON part.command_id = accepted.accepting_command_id
          WHERE accepted.accepted_input_id = $1
          ORDER BY part.position",
    )
    .bind(accepted_input_id_to_uuid(accepted_input))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

struct EncodedContentPart<'a> {
    kind: &'static str,
    text: Option<&'a str>,
    digest: Option<&'a [u8]>,
    attachment_kind: Option<&'static str>,
    media_type: Option<&'a str>,
    filename: Option<&'a str>,
}

fn encode_content_part(part: &UserContentPart) -> EncodedContentPart<'_> {
    match part {
        UserContentPart::Text { value } => EncodedContentPart {
            kind: "text",
            text: Some(value.as_str()),
            digest: None,
            attachment_kind: None,
            media_type: None,
            filename: None,
        },
        UserContentPart::Attachment {
            digest,
            kind,
            media_type,
            display_filename,
        } => EncodedContentPart {
            kind: "attachment",
            text: None,
            digest: Some(digest.as_bytes()),
            attachment_kind: Some(match kind {
                AttachmentKind::Image => "image",
                AttachmentKind::Document => "document",
                AttachmentKind::File => "file",
            }),
            media_type: Some(media_type.as_str()),
            filename: display_filename
                .as_ref()
                .map(AttachmentDisplayFilename::as_str),
        },
    }
}

struct EncodedActor {
    kind: &'static str,
    turn: Option<Uuid>,
    tool_request: Option<Uuid>,
}

fn encode_actor(actor: Actor) -> EncodedActor {
    match actor {
        Actor::User => EncodedActor {
            kind: "user",
            turn: None,
            tool_request: None,
        },
        Actor::Core => EncodedActor {
            kind: "core",
            turn: None,
            tool_request: None,
        },
        Actor::Model { turn } => EncodedActor {
            kind: "model",
            turn: Some(turn.into_uuid()),
            tool_request: None,
        },
        Actor::Recovery => EncodedActor {
            kind: "recovery",
            turn: None,
            tool_request: None,
        },
        Actor::Tool { request } => EncodedActor {
            kind: "tool",
            turn: None,
            tool_request: Some(request.into_uuid()),
        },
    }
}

#[derive(Clone, Copy)]
struct EncodedSelection {
    kind: Option<&'static str>,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
}

impl EncodedSelection {
    const fn absent() -> Self {
        Self {
            kind: None,
            direct: None,
            alias: None,
        }
    }
}

fn encode_selection(selection: ModelSelectionRequest) -> EncodedSelection {
    match selection {
        ModelSelectionRequest::Direct(selection) => EncodedSelection {
            kind: Some("direct"),
            direct: Some(selection.into_uuid()),
            alias: None,
        },
        ModelSelectionRequest::Alias(alias) => EncodedSelection {
            kind: Some("alias"),
            direct: None,
            alias: Some(alias.into_uuid()),
        },
    }
}

struct EncodedFrozenModel {
    kind: &'static str,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    alias_selected: Option<Uuid>,
}

fn encode_frozen_model(model: &FrozenModelSelection) -> EncodedFrozenModel {
    match model {
        FrozenModelSelection::Direct(selection) => EncodedFrozenModel {
            kind: "direct",
            direct: Some(selection.into_uuid()),
            alias: None,
            alias_selected: None,
        },
        FrozenModelSelection::FrozenAlias { alias, definition } => EncodedFrozenModel {
            kind: "frozen_alias",
            direct: None,
            alias: Some(alias.into_uuid()),
            alias_selected: Some(definition.selected().into_uuid()),
        },
    }
}

struct EncodedDelivery {
    kind: &'static str,
    descendant_scope: Option<&'static str>,
    expected_active_turn: Option<Uuid>,
    expected_defaults_version: Option<Decimal>,
    model_override_kind: Option<&'static str>,
    replacement: EncodedSelection,
    model_settings: Value,
}

fn encode_delivery(delivery: DeliveryRequest) -> EncodedDelivery {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration } => {
            encode_configured_delivery("start_when_no_active_turn", None, configuration)
        }
        DeliveryRequest::Interrupt {
            expected_active_turn,
            descendant_scope,
            configuration,
        } => {
            let mut encoded = encode_configured_delivery(
                "interrupt",
                Some(expected_active_turn.into_uuid()),
                configuration,
            );
            encoded.descendant_scope = Some(descendant_scope_to_str(descendant_scope));
            encoded
        }
        DeliveryRequest::NextSafePoint {
            expected_active_turn,
        } => EncodedDelivery {
            kind: "next_safe_point",
            descendant_scope: None,
            expected_active_turn: Some(expected_active_turn.into_uuid()),
            expected_defaults_version: None,
            model_override_kind: None,
            replacement: EncodedSelection::absent(),
            model_settings: model_settings_overlay_to_json(
                signalbox_domain::ModelSettingsOverlay::inherit_all(),
            ),
        },
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            configuration,
        } => encode_configured_delivery(
            "after_current_turn",
            Some(expected_active_turn.into_uuid()),
            configuration,
        ),
    }
}

fn encode_configured_delivery(
    kind: &'static str,
    expected_active_turn: Option<Uuid>,
    configuration: PerInputConfigurationChoices,
) -> EncodedDelivery {
    let (model_override_kind, replacement) = match configuration.model() {
        ModelSelectionOverride::UseSessionDefault => {
            ("use_session_default", EncodedSelection::absent())
        }
        ModelSelectionOverride::ReplaceWith(selection) => {
            ("replace_with", encode_selection(selection))
        }
    };
    EncodedDelivery {
        kind,
        descendant_scope: None,
        expected_active_turn,
        expected_defaults_version: Some(defaults_version_to_numeric(
            configuration.expected_session_defaults_version(),
        )),
        model_override_kind: Some(model_override_kind),
        replacement,
        model_settings: model_settings_overlay_to_json(configuration.model_settings()),
    }
}

struct EncodedResult {
    kind: &'static str,
    rejection_kind: Option<&'static str>,
    session: SessionId,
    accepted_input: Option<Uuid>,
    turn: Option<Uuid>,
    actual_active_turn: Option<Uuid>,
    expected_active_turn: Option<Uuid>,
    expected_defaults_version: Option<Decimal>,
    current_defaults_version: Option<Decimal>,
    unknown_alias: Option<Uuid>,
    selected_defaults_version: Option<Decimal>,
    last_position: Option<Decimal>,
    existing_interrupt_command: Option<Uuid>,
    attachment_digest: Option<Vec<u8>>,
    attachment_maximum_bytes: Option<Decimal>,
}

fn encode_result(
    result: &SubmitInputResult,
    delivery: DeliveryRequest,
    command_session: SessionId,
) -> EncodedResult {
    match result {
        SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(result)) => EncodedResult {
            kind: APPLIED,
            rejection_kind: None,
            session: result.session(),
            accepted_input: Some(accepted_input_id_to_uuid(result.accepted_input())),
            turn: Some(turn_id_to_uuid(result.turn())),
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(result)) => {
            EncodedResult {
                kind: APPLIED,
                rejection_kind: None,
                session: result.session(),
                accepted_input: Some(accepted_input_id_to_uuid(result.accepted_input())),
                turn: None,
                actual_active_turn: Some(turn_id_to_uuid(result.binding().source_turn())),
                expected_active_turn: None,
                expected_defaults_version: None,
                current_defaults_version: None,
                unknown_alias: None,
                selected_defaults_version: None,
                last_position: None,
                existing_interrupt_command: None,
                attachment_digest: None,
                attachment_maximum_bytes: None,
            }
        }
        SubmitInputResult::Rejected(SubmitInputRejectedResult::AttachmentBlobNotFound {
            digest,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("attachment_blob_not_found"),
            session: command_session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: Some(digest.as_bytes().to_vec()),
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
            maximum_bytes,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("attachment_byte_budget_exceeded"),
            session: command_session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: Some(Decimal::from(*maximum_bytes)),
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
            session,
            active_turn,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("active_turn_present"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*active_turn)),
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
            session,
            expected_active_turn,
            actual_active_turn,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("active_turn_mismatch"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*actual_active_turn)),
            expected_active_turn: Some(turn_id_to_uuid(*expected_active_turn)),
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::SessionNotFound { session }) => {
            EncodedResult {
                kind: REJECTED,
                rejection_kind: Some("session_not_found"),
                session: *session,
                accepted_input: None,
                turn: None,
                actual_active_turn: None,
                expected_active_turn: None,
                expected_defaults_version: None,
                current_defaults_version: None,
                unknown_alias: None,
                selected_defaults_version: None,
                last_position: None,
                existing_interrupt_command: None,
                attachment_digest: None,
                attachment_maximum_bytes: None,
            }
        }
        SubmitInputResult::Rejected(SubmitInputRejectedResult::NoActiveTurn {
            session,
            expected_active_turn,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("no_active_turn"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: Some(turn_id_to_uuid(*expected_active_turn)),
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                session,
                expected,
                current,
            },
        ) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("session_defaults_version_mismatch"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: Some(defaults_version_to_numeric(*expected)),
            current_defaults_version: Some(defaults_version_to_numeric(*current)),
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::UnknownModelAlias {
            session,
            alias,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("unknown_model_alias"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: Some(alias.into_uuid()),
            selected_defaults_version: configured_defaults_version(delivery)
                .map(defaults_version_to_numeric),
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::AcceptancePositionExhausted {
            session,
            last,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("acceptance_position_exhausted"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: Some(input_position_to_numeric(*last)),
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SafePointUnavailableWhileStopping {
                session,
                active_turn,
                existing_command,
            },
        ) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("safe_point_unavailable_while_stopping"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*active_turn)),
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: Some(durable_command_id_to_uuid(*existing_command)),
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::InterruptAlreadyApplied {
            session,
            active_turn,
            existing_command,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("interrupt_already_applied"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*active_turn)),
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: Some(durable_command_id_to_uuid(*existing_command)),
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                session,
                active_turn,
            },
        ) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("interrupt_unavailable_while_awaiting_approval"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*active_turn)),
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
    }
}

fn configured_defaults_version(
    delivery: DeliveryRequest,
) -> Option<SessionConfigurationDefaultsVersion> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration }
        | DeliveryRequest::Interrupt { configuration, .. }
        | DeliveryRequest::AfterCurrentTurn { configuration, .. } => {
            Some(configuration.expected_session_defaults_version())
        }
        DeliveryRequest::NextSafePoint { .. } => None,
    }
}

fn configured_model_settings(
    delivery: DeliveryRequest,
) -> Option<signalbox_domain::ModelSettingsOverlay> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration }
        | DeliveryRequest::Interrupt { configuration, .. }
        | DeliveryRequest::AfterCurrentTurn { configuration, .. } => {
            Some(configuration.model_settings())
        }
        DeliveryRequest::NextSafePoint { .. } => None,
    }
}

async fn load_complete_rows(
    connection: &mut PgConnection,
    command_ids: &[Uuid],
) -> Result<Vec<PgRow>, SubmitInputRepositoryError> {
    let rows = sqlx::query(
        "SELECT
            registry.command_id AS registry_command_id,
            registry.command_kind AS registry_kind,
            registry.storage_version AS registry_version,
            typed.command_id AS typed_command_id,
            typed.command_kind AS typed_kind,
            typed.storage_version AS typed_version,
            typed.session_id AS command_session_id,
            typed.actor_kind,
            typed.actor_turn_id,
            typed.actor_tool_request_id,
            (
                SELECT COALESCE(
                    jsonb_agg(
                        jsonb_build_object(
                            'position', part.position,
                            'part_kind', part.part_kind,
                            'text_value', part.text_value,
                            'blob_digest', CASE
                                WHEN part.blob_digest IS NULL THEN NULL
                                ELSE 'sha256:' || encode(part.blob_digest, 'hex')
                            END,
                            'attachment_kind', part.attachment_kind,
                            'declared_media_type', part.declared_media_type,
                            'display_filename', part.display_filename
                        ) ORDER BY part.position
                    ),
                    '[]'::jsonb
                )
                  FROM submit_input_command_content_part AS part
                 WHERE part.command_id = typed.command_id
            ) AS command_content_parts,
            typed.delivery_kind AS command_delivery_kind,
            typed.descendant_scope AS command_descendant_scope,
            typed.expected_active_turn_id AS command_expected_active_turn_id,
            typed.expected_defaults_version AS command_expected_defaults_version,
            typed.model_override_kind AS command_model_override_kind,
            typed.replacement_model_kind AS command_replacement_model_kind,
            typed.replacement_direct_model_selection_id AS command_replacement_direct_id,
            typed.replacement_model_alias_id AS command_replacement_alias_id,
            typed.model_settings_override AS command_model_settings_override,
            typed.result_kind,
            typed.rejection_kind,
            typed.result_session_id,
            typed.result_accepted_input_id,
            typed.result_turn_id,
            typed.result_actual_active_turn_id,
            typed.result_expected_active_turn_id,
            typed.result_expected_defaults_version,
            typed.result_current_defaults_version,
            typed.result_unknown_alias_id,
            typed.result_selected_defaults_version,
            typed.result_last_position,
            typed.result_existing_interrupt_command_id,
            typed.result_attachment_digest,
            typed.result_attachment_maximum_bytes,
            accepted.accepting_command_id,
            accepted.accepted_input_id,
            accepted.session_id AS accepted_session_id,
            (
                SELECT COALESCE(
                    jsonb_agg(
                        jsonb_build_object(
                            'position', part.position,
                            'part_kind', part.part_kind,
                            'text_value', part.text_value,
                            'blob_digest', CASE
                                WHEN part.blob_digest IS NULL THEN NULL
                                ELSE 'sha256:' || encode(part.blob_digest, 'hex')
                            END,
                            'attachment_kind', part.attachment_kind,
                            'declared_media_type', part.declared_media_type,
                            'display_filename', part.display_filename
                        ) ORDER BY part.position
                    ),
                    '[]'::jsonb
                )
                  FROM accepted_input_content_part AS part
                 WHERE part.accepted_input_id = accepted.accepted_input_id
            ) AS accepted_content_parts,
            accepted.delivery_kind AS accepted_delivery_kind,
            accepted.descendant_scope AS accepted_descendant_scope,
            accepted.expected_active_turn_id AS accepted_expected_active_turn_id,
            accepted.expected_defaults_version AS accepted_expected_defaults_version,
            accepted.model_override_kind AS accepted_model_override_kind,
            accepted.replacement_model_kind AS accepted_replacement_model_kind,
            accepted.replacement_direct_model_selection_id AS accepted_replacement_direct_id,
            accepted.replacement_model_alias_id AS accepted_replacement_alias_id,
            accepted.model_settings_override AS accepted_model_settings_override,
            accepted.acceptance_position AS accepted_position,
            accepted.disposition_kind,
            accepted.origin_turn_id,
            queued.turn_id AS queued_turn_id,
            queued.accepted_input_id AS queued_accepted_input_id,
            queued.session_id AS queued_session_id,
            queued.acceptance_position AS queued_position,
            queued.priority_kind,
            queued.interrupt_predecessor_turn_id,
            non_accepted_predecessor.session_id
                AS non_accepted_predecessor_session_id,
            non_accepted_predecessor.turn_id
                AS non_accepted_predecessor_turn_id,
            queued.defaults_version AS queued_defaults_version,
            queued.requested_model_kind,
            queued.requested_direct_model_selection_id,
            queued.requested_model_alias_id,
            queued.frozen_model_kind,
            queued.frozen_direct_model_selection_id,
            queued.frozen_model_alias_id,
            queued.frozen_alias_selected_direct_id,
            queued.model_parameters,
            queued.known_provider_failure_retry,
            queued.model_fallback,
            queued.dangerous_tool_auto_approval AS queued_tool_auto_approval,
            configuration_origin.model_settings_evidence_required
                AS origin_model_settings_evidence_required,
            defaults.session_id AS defaults_session_id,
            defaults.version AS defaults_version,
            defaults.model_selection_kind AS defaults_model_kind,
            defaults.direct_model_selection_id AS defaults_direct_id,
            defaults.model_alias_id AS defaults_alias_id,
            defaults.dangerous_tool_auto_approval AS defaults_tool_auto_approval,
            defaults.model_settings AS defaults_model_settings,
            settings.selected_direct_model_id AS settings_selected_direct_id,
            settings.session_id AS settings_session_id,
            settings.defaults_version AS settings_defaults_version,
            settings.per_call_model_settings,
            settings.resolved_model_settings,
            settings.adjusted_from_selection_id,
            settings.adjustments AS model_settings_adjustments,
            (
                SELECT count(*)
                  FROM accepted_input AS effect
                 WHERE effect.accepting_command_id = typed.command_id
            ) AS accepted_effect_count,
            (
                SELECT count(*)
                  FROM queued_input_origin AS effect_queue
                  JOIN accepted_input AS effect_input
                    ON effect_input.accepted_input_id = effect_queue.accepted_input_id
                 WHERE effect_input.accepting_command_id = typed.command_id
            ) AS queued_effect_count
         FROM durable_command AS registry
         LEFT JOIN submit_input_command AS typed
           ON typed.command_id = registry.command_id
         LEFT JOIN accepted_input AS accepted
           ON accepted.accepted_input_id = typed.result_accepted_input_id
         LEFT JOIN queued_input_origin AS queued
           ON queued.accepted_input_id = accepted.accepted_input_id
         LEFT JOIN turn_lifecycle AS non_accepted_predecessor
           ON non_accepted_predecessor.session_id = queued.session_id
          AND non_accepted_predecessor.turn_id = typed.expected_active_turn_id
          AND non_accepted_predecessor.origin_kind = 'delegation'
          AND (
                non_accepted_predecessor.state_kind = 'terminal'
                OR non_accepted_predecessor.delegation_runtime_terminal
          )
          AND typed.delivery_kind = 'interrupt'
          AND queued.priority_kind = 'interrupt_immediately_after'
          AND queued.interrupt_predecessor_turn_id =
                  non_accepted_predecessor.turn_id
          AND accepted_input_turn_queue_predecessor(
                  queued.session_id, queued.turn_id
              ) = non_accepted_predecessor.turn_id
         LEFT JOIN LATERAL (
              WITH RECURSIVE configuration_chain AS (
                  SELECT origin.*
                    FROM queued_input_origin AS origin
                   WHERE origin.turn_id = queued.turn_id
                     AND origin.session_id = queued.session_id
                  UNION
                  SELECT source.*
                    FROM configuration_chain AS current
                    JOIN queued_input_origin AS source
                      ON source.turn_id = current.source_configuration_turn_id
                     AND source.session_id = current.session_id
              )
              SELECT model_settings_evidence_required
                FROM configuration_chain
               WHERE source_configuration_turn_id IS NULL
         ) AS configuration_origin ON TRUE
         LEFT JOIN session_defaults_version AS defaults
           ON defaults.session_id = typed.result_session_id
          AND defaults.version = COALESCE(
                queued.defaults_version,
                typed.result_selected_defaults_version
              )
         LEFT JOIN turn_model_settings_resolved AS settings
           ON settings.accepted_input_id = accepted.accepted_input_id
          AND settings.turn_id = queued.turn_id
         WHERE registry.command_id = ANY($1)",
    )
    .bind(command_ids)
    .fetch_all(&mut *connection)
    .await?;

    Ok(rows)
}

async fn load_from_connection(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<ReconstitutedSubmitInput>, SubmitInputRepositoryError> {
    let command_uuid = durable_command_id_to_uuid(command_id);
    let mut rows = load_complete_rows(connection, &[command_uuid]).await?;
    let Some(row) = rows.pop() else {
        return Ok(None);
    };
    if !rows.is_empty() {
        return Err(SubmitInputCorruption::Inconsistent("duplicate complete command rows").into());
    }
    let related = load_related_turn_evidence(connection, &row).await?;
    let existing_interrupt = load_existing_interrupt(connection, &row).await?;
    decode_complete(
        row,
        command_id,
        related.origin,
        related.non_accepted_predecessor,
        existing_interrupt,
    )
    .map(Some)
}

async fn load_existing_interrupt(
    connection: &mut PgConnection,
    row: &PgRow,
) -> Result<Option<AppliedInterruptCommandResult>, SubmitInputRepositoryError> {
    let Some(command_uuid) =
        row.try_get::<Option<Uuid>, _>("result_existing_interrupt_command_id")?
    else {
        return Ok(None);
    };
    let command = durable_command_id_from_uuid(command_uuid)
        .map_err(|_| SubmitInputCorruption::Inconsistent("existing interrupt command identity"))?;
    let mut rows = load_complete_rows(connection, &[command_uuid]).await?;
    let interrupt_row = rows
        .pop()
        .ok_or(SubmitInputCorruption::Missing("existing interrupt command"))?;
    if !rows.is_empty() {
        return Err(
            SubmitInputCorruption::Inconsistent("duplicate existing interrupt command").into(),
        );
    }
    let predecessor = load_related_turn_evidence(connection, &interrupt_row).await?;
    let receipt = decode_complete(
        interrupt_row,
        command,
        predecessor.origin,
        predecessor.non_accepted_predecessor,
        None,
    )?;
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) = receipt.result()
    else {
        return Err(
            SubmitInputCorruption::Inconsistent("existing interrupt was not applied").into(),
        );
    };
    origin
        .applied_interrupt()
        .copied()
        .map(Some)
        .ok_or_else(|| SubmitInputCorruption::Inconsistent("existing interrupt authority").into())
}

struct RelatedTurnEvidence {
    origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    non_accepted_predecessor: Option<NonAcceptedTurnPredecessorReconstitutionInput>,
}

async fn load_related_turn_evidence(
    connection: &mut PgConnection,
    row: &PgRow,
) -> Result<RelatedTurnEvidence, SubmitInputRepositoryError> {
    let Some(key) = related_turn_origin_key(row)? else {
        if non_accepted_predecessor(row)?.is_some() {
            return Err(SubmitInputCorruption::Inconsistent(
                "non-accepted predecessor without related turn",
            )
            .into());
        }
        return Ok(RelatedTurnEvidence {
            origin: None,
            non_accepted_predecessor: None,
        });
    };
    if let Some(predecessor) = non_accepted_predecessor(row)? {
        if (
            predecessor.session.into_uuid(),
            predecessor.turn.into_uuid(),
        ) != key
        {
            return Err(SubmitInputCorruption::Inconsistent(
                "non-accepted predecessor correlation",
            )
            .into());
        }
        return Ok(RelatedTurnEvidence {
            origin: None,
            non_accepted_predecessor: Some(predecessor),
        });
    }
    let mut origins = load_turn_origin_graph(connection, &BTreeSet::from([key])).await?;
    let origin = origins
        .remove(&key)
        .ok_or(SubmitInputCorruption::Missing("related turn origin"))?;
    Ok(RelatedTurnEvidence {
        origin: Some(origin),
        non_accepted_predecessor: None,
    })
}

fn non_accepted_predecessor(
    row: &PgRow,
) -> Result<Option<NonAcceptedTurnPredecessorReconstitutionInput>, SubmitInputRepositoryError> {
    let session: Option<Uuid> = row.try_get("non_accepted_predecessor_session_id")?;
    let turn: Option<Uuid> = row.try_get("non_accepted_predecessor_turn_id")?;
    match (session, turn) {
        (None, None) => Ok(None),
        (Some(session), Some(turn)) => Ok(Some(NonAcceptedTurnPredecessorReconstitutionInput {
            session: session_id_from_uuid(session),
            turn: turn_id_from_uuid(turn),
        })),
        _ => Err(SubmitInputCorruption::Inconsistent("non-accepted predecessor shape").into()),
    }
}

fn related_turn_origin_key(
    row: &PgRow,
) -> Result<Option<StoredTurnOriginKey>, SubmitInputRepositoryError> {
    let result_kind: Option<String> = row.try_get("result_kind")?;
    let rejection_kind: Option<String> = row.try_get("rejection_kind")?;
    let delivery_kind: Option<String> = row.try_get("command_delivery_kind")?;
    let source_turn = match (
        result_kind.as_deref(),
        rejection_kind.as_deref(),
        delivery_kind.as_deref(),
    ) {
        (Some(APPLIED), None, Some("interrupt" | "after_current_turn" | "next_safe_point")) => {
            required(row, "command_expected_active_turn_id")?
        }
        (
            Some(REJECTED),
            Some(
                "active_turn_present"
                | "active_turn_mismatch"
                | "safe_point_unavailable_while_stopping"
                | "interrupt_already_applied"
                | "interrupt_unavailable_while_awaiting_approval",
            ),
            _,
        ) => required(row, "result_actual_active_turn_id")?,
        (
            Some(REJECTED),
            Some(
                "session_defaults_version_mismatch"
                | "unknown_model_alias"
                | "acceptance_position_exhausted",
            ),
            Some("interrupt" | "after_current_turn" | "next_safe_point"),
        ) => required(row, "command_expected_active_turn_id")?,
        _ => return Ok(None),
    };
    Ok(Some((required(row, "result_session_id")?, source_turn)))
}

fn decode_stored_turn_origin_provenance(
    row: &PgRow,
) -> Result<(StoredTurnOriginProvenance, Option<Uuid>), SubmitInputRepositoryError> {
    let command: Option<Uuid> = row.try_get("origin_command_id")?;
    let generation: Option<Decimal> = row.try_get("origin_goal_generation")?;
    match (command, generation) {
        // A bound dispatch turn carries both: the command accepted its tagged
        // context and the generation records the authority it runs under. The
        // command is what reconstitutes the origin, so it decides the shape and
        // the generation rides alongside it.
        (Some(command), _) => {
            let command_id = durable_command_id_from_uuid(command)
                .map_err(|_| SubmitInputCorruption::Inconsistent("turn origin command identity"))?;
            Ok((
                StoredTurnOriginProvenance::Submit(command_id),
                Some(command),
            ))
        }
        (None, Some(generation)) => {
            let generation = positive_u64_from_numeric(generation).map_err(|reason| {
                SubmitInputCorruption::InvalidOrdinal {
                    field: "origin_goal_generation",
                    reason,
                }
            })?;
            let generation = NonZeroU64::new(generation).ok_or(
                SubmitInputCorruption::Inconsistent("goal origin generation"),
            )?;
            let source_event: Option<Decimal> = row.try_get("origin_goal_source_event_ordinal")?;
            let predecessor: Option<Uuid> = row.try_get("origin_goal_predecessor_turn_id")?;
            let source = match (source_event, predecessor) {
                (Some(event), None) => {
                    let event = positive_u64_from_numeric(event).map_err(|reason| {
                        SubmitInputCorruption::InvalidOrdinal {
                            field: "origin_goal_source_event_ordinal",
                            reason,
                        }
                    })?;
                    GoalTurnSource::UserEvent(GoalEventOrdinal::new(NonZeroU64::new(event).ok_or(
                        SubmitInputCorruption::Inconsistent("goal origin source event"),
                    )?))
                }
                (None, Some(predecessor)) => {
                    GoalTurnSource::SuccessfulTurn(turn_id_from_uuid(predecessor))
                }
                (Some(_), Some(_)) | (None, None) => {
                    return Err(SubmitInputCorruption::Inconsistent("goal origin source").into());
                }
            };
            let content = decode_content(
                required(row, "origin_content_parts")?,
                "goal origin content",
            )?;
            Ok((
                StoredTurnOriginProvenance::Goal {
                    generation: GoalGeneration::new(generation),
                    source,
                    content,
                },
                None,
            ))
        }
        // Neither a command nor a generation names no origin at all.
        (None, None) => Err(SubmitInputCorruption::Inconsistent("turn origin provenance").into()),
    }
}

pub(crate) async fn load_turn_origin_graph(
    connection: &mut PgConnection,
    roots: &BTreeSet<StoredTurnOriginKey>,
) -> Result<
    BTreeMap<StoredTurnOriginKey, SubmitInputTurnOriginReconstitutionInput>,
    SubmitInputRepositoryError,
> {
    if roots.is_empty() {
        return Ok(BTreeMap::new());
    }

    let source_sessions = roots
        .iter()
        .map(|(session, _)| *session)
        .collect::<Vec<_>>();
    let source_turns = roots.iter().map(|(_, turn)| *turn).collect::<Vec<_>>();
    let link_rows = sqlx::query(
        "WITH RECURSIVE origin_turn(session_id, turn_id) AS (
            SELECT root.session_id, root.turn_id
              FROM UNNEST($1::uuid[], $2::uuid[]) AS root(session_id, turn_id)
            UNION
            SELECT
                current.session_id,
                CASE accepted.disposition_kind
                    WHEN 'reclassified_as_turn_origin'
                        THEN accepted.expected_active_turn_id
                    ELSE command.expected_active_turn_id
                END
              FROM origin_turn AS current
              JOIN turn_lifecycle AS turn
                ON turn.turn_id = current.turn_id
               AND turn.session_id = current.session_id
              JOIN queued_input_origin AS queued
                ON queued.turn_id = turn.turn_id
               AND queued.session_id = turn.session_id
               AND queued.accepted_input_id = turn.origin_accepted_input_id
              JOIN accepted_input AS accepted
                ON accepted.accepted_input_id = queued.accepted_input_id
               AND accepted.session_id = turn.session_id
               AND accepted.origin_turn_id = turn.turn_id
               AND accepted.disposition_kind IN (
                    'origin_of',
                    'reclassified_as_turn_origin'
               )
              LEFT JOIN submit_input_command AS command
                ON command.command_id = accepted.accepting_command_id
             WHERE (
                    accepted.disposition_kind = 'reclassified_as_turn_origin'
                    AND accepted.expected_active_turn_id IS NOT NULL
               ) OR (
                    accepted.disposition_kind = 'origin_of'
                    AND command.delivery_kind IN (
                        'interrupt',
                        'after_current_turn'
                    )
                    AND command.expected_active_turn_id IS NOT NULL
               )
        )
        SELECT
            current.session_id AS origin_session_id,
            current.turn_id AS origin_turn_id,
            accepted.accepting_command_id AS origin_command_id,
            accepted.accepted_input_id AS origin_accepted_input_id,
            (
                SELECT COALESCE(
                    jsonb_agg(
                        jsonb_build_object(
                            'position', part.position,
                            'part_kind', part.part_kind,
                            'text_value', part.text_value,
                            'blob_digest', CASE
                                WHEN part.blob_digest IS NULL THEN NULL
                                ELSE 'sha256:' || encode(part.blob_digest, 'hex')
                            END,
                            'attachment_kind', part.attachment_kind,
                            'declared_media_type', part.declared_media_type,
                            'display_filename', part.display_filename
                        ) ORDER BY part.position
                    ),
                    '[]'::jsonb
                )
                  FROM accepted_input_content_part AS part
                 WHERE part.accepted_input_id = accepted.accepted_input_id
            ) AS origin_content_parts,
            goal.goal_generation AS origin_goal_generation,
            goal.source_event_ordinal AS origin_goal_source_event_ordinal,
            goal.predecessor_turn_id AS origin_goal_predecessor_turn_id,
            accepted.disposition_kind AS origin_disposition_kind,
            accepted.expected_active_turn_id AS reclassified_source_turn_id,
            queued.acceptance_position AS origin_acceptance_position,
            queued.priority_kind AS origin_priority_kind,
            queued.interrupt_predecessor_turn_id AS origin_interrupt_predecessor_turn_id,
            command.delivery_kind AS origin_delivery_kind,
            command.expected_active_turn_id AS origin_predecessor_turn_id,
            source.state_kind AS source_state_kind,
            source.terminal_disposition_kind AS source_terminal_disposition_kind,
            source.terminal_model_call_id AS source_terminal_model_call_id,
            source.terminal_tool_attempt_id AS source_terminal_tool_attempt_id,
            source_automatic.model_call_id
                AS source_automatic_reconciliation_model_call_id,
            source_automatic.tool_attempt_id
                AS source_automatic_reconciliation_tool_attempt_id,
            source_automatic.state_kind
                AS source_automatic_reconciliation_state_kind,
            source_automatic.attempt_count
                AS source_automatic_reconciliation_attempt_count,
            COALESCE(
                source_attempt.interrupt_command_id,
                source_interrupt.command_id
            ) AS source_interrupt_command_id
          FROM origin_turn AS current
          JOIN turn_lifecycle AS turn
            ON turn.turn_id = current.turn_id
           AND turn.session_id = current.session_id
          JOIN queued_input_origin AS queued
            ON queued.turn_id = turn.turn_id
           AND queued.session_id = turn.session_id
           AND queued.accepted_input_id = turn.origin_accepted_input_id
          JOIN accepted_input AS accepted
            ON accepted.accepted_input_id = queued.accepted_input_id
           AND accepted.session_id = turn.session_id
           AND accepted.origin_turn_id = turn.turn_id
           AND accepted.disposition_kind IN (
                'origin_of',
                'reclassified_as_turn_origin'
           )
          LEFT JOIN submit_input_command AS command
            ON command.command_id = accepted.accepting_command_id
          LEFT JOIN goal_turn AS goal
            ON goal.session_id = accepted.session_id
           AND goal.turn_id = accepted.origin_turn_id
           AND goal.accepted_input_id = accepted.accepted_input_id
          LEFT JOIN turn_lifecycle AS source
            ON source.turn_id = accepted.expected_active_turn_id
           AND source.session_id = accepted.session_id
          LEFT JOIN turn_attempt AS source_attempt
            ON source_attempt.turn_attempt_id = source.terminal_attempt_id
           AND source_attempt.turn_id = source.turn_id
           AND source_attempt.session_id = source.session_id
          LEFT JOIN automatic_reconciliation AS source_automatic
            ON source_automatic.turn_id = source.turn_id
           AND source_automatic.session_id = source.session_id
          LEFT JOIN LATERAL (
                SELECT interrupt.command_id
                  FROM submit_input_command AS interrupt
                  JOIN accepted_input AS interrupt_accepted
                    ON interrupt_accepted.accepting_command_id =
                        interrupt.command_id
                   AND interrupt_accepted.accepted_input_id =
                        interrupt.result_accepted_input_id
                   AND interrupt_accepted.session_id =
                        interrupt.result_session_id
                   AND interrupt_accepted.origin_turn_id =
                        interrupt.result_turn_id
                  JOIN queued_input_origin AS interrupt_successor
                    ON interrupt_successor.accepted_input_id =
                        interrupt_accepted.accepted_input_id
                   AND interrupt_successor.turn_id =
                        interrupt_accepted.origin_turn_id
                   AND interrupt_successor.session_id =
                        interrupt_accepted.session_id
                   AND interrupt_successor.priority_kind =
                        'interrupt_immediately_after'
                   AND interrupt_successor.interrupt_predecessor_turn_id =
                        source.turn_id
                 WHERE interrupt.session_id = source.session_id
                   AND interrupt.delivery_kind = 'interrupt'
                   AND interrupt.expected_active_turn_id = source.turn_id
                   AND interrupt.result_kind = 'applied'
                   AND interrupt.rejection_kind IS NULL
                   AND interrupt_accepted.disposition_kind = 'origin_of'
          ) AS source_interrupt ON TRUE
         ORDER BY current.session_id, current.turn_id",
    )
    .bind(&source_sessions)
    .bind(&source_turns)
    .fetch_all(&mut *connection)
    .await?;

    let mut links = BTreeMap::new();
    let mut commands = BTreeMap::new();
    for row in link_rows {
        let key = (
            required(&row, "origin_session_id")?,
            required(&row, "origin_turn_id")?,
        );
        let (provenance, command_uuid) = decode_stored_turn_origin_provenance(&row)?;
        let accepted_input =
            accepted_input_id_from_uuid(required(&row, "origin_accepted_input_id")?);
        let queue_position = decode_position(&row, "origin_acceptance_position")?;
        let interrupt_predecessor: Option<Uuid> =
            row.try_get("origin_interrupt_predecessor_turn_id")?;
        let queue_order = match required::<String>(&row, "origin_priority_kind")?.as_str() {
            "ordinary" if interrupt_predecessor.is_none() => {
                AcceptedInputQueueOrder::ordinary(queue_position)
            }
            "interrupt_immediately_after" => AcceptedInputQueueOrder::interrupt_immediately_after(
                queue_position,
                turn_id_from_uuid(interrupt_predecessor.ok_or(SubmitInputCorruption::Missing(
                    "origin_interrupt_predecessor_turn_id",
                ))?),
            ),
            "ordinary" => {
                return Err(SubmitInputCorruption::Inconsistent("ordinary origin priority").into());
            }
            value => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "origin_priority_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        let disposition_kind: String = required(&row, "origin_disposition_kind")?;
        let delivery_kind: Option<String> = row.try_get("origin_delivery_kind")?;
        let predecessor_turn: Option<Uuid> = row.try_get("origin_predecessor_turn_id")?;
        let reclassified_source: Option<Uuid> = row.try_get("reclassified_source_turn_id")?;
        let source_state: Option<String> = row.try_get("source_state_kind")?;
        let source_disposition: Option<String> = row.try_get("source_terminal_disposition_kind")?;
        let kind = match &provenance {
            StoredTurnOriginProvenance::Goal { .. } => {
                let ordinary_priority = match queue_order.priority() {
                    AcceptedInputQueuePriority::Ordinary => true,
                    AcceptedInputQueuePriority::InterruptImmediatelyAfter { .. } => false,
                };
                if disposition_kind != "origin_of"
                    || delivery_kind.is_some()
                    || predecessor_turn.is_some()
                    || reclassified_source.is_some()
                    || !ordinary_priority
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("goal turn origin shape").into(),
                    );
                }
                StoredTurnOriginKind::Direct { predecessor: None }
            }
            StoredTurnOriginProvenance::Submit(_) => match (
                disposition_kind.as_str(),
                delivery_kind
                    .as_deref()
                    .ok_or(SubmitInputCorruption::Missing("origin_delivery_kind"))?,
                predecessor_turn,
                reclassified_source,
            ) {
                ("origin_of", "start_when_no_active_turn", None, None) => {
                    StoredTurnOriginKind::Direct { predecessor: None }
                }
                ("origin_of", "after_current_turn", Some(turn), Some(source)) if turn == source => {
                    StoredTurnOriginKind::Direct {
                        predecessor: Some((key.0, turn)),
                    }
                }
                ("origin_of", "interrupt", Some(turn), Some(source))
                    if turn == source && interrupt_predecessor == Some(turn) =>
                {
                    StoredTurnOriginKind::Direct {
                        predecessor: Some((key.0, turn)),
                    }
                }
                ("reclassified_as_turn_origin", "next_safe_point", Some(source), Some(binding))
                    if source == binding && source_state.as_deref() == Some("terminal") =>
                {
                    let source_disposition = match source_disposition.as_deref() {
                        Some("completed") => StoredTerminalTurnDisposition::Completed,
                        Some("refused") => StoredTerminalTurnDisposition::Refused,
                        Some("failed") => StoredTerminalTurnDisposition::Failed,
                        Some("cancelled") => {
                            let command = durable_command_id_from_uuid(required(
                                &row,
                                "source_interrupt_command_id",
                            )?)
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "cancelled source interrupt command",
                                )
                            })?;
                            StoredTerminalTurnDisposition::Cancelled {
                                interrupt_command: command,
                            }
                        }
                        Some("reconciliation_required") => {
                            let model_call: Option<Uuid> =
                                row.try_get("source_terminal_model_call_id")?;
                            let tool_attempt: Option<Uuid> =
                                row.try_get("source_terminal_tool_attempt_id")?;
                            let ambiguous_operation = match (model_call, tool_attempt) {
                                (Some(call), None) => {
                                    IssuedOperationRef::ModelCall(ModelCallId::from_uuid(call))
                                }
                                (None, Some(attempt)) => IssuedOperationRef::ToolAttempt(
                                    ToolAttemptId::from_uuid(attempt),
                                ),
                                (Some(_), Some(_)) | (None, None) => {
                                    return Err(SubmitInputCorruption::Inconsistent(
                                        "reconciliation source ambiguous operation",
                                    )
                                    .into());
                                }
                            };
                            let command: Option<Uuid> =
                                row.try_get("source_interrupt_command_id")?;
                            let automatic_call: Option<Uuid> =
                                row.try_get("source_automatic_reconciliation_model_call_id")?;
                            let automatic_tool_attempt: Option<Uuid> =
                                row.try_get("source_automatic_reconciliation_tool_attempt_id")?;
                            let automatic_state: Option<String> =
                                row.try_get("source_automatic_reconciliation_state_kind")?;
                            let automatic_attempts: Option<i32> =
                                row.try_get("source_automatic_reconciliation_attempt_count")?;
                            let authority = if let Some(command) = command {
                                StoredAutomaticReconciliationAuthority::AppliedInterrupt(
                                    durable_command_id_from_uuid(command).map_err(|_| {
                                        SubmitInputCorruption::Inconsistent(
                                            "reconciliation source interrupt command",
                                        )
                                    })?,
                                )
                            } else {
                                let (Some("reconciled"), Some(attempts)) =
                                    (automatic_state.as_deref(), automatic_attempts)
                                else {
                                    return Err(SubmitInputCorruption::Missing(
                                        "reconciliation source authority",
                                    )
                                    .into());
                                };
                                let operation_matches = match ambiguous_operation {
                                    IssuedOperationRef::ModelCall(ambiguous_call) => {
                                        automatic_call == Some(ambiguous_call.into_uuid())
                                            && automatic_tool_attempt.is_none()
                                    }
                                    IssuedOperationRef::ToolAttempt(ambiguous_attempt) => {
                                        automatic_tool_attempt
                                            == Some(ambiguous_attempt.into_uuid())
                                            && automatic_call.is_none()
                                    }
                                };
                                if !operation_matches {
                                    return Err(SubmitInputCorruption::Inconsistent(
                                        "reconciliation source automatic operation",
                                    )
                                    .into());
                                }
                                let attempt = u32::try_from(attempts)
                                    .ok()
                                    .and_then(NonZeroU32::new)
                                    .filter(|attempt| attempt.get() <= 5)
                                    .ok_or(SubmitInputCorruption::Inconsistent(
                                        "reconciliation source automatic attempt",
                                    ))?;
                                StoredAutomaticReconciliationAuthority::AutomaticRecovery(attempt)
                            };
                            StoredTerminalTurnDisposition::ReconciliationRequired {
                                authority,
                                ambiguous_operation,
                            }
                        }
                        Some(value) => {
                            return Err(SubmitInputCorruption::Unsupported {
                                field: "reclassified source terminal disposition",
                                value: value.to_owned(),
                            }
                            .into());
                        }
                        None => {
                            return Err(SubmitInputCorruption::Missing(
                                "reclassified source terminal disposition",
                            )
                            .into());
                        }
                    };
                    StoredTurnOriginKind::Reclassified {
                        source: (key.0, source),
                        source_disposition,
                    }
                }
                ("origin_of" | "reclassified_as_turn_origin", _, _, _) => {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "turn origin predecessor shape",
                    )
                    .into());
                }
                (value, _, _, _) => {
                    return Err(SubmitInputCorruption::Unsupported {
                        field: "turn origin accepted-input disposition_kind",
                        value: value.to_owned(),
                    }
                    .into());
                }
            },
        };
        if links
            .insert(
                key,
                StoredTurnOriginLink {
                    provenance,
                    kind,
                    accepted_input,
                    queue_order,
                },
            )
            .is_some()
        {
            return Err(SubmitInputCorruption::Inconsistent("duplicate turn origin").into());
        }
        if let Some(command_uuid) = command_uuid
            && commands.insert(command_uuid, key).is_some()
        {
            return Err(
                SubmitInputCorruption::Inconsistent("turn origin command reused by turns").into(),
            );
        }
    }

    for root in roots {
        if !links.contains_key(root) {
            return Err(SubmitInputCorruption::Missing("related turn origin").into());
        }
    }
    for link in links.values() {
        if let Some(dependency) = link.kind.dependency()
            && !links.contains_key(&dependency)
        {
            return Err(SubmitInputCorruption::Missing("turn origin predecessor").into());
        }
    }

    let command_uuids = commands.keys().copied().collect::<Vec<_>>();
    let complete_rows = load_complete_rows(connection, &command_uuids).await?;
    let mut rows_by_command = BTreeMap::new();
    for row in complete_rows {
        let command_uuid: Uuid = required(&row, "registry_command_id")?;
        if !commands.contains_key(&command_uuid) {
            return Err(
                SubmitInputCorruption::Inconsistent("unexpected turn origin command").into(),
            );
        }
        if rows_by_command.insert(command_uuid, row).is_some() {
            return Err(
                SubmitInputCorruption::Inconsistent("duplicate turn origin command rows").into(),
            );
        }
    }
    if rows_by_command.len() != commands.len() {
        return Err(SubmitInputCorruption::Missing("turn origin command").into());
    }

    let decode_order = turn_origin_dependency_order(
        links
            .iter()
            .map(|(key, link)| (*key, link.kind.dependency())),
    )
    .ok_or(SubmitInputCorruption::Inconsistent(
        "turn origin predecessor cycle",
    ))?;
    let mut decoded = BTreeMap::new();
    for ready in decode_order {
        let link = links
            .remove(&ready)
            .ok_or(SubmitInputCorruption::Missing("related turn origin"))?;
        let command_id = match link.provenance {
            StoredTurnOriginProvenance::Submit(command_id) => command_id,
            StoredTurnOriginProvenance::Goal {
                generation,
                source,
                content,
            } => {
                let direct_origin = match link.kind {
                    StoredTurnOriginKind::Direct { predecessor: None } => true,
                    StoredTurnOriginKind::Direct {
                        predecessor: Some(_),
                    }
                    | StoredTurnOriginKind::Reclassified { .. } => false,
                };
                if !direct_origin {
                    return Err(
                        SubmitInputCorruption::Inconsistent("goal origin dependency").into(),
                    );
                }
                let turn = turn_id_from_uuid(ready.1);
                let reconstructed = SubmitInputTurnOriginReconstitutionInput::from_goal(
                    GoalTurnOriginConstructionInput {
                        generation,
                        source,
                        session: session_id_from_uuid(ready.0),
                        accepted_input: link.accepted_input,
                        turn,
                        acceptance_position: link.queue_order.acceptance_position(),
                        content,
                        lifecycle: AcceptedInputLifecycle::new(
                            link.accepted_input,
                            AcceptedInputDisposition::OriginOf(turn),
                        ),
                        queue_accepted_input: link.accepted_input,
                        queue_session: session_id_from_uuid(ready.0),
                        queue_turn: turn,
                        queue_order: link.queue_order,
                    },
                );
                decoded.insert(ready, reconstructed);
                continue;
            }
        };
        let command_uuid = durable_command_id_to_uuid(command_id);
        let row = rows_by_command
            .remove(&command_uuid)
            .ok_or(SubmitInputCorruption::Missing("turn origin command"))?;
        let dependency = link
            .kind
            .dependency()
            .map(|key| {
                decoded
                    .get(&key)
                    .cloned()
                    .ok_or(SubmitInputCorruption::Missing("turn origin predecessor"))
            })
            .transpose()?;
        let receipt = decode_complete(row, command_id, dependency.clone(), None, None)?;
        let reconstructed = match link.kind {
            StoredTurnOriginKind::Direct { .. } => {
                let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
                    receipt.result()
                else {
                    return Err(
                        SubmitInputCorruption::Inconsistent("turn origin command result").into(),
                    );
                };
                if session_id_to_uuid(applied.session()) != ready.0
                    || turn_id_to_uuid(applied.turn()) != ready.1
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("turn origin correlation").into(),
                    );
                }
                SubmitInputTurnOriginReconstitutionInput::new(
                    SubmitInputDirectTurnOriginConstructionInput {
                        receipt,
                        lifecycle: AcceptedInputLifecycle::new(
                            link.accepted_input,
                            AcceptedInputDisposition::OriginOf(turn_id_from_uuid(ready.1)),
                        ),
                        queue_accepted_input: link.accepted_input,
                        queue_session: session_id_from_uuid(ready.0),
                        queue_turn: turn_id_from_uuid(ready.1),
                        queue_order: link.queue_order,
                    },
                )
            }
            StoredTurnOriginKind::Reclassified {
                source,
                source_disposition,
            } => {
                let SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(applied)) =
                    receipt.result()
                else {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "reclassified origin command result",
                    )
                    .into());
                };
                if session_id_to_uuid(applied.session()) != ready.0
                    || applied.accepted_input() != link.accepted_input
                    || applied.binding().source_turn() != turn_id_from_uuid(source.1)
                {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "reclassified origin correlation",
                    )
                    .into());
                }
                let source_origin = dependency
                    .ok_or(SubmitInputCorruption::Missing("reclassified source origin"))?;
                let source_turn = turn_id_from_uuid(source.1);
                let source_terminal = match source_disposition {
                    StoredTerminalTurnDisposition::Completed
                    | StoredTerminalTurnDisposition::Refused
                    | StoredTerminalTurnDisposition::Failed => {
                        SubmitInputTerminalSourceReconstitutionInput::new(
                            SubmitInputTerminalSourceConstructionInput {
                                origin: source_origin.clone(),
                                turn: source_turn,
                                disposition: source_disposition.unstopped_domain().ok_or(
                                    SubmitInputCorruption::Inconsistent(
                                        "terminal source disposition",
                                    ),
                                )?,
                            },
                        )
                    }
                    StoredTerminalTurnDisposition::Cancelled { interrupt_command } => {
                        let interrupt_uuid = durable_command_id_to_uuid(interrupt_command);
                        let mut interrupt_rows =
                            load_complete_rows(connection, &[interrupt_uuid]).await?;
                        let interrupt_row = interrupt_rows.pop().ok_or(
                            SubmitInputCorruption::Missing("cancelled source interrupt command"),
                        )?;
                        if !interrupt_rows.is_empty() {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "duplicate cancelled source interrupt command",
                            )
                            .into());
                        }
                        let interrupt_receipt = decode_complete(
                            interrupt_row,
                            interrupt_command,
                            Some(source_origin.clone()),
                            None,
                            None,
                        )?;
                        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                            interrupt_origin,
                        )) = interrupt_receipt.result()
                        else {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "cancelled source interrupt result",
                            )
                            .into());
                        };
                        SubmitInputTerminalSourceReconstitutionInput::new(
                            SubmitInputTerminalSourceConstructionInput {
                                origin: source_origin.clone(),
                                turn: source_turn,
                                disposition: signalbox_domain::TurnDisposition::Cancelled {
                                    cause: interrupt_origin
                                        .applied_interrupt()
                                        .ok_or(SubmitInputCorruption::Inconsistent(
                                            "cancelled source interrupt authority",
                                        ))?
                                        .proof(),
                                },
                            },
                        )
                    }
                    StoredTerminalTurnDisposition::ReconciliationRequired {
                        authority,
                        ambiguous_operation,
                    } => match authority {
                        StoredAutomaticReconciliationAuthority::AppliedInterrupt(
                            interrupt_command,
                        ) => {
                            let interrupt_uuid = durable_command_id_to_uuid(interrupt_command);
                            let mut interrupt_rows =
                                load_complete_rows(connection, &[interrupt_uuid]).await?;
                            let interrupt_row =
                                interrupt_rows.pop().ok_or(SubmitInputCorruption::Missing(
                                    "reconciliation source interrupt command",
                                ))?;
                            if !interrupt_rows.is_empty() {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "duplicate reconciliation source interrupt command",
                                )
                                .into());
                            }
                            let interrupt_receipt = decode_complete(
                                interrupt_row,
                                interrupt_command,
                                Some(source_origin.clone()),
                                None,
                                None,
                            )?;
                            let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                                interrupt_origin,
                            )) = interrupt_receipt.result()
                            else {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "reconciliation source interrupt result",
                                )
                                .into());
                            };
                            let interrupt = interrupt_origin
                                .applied_interrupt()
                                .ok_or(SubmitInputCorruption::Inconsistent(
                                    "reconciliation source interrupt authority",
                                ))?
                                .proof();
                            match ambiguous_operation {
                            IssuedOperationRef::ModelCall(ambiguous_call) => {
                                SubmitInputTerminalSourceReconstitutionInput::
                                    interrupted_model_call_reconciliation(
                                        SubmitInputInterruptedModelCallReconciliationConstructionInput {
                                            origin: source_origin.clone(),
                                            turn: source_turn,
                                            ambiguous_call,
                                            interrupt,
                                        },
                                    )
                            }
                            IssuedOperationRef::ToolAttempt(ambiguous_attempt) => {
                                SubmitInputTerminalSourceReconstitutionInput::
                                    interrupted_tool_reconciliation(
                                        SubmitInputInterruptedToolReconciliationConstructionInput {
                                            origin: source_origin.clone(),
                                            turn: source_turn,
                                            ambiguous_attempt,
                                            interrupt,
                                        },
                                    )
                            }
                            }
                        }
                        StoredAutomaticReconciliationAuthority::AutomaticRecovery(attempt) => {
                            SubmitInputTerminalSourceReconstitutionInput::automatic_reconciliation(
                                SubmitInputAutomaticReconciliationConstructionInput {
                                    origin: source_origin.clone(),
                                    turn: source_turn,
                                    ambiguous_operation,
                                    attempt,
                                },
                            )
                        }
                    },
                };
                SubmitInputTurnOriginReconstitutionInput::reclassified(
                    SubmitInputReclassifiedTurnOriginConstructionInput {
                        receipt,
                        lifecycle: AcceptedInputLifecycle::new(
                            link.accepted_input,
                            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                                turn: turn_id_from_uuid(ready.1),
                                reason: SteeringReclassificationReason::NoSafePointBeforeTerminal,
                            },
                        ),
                        queue_accepted_input: link.accepted_input,
                        queue_session: session_id_from_uuid(ready.0),
                        queue_turn: turn_id_from_uuid(ready.1),
                        queue_order: link.queue_order,
                        source_terminal,
                    },
                )
            }
        };
        decoded.insert(ready, reconstructed);
    }
    debug_assert!(links.is_empty());

    Ok(decoded)
}

fn decode_complete(
    row: PgRow,
    command_id: DurableCommandId,
    related_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    non_accepted_predecessor: Option<NonAcceptedTurnPredecessorReconstitutionInput>,
    existing_interrupt: Option<AppliedInterruptCommandResult>,
) -> Result<ReconstitutedSubmitInput, SubmitInputRepositoryError> {
    require_spelling(&row, "registry_kind", SUBMIT_INPUT_KIND)?;
    let registry_version = require_supported_version(&row, "registry_version")?;
    let typed_id: Uuid = required(&row, "typed_command_id")?;
    if typed_id != durable_command_id_to_uuid(command_id) {
        return Err(SubmitInputCorruption::Inconsistent("typed command identity").into());
    }
    require_spelling(&row, "typed_kind", SUBMIT_INPUT_KIND)?;
    let typed_version = require_supported_version(&row, "typed_version")?;
    if registry_version != typed_version {
        return Err(SubmitInputCorruption::Inconsistent("command storage version").into());
    }

    // Decode-level checks reject unknown or malformed actor spellings here;
    // comparing the decoded actor against the canonical command's actor is
    // domain-owned semantics and happens inside reconstitution.
    let actor = decode_actor(
        required(&row, "actor_kind")?,
        row.try_get("actor_turn_id")?,
        row.try_get("actor_tool_request_id")?,
    )?;
    let command_model_settings_override: Value = required(&row, "command_model_settings_override")?;
    let session = session_id_from_uuid(required(&row, "command_session_id")?);
    let content = decode_content(required(&row, "command_content_parts")?, "command content")?;
    let delivery = decode_delivery(
        required(&row, "command_delivery_kind")?,
        row.try_get("command_descendant_scope")?,
        row.try_get("command_expected_active_turn_id")?,
        row.try_get("command_expected_defaults_version")?,
        row.try_get("command_model_override_kind")?,
        row.try_get("command_replacement_model_kind")?,
        row.try_get("command_replacement_direct_id")?,
        row.try_get("command_replacement_alias_id")?,
        command_model_settings_override,
        "command delivery",
    )?;
    let command = match (actor, delivery) {
        (
            Actor::Core,
            DeliveryRequest::Interrupt {
                expected_active_turn,
                descendant_scope,
                configuration,
            },
        ) => SubmitInput::new_core_interrupt(
            command_id,
            session,
            content,
            expected_active_turn,
            descendant_scope,
            configuration,
        ),
        (_, delivery) => SubmitInput::new(command_id, session, content, delivery),
    };

    let result_kind: String = required(&row, "result_kind")?;
    let rejection_kind: Option<String> = row.try_get("rejection_kind")?;
    let result_session = session_id_from_uuid(required(&row, "result_session_id")?);
    let result_accepted: Option<Uuid> = row.try_get("result_accepted_input_id")?;
    let result_turn: Option<Uuid> = row.try_get("result_turn_id")?;
    let result_actual_turn: Option<Uuid> = row.try_get("result_actual_active_turn_id")?;
    let result_expected_turn: Option<Uuid> = row.try_get("result_expected_active_turn_id")?;
    let result_expected_defaults: Option<Decimal> =
        row.try_get("result_expected_defaults_version")?;
    let result_current_defaults: Option<Decimal> =
        row.try_get("result_current_defaults_version")?;
    let result_unknown_alias: Option<Uuid> = row.try_get("result_unknown_alias_id")?;
    let result_selected_defaults: Option<Decimal> =
        row.try_get("result_selected_defaults_version")?;
    let result_last_position: Option<Decimal> = row.try_get("result_last_position")?;
    let result_existing_interrupt: Option<Uuid> =
        row.try_get("result_existing_interrupt_command_id")?;
    let result_attachment_digest: Option<Vec<u8>> = row.try_get("result_attachment_digest")?;
    let result_attachment_maximum_bytes: Option<Decimal> =
        row.try_get("result_attachment_maximum_bytes")?;
    let accepted_effect_count: i64 = required(&row, "accepted_effect_count")?;
    let queued_effect_count: i64 = required(&row, "queued_effect_count")?;

    let input = match (result_kind.as_str(), rejection_kind.as_deref()) {
        (APPLIED, None) => {
            if result_expected_turn.is_some()
                || result_expected_defaults.is_some()
                || result_current_defaults.is_some()
                || result_unknown_alias.is_some()
                || result_selected_defaults.is_some()
                || result_last_position.is_some()
                || result_existing_interrupt.is_some()
                || result_attachment_digest.is_some()
                || result_attachment_maximum_bytes.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent("applied result fields").into());
            }
            if accepted_effect_count != 1 {
                return Err(
                    SubmitInputCorruption::Inconsistent("applied effect cardinality").into(),
                );
            }
            let result_accepted = accepted_input_id_from_uuid(
                result_accepted
                    .ok_or(SubmitInputCorruption::Missing("result_accepted_input_id"))?,
            );
            match (result_turn, result_actual_turn) {
                (Some(result_turn), None) if queued_effect_count == 1 => {
                    decode_applied_turn_origin(
                        &row,
                        command,
                        actor,
                        result_session,
                        result_accepted,
                        turn_id_from_uuid(result_turn),
                        RelatedTurnEvidence {
                            origin: related_turn_origin,
                            non_accepted_predecessor,
                        },
                    )?
                }
                (None, Some(source_turn)) if queued_effect_count <= 1 => {
                    let source_turn_origin = related_turn_origin.ok_or(
                        SubmitInputCorruption::Missing("pending steering source turn origin"),
                    )?;
                    if non_accepted_predecessor.is_some() {
                        return Err(SubmitInputCorruption::Inconsistent(
                            "pending steering non-accepted predecessor",
                        )
                        .into());
                    }
                    decode_applied_pending_steering(
                        &row,
                        command,
                        actor,
                        result_session,
                        result_accepted,
                        turn_id_from_uuid(source_turn),
                        source_turn_origin,
                    )?
                }
                _ => {
                    return Err(
                        SubmitInputCorruption::Inconsistent("applied variant correlation").into(),
                    );
                }
            }
        }
        (REJECTED, Some(kind)) => {
            if non_accepted_predecessor.is_some() {
                return Err(SubmitInputCorruption::Inconsistent(
                    "rejected command non-accepted predecessor",
                )
                .into());
            }
            if accepted_effect_count != 0 || queued_effect_count != 0 {
                return Err(
                    SubmitInputCorruption::Inconsistent("rejected command has effects").into(),
                );
            }
            if result_accepted.is_some() || result_turn.is_some() {
                return Err(
                    SubmitInputCorruption::Inconsistent("rejected applied identities").into(),
                );
            }
            decode_rejected(
                &row,
                command,
                actor,
                result_session,
                related_turn_origin,
                kind,
                result_actual_turn,
                result_expected_turn,
                result_expected_defaults,
                result_current_defaults,
                result_unknown_alias,
                result_selected_defaults,
                result_last_position,
                result_existing_interrupt,
                result_attachment_digest,
                result_attachment_maximum_bytes,
                existing_interrupt,
            )?
        }
        (APPLIED, Some(_)) | (REJECTED, None) => {
            return Err(SubmitInputCorruption::Inconsistent("terminal result shape").into());
        }
        (value, _) => {
            return Err(SubmitInputCorruption::Unsupported {
                field: "result_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };

    input
        .reconstitute()
        .map_err(|error| SubmitInputCorruption::Domain(error.failure()).into())
}

fn decode_applied_turn_origin(
    row: &PgRow,
    command: SubmitInput,
    stored_actor: Actor,
    result_session: SessionId,
    result_accepted_input: AcceptedInputId,
    result_turn: TurnId,
    predecessor: RelatedTurnEvidence,
) -> Result<SubmitInputReconstitutionInput, SubmitInputRepositoryError> {
    let accepting_command_uuid: Uuid = required(row, "accepting_command_id")?;
    let accepting_command = durable_command_id_from_uuid(accepting_command_uuid)
        .map_err(|_| SubmitInputCorruption::Inconsistent("accepting command identity"))?;
    let accepted_input = accepted_input_id_from_uuid(required(row, "accepted_input_id")?);
    let accepted_session = session_id_from_uuid(required(row, "accepted_session_id")?);
    let accepted_content =
        decode_content(required(row, "accepted_content_parts")?, "accepted content")?;
    let accepted_delivery = decode_delivery(
        required(row, "accepted_delivery_kind")?,
        row.try_get("accepted_descendant_scope")?,
        row.try_get("accepted_expected_active_turn_id")?,
        row.try_get("accepted_expected_defaults_version")?,
        row.try_get("accepted_model_override_kind")?,
        row.try_get("accepted_replacement_model_kind")?,
        row.try_get("accepted_replacement_direct_id")?,
        row.try_get("accepted_replacement_alias_id")?,
        required(row, "accepted_model_settings_override")?,
        "accepted delivery",
    )?;
    let accepted_position = decode_position(row, "accepted_position")?;
    require_spelling(row, "disposition_kind", "origin_of")?;
    let accepted_origin_turn = turn_id_from_uuid(required(row, "origin_turn_id")?);

    let queued_turn = turn_id_from_uuid(required(row, "queued_turn_id")?);
    let queued_accepted = accepted_input_id_from_uuid(required(row, "queued_accepted_input_id")?);
    if queued_accepted != accepted_input {
        return Err(SubmitInputCorruption::Inconsistent("queued accepted input").into());
    }
    let queued_session = session_id_from_uuid(required(row, "queued_session_id")?);
    let queued_position = decode_position(row, "queued_position")?;
    let queue_order = match required::<String>(row, "priority_kind")?.as_str() {
        "ordinary" => {
            if row
                .try_get::<Option<Uuid>, _>("interrupt_predecessor_turn_id")?
                .is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent("ordinary queue priority").into());
            }
            AcceptedInputQueueOrder::ordinary(queued_position)
        }
        "interrupt_immediately_after" => AcceptedInputQueueOrder::interrupt_immediately_after(
            queued_position,
            turn_id_from_uuid(required(row, "interrupt_predecessor_turn_id")?),
        ),
        value => {
            return Err(SubmitInputCorruption::Unsupported {
                field: "priority_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    require_spelling(row, "model_parameters", "provider_defaults")?;
    require_spelling(row, "known_provider_failure_retry", "disabled")?;
    require_spelling(row, "model_fallback", "disabled")?;
    let defaults_version = decode_defaults_version(row, "queued_defaults_version")?;
    let model_settings_evidence_required: bool =
        required(row, "origin_model_settings_evidence_required")?;

    let defaults_session = session_id_from_uuid(required(row, "defaults_session_id")?);
    let joined_defaults_version = decode_defaults_version(row, "defaults_version")?;
    if joined_defaults_version != defaults_version {
        return Err(SubmitInputCorruption::Inconsistent("selected defaults version").into());
    }
    let defaults = decode_defaults(
        required(row, "defaults_model_kind")?,
        row.try_get("defaults_direct_id")?,
        row.try_get("defaults_alias_id")?,
        required(row, "defaults_tool_auto_approval")?,
        required(row, "defaults_model_settings")?,
        "selected defaults",
    )?;
    let stored_requested_model = decode_model_selection(
        required(row, "requested_model_kind")?,
        row.try_get("requested_direct_model_selection_id")?,
        row.try_get("requested_model_alias_id")?,
        "requested model",
    )?;
    let stored_frozen_model = decode_frozen_model(
        required(row, "frozen_model_kind")?,
        row.try_get("frozen_direct_model_selection_id")?,
        row.try_get("frozen_model_alias_id")?,
        row.try_get("frozen_alias_selected_direct_id")?,
    )?;
    let stored_settings_selection: Option<Uuid> = row.try_get("settings_selected_direct_id")?;
    let stored_settings_session: Option<Uuid> = row.try_get("settings_session_id")?;
    let stored_settings_defaults: Option<Decimal> = row.try_get("settings_defaults_version")?;
    let stored_per_call_settings: Option<Value> = row.try_get("per_call_model_settings")?;
    let stored_resolved_settings: Option<Value> = row.try_get("resolved_model_settings")?;
    let stored_adjusted_from_selection: Option<Uuid> = row.try_get("adjusted_from_selection_id")?;
    let stored_adjustments: Option<Value> = row.try_get("model_settings_adjustments")?;
    let (stored_model_settings, stored_model_settings_adjustments) = match (
        stored_settings_selection,
        stored_settings_session,
        stored_settings_defaults,
        stored_per_call_settings,
        stored_resolved_settings,
        stored_adjustments,
    ) {
        (None, None, None, None, None, None) => (None, Vec::new()),
        (
            Some(selection),
            Some(settings_session),
            Some(settings_defaults),
            Some(per_call),
            Some(settings),
            Some(adjustments),
        ) => {
            let per_call = model_settings_overlay_from_json(per_call)
                .map_err(|_| SubmitInputCorruption::Inconsistent("per-call model settings"))?;
            let settings = model_settings_from_json(settings)
                .map_err(|_| SubmitInputCorruption::Inconsistent("resolved model settings"))?;
            let adjustments = model_change_adjustments_from_json(adjustments)
                .map_err(|_| SubmitInputCorruption::Inconsistent("model settings adjustments"))?;
            let adjusted_from_selection =
                stored_adjusted_from_selection.map(DirectModelSelection::from_uuid);
            let expected_adjusted_from_selection = (!adjustments.is_empty())
                .then_some(defaults.model_settings().validated_for())
                .flatten();
            if DirectModelSelection::from_uuid(selection) != stored_frozen_model.selected_direct()
                || session_id_from_uuid(settings_session) != result_session
                || defaults_version_from_numeric(settings_defaults).map_err(|reason| {
                    SubmitInputCorruption::InvalidOrdinal {
                        field: "settings defaults version",
                        reason,
                    }
                })? != defaults_version
                || per_call
                    != configured_model_settings(accepted_delivery).ok_or(
                        SubmitInputCorruption::Inconsistent("origin delivery settings"),
                    )?
                || adjusted_from_selection != expected_adjusted_from_selection
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "resolved model settings correlation",
                )
                .into());
            }
            (Some(settings), adjustments)
        }
        _ => {
            return Err(
                SubmitInputCorruption::Inconsistent("resolved model settings event shape").into(),
            );
        }
    };
    if model_settings_evidence_required && stored_model_settings.is_none() {
        return Err(SubmitInputCorruption::Missing("turn model settings evidence").into());
    }
    Ok(SubmitInputReconstitutionInput::applied_turn_origin(
        SubmitInputAppliedTurnOriginReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_accepted_input,
            result_turn,
            predecessor_origin: predecessor.origin,
            non_accepted_predecessor: predecessor.non_accepted_predecessor,
            accepted_command: accepting_command,
            accepted_input,
            accepted_session,
            accepted_content,
            accepted_delivery,
            accepted_position,
            accepted_disposition: AcceptedInputDisposition::OriginOf(accepted_origin_turn),
            queue_session: queued_session,
            queue_turn: queued_turn,
            queue_order,
            defaults_session,
            defaults_version,
            defaults,
            stored_requested_model,
            stored_frozen_model,
            stored_model_settings,
            stored_model_settings_adjustments,
        },
    ))
}

fn decode_applied_pending_steering(
    row: &PgRow,
    command: SubmitInput,
    stored_actor: Actor,
    result_session: SessionId,
    result_accepted_input: AcceptedInputId,
    result_source_turn: TurnId,
    source_turn_origin: SubmitInputTurnOriginReconstitutionInput,
) -> Result<SubmitInputReconstitutionInput, SubmitInputRepositoryError> {
    let accepting_command_uuid: Uuid = required(row, "accepting_command_id")?;
    let accepting_command = durable_command_id_from_uuid(accepting_command_uuid)
        .map_err(|_| SubmitInputCorruption::Inconsistent("accepting command identity"))?;
    let accepted_input = accepted_input_id_from_uuid(required(row, "accepted_input_id")?);
    let accepted_session = session_id_from_uuid(required(row, "accepted_session_id")?);
    let accepted_content =
        decode_content(required(row, "accepted_content_parts")?, "accepted content")?;
    let accepted_delivery = decode_delivery(
        required(row, "accepted_delivery_kind")?,
        row.try_get("accepted_descendant_scope")?,
        row.try_get("accepted_expected_active_turn_id")?,
        row.try_get("accepted_expected_defaults_version")?,
        row.try_get("accepted_model_override_kind")?,
        row.try_get("accepted_replacement_model_kind")?,
        row.try_get("accepted_replacement_direct_id")?,
        row.try_get("accepted_replacement_alias_id")?,
        required(row, "accepted_model_settings_override")?,
        "accepted delivery",
    )?;
    let accepted_position = decode_position(row, "accepted_position")?;

    Ok(SubmitInputReconstitutionInput::applied_pending_steering(
        SubmitInputAppliedPendingSteeringReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_accepted_input,
            result_source_turn,
            source_turn_origin,
            accepted_command: accepting_command,
            accepted_input,
            accepted_session,
            accepted_content,
            accepted_delivery,
            accepted_position,
        },
    ))
}

#[allow(clippy::too_many_arguments)]
fn decode_rejected(
    row: &PgRow,
    command: SubmitInput,
    stored_actor: Actor,
    result_session: SessionId,
    active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    rejection_kind: &str,
    actual_turn: Option<Uuid>,
    expected_turn: Option<Uuid>,
    expected_defaults: Option<Decimal>,
    current_defaults: Option<Decimal>,
    unknown_alias: Option<Uuid>,
    selected_defaults: Option<Decimal>,
    last_position: Option<Decimal>,
    existing_interrupt_command: Option<Uuid>,
    attachment_digest: Option<Vec<u8>>,
    attachment_maximum_bytes: Option<Decimal>,
    existing_interrupt: Option<AppliedInterruptCommandResult>,
) -> Result<SubmitInputReconstitutionInput, SubmitInputRepositoryError> {
    if !matches!(
        rejection_kind,
        "safe_point_unavailable_while_stopping" | "interrupt_already_applied"
    ) && (existing_interrupt_command.is_some() || existing_interrupt.is_some())
    {
        return Err(
            SubmitInputCorruption::Inconsistent("unexpected existing interrupt result").into(),
        );
    }
    if !matches!(
        rejection_kind,
        "attachment_blob_not_found" | "attachment_byte_budget_exceeded"
    ) && (attachment_digest.is_some() || attachment_maximum_bytes.is_some())
    {
        return Err(
            SubmitInputCorruption::Inconsistent("unexpected attachment result evidence").into(),
        );
    }
    match rejection_kind {
        "attachment_blob_not_found" => {
            require_all_absent(
                actual_turn,
                expected_turn,
                expected_defaults,
                current_defaults,
                unknown_alias,
                selected_defaults,
                last_position,
                "attachment-blob-not-found result fields",
            )?;
            if attachment_maximum_bytes.is_some() {
                return Err(SubmitInputCorruption::Inconsistent(
                    "attachment-blob-not-found maximum bytes",
                )
                .into());
            }
            let digest = attachment_digest
                .ok_or(SubmitInputCorruption::Missing("result_attachment_digest"))?;
            let digest = <[u8; 32]>::try_from(digest)
                .map_err(|_| SubmitInputCorruption::Inconsistent("result attachment digest"))?;
            Ok(
                SubmitInputReconstitutionInput::rejected_attachment_blob_not_found(
                    SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_digest: signalbox_domain::BlobDigest::from_bytes(digest),
                    },
                ),
            )
        }
        "attachment_byte_budget_exceeded" => {
            require_all_absent(
                actual_turn,
                expected_turn,
                expected_defaults,
                current_defaults,
                unknown_alias,
                selected_defaults,
                last_position,
                "attachment-byte-budget-exceeded result fields",
            )?;
            if attachment_digest.is_some() {
                return Err(SubmitInputCorruption::Inconsistent(
                    "attachment-byte-budget-exceeded digest",
                )
                .into());
            }
            let maximum_bytes = positive_u64_from_numeric(attachment_maximum_bytes.ok_or(
                SubmitInputCorruption::Missing("result_attachment_maximum_bytes"),
            )?)
            .map_err(|_| SubmitInputCorruption::Inconsistent("result attachment maximum bytes"))?;
            Ok(
                SubmitInputReconstitutionInput::rejected_attachment_byte_budget_exceeded(
                    SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_maximum_bytes: maximum_bytes,
                    },
                ),
            )
        }
        "session_not_found" => {
            require_all_absent(
                actual_turn,
                expected_turn,
                expected_defaults,
                current_defaults,
                unknown_alias,
                selected_defaults,
                last_position,
                "session-not-found result fields",
            )?;
            Ok(SubmitInputReconstitutionInput::rejected_session_not_found(
                SubmitInputRejectedSessionNotFoundReconstitutionInput {
                    command,
                    stored_actor,
                    result_session,
                },
            ))
        }
        "no_active_turn" => {
            if actual_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("no-active-turn result fields").into(),
                );
            }
            Ok(SubmitInputReconstitutionInput::rejected_no_active_turn(
                SubmitInputRejectedNoActiveTurnReconstitutionInput {
                    command,
                    stored_actor,
                    result_session,
                    result_expected_active_turn: turn_id_from_uuid(expected_turn.ok_or(
                        SubmitInputCorruption::Missing("result_expected_active_turn_id"),
                    )?),
                },
            ))
        }
        "active_turn_present" => {
            if expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "active-turn-present result fields",
                )
                .into());
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_active_turn_present(
                    SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_active_turn: turn_id_from_uuid(actual_turn.ok_or(
                            SubmitInputCorruption::Missing("result_actual_active_turn_id"),
                        )?),
                        active_turn_origin: active_turn_origin
                            .ok_or(SubmitInputCorruption::Missing("active turn origin"))?,
                    },
                ),
            )
        }
        "active_turn_mismatch" => {
            if expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "active-turn-mismatch result fields",
                )
                .into());
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                    SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_expected_active_turn: turn_id_from_uuid(expected_turn.ok_or(
                            SubmitInputCorruption::Missing("result_expected_active_turn_id"),
                        )?),
                        result_actual_active_turn: turn_id_from_uuid(actual_turn.ok_or(
                            SubmitInputCorruption::Missing("result_actual_active_turn_id"),
                        )?),
                        actual_turn_origin: active_turn_origin
                            .ok_or(SubmitInputCorruption::Missing("actual turn origin"))?,
                    },
                ),
            )
        }
        "session_defaults_version_mismatch" => {
            if actual_turn.is_some()
                || expected_turn.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("defaults-mismatch result fields").into(),
                );
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                    SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_expected: decode_optional_defaults_version(
                            expected_defaults,
                            "result_expected_defaults_version",
                        )?
                        .ok_or(SubmitInputCorruption::Missing(
                            "result_expected_defaults_version",
                        ))?,
                        result_current: decode_optional_defaults_version(
                            current_defaults,
                            "result_current_defaults_version",
                        )?
                        .ok_or(SubmitInputCorruption::Missing(
                            "result_current_defaults_version",
                        ))?,
                        active_turn_origin,
                    },
                ),
            )
        }
        "unknown_model_alias" => {
            if actual_turn.is_some()
                || expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || last_position.is_some()
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("unknown-alias result fields").into(),
                );
            }
            let selected = decode_optional_defaults_version(
                selected_defaults,
                "result_selected_defaults_version",
            )?
            .ok_or(SubmitInputCorruption::Missing(
                "result_selected_defaults_version",
            ))?;
            let defaults_session = session_id_from_uuid(required(row, "defaults_session_id")?);
            let defaults_version = decode_defaults_version(row, "defaults_version")?;
            let defaults = decode_defaults(
                required(row, "defaults_model_kind")?,
                row.try_get("defaults_direct_id")?,
                row.try_get("defaults_alias_id")?,
                required(row, "defaults_tool_auto_approval")?,
                required(row, "defaults_model_settings")?,
                "selected defaults",
            )?;
            if selected != defaults_version {
                return Err(
                    SubmitInputCorruption::Inconsistent("unknown-alias defaults version").into(),
                );
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                    SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_alias: ModelAlias::from_uuid(
                            unknown_alias
                                .ok_or(SubmitInputCorruption::Missing("result_unknown_alias_id"))?,
                        ),
                        defaults_session,
                        defaults_version,
                        defaults,
                        active_turn_origin,
                    },
                ),
            )
        }
        "acceptance_position_exhausted" => {
            if actual_turn.is_some()
                || expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "position-exhausted result fields",
                )
                .into());
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                    SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_last_position: decode_optional_position(
                            last_position,
                            "result_last_position",
                        )?
                        .ok_or(SubmitInputCorruption::Missing("result_last_position"))?,
                        active_turn_origin,
                    },
                ),
            )
        }
        "safe_point_unavailable_while_stopping" => {
            if expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "stopping safe-point result fields",
                )
                .into());
            }
            let active_turn = turn_id_from_uuid(actual_turn.ok_or(
                SubmitInputCorruption::Missing("result_actual_active_turn_id"),
            )?);
            let stored_command = durable_command_id_from_uuid(existing_interrupt_command.ok_or(
                SubmitInputCorruption::Missing("result_existing_interrupt_command_id"),
            )?)
            .map_err(|_| {
                SubmitInputCorruption::Inconsistent("existing interrupt command identity")
            })?;
            let interrupt = existing_interrupt.ok_or(SubmitInputCorruption::Missing(
                "existing interrupt authority",
            ))?;
            if stored_command != interrupt.proof().command() {
                return Err(
                    SubmitInputCorruption::Inconsistent("existing interrupt command").into(),
                );
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_safe_point_unavailable_while_stopping(
                    SubmitInputRejectedSafePointUnavailableWhileStoppingReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_active_turn: active_turn,
                        active_turn_origin: active_turn_origin
                            .ok_or(SubmitInputCorruption::Missing("active turn origin"))?,
                        existing_interrupt: interrupt,
                    },
                ),
            )
        }
        "interrupt_already_applied" => {
            if expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "already-applied interrupt result fields",
                )
                .into());
            }
            let active_turn = turn_id_from_uuid(actual_turn.ok_or(
                SubmitInputCorruption::Missing("result_actual_active_turn_id"),
            )?);
            let stored_command = durable_command_id_from_uuid(existing_interrupt_command.ok_or(
                SubmitInputCorruption::Missing("result_existing_interrupt_command_id"),
            )?)
            .map_err(|_| {
                SubmitInputCorruption::Inconsistent("existing interrupt command identity")
            })?;
            let interrupt = existing_interrupt.ok_or(SubmitInputCorruption::Missing(
                "existing interrupt authority",
            ))?;
            Ok(
                SubmitInputReconstitutionInput::rejected_interrupt_already_applied(
                    SubmitInputRejectedInterruptAlreadyAppliedReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_active_turn: active_turn,
                        result_existing_command: stored_command,
                        active_turn_origin: active_turn_origin
                            .ok_or(SubmitInputCorruption::Missing("active turn origin"))?,
                        existing_interrupt: interrupt,
                    },
                ),
            )
        }
        "interrupt_unavailable_while_awaiting_approval" => {
            if expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "parked-approval interrupt result fields",
                )
                .into());
            }
            let active_turn = turn_id_from_uuid(actual_turn.ok_or(
                SubmitInputCorruption::Missing("result_actual_active_turn_id"),
            )?);
            let input =
                SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput {
                    command,
                    stored_actor,
                    result_session,
                    result_active_turn: active_turn,
                    active_turn_origin: active_turn_origin
                        .ok_or(SubmitInputCorruption::Missing("active turn origin"))?,
                };
            Ok(
                SubmitInputReconstitutionInput::rejected_interrupt_unavailable_while_awaiting_approval(
                    input,
                ),
            )
        }
        value => Err(SubmitInputCorruption::Unsupported {
            field: "rejection_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, SubmitInputRepositoryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| SubmitInputCorruption::Missing(field).into())
}

fn require_spelling(
    row: &PgRow,
    field: &'static str,
    expected: &str,
) -> Result<(), SubmitInputRepositoryError> {
    let actual: String = required(row, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(SubmitInputCorruption::Unsupported {
            field,
            value: actual,
        }
        .into())
    }
}

fn require_supported_version(
    row: &PgRow,
    field: &'static str,
) -> Result<i16, SubmitInputRepositoryError> {
    let actual: i16 = required(row, field)?;
    if actual == STORAGE_VERSION {
        Ok(actual)
    } else {
        Err(SubmitInputCorruption::Unsupported {
            field,
            value: actual.to_string(),
        }
        .into())
    }
}

#[allow(clippy::too_many_arguments)]
fn require_all_absent(
    actual_turn: Option<Uuid>,
    expected_turn: Option<Uuid>,
    expected_defaults: Option<Decimal>,
    current_defaults: Option<Decimal>,
    unknown_alias: Option<Uuid>,
    selected_defaults: Option<Decimal>,
    last_position: Option<Decimal>,
    relationship: &'static str,
) -> Result<(), SubmitInputRepositoryError> {
    if actual_turn.is_none()
        && expected_turn.is_none()
        && expected_defaults.is_none()
        && current_defaults.is_none()
        && unknown_alias.is_none()
        && selected_defaults.is_none()
        && last_position.is_none()
    {
        Ok(())
    } else {
        Err(SubmitInputCorruption::Inconsistent(relationship).into())
    }
}

fn decode_actor(
    kind: String,
    turn: Option<Uuid>,
    tool_request: Option<Uuid>,
) -> Result<Actor, SubmitInputRepositoryError> {
    match (kind.as_str(), turn, tool_request) {
        ("user", None, None) => Ok(Actor::User),
        ("core", None, None) => Ok(Actor::Core),
        ("model", Some(turn), None) => Ok(Actor::Model {
            turn: TurnId::from_uuid(turn),
        }),
        ("recovery", None, None) => Ok(Actor::Recovery),
        ("tool", None, Some(request)) => Ok(Actor::Tool {
            request: ToolRequestId::from_uuid(request),
        }),
        ("user" | "model" | "recovery" | "tool", _, _) => {
            Err(SubmitInputCorruption::Inconsistent("actor fields").into())
        }
        _ => Err(SubmitInputCorruption::Unsupported {
            field: "actor_kind",
            value: kind,
        }
        .into()),
    }
}

fn decode_content(
    stored: Value,
    field: &'static str,
) -> Result<UserContent, SubmitInputRepositoryError> {
    crate::user_content::decode(stored).map_err(|error| match error {
        crate::user_content::StoredUserContentError::UnsupportedPartKind(value)
        | crate::user_content::StoredUserContentError::UnsupportedAttachmentKind(value) => {
            SubmitInputCorruption::Unsupported { field, value }.into()
        }
        crate::user_content::StoredUserContentError::Malformed => {
            SubmitInputCorruption::Inconsistent(field).into()
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_delivery(
    kind: String,
    descendant_scope: Option<String>,
    expected_active_turn: Option<Uuid>,
    expected_defaults_version: Option<Decimal>,
    model_override_kind: Option<String>,
    replacement_model_kind: Option<String>,
    replacement_direct: Option<Uuid>,
    replacement_alias: Option<Uuid>,
    model_settings_override: Value,
    field: &'static str,
) -> Result<DeliveryRequest, SubmitInputRepositoryError> {
    if kind != "interrupt" && descendant_scope.is_some() {
        return Err(SubmitInputCorruption::Inconsistent(field).into());
    }
    let model_settings_override = model_settings_overlay_from_json(model_settings_override)
        .map_err(|_| SubmitInputCorruption::Inconsistent("model settings override"))?;
    match kind.as_str() {
        "start_when_no_active_turn" => {
            if expected_active_turn.is_some() {
                return Err(SubmitInputCorruption::Inconsistent(field).into());
            }
            Ok(DeliveryRequest::StartWhenNoActiveTurn {
                configuration: decode_configuration(
                    expected_defaults_version,
                    model_override_kind,
                    replacement_model_kind,
                    replacement_direct,
                    replacement_alias,
                    model_settings_override,
                    field,
                )?,
            })
        }
        "interrupt" | "after_current_turn" => {
            let turn = TurnId::from_uuid(
                expected_active_turn
                    .ok_or(SubmitInputCorruption::Missing("expected_active_turn_id"))?,
            );
            let configuration = decode_configuration(
                expected_defaults_version,
                model_override_kind,
                replacement_model_kind,
                replacement_direct,
                replacement_alias,
                model_settings_override,
                field,
            )?;
            if kind == "interrupt" {
                Ok(DeliveryRequest::Interrupt {
                    expected_active_turn: turn,
                    descendant_scope: descendant_scope_from_str(
                        descendant_scope
                            .as_deref()
                            .ok_or(SubmitInputCorruption::Missing("descendant_scope"))?,
                    )?,
                    configuration,
                })
            } else {
                Ok(DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: turn,
                    configuration,
                })
            }
        }
        "next_safe_point" => {
            if expected_defaults_version.is_some()
                || model_override_kind.is_some()
                || replacement_model_kind.is_some()
                || replacement_direct.is_some()
                || replacement_alias.is_some()
                || model_settings_override != signalbox_domain::ModelSettingsOverlay::inherit_all()
            {
                return Err(SubmitInputCorruption::Inconsistent(field).into());
            }
            Ok(DeliveryRequest::NextSafePoint {
                expected_active_turn: TurnId::from_uuid(
                    expected_active_turn
                        .ok_or(SubmitInputCorruption::Missing("expected_active_turn_id"))?,
                ),
            })
        }
        _ => Err(SubmitInputCorruption::Unsupported { field, value: kind }.into()),
    }
}

const fn descendant_scope_to_str(value: DescendantTerminationScope) -> &'static str {
    match value {
        DescendantTerminationScope::ParentAlone => "parent_alone",
        DescendantTerminationScope::ParentAndDescendants => "parent_and_descendants",
    }
}

const fn parent_termination_kind_to_str(value: ParentTerminationKind) -> &'static str {
    match value {
        ParentTerminationKind::Stopped => "stopped",
        ParentTerminationKind::Cancelled => "cancelled",
    }
}

fn descendant_scope_from_str(
    value: &str,
) -> Result<DescendantTerminationScope, SubmitInputRepositoryError> {
    match value {
        "parent_alone" => Ok(DescendantTerminationScope::ParentAlone),
        "parent_and_descendants" => Ok(DescendantTerminationScope::ParentAndDescendants),
        value => Err(SubmitInputCorruption::Unsupported {
            field: "descendant_scope",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_configuration(
    expected_defaults_version: Option<Decimal>,
    model_override_kind: Option<String>,
    replacement_model_kind: Option<String>,
    replacement_direct: Option<Uuid>,
    replacement_alias: Option<Uuid>,
    model_settings_override: signalbox_domain::ModelSettingsOverlay,
    field: &'static str,
) -> Result<PerInputConfigurationChoices, SubmitInputRepositoryError> {
    let expected =
        decode_optional_defaults_version(expected_defaults_version, "expected_defaults_version")?
            .ok_or(SubmitInputCorruption::Missing("expected_defaults_version"))?;
    let model = match model_override_kind.as_deref() {
        Some("use_session_default") => {
            if replacement_model_kind.is_some()
                || replacement_direct.is_some()
                || replacement_alias.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(field).into());
            }
            ModelSelectionOverride::UseSessionDefault
        }
        Some("replace_with") => ModelSelectionOverride::ReplaceWith(decode_model_selection(
            replacement_model_kind
                .ok_or(SubmitInputCorruption::Missing("replacement_model_kind"))?,
            replacement_direct,
            replacement_alias,
            "replacement model",
        )?),
        Some(value) => {
            return Err(SubmitInputCorruption::Unsupported {
                field: "model_override_kind",
                value: value.to_owned(),
            }
            .into());
        }
        None => return Err(SubmitInputCorruption::Missing("model_override_kind").into()),
    };
    Ok(PerInputConfigurationChoices::with_model_settings(
        expected,
        model,
        model_settings_override,
    ))
}

fn decode_defaults_version(
    row: &PgRow,
    field: &'static str,
) -> Result<SessionConfigurationDefaultsVersion, SubmitInputRepositoryError> {
    let value: Decimal = required(row, field)?;
    defaults_version_from_numeric(value)
        .map_err(|reason| SubmitInputCorruption::InvalidOrdinal { field, reason }.into())
}

fn decode_optional_defaults_version(
    value: Option<Decimal>,
    field: &'static str,
) -> Result<Option<SessionConfigurationDefaultsVersion>, SubmitInputRepositoryError> {
    value
        .map(|value| {
            defaults_version_from_numeric(value)
                .map_err(|reason| SubmitInputCorruption::InvalidOrdinal { field, reason }.into())
        })
        .transpose()
}

fn decode_position(
    row: &PgRow,
    field: &'static str,
) -> Result<SessionInputPosition, SubmitInputRepositoryError> {
    let value: Decimal = required(row, field)?;
    input_position_from_numeric(value)
        .map_err(|reason| SubmitInputCorruption::InvalidOrdinal { field, reason }.into())
}

fn decode_optional_position(
    value: Option<Decimal>,
    field: &'static str,
) -> Result<Option<SessionInputPosition>, SubmitInputRepositoryError> {
    value
        .map(|value| {
            input_position_from_numeric(value)
                .map_err(|reason| SubmitInputCorruption::InvalidOrdinal { field, reason }.into())
        })
        .transpose()
}

/// Reconstitutes the model selection and dangerous-tool posture one origin
/// froze, deliberately without the epoch's system prompt.
///
/// This projection is batched over every command a session load reconstitutes,
/// so selecting the epoch's prompt here would return and retain one copy of the
/// same bounded megabyte text per row. The prompt has exactly two readers, both
/// single-epoch: the session aggregate's current-defaults load
/// (`crate::session`) and model-call preparation's frozen-epoch read
/// (`crate::model_execution`). Neither reads it from a submit-input receipt.
fn decode_defaults(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    dangerous_tool_auto_approval: String,
    model_settings: Value,
    field: &'static str,
) -> Result<SessionConfigurationDefaults, SubmitInputRepositoryError> {
    let model = decode_model_selection(kind, direct, alias, field)?;
    let dangerous_tool_auto_approval =
        dangerous_tool_auto_approval_from_str(&dangerous_tool_auto_approval).ok_or({
            SubmitInputCorruption::Unsupported {
                field: "dangerous_tool_auto_approval",
                value: dangerous_tool_auto_approval,
            }
        })?;
    let model_settings = model_settings_from_json(model_settings)
        .map_err(|_| SubmitInputCorruption::Inconsistent("model settings"))?;
    SessionConfigurationDefaults::complete_with_model_settings(
        model,
        dangerous_tool_auto_approval,
        None,
        model_settings,
    )
    .ok_or_else(|| {
        SubmitInputRepositoryError::from(SubmitInputCorruption::Inconsistent(
            "model settings validation selection",
        ))
    })
}

fn decode_dangerous_tool_auto_approval(
    row: &PgRow,
    column: &'static str,
    field: &'static str,
) -> Result<signalbox_domain::DangerousToolAutoApproval, SubmitInputRepositoryError> {
    let value: String = required(row, column)?;
    dangerous_tool_auto_approval_from_str(&value)
        .ok_or_else(|| SubmitInputCorruption::Unsupported { field, value }.into())
}

fn decode_model_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    field: &'static str,
) -> Result<ModelSelectionRequest, SubmitInputRepositoryError> {
    match (kind.as_str(), direct, alias) {
        ("direct", Some(selection), None) => Ok(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(selection),
        )),
        ("alias", None, Some(alias)) => {
            Ok(ModelSelectionRequest::Alias(ModelAlias::from_uuid(alias)))
        }
        ("direct" | "alias", _, _) => Err(SubmitInputCorruption::Inconsistent(field).into()),
        _ => Err(SubmitInputCorruption::Unsupported { field, value: kind }.into()),
    }
}

fn decode_frozen_model(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    alias_selected: Option<Uuid>,
) -> Result<FrozenModelSelection, SubmitInputRepositoryError> {
    match (kind.as_str(), direct, alias, alias_selected) {
        ("direct", Some(selection), None, None) => Ok(FrozenModelSelection::Direct(
            DirectModelSelection::from_uuid(selection),
        )),
        ("frozen_alias", None, Some(alias), Some(selected)) => {
            Ok(FrozenModelSelection::FrozenAlias {
                alias: ModelAlias::from_uuid(alias),
                definition: FrozenAliasDefinition::selecting(DirectModelSelection::from_uuid(
                    selected,
                )),
            })
        }
        ("direct" | "frozen_alias", _, _, _) => {
            Err(SubmitInputCorruption::Inconsistent("frozen model").into())
        }
        _ => Err(SubmitInputCorruption::Unsupported {
            field: "frozen_model_kind",
            value: kind,
        }
        .into()),
    }
}

fn decode_optional_token_count(
    row: &PgRow,
    field: &'static str,
) -> Result<Option<u64>, SubmitInputRepositoryError> {
    let value: Option<Decimal> = row.try_get(field)?;
    let Some(value) = value else {
        return Ok(None);
    };
    if !value.fract().is_zero() || value < Decimal::ZERO {
        return Err(
            SubmitInputCorruption::Inconsistent("compaction model call token usage").into(),
        );
    }
    u64::try_from(value).map(Some).map_err(|_| {
        SubmitInputCorruption::Inconsistent("compaction model call token usage").into()
    })
}

fn decode_model_call_disposition(
    value: &str,
) -> Result<ModelCallDisposition, SubmitInputRepositoryError> {
    match value {
        "completed" => Ok(ModelCallDisposition::Completed),
        "known_failed" => Ok(ModelCallDisposition::KnownFailed),
        "refused" => Ok(ModelCallDisposition::Refused),
        "cancelled" => Ok(ModelCallDisposition::Cancelled),
        "ambiguous" => Ok(ModelCallDisposition::Ambiguous),
        value => Err(SubmitInputCorruption::Unsupported {
            field: "model call terminal_disposition_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

async fn inspect_registry(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<CommandKind>, SubmitInputRepositoryError> {
    command_registry::inspect(connection, command_id)
        .await
        .map_err(map_registry_error)
}

fn map_registry_error(error: RegistryInspectionError) -> SubmitInputRepositoryError {
    match error {
        RegistryInspectionError::Database(error) => error.into(),
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedKind(value)) => {
            SubmitInputCorruption::Unsupported {
                field: "registry_kind",
                value,
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedVersion(value)) => {
            SubmitInputCorruption::Unsupported {
                field: "registry_version",
                value: value.to_string(),
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::MissingTypedRecord(_)) => {
            SubmitInputCorruption::Missing("typed_command_id").into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::ConflictingTypedRecords) => {
            SubmitInputCorruption::Inconsistent("typed command family").into()
        }
    }
}
