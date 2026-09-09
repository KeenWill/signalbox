//! Atomic PostgreSQL persistence and replay for durable input acceptance.

mod attachment;
mod decode;
mod encode;
mod handle;
mod load;
mod prepare;
mod scheduling_projection;
mod write;

pub(crate) use scheduling_projection::load_scheduling_projection;

pub(crate) use handle::handle_in_transaction;

pub use prepare::FreshInitialInput;
pub(crate) use prepare::insert_fresh_initial_input;
pub(crate) use prepare::require_recorded_batch;

pub(crate) use decode::decode_goal_origin_configuration;
pub(crate) use decode::require_applied_interrupt_from_attempt;

use load::load_from_connection;

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU32;

use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_application::{SubmitInputOutcome, SubmitInputTransaction};
use signalbox_domain::{
    AcceptedInputId, AcceptedInputQueueOrder, AcceptedInputSchedulingProjection,
    AcceptedInputSchedulingReconstitutionFailure, Actor, CancelledModelCallTurnIdentities,
    CommandPrincipal, DeliveryRequest, DescendantTerminationScope, DirectModelSelection,
    DurableCommandId, FrozenAliasDefinition, FrozenModelSelection, GoalGeneration, GoalTurnSource,
    IssuedOperationRef, ModelAlias, ModelCallDisposition, ModelCapabilityCatalog,
    ModelSelectionOverride, ModelSelectionRequest, NonEmptyUnicodeTextFailure,
    ParentTerminationKind, PerInputConfigurationChoices, PreparedSubmitInput,
    ReconstitutedSubmitInput, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionInputPosition, SubmitInput, SubmitInputReconstitutionFailure, SubmitInputResult,
    ToolRequestId, TurnAttemptId, TurnId, UnsupportedModelSetting, UserContent,
};
use sqlx::{FromRow, PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    command_registry::{self, CommandKind, RegistryCorruption, RegistryInspectionError},
    mapping::{
        PositiveOrdinalMappingError, dangerous_tool_auto_approval_from_str,
        defaults_version_from_numeric, input_position_from_numeric, model_settings_from_json,
        model_settings_overlay_from_json,
    },
    model_execution::ModelCallRepositoryError,
    session::SessionCorruption,
};

const STORAGE_VERSION: i16 = 4;
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
/// A transient admission refusal, database failure, wrong purpose-specific load, or integrity failure.
#[derive(Debug)]
pub enum SubmitInputRepositoryError {
    #[error("SubmitInput awaits repository-watch kickoff during checkout provisioning")]
    /// Client input is deferred without claiming its command identity while
    /// the repository-watch kickoff owns the first input under the provisioning hold.
    CheckoutProvisioningPending,
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

pub(crate) enum TransactionDecision {
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
    stop_receipt: bool,
}

impl SubmitInputRepository {
    /// Uses the supplied pool for atomic handling and fail-closed loads.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            model_capabilities: None,
            attachment_maximum_bytes: None,
            stop_receipt: false,
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
            stop_receipt: false,
        }
    }

    /// Installs the deployment ceiling used for claim-first attachment
    /// catalog admission.
    #[must_use]
    pub const fn with_attachment_maximum_bytes(mut self, maximum_bytes: u64) -> Self {
        self.attachment_maximum_bytes = Some(maximum_bytes);
        self
    }

    /// Records stop receipt metadata with a newly committed input command.
    #[must_use]
    pub const fn with_stop_receipt(mut self) -> Self {
        self.stop_receipt = true;
        self
    }

    /// Reads the receipt kind retained by the command's first handling.
    pub async fn is_recorded_stop(
        &self,
        command_id: DurableCommandId,
    ) -> Result<bool, SubmitInputRepositoryError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM submit_input_stop_receipt WHERE command_id = $1)",
        )
        .bind(command_id.into_uuid())
        .fetch_one(&self.pool)
        .await?)
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
        principal: impl Into<Option<CommandPrincipal>>,
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
        let principal = principal.into();
        let command_id = command.command_id();
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
                if self.stop_receipt {
                    sqlx::query("INSERT INTO submit_input_stop_receipt (command_id) VALUES ($1)")
                        .bind(command_id.into_uuid())
                        .execute(&mut *transaction)
                        .await?;
                }
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
                | CommandKind::ReloadConfiguration
                | CommandKind::ProvisionOauthCredential
                | CommandKind::ReprovisionOauthCredential
                | CommandKind::DeleteOauthCredential
                | CommandKind::ClearCredentialExclusion
                | CommandKind::CancelProgramRun
                | CommandKind::ReplaceLostRunner
                | CommandKind::AbandonLostRunner
                | CommandKind::PromotePendingRunner
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
    if matches!(actual, 3 | 4) {
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

async fn decode_actor(
    kind: String,
    turn: Option<Uuid>,
    tool_request: Option<Uuid>,
    program_run: Option<Uuid>,
    verified_program_run: Option<Uuid>,
    version: i16,
) -> Result<Actor, SubmitInputRepositoryError> {
    if program_run.is_some() || verified_program_run.is_some() {
        return match (
            kind.as_str(),
            turn,
            tool_request,
            program_run,
            verified_program_run,
            version,
        ) {
            ("program", None, None, Some(run), Some(verified), 4) if run == verified => {
                let run = signalbox_domain::ProgramRunId::from_uuid(run);
                signalbox_domain::program_session::ProgramSessionHost::new(
                    StoredProgramRunReference { run },
                )
                .session_capability(run)
                .await?
                .map(|capability| capability.actor())
                .ok_or_else(|| {
                    SubmitInputCorruption::Inconsistent("program actor reference").into()
                })
            }
            _ => Err(
                SubmitInputCorruption::Inconsistent("program actor reference or version").into(),
            ),
        };
    }
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
        ("user" | "core" | "model" | "recovery" | "tool" | "program", _, _) => {
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

// Created only after the typed row's run equals its joined retained journal anchor.
struct StoredProgramRunReference {
    run: signalbox_domain::ProgramRunId,
}

impl signalbox_domain::program_session::ProgramRunVerifier for StoredProgramRunReference {
    type Error = SubmitInputRepositoryError;

    async fn verify_run(&self, run: signalbox_domain::ProgramRunId) -> Result<bool, Self::Error> {
        Ok(self.run == run)
    }
}
