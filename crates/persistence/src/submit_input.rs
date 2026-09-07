//! Atomic PostgreSQL persistence and replay for durable input acceptance.

mod attachment;
mod decode;
mod encode;
mod load;
mod scheduling_projection;
mod write;

pub(crate) use scheduling_projection::load_scheduling_projection;

use attachment::{
    load_runner_recovery_yielded_attempt, persist_runner_recovery_interrupt_effect,
    prepare_attachment_authority_rejection, prospective_attachment_frontier_exceeds_bound,
    session_has_attachment_parts, supersede_automatic_reconciliation,
    terminalize_retryable_runner_recovery_attempt,
};
pub(crate) use decode::decode_goal_origin_configuration;
pub(crate) use decode::require_applied_interrupt_from_attempt;

use decode::{decode_complete, map_tool_loop_error};

pub(crate) use load::load_turn_origin_graph;
use load::{
    load_complete_rows, load_existing_interrupt, load_from_connection, non_accepted_predecessor,
    related_turn_origin_key,
};

use write::{insert_prepared_command, insert_prepared_effects, settle_injection_receipt};

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::NonZeroU32;

use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_application::{SubmitInputIdGenerator, SubmitInputOutcome, SubmitInputTransaction};
use signalbox_domain::{
    AcceptedInputId, AcceptedInputQueueOrder, AcceptedInputSchedulingProjection,
    AcceptedInputSchedulingReconstitutionFailure, AcceptedInputTurnSchedulingStatus, Actor,
    CancelledModelCallTurnIdentities, CommandPrincipal, ContextFrontierId, DeliveryRequest,
    DescendantTerminationScope, DirectModelSelection, DurableCommandId, FrozenAliasDefinition,
    FrozenModelSelection, GoalGeneration, GoalTurnSource, IssuedOperationRef, ModelAlias,
    ModelCallDisposition, ModelCallInterruptOutcome, ModelCallTerminalOutcome,
    ModelCapabilityCatalog, ModelSelectionOverride, ModelSelectionRequest,
    NonEmptyUnicodeTextFailure, OriginModelSettingsError, ParentTerminationKind,
    PerInputConfigurationChoices, PreparedSubmitInput, ReconstitutedSubmitInput,
    SemanticTranscriptEntryId, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionInputPosition, SubmitInput, SubmitInputAppliedResult, SubmitInputPreparationFailure,
    SubmitInputReconstitutionFailure, SubmitInputResult, ToolRequestId, TurnAttemptId, TurnId,
    UnsupportedModelSetting, UserContent,
};
use sqlx::{FromRow, PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    command_registry::{
        self, CommandKind, RegistryCorruption, RegistryInspectionError, SUBMIT_INPUT_KIND,
    },
    mapping::{
        PositiveOrdinalMappingError, dangerous_tool_auto_approval_from_str,
        defaults_version_from_numeric, durable_command_id_from_uuid, durable_command_id_to_uuid,
        input_position_from_numeric, model_settings_from_json, model_settings_overlay_from_json,
        session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid,
    },
    model_execution::{
        ModelCallRepositoryError, attach_interrupt_reclassification_candidates,
        attach_interrupt_reclassification_candidates_for_activated,
        attach_interrupt_reclassification_candidates_for_active,
        attach_recovery_interrupt_reclassification_candidates,
        attach_recovery_interrupt_reclassification_candidates_for_activated,
        load_delegated_runner_recovery_for_interrupt, lock_delegated_child_endpoint_sessions,
        persist_stop_requested, persist_terminal_outcome, persist_tool_reconciliation_required,
        require_live_execution_for_restart,
    },
    session::{SessionCorruption, SessionRepositoryError, load_session_from_connection},
    tool_loop::{
        deny_awaiting_approvals_for_interrupt, load_active_batch_from_connection,
        load_optional_foreground_delegation_outcome, load_recovery_batch_by_attempt,
        load_runner_recovery_batch_without_attempt, load_runner_recovery_cancellation_batch,
        load_runner_recovery_source_snapshot, persist_ended_attempt,
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
mod tests;

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

#[derive(signalbox_derive::OperatorError)]
/// A durable shape that cannot reconstruct one complete input handling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubmitInputCorruption {
    #[error("missing durable SubmitInput {field_0}")]
    /// One required row or field is absent.
    Missing(&'static str),
    #[error("unsupported SubmitInput {field}: {value}")]
    /// A closed discriminator or representation version is unsupported.
    Unsupported {
        /// The record field that could not be decoded.
        field: &'static str,
        /// The durable spelling that was observed.
        value: String,
    },
    #[error("inconsistent SubmitInput {field_0}")]
    /// Typed records or variant-specific fields disagree.
    Inconsistent(&'static str),
    #[error("invalid SubmitInput {field}: {reason}")]
    /// A stored positive ordinal cannot construct the domain value.
    InvalidOrdinal {
        /// The ordinal-bearing field.
        field: &'static str,
        /// Why its numeric representation is invalid.
        reason: PositiveOrdinalMappingError,
    },
    #[error("invalid SubmitInput {field}: {failure:?}")]
    /// Exact stored text cannot construct baseline user content.
    InvalidContent {
        /// The content-bearing field.
        field: &'static str,
        /// Why the exact stored text is outside the baseline.
        failure: NonEmptyUnicodeTextFailure,
    },
    #[error("SubmitInput current Session is invalid: {field_0}")]
    /// The current session projection required for first handling is invalid.
    CurrentSession(SessionCorruption),
    #[error("SubmitInput domain reconstitution failed: {field_0:?}")]
    /// Checked stored values fail domain-owned receipt correlation.
    Domain(SubmitInputReconstitutionFailure),
    #[error("SubmitInput scheduling reconstitution failed: {field_0:?}")]
    /// Complete scheduling facts fail domain-owned aggregate reconstruction.
    Scheduling(AcceptedInputSchedulingReconstitutionFailure),
}

#[derive(signalbox_derive::OperatorError)]
/// A database failure, wrong purpose-specific load, or integrity failure.
#[derive(Debug)]
pub enum SubmitInputRepositoryError {
    #[error("SubmitInput database failure: {field_0}")]
    /// PostgreSQL failed before any commit could have succeeded.
    Database(#[source] sqlx::Error),
    #[error("SubmitInput commit outcome is ambiguous: {field_0}")]
    /// PostgreSQL obscured whether the requested commit succeeded.
    CommitAmbiguous(#[source] sqlx::Error),
    #[error("durable command {command_id:?} does not name SubmitInput")]
    /// A purpose-specific load named a valid command of another admitted kind.
    DifferentCommandKind {
        /// The user-global identifier that names another kind.
        command_id: DurableCommandId,
    },
    #[error(
        "SubmitInput command {command_id:?} proposed accepted input {accepted_input:?}, which is already the origin of active turn {active_turn:?}"
    )]
    /// A generated accepted-input candidate reused the active turn's origin.
    AcceptedInputIdentityCollision {
        /// The unclaimed durable command.
        command_id: DurableCommandId,
        /// The authoritative active turn.
        active_turn: TurnId,
        /// The colliding accepted-input candidate and active origin.
        accepted_input: AcceptedInputId,
    },
    #[error(transparent)]
    /// A caller-owned explicit setting is unsupported by the selected model.
    UnsupportedModelSetting(#[source] UnsupportedModelSetting),
    #[error(transparent)]
    /// Durable records cannot reconstruct the requested domain value.
    Corruption(#[source] SubmitInputCorruption),
    #[error("SubmitInput model execution failed: {field_0}")]
    /// The active turn's model-execution aggregate could not apply or persist
    /// the correlated stop transition.
    ModelExecution(#[source] Box<ModelCallRepositoryError>),
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
                | CommandKind::ProvisionOauthCredential
                | CommandKind::ReprovisionOauthCredential
                | CommandKind::DeleteOauthCredential
                | CommandKind::ClearCredentialExclusion
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
            | CommandKind::ProvisionOauthCredential
            | CommandKind::ReprovisionOauthCredential
            | CommandKind::DeleteOauthCredential
            | CommandKind::ClearCredentialExclusion
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
                | CommandKind::ProvisionOauthCredential
                | CommandKind::ReprovisionOauthCredential
                | CommandKind::DeleteOauthCredential
                | CommandKind::ClearCredentialExclusion
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
