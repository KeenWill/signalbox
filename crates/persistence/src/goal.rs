//! PostgreSQL storage for session-scoped commissioned goals.
//!
//! The event stream is authoritative. Loads decode every durable event and
//! replay it through the domain aggregate; no mutable current-state row exists.

use std::{error::Error, fmt, num::NonZeroU64};

use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, CommandPrincipal, CoreAgency, DescendantTerminationScope, DurableCommandId,
    FinishCheckVerdict, FinishCondition, FrozenAliasDefinition, Goal, GoalBlockProvenance,
    GoalBlockedReasonKind, GoalCommandRejection, GoalCommandResult, GoalEvent, GoalEventKind,
    GoalEventOrdinal, GoalGeneration, GoalGuidance, GoalModelBlockedReasonKind,
    GoalModelProvenance, GoalNeed, GoalReconstitutionFailure, GoalReconstitutionInput, GoalReport,
    GoalSchedulerProvenance, GoalState, GoalStatement, GoalTextError, GoalTransitionError,
    GoalTransitionFailure, GoalTurnSource, GoalUserAction, GoalUserCommand, GoalUserProvenance,
    LifecycleActor, ModelAlias, ModelSelectionOverride, OriginConfiguration,
    ReconstitutedGoalCommand, SessionClosureOutcome, SessionId, SessionTerminalOutcome, TurnId,
};
use sqlx::{PgConnection, PgPool, Row, types::Uuid};

use crate::{
    command_registry::{self, CommandKind, GOAL_KIND, RegistryCorruption, RegistryInspectionError},
    commit_failure_is_ambiguous,
    goal_turn::{
        GoalTurnAcceptancePosition, GoalTurnCandidates, GoalTurnContinuationOutcome,
        GoalTurnInsertion, GoalTurnTerminalState, bind_goal_turn, continuation_exists,
        current_goal_turn, goal_turn_frozen_alias_definition, goal_turn_generation,
        goal_turn_terminal_state, insert_goal_turn, next_goal_turn_acceptance_position,
        retire_ineligible_queued_goal_turn,
    },
    mapping::{
        DurableCommandIdMappingError, GoalEventDiscriminator, GoalOperationKind,
        PositiveOrdinalMappingError, dispatching_module_from_str, dispatching_module_to_str,
        durable_command_id_from_uuid, durable_command_id_to_uuid, goal_blocked_reason_from_str,
        goal_blocked_reason_to_str, goal_command_rejection_from_str, goal_command_rejection_to_str,
        goal_event_kind_from_str, goal_event_kind_to_str, goal_model_blocked_reason_from_str,
        goal_operation_from_str, goal_operation_to_str, lifecycle_actor_to_str,
        positive_u64_from_numeric, session_closure_outcome_from_str,
        session_closure_outcome_to_str, session_id_from_uuid, session_id_to_uuid,
        tool_request_id_from_uuid, tool_request_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid,
    },
    outbox::{self, OutboxEvent},
    session::SessionCorruption,
    session_lifecycle::SessionLifecycleRepositoryError,
};

const STORAGE_VERSION: i16 = 1;

/// Closed durable cause for an execution failure that requires an operator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalExecutionFailureRecoveryCause {
    /// No safe context-compaction boundary fits the configured model window.
    ContextCompactionInputDoesNotFit,
}

impl GoalExecutionFailureRecoveryCause {
    /// Returns the closed durable spelling used by storage and telemetry.
    pub const fn code(self) -> &'static str {
        match self {
            Self::ContextCompactionInputDoesNotFit => "context_compaction_input_does_not_fit",
        }
    }

    fn parse(value: &str) -> Result<Self, GoalCorruption> {
        match value {
            "context_compaction_input_does_not_fit" => Ok(Self::ContextCompactionInputDoesNotFit),
            value => Err(GoalCorruption::Unsupported {
                field: "goal_execution_failure_recovery cause_kind",
                value: value.to_owned(),
            }),
        }
    }
}

/// Result of handling a user-global goal command identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalCommandHandlingOutcome {
    /// First handling or structurally equal replay returns the durable result.
    Recorded(GoalCommandResult),
    /// The identity already has another user-global meaning.
    ConflictingReuse {
        /// The retained conflicting command identity.
        command_id: DurableCommandId,
    },
    /// The expected lineage head no longer held under the session lock, so
    /// nothing was applied and the identity remains unspent.
    LineageMoved,
    /// Another live commissioned session owns the same pull-request target.
    TargetBusy {
        /// The competing live session.
        session: SessionId,
    },
}

/// Result of a scheduler- or model-provenance transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalTransitionOutcome {
    /// The transition appended this event.
    Applied(GoalEvent),
    /// The session's closure is pending; the closure settles the goal.
    SessionClosing,
    /// The session has no attached goal.
    GoalNotAttached,
    /// The current state rejected the requested transition.
    Rejected(GoalTransitionError),
    /// Scheduler provenance did not name a turn in the current goal generation.
    NotCurrentGoalTurn,
}

/// One latest execution-failure block selected for daemon reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingGoalExecutionFailure {
    session: SessionId,
    blocked: GoalEventOrdinal,
}

impl PendingGoalExecutionFailure {
    /// Returns the session whose goal remains blocked.
    pub const fn session(self) -> SessionId {
        self.session
    }

    /// Returns the exact blocked event the reconciliation must answer.
    pub const fn blocked(self) -> GoalEventOrdinal {
        self.blocked
    }
}

/// A durable goal shape that cannot reconstruct domain values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalCorruption {
    /// One required row or field is absent.
    Missing(&'static str),
    /// A closed discriminator is unsupported.
    Unsupported {
        /// The unsupported field.
        field: &'static str,
        /// The exact stored spelling.
        value: String,
    },
    /// Stored variant fields or relationships disagree.
    Inconsistent(&'static str),
    /// A PostgreSQL column could not decode into its declared Rust representation.
    Column(&'static str),
    /// A positive stored ordinal cannot map to the domain.
    InvalidOrdinal(PositiveOrdinalMappingError),
    /// Stored bounded text cannot map to the domain.
    InvalidText(GoalTextError),
    /// Stored user-command provenance uses a sentinel identity.
    InvalidCommandId(DurableCommandIdMappingError),
    /// The complete history failed domain-owned replay.
    Domain(GoalReconstitutionFailure),
    /// The session configuration needed to accept a goal turn is corrupt.
    Session(SessionCorruption),
}
impl fmt::Display for GoalCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing goal {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported goal {field}: {value}")
            }
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent goal {relationship}")
            }
            Self::Column(reason) => {
                write!(formatter, "invalid goal column representation: {reason}")
            }
            Self::InvalidOrdinal(reason) => write!(formatter, "invalid goal ordinal: {reason}"),
            Self::InvalidText(reason) => write!(formatter, "invalid goal text: {reason}"),
            Self::InvalidCommandId(reason) => {
                write!(formatter, "invalid goal command identity: {reason}")
            }
            Self::Domain(reason) => write!(formatter, "goal reconstitution failed: {reason:?}"),
            Self::Session(reason) => write!(formatter, "goal turn Session is invalid: {reason}"),
        }
    }
}

impl Error for GoalCorruption {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidOrdinal(error) => Some(error),
            Self::InvalidText(error) => Some(error),
            Self::InvalidCommandId(error) => Some(error),
            Self::Session(error) => Some(error),
            Self::Missing(_)
            | Self::Unsupported { .. }
            | Self::Inconsistent(_)
            | Self::Column(_)
            | Self::Domain(_) => None,
        }
    }
}

/// A database failure, ambiguous commit, wrong load purpose, or corruption.
#[derive(Debug)]
pub enum GoalRepositoryError {
    /// PostgreSQL could not complete the operation.
    Database(sqlx::Error),
    /// PostgreSQL did not reveal whether the final commit took effect.
    CommitAmbiguous(sqlx::Error),
    /// A purpose-specific load named another admitted command kind.
    DifferentCommandKind {
        /// The user-global identity naming another command kind.
        command_id: DurableCommandId,
    },
    /// Durable rows cannot reconstruct the requested value.
    Corruption(GoalCorruption),
}

impl fmt::Display for GoalRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "goal database failure: {error}"),
            Self::CommitAmbiguous(error) => {
                write!(formatter, "goal commit outcome is ambiguous: {error}")
            }
            Self::DifferentCommandKind { command_id } => {
                write!(
                    formatter,
                    "durable command {command_id:?} does not name a goal command"
                )
            }
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for GoalRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::DifferentCommandKind { .. } => None,
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for GoalRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<GoalCorruption> for GoalRepositoryError {
    fn from(error: GoalCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL implementation of goal commands, transitions, and history loads.
#[derive(Clone, Debug)]
pub struct GoalRepository {
    pool: PgPool,
}

impl GoalRepository {
    /// Uses the supplied pool for independent goal transactions.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Loads the durable operator-required cause for one failed goal turn.
    pub async fn execution_failure_recovery_cause(
        &self,
        session: SessionId,
        turn: TurnId,
    ) -> Result<Option<GoalExecutionFailureRecoveryCause>, GoalRepositoryError> {
        let cause = sqlx::query_scalar::<_, String>(
            "SELECT cause_kind
               FROM goal_execution_failure_recovery
              WHERE session_id = $1
                AND turn_id = $2",
        )
        .bind(session_id_to_uuid(session))
        .bind(turn_id_to_uuid(turn))
        .fetch_optional(&self.pool)
        .await?;
        cause
            .as_deref()
            .map(GoalExecutionFailureRecoveryCause::parse)
            .transpose()
            .map_err(GoalRepositoryError::Corruption)
    }

    /// Claims and handles an unseen user command, atomically scheduling a turn
    /// for each applied pursuing transition, or resolves its durable meaning.
    pub async fn handle_user_command<SelectDefinition>(
        &self,
        command: GoalUserCommand,
        candidates: Option<GoalTurnCandidates>,
        select_definition: SelectDefinition,
    ) -> Result<GoalCommandHandlingOutcome, GoalRepositoryError>
    where
        SelectDefinition: FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    {
        self.handle_command(
            command,
            candidates,
            None,
            CommandPrincipal::Operator,
            select_definition,
        )
        .await
    }

    /// Handles a user command only while the goal's last recorded event is
    /// still `expected_head`, deciding that under the same session lock the
    /// command applies under.
    ///
    /// A caller that read the lineage in an earlier transaction cannot
    /// otherwise know which state its command lands on: between that read and
    /// this lock the goal may have been resumed, blocked again for an unrelated
    /// reason, stopped, or superseded, and an unconditional command would apply
    /// to whatever it finds. An unmet expectation rolls the claim back, so the
    /// identity stays unspent and a later attempt may still use it.
    pub async fn handle_expected_user_command<SelectDefinition>(
        &self,
        command: GoalUserCommand,
        candidates: Option<GoalTurnCandidates>,
        expected_head: GoalEventOrdinal,
        select_definition: SelectDefinition,
    ) -> Result<GoalCommandHandlingOutcome, GoalRepositoryError>
    where
        SelectDefinition: FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    {
        self.handle_command(
            command,
            candidates,
            Some(expected_head),
            CommandPrincipal::Core,
            select_definition,
        )
        .await
    }

    async fn handle_command<SelectDefinition>(
        &self,
        command: GoalUserCommand,
        candidates: Option<GoalTurnCandidates>,
        expected_head: Option<GoalEventOrdinal>,
        principal: CommandPrincipal,
        select_definition: SelectDefinition,
    ) -> Result<GoalCommandHandlingOutcome, GoalRepositoryError>
    where
        SelectDefinition: FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    {
        let command_id = command.command_id();
        let mut transaction = self.pool.begin().await?;
        if let Some(kind) = inspect_registry(&mut transaction, command_id).await? {
            let outcome = existing_or_conflicting(&mut transaction, &command, kind).await?;
            transaction.rollback().await?;
            return Ok(outcome);
        }
        if command.action().starts_pursuit()
            && let Some(session) =
                crate::commissioned_dispatch::lock_competing_pull_request_session(
                    &mut transaction,
                    command.session(),
                )
                .await?
        {
            transaction.rollback().await?;
            return Ok(GoalCommandHandlingOutcome::TargetBusy { session });
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
        .bind(GOAL_KIND)
        .bind(STORAGE_VERSION)
        .bind(issuer.0)
        .bind(issuer.1)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            let kind = inspect_registry(&mut transaction, command_id)
                .await?
                .ok_or(GoalCorruption::Inconsistent(
                    "winner command claim disappeared",
                ))?;
            let outcome = existing_or_conflicting(&mut transaction, &command, kind).await?;
            transaction.rollback().await?;
            return Ok(outcome);
        }

        if matches!(
            command.action(),
            GoalUserAction::Stop {
                descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            }
        ) {
            sqlx::query(crate::lock_inventory::DELEGATION_TERMINATION_SESSION_FRONTIER)
                .bind(session_id_to_uuid(command.session()))
                .bind("stopped")
                .execute(&mut *transaction)
                .await?;
        }

        let session_exists = lock_session(&mut transaction, command.session()).await?;

        // An automatic resume names the block it answers; a park taken since is
        // the same "the lineage moved under us" case, and lifting it would undo
        // an operator hold and schedule new model work. Read under the session
        // lock, so a park cannot commit between the decision and the resume.
        // An operator's own resume names no expected head and still lifts it.
        if session_exists
            && expected_head.is_some()
            && !session_admits_automatic_resume(&mut transaction, command.session()).await?
        {
            transaction.rollback().await?;
            return Ok(GoalCommandHandlingOutcome::LineageMoved);
        }

        // A committed closure has already decided this session's outcome, and a
        // goal event contradicting that decision would make the settlement
        // refuse, with the handoff standing and activation frozen behind it. An
        // internal call names an expected head and reads that as the lineage
        // moving; a client command names none and takes the durable
        // `session_closing` rejection below.
        if session_exists
            && expected_head.is_some()
            && session_holds_committed_closure(&mut transaction, command.session()).await?
        {
            transaction.rollback().await?;
            return Ok(GoalCommandHandlingOutcome::LineageMoved);
        }

        let mut result = if !session_exists {
            GoalCommandResult::Rejected(GoalCommandRejection::SessionNotFound)
        } else if session_is_closing(&mut transaction, command.session()).await? {
            GoalCommandResult::Rejected(GoalCommandRejection::SessionClosing)
        } else {
            match apply_user_command(&mut transaction, &command, expected_head).await? {
                UserCommandApplication::Recorded(result) => result,
                UserCommandApplication::LineageMoved => {
                    // Rolling back releases the claim as well as the command's
                    // own writes, which is what leaves the identity unspent.
                    transaction.rollback().await?;
                    return Ok(GoalCommandHandlingOutcome::LineageMoved);
                }
            }
        };
        let starts_pursuit = match &result {
            GoalCommandResult::Applied(event) => event_starts_pursuit(event),
            GoalCommandResult::Rejected(_) => false,
        };
        match &result {
            GoalCommandResult::Applied(_) => {
                lock_scheduler(&mut transaction, command.session()).await?;
            }
            GoalCommandResult::Rejected(_) => {}
        }
        let turn_admission = if starts_pursuit {
            match current_origin_configuration(
                &mut transaction,
                command.session(),
                select_definition,
            )
            .await?
            {
                CurrentOriginConfiguration::Selected(configuration) => {
                    match next_goal_turn_acceptance_position(&mut transaction, command.session())
                        .await?
                    {
                        GoalTurnAcceptancePosition::Available(position) => {
                            Some((configuration, position))
                        }
                        GoalTurnAcceptancePosition::Exhausted { .. } => {
                            result = GoalCommandResult::Rejected(
                                GoalCommandRejection::AcceptancePositionExhausted,
                            );
                            None
                        }
                    }
                }
                CurrentOriginConfiguration::UnknownAlias(_) => {
                    result = GoalCommandResult::Rejected(GoalCommandRejection::UnknownModelAlias);
                    None
                }
            }
        } else {
            None
        };
        insert_command(&mut transaction, &command, &result).await?;
        match &result {
            GoalCommandResult::Applied(event) => {
                insert_event(&mut transaction, command.session(), event).await?;
                if matches!(command.action(), GoalUserAction::Attach(_)) {
                    crate::session_lifecycle::confer_ownership_in_transaction(
                        &mut transaction,
                        command.session(),
                        principal.classify(None),
                    )
                    .await
                    .map_err(|error| match error {
                        SessionLifecycleRepositoryError::Database(error)
                        | SessionLifecycleRepositoryError::CommitAmbiguous(error) => {
                            GoalRepositoryError::Database(error)
                        }
                        _ => GoalCorruption::Inconsistent("attached session refused ownership")
                            .into(),
                    })?;
                }
                if event_may_retire_queued_turn(event) {
                    retire_ineligible_queued_goal_turn(&mut transaction, command.session()).await?;
                }
                if event_starts_pursuit(event) {
                    let candidates = candidates.ok_or(GoalCorruption::Missing(
                        "turn candidates for pursuing command",
                    ))?;
                    let (configuration, position) = turn_admission.ok_or(
                        GoalCorruption::Missing("turn admission for pursuing command"),
                    )?;
                    let goal = load_goal_from_connection(&mut transaction, command.session())
                        .await?
                        .ok_or(GoalCorruption::Missing("scheduled command goal"))?;
                    insert_goal_turn(
                        &mut transaction,
                        command.session(),
                        goal.current().generation(),
                        GoalTurnSource::UserEvent(event.ordinal()),
                        pursuit_input(&goal, event)?,
                        &configuration,
                        GoalTurnInsertion::new(position, candidates),
                    )
                    .await?;
                }
            }
            GoalCommandResult::Rejected(_) => {}
        }
        sqlx::query("SELECT materialize_session_delegation_termination_cascade($1, 'stopped')")
            .bind(durable_command_id_to_uuid(command_id))
            .execute(&mut *transaction)
            .await?;
        commit(transaction).await?;
        Ok(GoalCommandHandlingOutcome::Recorded(result))
    }

    /// Loads the exact assistant-text part immediately preceding a correlated
    /// `goal_declare` request when it is the final part of the same provider response.
    pub async fn load_model_declaration_text(
        &self,
        session: SessionId,
        provenance: GoalModelProvenance,
    ) -> Result<Option<String>, GoalRepositoryError> {
        let text = sqlx::query_scalar::<_, String>(
            "SELECT declaration.assistant_text_value
               FROM tool_request AS request
               JOIN semantic_transcript_entry AS tool_use
                 ON tool_use.source_session_id = request.session_id
                AND tool_use.producing_model_call_id = request.producing_model_call_id
                AND tool_use.payload_kind = 'assistant_tool_use'
                AND tool_use.assistant_tool_request_id = request.request_id
               JOIN semantic_transcript_entry AS declaration
                 ON declaration.source_session_id = tool_use.source_session_id
                AND declaration.producing_model_call_id = tool_use.producing_model_call_id
                AND declaration.payload_kind = 'assistant_text'
                AND declaration.assistant_response_part_ordinal + 1 =
                    tool_use.assistant_response_part_ordinal
              WHERE request.request_id = $1
                AND request.session_id = $2
                AND request.turn_id = $3
                AND request.tool_name = 'goal_declare'
                AND NOT EXISTS (
                    SELECT 1
                      FROM semantic_transcript_entry AS later_part
                     WHERE later_part.source_session_id = tool_use.source_session_id
                       AND later_part.producing_model_call_id =
                           tool_use.producing_model_call_id
                       AND later_part.assistant_response_part_ordinal >
                           tool_use.assistant_response_part_ordinal
                )",
        )
        .bind(tool_request_id_to_uuid(provenance.tool_request()))
        .bind(session_id_to_uuid(session))
        .bind(turn_id_to_uuid(provenance.turn()))
        .fetch_optional(&self.pool)
        .await?;
        Ok(text)
    }

    /// Loads one complete durable user-command receipt.
    pub async fn load_command(
        &self,
        command_id: DurableCommandId,
    ) -> Result<Option<ReconstitutedGoalCommand>, GoalRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        match inspect_registry(&mut connection, command_id).await? {
            None => Ok(None),
            Some(CommandKind::Goal) => {
                load_command_from_connection(&mut connection, command_id).await
            }
            Some(
                CommandKind::CreateSession
                | CommandKind::CreateSessionFromImportedFrontier
                | CommandKind::ReplaceSessionDefaults
                | CommandKind::ReplaceSessionMetadata
                | CommandKind::SubmitInput
                | CommandKind::DecideToolRequest
                | CommandKind::OverrideDeniedToolRequest
                | CommandKind::ReviewWorkflow
                | CommandKind::ReviewOrchestration
                | CommandKind::CompactSession
                | CommandKind::UpdateSessionPlacement
                | CommandKind::RegisterWorkspace
                | CommandKind::MintGitRemote
                | CommandKind::WithdrawGitRemote
                | CommandKind::SessionLifecycle,
            ) => Err(GoalRepositoryError::DifferentCommandKind { command_id }),
        }
    }

    /// Loads and domain-replays a session's full goal lineage.
    pub async fn load_goal(&self, session: SessionId) -> Result<Option<Goal>, GoalRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        load_goal_from_connection(&mut connection, session).await
    }

    /// Whether the daemon holds the session's liveness obligation: an
    /// automatic resume is owed to an owned session only.
    pub async fn session_owned(&self, session: SessionId) -> Result<bool, GoalRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        session_owned(&mut connection, session).await
    }

    /// Loads the current turn in one goal generation.
    ///
    /// The daemon uses this only to associate a pre-block execution failure
    /// with the automatic resume whose budget it would otherwise spend. The
    /// guarded goal transition still revalidates the same turn under the
    /// session lock before appending anything.
    pub async fn load_current_goal_turn(
        &self,
        session: SessionId,
        generation: GoalGeneration,
    ) -> Result<Option<TurnId>, GoalRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        current_goal_turn(&mut connection, session, generation).await
    }

    /// Selects failed turns whose automatic resume does not spend its budget.
    ///
    /// Reconciled ambiguity is infrastructure work whether it originated at
    /// startup or from the live watchdog. A definitive provider response is
    /// likewise external only for transient rate limiting, overload, or an
    /// internal provider failure. A continuation closed for configured context
    /// headroom is also daemon-owned. Session-actionable provider failures
    /// remain chargeable.
    pub async fn unchargeable_automatic_resume_turns(
        &self,
        session: SessionId,
        turns: &[TurnId],
    ) -> Result<Box<[TurnId]>, GoalRepositoryError> {
        if turns.is_empty() {
            return Ok(Box::new([]));
        }
        let turn_ids = turns
            .iter()
            .map(|turn| turn_id_to_uuid(*turn))
            .collect::<Vec<_>>();
        let rows = sqlx::query(
            "SELECT lifecycle.turn_id
               FROM turn_lifecycle AS lifecycle
               LEFT JOIN automatic_reconciliation AS recovery
                 ON recovery.turn_id = lifecycle.turn_id
                AND recovery.session_id = lifecycle.session_id
               LEFT JOIN model_call AS terminal_call
                 ON terminal_call.model_call_id = lifecycle.terminal_model_call_id
                AND terminal_call.turn_id = lifecycle.turn_id
                AND terminal_call.session_id = lifecycle.session_id
               LEFT JOIN tool_continuation_context_headroom AS headroom
                 ON headroom.terminal_attempt_id = lifecycle.terminal_attempt_id
                AND headroom.turn_id = lifecycle.turn_id
                AND headroom.session_id = lifecycle.session_id
              WHERE lifecycle.session_id = $1
                AND lifecycle.turn_id = ANY($2::uuid[])
                AND (recovery.state_kind = 'reconciled'
                     OR headroom.terminal_attempt_id IS NOT NULL
                     OR terminal_call.terminal_provider_failure_cause IN
                        ('rate_limited', 'overloaded', 'provider_internal'))
              ORDER BY lifecycle.turn_id",
        )
        .bind(session_id_to_uuid(session))
        .bind(turn_ids)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| Ok(turn_id_from_uuid(column(row, "turn_id")?)))
            .collect::<Result<Vec<_>, GoalRepositoryError>>()
            .map(Vec::into_boxed_slice)
    }

    /// Lists latest execution-failure blocks carrying one exact need.
    ///
    /// The need distinguishes daemon-scheduled automatic resumption from
    /// execution-failure blocks that deliberately require an operator, such as
    /// an unattended approval escalation. A later event removes the session
    /// from this inventory without mutating the historical block.
    pub async fn pending_execution_failures_with_need(
        &self,
        need: &GoalNeed,
    ) -> Result<Box<[PendingGoalExecutionFailure]>, GoalRepositoryError> {
        let rows = sqlx::query(
            // Auto-resume is an owned-session obligation: without the
            // conjunct a conversation someone attached a goal to keeps
            // spending model work on its own.
            "SELECT event.session_id, event.event_ordinal
               FROM goal_event AS event
               LEFT JOIN goal_execution_failure_resumption_arm AS arm
                 ON arm.session_id = event.session_id
                AND arm.event_ordinal = event.event_ordinal
               JOIN session_lifecycle AS lifecycle
                 ON lifecycle.session_id = event.session_id
                AND lifecycle.owned
                AND lifecycle.state_kind <> 'parked'
                AND lifecycle.pending_terminal_outcome_kind IS NULL
              WHERE event.event_kind = 'blocked'
                AND event.blocked_reason = 'execution_failure'
                AND COALESCE(arm.need, event.need) = $1
                AND NOT EXISTS (
                    SELECT 1
                      FROM goal_event AS later
                     WHERE later.session_id = event.session_id
                       AND later.event_ordinal > event.event_ordinal)
              ORDER BY event.session_id",
        )
        .bind(need.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(PendingGoalExecutionFailure {
                    session: session_id_from_uuid(column(row, "session_id")?),
                    blocked: GoalEventOrdinal::new(positive(column(row, "event_ordinal")?)?),
                })
            })
            .collect::<Result<Vec<_>, GoalRepositoryError>>()
            .map(Vec::into_boxed_slice)
    }

    /// Selects one session's latest owned execution-failure block carrying an
    /// exact effective need while holding the session lock.
    ///
    /// Adoption and block append both serialize on this lock. Rechecking here
    /// therefore closes either ordering of those commits without arming blocks
    /// whose durable need requires an operator.
    pub async fn pending_owned_execution_failure_with_need(
        &self,
        session: SessionId,
        need: &GoalNeed,
    ) -> Result<Option<GoalEventOrdinal>, GoalRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let exists = lock_session(&mut transaction, session).await?;
        if !exists || !session_admits_automatic_resume(&mut transaction, session).await? {
            transaction.rollback().await?;
            return Ok(None);
        }
        let ordinal = sqlx::query_scalar::<_, Decimal>(
            "SELECT event.event_ordinal
               FROM goal_event AS event
               LEFT JOIN goal_execution_failure_resumption_arm AS arm
                 ON arm.session_id = event.session_id
                AND arm.event_ordinal = event.event_ordinal
              WHERE event.session_id = $1
                AND event.event_kind = 'blocked'
                AND event.blocked_reason = 'execution_failure'
                AND COALESCE(arm.need, event.need) = $2
                AND NOT EXISTS (
                    SELECT 1
                      FROM goal_event AS later
                     WHERE later.session_id = event.session_id
                       AND later.event_ordinal > event.event_ordinal)",
        )
        .bind(session_id_to_uuid(session))
        .bind(need.as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        transaction.rollback().await?;
        ordinal
            .map(|ordinal| positive(ordinal).map(GoalEventOrdinal::new))
            .transpose()
            .map_err(Into::into)
    }

    /// Persists the scheduled need an adopted execution-failure block acquires.
    ///
    /// The session lock serializes the ownership check with release and block
    /// append. The append-only overlay makes an ambiguous retry idempotent and
    /// leaves the historical goal event unchanged.
    pub async fn arm_owned_execution_failure(
        &self,
        session: SessionId,
        unmonitored_need: &GoalNeed,
        scheduled_need: &GoalNeed,
    ) -> Result<Option<GoalEventOrdinal>, GoalRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let exists = lock_session(&mut transaction, session).await?;
        if !exists || !session_admits_automatic_resume(&mut transaction, session).await? {
            transaction.rollback().await?;
            return Ok(None);
        }
        let ordinal = sqlx::query_scalar::<_, Decimal>(
            "WITH candidate AS (
                SELECT event.session_id, event.event_ordinal
                  FROM goal_event AS event
                 WHERE event.session_id = $1
                   AND event.event_kind = 'blocked'
                   AND event.blocked_reason = 'execution_failure'
                   AND event.need = $2
                   AND NOT EXISTS (
                       SELECT 1
                         FROM goal_event AS later
                        WHERE later.session_id = event.session_id
                          AND later.event_ordinal > event.event_ordinal)
            ), inserted AS (
                INSERT INTO goal_execution_failure_resumption_arm
                    (session_id, event_ordinal, need)
                SELECT session_id, event_ordinal, $3 FROM candidate
                ON CONFLICT DO NOTHING
                RETURNING event_ordinal
            )
            SELECT event_ordinal FROM inserted
            UNION ALL
            SELECT arm.event_ordinal
              FROM goal_execution_failure_resumption_arm AS arm
              JOIN candidate USING (session_id, event_ordinal)
             WHERE arm.need = $3
            LIMIT 1",
        )
        .bind(session_id_to_uuid(session))
        .bind(unmonitored_need.as_str())
        .bind(scheduled_need.as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        commit(transaction).await?;
        ordinal
            .map(|ordinal| positive(ordinal).map(GoalEventOrdinal::new))
            .transpose()
            .map_err(Into::into)
    }

    /// Reconciles one current goal turn's durable terminal disposition.
    ///
    /// Nonterminal work is left alone, completion queues one idempotent
    /// successor, and every unsuccessful terminal or successor-admission failure
    /// blocks pursuit with scheduler-only execution-failure provenance.
    pub async fn reconcile_current_after_execution<SelectDefinition>(
        &self,
        session: SessionId,
        candidates: GoalTurnCandidates,
        failure_need: GoalNeed,
        select_definition: SelectDefinition,
    ) -> Result<GoalTurnContinuationOutcome, GoalRepositoryError>
    where
        SelectDefinition: FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    {
        let mut transaction = self.pool.begin().await?;
        if !lock_session(&mut transaction, session).await? {
            transaction.rollback().await?;
            return Ok(GoalTurnContinuationOutcome::NotPursuing);
        }
        let Some(goal) = load_goal_from_connection(&mut transaction, session).await? else {
            transaction.rollback().await?;
            return Ok(GoalTurnContinuationOutcome::NotPursuing);
        };
        match goal.current().state() {
            GoalState::Pursuing => {}
            GoalState::Blocked { .. }
            | GoalState::Achieved { .. }
            | GoalState::UserStopped
            | GoalState::Superseded { .. }
            | GoalState::SessionClosed { .. } => {
                transaction.rollback().await?;
                return Ok(GoalTurnContinuationOutcome::NotPursuing);
            }
        }
        let generation = goal.current().generation();
        let predecessor = current_goal_turn(&mut transaction, session, generation)
            .await?
            .ok_or(GoalCorruption::Missing("current goal turn"))?;
        match goal_turn_terminal_state(&mut transaction, session, predecessor).await? {
            GoalTurnTerminalState::NotTerminal => {
                transaction.rollback().await?;
                return Ok(GoalTurnContinuationOutcome::NotTerminal);
            }
            GoalTurnTerminalState::Unsuccessful => {
                return block_goal_continuation(
                    transaction,
                    session,
                    goal,
                    failure_need,
                    predecessor,
                )
                .await;
            }
            GoalTurnTerminalState::Retired => {
                transaction.rollback().await?;
                return Ok(GoalTurnContinuationOutcome::NotPursuing);
            }
            GoalTurnTerminalState::Completed => {}
        }
        if !session_owned(&mut transaction, session).await? {
            transaction.rollback().await?;
            return Ok(GoalTurnContinuationOutcome::NotPursuing);
        }
        if continuation_exists(&mut transaction, session, predecessor).await? {
            transaction.rollback().await?;
            return Ok(GoalTurnContinuationOutcome::AlreadyScheduled);
        }
        let frozen_alias =
            goal_turn_frozen_alias_definition(&mut transaction, session, predecessor).await?;
        let configuration = match current_origin_configuration(&mut transaction, session, |alias| {
            select_definition_with_frozen_fallback(alias, select_definition(alias), frozen_alias)
        })
        .await?
        {
            CurrentOriginConfiguration::Selected(configuration) => configuration,
            CurrentOriginConfiguration::UnknownAlias(alias) => {
                transaction.rollback().await?;
                return Ok(GoalTurnContinuationOutcome::UnknownModelAlias { alias });
            }
        };
        let position = match next_goal_turn_acceptance_position(&mut transaction, session).await? {
            GoalTurnAcceptancePosition::Available(position) => position,
            GoalTurnAcceptancePosition::Exhausted { last } => {
                transaction.rollback().await?;
                return Ok(GoalTurnContinuationOutcome::AcceptancePositionExhausted { last });
            }
        };
        insert_goal_turn(
            &mut transaction,
            session,
            generation,
            GoalTurnSource::SuccessfulTurn(predecessor),
            goal.current().statement().as_str(),
            &configuration,
            GoalTurnInsertion::new(position, candidates),
        )
        .await?;
        commit(transaction).await?;
        Ok(GoalTurnContinuationOutcome::Scheduled {
            turn: candidates.turn(),
        })
    }

    /// Appends a model-declared blocked transition.
    pub async fn declare_blocked(
        &self,
        session: SessionId,
        reason: GoalModelBlockedReasonKind,
        need: GoalNeed,
        provenance: GoalModelProvenance,
    ) -> Result<GoalTransitionOutcome, GoalRepositoryError> {
        self.handle_system_transition(
            session,
            SystemTransition::Blocked {
                reason,
                need,
                provenance,
            },
        )
        .await
    }

    /// Appends a model-declared achievement gated on its finish check: a
    /// passing verdict commits `achieved_verified` to the session's terminal
    /// handoff, an unverified one `achieved_declared`, a failing one nothing.
    pub async fn declare_achieved(
        &self,
        session: SessionId,
        report: GoalReport,
        provenance: GoalModelProvenance,
        verdict: FinishCheckVerdict,
    ) -> Result<GoalTransitionOutcome, GoalRepositoryError> {
        self.handle_system_transition(
            session,
            SystemTransition::Achieved {
                report,
                provenance,
                verdict,
            },
        )
        .await
    }

    /// Loads the finish condition one session declares.
    pub async fn load_finish_condition(
        &self,
        session: SessionId,
    ) -> Result<Option<FinishCondition>, GoalRepositoryError> {
        let row = sqlx::query(
            "SELECT finish_condition_kind, finish_condition
               FROM session_lifecycle
              WHERE session_id = $1",
        )
        .bind(session_id_to_uuid(session))
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        crate::mapping::finish_condition_from_columns(
            row.try_get("finish_condition_kind")?,
            row.try_get("finish_condition")?,
        )
        .map_err(|detail| GoalCorruption::Inconsistent(detail).into())
    }

    /// Appends scheduler-only execution-failure blocking without retry.
    pub async fn block_execution_failure(
        &self,
        session: SessionId,
        need: GoalNeed,
        provenance: GoalSchedulerProvenance,
    ) -> Result<GoalTransitionOutcome, GoalRepositoryError> {
        self.handle_system_transition(
            session,
            SystemTransition::ExecutionFailure {
                need,
                unmonitored_need: None,
                provenance,
            },
        )
        .await
    }

    /// Appends scheduler-only execution-failure blocking and chooses the
    /// unmonitored need under the session lock when ownership was released.
    pub async fn block_execution_failure_for_current_ownership(
        &self,
        session: SessionId,
        owned_need: GoalNeed,
        unmonitored_need: GoalNeed,
        provenance: GoalSchedulerProvenance,
    ) -> Result<GoalTransitionOutcome, GoalRepositoryError> {
        self.handle_system_transition(
            session,
            SystemTransition::ExecutionFailure {
                need: owned_need,
                unmonitored_need: Some(unmonitored_need),
                provenance,
            },
        )
        .await
    }

    async fn handle_system_transition(
        &self,
        session: SessionId,
        mut transition: SystemTransition,
    ) -> Result<GoalTransitionOutcome, GoalRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        if !lock_session(&mut transaction, session).await? {
            transaction.rollback().await?;
            return Ok(GoalTransitionOutcome::GoalNotAttached);
        }
        if let SystemTransition::ExecutionFailure {
            need,
            unmonitored_need,
            ..
        } = &mut transition
        {
            *need = failure_need_for_current_ownership(
                &mut transaction,
                session,
                need.clone(),
                unmonitored_need.take(),
            )
            .await?;
        }
        if matches!(&transition, SystemTransition::Achieved { .. })
            && session_holds_committed_closure(&mut transaction, session).await?
        {
            transaction.rollback().await?;
            return Ok(GoalTransitionOutcome::NotCurrentGoalTurn);
        }
        let Some(goal) = load_goal_from_connection(&mut transaction, session).await? else {
            transaction.rollback().await?;
            return Ok(GoalTransitionOutcome::GoalNotAttached);
        };
        if session_is_closing(&mut transaction, session).await? {
            transaction.rollback().await?;
            return Ok(GoalTransitionOutcome::SessionClosing);
        }
        match transition.authority() {
            SystemTransitionAuthority::ModelDeclaration => {}
            SystemTransitionAuthority::SchedulerFailure => {
                if let Some(event) = recorded_scheduler_failure(&goal, transition.turn()) {
                    let event = event.clone();
                    transaction.rollback().await?;
                    return Ok(GoalTransitionOutcome::Applied(event));
                }
            }
        }
        let generation = goal_turn_generation(&mut transaction, session, transition.turn()).await?;
        if generation != Some(goal.current().generation()) {
            transaction.rollback().await?;
            return Ok(GoalTransitionOutcome::NotCurrentGoalTurn);
        }
        if current_goal_turn(&mut transaction, session, goal.current().generation()).await?
            != Some(transition.turn())
        {
            transaction.rollback().await?;
            return Ok(GoalTransitionOutcome::NotCurrentGoalTurn);
        }
        match transition.authority() {
            SystemTransitionAuthority::ModelDeclaration => {}
            SystemTransitionAuthority::SchedulerFailure => {
                if goal_turn_terminal_state(&mut transaction, session, transition.turn()).await?
                    != GoalTurnTerminalState::Unsuccessful
                {
                    transaction.rollback().await?;
                    return Ok(GoalTransitionOutcome::NotCurrentGoalTurn);
                }
            }
        }
        let mut settled = None;
        let transitioned = match transition {
            SystemTransition::Blocked {
                reason,
                need,
                provenance,
            } => goal.declare_blocked(reason, need, provenance),
            SystemTransition::Achieved {
                report,
                provenance,
                verdict,
            } => match verdict {
                FinishCheckVerdict::Failed { detail } => goal.block_finish_check(
                    GoalNeed::try_new(detail).map_err(|_| {
                        GoalCorruption::Inconsistent("finish check result is not need text")
                    })?,
                    provenance,
                ),
                FinishCheckVerdict::Passed => {
                    settled = Some((SessionTerminalOutcome::AchievedVerified, provenance));
                    goal.declare_achieved(report, provenance)
                }
                FinishCheckVerdict::Unverified => {
                    settled = Some((SessionTerminalOutcome::AchievedDeclared, provenance));
                    goal.declare_achieved(report, provenance)
                }
            },
            SystemTransition::ExecutionFailure {
                need, provenance, ..
            } => goal.block_execution_failure(need, provenance),
        };
        let goal = match transitioned {
            Ok(goal) => goal,
            Err(error) => {
                transaction.rollback().await?;
                return Ok(GoalTransitionOutcome::Rejected(error));
            }
        };
        let event = latest_event(&goal)?;
        insert_event(&mut transaction, session, &event).await?;
        if let Some((outcome, provenance)) = settled {
            crate::session_lifecycle::commit_pending_terminal_in_transaction(
                &mut transaction,
                session,
                outcome,
                LifecycleActor::Core {
                    agency: CoreAgency::Tool {
                        request: provenance.tool_request(),
                    },
                },
            )
            .await
            .map_err(|error| match error {
                SessionLifecycleRepositoryError::Database(error)
                | SessionLifecycleRepositoryError::CommitAmbiguous(error) => {
                    GoalRepositoryError::Database(error)
                }
                _ => GoalCorruption::Inconsistent("achieved session refused its handoff").into(),
            })?;
        }
        // A blocked or achieved transition retires the generation's queued
        // turns from the live-queue projection and the timeline work facts,
        // so the change must reach the process monitor as a durable outbox
        // event: an open follow stream otherwise retains the old queue state
        // with no cursor advance to force a resynchronization.
        retire_ineligible_queued_goal_turn(&mut transaction, session).await?;
        commit(transaction).await?;
        Ok(GoalTransitionOutcome::Applied(event))
    }
}

/// Applies scheduler failure authority inside a transaction that already owns
/// the session lock and has terminalized the exact failed turn.
///
/// Approval-judge headless closeout uses this boundary so its turn failure,
/// blocked goal event, repository-watch requeue, and singleton release share
/// one commit. The ordinary public method remains the entry point for
/// independent scheduler passes.
pub(crate) async fn block_execution_failure_locked(
    connection: &mut PgConnection,
    session: SessionId,
    need: GoalNeed,
    provenance: GoalSchedulerProvenance,
) -> Result<GoalTransitionOutcome, GoalRepositoryError> {
    let Some(goal) = load_goal_from_connection(connection, session).await? else {
        return Ok(GoalTransitionOutcome::GoalNotAttached);
    };
    if let Some(event) = recorded_scheduler_failure(&goal, provenance.turn()) {
        return Ok(GoalTransitionOutcome::Applied(event.clone()));
    }
    let generation = goal_turn_generation(connection, session, provenance.turn()).await?;
    if generation != Some(goal.current().generation()) {
        return Ok(GoalTransitionOutcome::NotCurrentGoalTurn);
    }
    if current_goal_turn(connection, session, goal.current().generation()).await?
        != Some(provenance.turn())
    {
        return Ok(GoalTransitionOutcome::NotCurrentGoalTurn);
    }
    if goal_turn_terminal_state(connection, session, provenance.turn()).await?
        != GoalTurnTerminalState::Unsuccessful
    {
        return Ok(GoalTransitionOutcome::NotCurrentGoalTurn);
    }
    let transitioned = match goal.block_execution_failure(need, provenance) {
        Ok(goal) => goal,
        Err(error) => return Ok(GoalTransitionOutcome::Rejected(error)),
    };
    let event = latest_event(&transitioned)?;
    insert_event(connection, session, &event).await?;
    // Same monitor-visibility requirement as `handle_system_transition`: a
    // blocked transition that retires a queued turn from the live projection
    // must surface as a durable outbox event.
    retire_ineligible_queued_goal_turn(connection, session).await?;
    Ok(GoalTransitionOutcome::Applied(event))
}

/// Records an operator-required recovery cause inside the transaction that
/// terminalizes the exact failed turn.
pub(crate) async fn record_execution_failure_recovery_cause(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    cause: GoalExecutionFailureRecoveryCause,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO goal_execution_failure_recovery
            (turn_id, session_id, cause_kind)
         VALUES ($1, $2, $3)",
    )
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .bind(cause.code())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// Whether a closure is committed to the session's terminal handoff.
async fn session_is_closing(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<bool, GoalRepositoryError> {
    let closing: Option<bool> = sqlx::query_scalar(
        "SELECT pending_terminal_outcome_kind IS NOT NULL OR state_kind = 'terminal'
           FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    Ok(closing.unwrap_or(false))
}

/// Whether a closure has already committed this session to an outcome.
async fn session_holds_committed_closure(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<bool, GoalRepositoryError> {
    let committed: Option<bool> = sqlx::query_scalar(
        "SELECT pending_terminal_outcome_kind IS NOT NULL
           FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    Ok(committed.unwrap_or(false))
}

/// Whether the session still admits the automatic resume that named its block.
///
/// Both facts are read under the session lock the caller already holds: an
/// in-memory timer armed before either changed would otherwise resume a
/// conversation that has since been released, or lift a park that has since
/// been taken.
async fn session_admits_automatic_resume(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<bool, GoalRepositoryError> {
    let admits: Option<bool> = sqlx::query_scalar(
        "SELECT owned AND state_kind <> 'parked'
                AND pending_terminal_outcome_kind IS NULL
           FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    Ok(admits.unwrap_or(false))
}

async fn failure_need_for_current_ownership(
    connection: &mut PgConnection,
    session: SessionId,
    owned_need: GoalNeed,
    unmonitored_need: Option<GoalNeed>,
) -> Result<GoalNeed, GoalRepositoryError> {
    let Some(unmonitored_need) = unmonitored_need else {
        return Ok(owned_need);
    };
    let owned = session_owned(connection, session).await?;
    Ok(if owned { owned_need } else { unmonitored_need })
}

async fn session_owned(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<bool, GoalRepositoryError> {
    let owned: Option<bool> =
        sqlx::query_scalar("SELECT owned FROM session_lifecycle WHERE session_id = $1")
            .bind(session_id_to_uuid(session))
            .fetch_optional(&mut *connection)
            .await?;
    Ok(owned.unwrap_or(false))
}

fn recorded_scheduler_failure(goal: &Goal, turn: TurnId) -> Option<&GoalEvent> {
    goal.events().iter().find(|event| match event.kind() {
        GoalEventKind::Blocked { block, .. } => match block {
            signalbox_domain::GoalBlockProvenance::ExecutionFailure { provenance } => {
                provenance.turn() == turn
            }
            signalbox_domain::GoalBlockProvenance::Model { .. }
            | signalbox_domain::GoalBlockProvenance::FinishCheck { .. } => false,
        },
        GoalEventKind::Commissioned { .. }
        | GoalEventKind::Resumed { .. }
        | GoalEventKind::Achieved { .. }
        | GoalEventKind::UserStopped { .. }
        | GoalEventKind::Superseded { .. }
        | GoalEventKind::SessionClosed { .. } => false,
    })
}

fn event_may_retire_queued_turn(event: &GoalEvent) -> bool {
    match event.kind() {
        GoalEventKind::UserStopped { .. }
        | GoalEventKind::Superseded { .. }
        | GoalEventKind::SessionClosed { .. } => true,
        GoalEventKind::Commissioned { .. }
        | GoalEventKind::Blocked { .. }
        | GoalEventKind::Resumed { .. }
        | GoalEventKind::Achieved { .. } => false,
    }
}

fn scheduler_failure_rejection(
    failure: GoalTransitionFailure,
) -> Result<GoalTurnContinuationOutcome, GoalCorruption> {
    match failure {
        GoalTransitionFailure::EventOrdinalExhausted => {
            Ok(GoalTurnContinuationOutcome::EventOrdinalExhausted)
        }
        GoalTransitionFailure::RequiresPursuing
        | GoalTransitionFailure::RequiresBlocked
        | GoalTransitionFailure::RequiresPursuingOrBlocked
        | GoalTransitionFailure::RequiresNoActiveGoal
        | GoalTransitionFailure::GenerationExhausted => Err(GoalCorruption::Inconsistent(
            "pursuing goal rejected scheduler failure blocking",
        )),
    }
}

/// Commissions a dispatched session's goal against a turn the dispatch accepted.
///
/// Repository-watch dispatch creates a session, its first input, and this goal
/// at one commit boundary, so a dispatched session is never durably visible
/// without the goal that states the authority it was dispatched under. The
/// session cannot perform this itself: only an existing goal admits a model
/// declaration, so a session with no goal has no transition to make.
///
/// The turn that carries the dispatch's tagged context is the generation's own
/// first turn rather than a predecessor of one. Minting a separate turn for the
/// statement would run the dispatched template a second time for one event, and
/// scheduling that turn first would make the session act on the statement
/// before the event it was dispatched for ever arrived, because a turn's
/// acceptance position is also its execution order.
///
/// Every branch here is a fail-closed assertion rather than a recoverable
/// outcome. The caller has just created this session inside this transaction,
/// so a rejected commission, an absent session, or an occupied identity is a
/// contradiction in durable state, not a race a dispatch could lose.
pub(crate) async fn insert_fresh_commissioned_goal(
    connection: &mut PgConnection,
    command: GoalUserCommand,
    principal: CommandPrincipal,
    accepted_input: AcceptedInputId,
    turn: TurnId,
) -> Result<(), GoalRepositoryError> {
    let issuer = crate::command_registry::issuer_columns(principal);
    let claimed = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at,
             issuer_kind, issuer_module)
         VALUES ($1, $2, $3, transaction_timestamp(), $4, $5)
         ON CONFLICT DO NOTHING",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(GOAL_KIND)
    .bind(STORAGE_VERSION)
    .bind(issuer.0)
    .bind(issuer.1)
    .execute(&mut *connection)
    .await?
    .rows_affected()
        == 1;
    if !claimed {
        return Err(GoalCorruption::Inconsistent("fresh dispatch goal command identity").into());
    }
    if !lock_session(connection, command.session()).await? {
        return Err(GoalCorruption::Missing("dispatched goal session").into());
    }
    let result = apply_unconditional_user_command(connection, &command).await?;
    let GoalCommandResult::Applied(event) = &result else {
        return Err(GoalCorruption::Inconsistent("rejected dispatch goal commission").into());
    };
    lock_scheduler(connection, command.session()).await?;
    insert_command(connection, &command, &result).await?;
    insert_event(connection, command.session(), event).await?;
    let goal = load_goal_from_connection(connection, command.session())
        .await?
        .ok_or(GoalCorruption::Missing("dispatched goal"))?;
    bind_goal_turn(
        connection,
        command.session(),
        goal.current().generation(),
        GoalTurnSource::UserEvent(event.ordinal()),
        accepted_input,
        turn,
    )
    .await
}

fn event_starts_pursuit(event: &GoalEvent) -> bool {
    match event.kind() {
        GoalEventKind::Commissioned { .. }
        | GoalEventKind::Resumed { .. }
        | GoalEventKind::Superseded { .. } => true,
        GoalEventKind::Blocked { .. }
        | GoalEventKind::Achieved { .. }
        | GoalEventKind::UserStopped { .. }
        | GoalEventKind::SessionClosed { .. } => false,
    }
}

fn pursuit_input<'a>(goal: &'a Goal, event: &'a GoalEvent) -> Result<&'a str, GoalCorruption> {
    match event.kind() {
        GoalEventKind::Resumed {
            guidance: Some(guidance),
            ..
        } => Ok(guidance.as_str()),
        GoalEventKind::Commissioned { .. }
        | GoalEventKind::Resumed { guidance: None, .. }
        | GoalEventKind::Superseded { .. } => Ok(goal.current().statement().as_str()),
        GoalEventKind::Blocked { .. }
        | GoalEventKind::Achieved { .. }
        | GoalEventKind::UserStopped { .. }
        | GoalEventKind::SessionClosed { .. } => Err(GoalCorruption::Inconsistent(
            "non-pursuing event scheduled a turn",
        )),
    }
}

fn select_definition_with_frozen_fallback(
    requested: ModelAlias,
    current: Option<FrozenAliasDefinition>,
    frozen: Option<(ModelAlias, FrozenAliasDefinition)>,
) -> Option<FrozenAliasDefinition> {
    current.or_else(|| {
        frozen
            .filter(|(alias, _)| *alias == requested)
            .map(|(_, definition)| definition)
    })
}

enum CurrentOriginConfiguration {
    Selected(OriginConfiguration),
    UnknownAlias(ModelAlias),
}

async fn current_origin_configuration<SelectDefinition>(
    connection: &mut PgConnection,
    session: SessionId,
    select_definition: SelectDefinition,
) -> Result<CurrentOriginConfiguration, GoalRepositoryError>
where
    SelectDefinition: FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
{
    let current = match crate::session::load_session_from_connection(connection, session).await {
        Ok(Some(session)) => session,
        Ok(None) => return Err(GoalCorruption::Missing("goal turn session").into()),
        Err(crate::session::SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(crate::session::SessionRepositoryError::Corruption(error)) => {
            return Err(GoalCorruption::Session(error).into());
        }
    };
    let defaults = current.current_configuration_defaults();
    let checked = defaults
        .derive_request(
            defaults.version(),
            ModelSelectionOverride::UseSessionDefault,
        )
        .map_err(|_| GoalCorruption::Inconsistent("current goal turn defaults version"))?;
    Ok(
        match OriginConfiguration::freeze(checked, select_definition) {
            Ok(configuration) => CurrentOriginConfiguration::Selected(configuration),
            Err(signalbox_domain::OriginModelSettingsError::UnknownAlias(error)) => {
                CurrentOriginConfiguration::UnknownAlias(error.alias())
            }
            Err(
                signalbox_domain::OriginModelSettingsError::MissingCapabilities { .. }
                | signalbox_domain::OriginModelSettingsError::Unsupported(_),
            ) => {
                return Err(
                    GoalCorruption::Inconsistent("current goal turn model settings").into(),
                );
            }
        },
    )
}

enum SystemTransition {
    Blocked {
        reason: GoalModelBlockedReasonKind,
        need: GoalNeed,
        provenance: GoalModelProvenance,
    },
    Achieved {
        report: GoalReport,
        provenance: GoalModelProvenance,
        verdict: FinishCheckVerdict,
    },
    ExecutionFailure {
        need: GoalNeed,
        unmonitored_need: Option<GoalNeed>,
        provenance: GoalSchedulerProvenance,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SystemTransitionAuthority {
    ModelDeclaration,
    SchedulerFailure,
}

impl SystemTransition {
    const fn authority(&self) -> SystemTransitionAuthority {
        match self {
            Self::Blocked { .. } | Self::Achieved { .. } => {
                SystemTransitionAuthority::ModelDeclaration
            }
            Self::ExecutionFailure { .. } => SystemTransitionAuthority::SchedulerFailure,
        }
    }

    const fn turn(&self) -> TurnId {
        match self {
            Self::Blocked { provenance, .. } | Self::Achieved { provenance, .. } => {
                provenance.turn()
            }
            Self::ExecutionFailure { provenance, .. } => provenance.turn(),
        }
    }
}

/// Applies a command whose caller requires no particular lineage head.
///
/// Passing no expectation is what makes the moved-lineage answer impossible,
/// so reaching it means the check answered a question nobody asked.
async fn apply_unconditional_user_command(
    connection: &mut PgConnection,
    command: &GoalUserCommand,
) -> Result<GoalCommandResult, GoalRepositoryError> {
    match apply_user_command(connection, command, None).await? {
        UserCommandApplication::Recorded(result) => Ok(result),
        UserCommandApplication::LineageMoved => Err(GoalCorruption::Inconsistent(
            "unconditional goal command reported a moved lineage",
        )
        .into()),
    }
}

/// What one user command did to the lineage the session lock revealed.
enum UserCommandApplication {
    /// The command produced this durable result.
    Recorded(GoalCommandResult),
    /// The caller's expected lineage head no longer held, so nothing applied.
    LineageMoved,
}

async fn apply_user_command(
    connection: &mut PgConnection,
    command: &GoalUserCommand,
    expected_head: Option<GoalEventOrdinal>,
) -> Result<UserCommandApplication, GoalRepositoryError> {
    let existing = load_goal_from_connection(connection, command.session()).await?;
    if let Some(expected) = expected_head
        && existing
            .as_ref()
            .and_then(|goal| goal.events().last())
            .map(GoalEvent::ordinal)
            != Some(expected)
    {
        return Ok(UserCommandApplication::LineageMoved);
    }
    let transitioned = match (command.action(), existing) {
        (GoalUserAction::Attach(statement), None) => Ok(Goal::commission(
            command.session(),
            statement.clone(),
            GoalUserProvenance::new(command.command_id()),
        )),
        (GoalUserAction::Attach(statement), Some(goal)) => goal.commission_successor(
            statement.clone(),
            GoalUserProvenance::new(command.command_id()),
        ),
        (GoalUserAction::Resume(_), None)
        | (GoalUserAction::Stop { .. }, None)
        | (GoalUserAction::Supersede(_), None) => {
            return Ok(UserCommandApplication::Recorded(
                GoalCommandResult::Rejected(GoalCommandRejection::GoalNotAttached),
            ));
        }
        (GoalUserAction::Resume(guidance), Some(goal)) => goal.resume(
            guidance.clone(),
            GoalUserProvenance::new(command.command_id()),
        ),
        (GoalUserAction::Stop { .. }, Some(goal)) => {
            goal.stop(GoalUserProvenance::new(command.command_id()))
        }
        (GoalUserAction::Supersede(statement), Some(goal)) => goal.supersede(
            statement.clone(),
            GoalUserProvenance::new(command.command_id()),
        ),
    };
    match transitioned {
        Ok(goal) => Ok(UserCommandApplication::Recorded(
            GoalCommandResult::Applied(latest_event(&goal)?),
        )),
        Err(error) => Ok(UserCommandApplication::Recorded(
            GoalCommandResult::Rejected(rejection_from_transition(error.failure())?),
        )),
    }
}

fn latest_event(goal: &Goal) -> Result<GoalEvent, GoalCorruption> {
    goal.events()
        .last()
        .cloned()
        .ok_or(GoalCorruption::Missing("latest event"))
}

fn rejection_from_transition(
    failure: GoalTransitionFailure,
) -> Result<GoalCommandRejection, GoalCorruption> {
    match failure {
        GoalTransitionFailure::RequiresBlocked => Ok(GoalCommandRejection::RequiresBlocked),
        GoalTransitionFailure::RequiresPursuingOrBlocked => {
            Ok(GoalCommandRejection::RequiresPursuingOrBlocked)
        }
        GoalTransitionFailure::RequiresNoActiveGoal => {
            Ok(GoalCommandRejection::GoalAlreadyAttached)
        }
        GoalTransitionFailure::GenerationExhausted => Ok(GoalCommandRejection::GenerationExhausted),
        GoalTransitionFailure::EventOrdinalExhausted => {
            Ok(GoalCommandRejection::EventOrdinalExhausted)
        }
        GoalTransitionFailure::RequiresPursuing => Err(GoalCorruption::Inconsistent(
            "user command produced a model-only rejection",
        )),
    }
}

async fn block_goal_continuation(
    mut transaction: sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
    goal: Goal,
    need: GoalNeed,
    predecessor: TurnId,
) -> Result<GoalTurnContinuationOutcome, GoalRepositoryError> {
    let transitioned =
        match goal.block_execution_failure(need, GoalSchedulerProvenance::new(predecessor)) {
            Ok(goal) => goal,
            Err(error) => {
                transaction.rollback().await?;
                return scheduler_failure_rejection(error.failure()).map_err(Into::into);
            }
        };
    let event = latest_event(&transitioned)?;
    insert_event(&mut transaction, session, &event).await?;
    commit(transaction).await?;
    Ok(GoalTurnContinuationOutcome::Blocked {
        event: event.ordinal(),
    })
}

/// Locks the session row `FOR NO KEY UPDATE`, returning whether it exists.
///
/// Every goal transition serializes on this row and nothing else, so a
/// transaction outside this module that must exclude goal transitions —
/// approval-judge completion, which rechecks the authority in force before
/// committing a decision — takes this lock, and takes it before any
/// `session_scheduler` lock, following the session-before-scheduler pair
/// order stated in `lock_inventory`.
pub(crate) async fn lock_session(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<bool, sqlx::Error> {
    Ok(
        sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::SUBMIT_INPUT_SESSION)
            .bind(session_id_to_uuid(session))
            .fetch_optional(&mut *connection)
            .await?
            .is_some(),
    )
}

pub(crate) async fn lock_scheduler(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<(), GoalRepositoryError> {
    let scheduler_exists =
        sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::SUBMIT_INPUT_SCHEDULER)
            .bind(session_id_to_uuid(session))
            .fetch_optional(&mut *connection)
            .await?
            .is_some();
    if !scheduler_exists {
        return Err(GoalCorruption::Missing("session scheduler row").into());
    }
    Ok(())
}

async fn commit(
    transaction: sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), GoalRepositoryError> {
    match transaction.commit().await {
        Ok(()) => Ok(()),
        Err(error) if commit_failure_is_ambiguous(&error) => {
            Err(GoalRepositoryError::CommitAmbiguous(error))
        }
        Err(error) => Err(error.into()),
    }
}

async fn existing_or_conflicting(
    connection: &mut PgConnection,
    command: &GoalUserCommand,
    kind: CommandKind,
) -> Result<GoalCommandHandlingOutcome, GoalRepositoryError> {
    match kind {
        CommandKind::Goal => {}
        CommandKind::CreateSession
        | CommandKind::CreateSessionFromImportedFrontier
        | CommandKind::ReplaceSessionDefaults
        | CommandKind::ReplaceSessionMetadata
        | CommandKind::SubmitInput
        | CommandKind::DecideToolRequest
        | CommandKind::OverrideDeniedToolRequest
        | CommandKind::ReviewWorkflow
        | CommandKind::ReviewOrchestration
        | CommandKind::CompactSession
        | CommandKind::UpdateSessionPlacement
        | CommandKind::RegisterWorkspace
        | CommandKind::MintGitRemote
        | CommandKind::WithdrawGitRemote
        | CommandKind::SessionLifecycle => {
            return Ok(GoalCommandHandlingOutcome::ConflictingReuse {
                command_id: command.command_id(),
            });
        }
    }
    let recorded = load_command_from_connection(connection, command.command_id())
        .await?
        .ok_or(GoalCorruption::Inconsistent("registry entry disappeared"))?;
    if recorded.command() == command {
        Ok(GoalCommandHandlingOutcome::Recorded(
            recorded.result().clone(),
        ))
    } else {
        Ok(GoalCommandHandlingOutcome::ConflictingReuse {
            command_id: command.command_id(),
        })
    }
}

async fn inspect_registry(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<CommandKind>, GoalRepositoryError> {
    command_registry::inspect(connection, command_id)
        .await
        .map_err(|error| match error {
            RegistryInspectionError::Database(error) => GoalRepositoryError::Database(error),
            RegistryInspectionError::Corruption(corruption) => {
                GoalRepositoryError::Corruption(registry_corruption(corruption))
            }
        })
}

fn registry_corruption(value: RegistryCorruption) -> GoalCorruption {
    match value {
        RegistryCorruption::UnsupportedKind(value) => GoalCorruption::Unsupported {
            field: "command kind",
            value,
        },
        RegistryCorruption::UnsupportedVersion(value) => GoalCorruption::Unsupported {
            field: "storage version",
            value: value.to_string(),
        },
        RegistryCorruption::MissingTypedRecord(_) => {
            GoalCorruption::Missing("typed command record")
        }
        RegistryCorruption::ConflictingTypedRecords => {
            GoalCorruption::Inconsistent("conflicting typed command records")
        }
    }
}

async fn insert_command(
    connection: &mut PgConnection,
    command: &GoalUserCommand,
    result: &GoalCommandResult,
) -> Result<(), GoalRepositoryError> {
    let operation = goal_operation_to_str(command.action());
    let statement = match command.action() {
        GoalUserAction::Attach(value) | GoalUserAction::Supersede(value) => Some(value.as_str()),
        GoalUserAction::Resume(_) | GoalUserAction::Stop { .. } => None,
    };
    let guidance = match command.action() {
        GoalUserAction::Resume(value) => value.as_ref().map(GoalGuidance::as_str),
        GoalUserAction::Attach(_) | GoalUserAction::Stop { .. } | GoalUserAction::Supersede(_) => {
            None
        }
    };
    let descendant_scope = match command.action() {
        GoalUserAction::Stop { descendant_scope } => {
            Some(descendant_scope_to_str(*descendant_scope))
        }
        GoalUserAction::Attach(_) | GoalUserAction::Resume(_) | GoalUserAction::Supersede(_) => {
            None
        }
    };
    let (result_kind, rejection_kind, result_ordinal) = match result {
        GoalCommandResult::Applied(event) => {
            ("applied", None, Some(Decimal::from(event.ordinal().get())))
        }
        GoalCommandResult::Rejected(reason) => (
            "rejected",
            Some(goal_command_rejection_to_str(*reason)),
            None,
        ),
    };
    sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, statement, guidance, descendant_scope, result_kind,
             rejection_kind, result_event_ordinal)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(GOAL_KIND)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(command.session()))
    .bind(operation)
    .bind(statement)
    .bind(guidance)
    .bind(descendant_scope)
    .bind(result_kind)
    .bind(rejection_kind)
    .bind(result_ordinal)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// Appends the terminal goal event one session closure owes its generation.
///
/// The closure lives in the lifecycle store, but the event is a goal-lineage
/// write, so its encoding stays here beside every other goal event's.
pub(crate) async fn insert_event_for_session_closure(
    connection: &mut PgConnection,
    session: SessionId,
    event: &GoalEvent,
) -> Result<(), GoalRepositoryError> {
    insert_event(connection, session, event).await?;
    // A closure retires its queued turn through the same committed path a stop
    // or a supersession uses; otherwise the turn stays live beneath a terminal
    // session.
    retire_ineligible_queued_goal_turn(connection, session).await?;
    Ok(())
}

async fn insert_event(
    connection: &mut PgConnection,
    session: SessionId,
    event: &GoalEvent,
) -> Result<(), GoalRepositoryError> {
    let encoded = EncodedEvent::from_event(event);
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, statement,
             blocked_reason, need, guidance, report, user_command_id,
             model_turn_id, model_tool_request_id, scheduler_turn_id,
             session_outcome_kind, closure_actor_kind, closure_actor_module,
             closure_actor_turn_id, closure_actor_tool_request_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                 $14, $15, $16, $17, $18)",
    )
    .bind(session_id_to_uuid(session))
    .bind(Decimal::from(event.ordinal().get()))
    .bind(Decimal::from(event.generation().get()))
    .bind(encoded.kind)
    .bind(encoded.statement)
    .bind(encoded.blocked_reason)
    .bind(encoded.need)
    .bind(encoded.guidance)
    .bind(encoded.report)
    .bind(encoded.user_command)
    .bind(encoded.model_turn)
    .bind(encoded.model_tool_request)
    .bind(encoded.scheduler_turn)
    .bind(encoded.session_outcome)
    .bind(encoded.closure_actor)
    .bind(encoded.closure_actor_module)
    .bind(encoded.closure_actor_turn)
    .bind(encoded.closure_actor_request)
    .execute(&mut *connection)
    .await?;
    outbox::append(
        connection,
        OutboxEvent::GoalChanged {
            session,
            event_ordinal: event.ordinal().get(),
        },
    )
    .await?;
    Ok(())
}

struct EncodedEvent<'a> {
    kind: &'static str,
    statement: Option<&'a str>,
    blocked_reason: Option<&'static str>,
    need: Option<&'a str>,
    guidance: Option<&'a str>,
    report: Option<&'a str>,
    user_command: Option<Uuid>,
    model_turn: Option<Uuid>,
    model_tool_request: Option<Uuid>,
    scheduler_turn: Option<Uuid>,
    session_outcome: Option<&'static str>,
    closure_actor: Option<&'static str>,
    closure_actor_module: Option<&'static str>,
    closure_actor_turn: Option<Uuid>,
    closure_actor_request: Option<Uuid>,
}

impl<'a> EncodedEvent<'a> {
    fn from_event(event: &'a GoalEvent) -> Self {
        let mut encoded = Self {
            kind: goal_event_kind_to_str(event.kind()),
            statement: None,
            blocked_reason: None,
            need: None,
            guidance: None,
            report: None,
            user_command: None,
            model_turn: None,
            model_tool_request: None,
            scheduler_turn: None,
            session_outcome: None,
            closure_actor: None,
            closure_actor_module: None,
            closure_actor_turn: None,
            closure_actor_request: None,
        };
        match event.kind() {
            GoalEventKind::Commissioned {
                statement,
                provenance,
            } => {
                encoded.statement = Some(statement.as_str());
                encoded.user_command = Some(durable_command_id_to_uuid(provenance.command()));
            }
            GoalEventKind::Blocked { block, need } => {
                encoded.blocked_reason = Some(goal_blocked_reason_to_str(block.reason_kind()));
                encoded.need = Some(need.as_str());
                match block {
                    GoalBlockProvenance::Model { provenance, .. }
                    | GoalBlockProvenance::FinishCheck { provenance } => {
                        encoded.model_turn = Some(turn_id_to_uuid(provenance.turn()));
                        encoded.model_tool_request =
                            Some(tool_request_id_to_uuid(provenance.tool_request()));
                    }
                    GoalBlockProvenance::ExecutionFailure { provenance } => {
                        encoded.scheduler_turn = Some(turn_id_to_uuid(provenance.turn()));
                    }
                }
            }
            GoalEventKind::Resumed {
                guidance,
                provenance,
            } => {
                encoded.guidance = guidance.as_ref().map(GoalGuidance::as_str);
                encoded.user_command = Some(durable_command_id_to_uuid(provenance.command()));
            }
            GoalEventKind::Achieved { report, provenance } => {
                encoded.report = Some(report.as_str());
                encoded.model_turn = Some(turn_id_to_uuid(provenance.turn()));
                encoded.model_tool_request =
                    Some(tool_request_id_to_uuid(provenance.tool_request()));
            }
            GoalEventKind::UserStopped { provenance } => {
                encoded.user_command = Some(durable_command_id_to_uuid(provenance.command()));
            }
            GoalEventKind::Superseded {
                replacement_statement,
                provenance,
            } => {
                encoded.statement = Some(replacement_statement.as_str());
                encoded.user_command = Some(durable_command_id_to_uuid(provenance.command()));
            }
            GoalEventKind::SessionClosed {
                outcome,
                provenance,
            } => {
                encoded.session_outcome = Some(session_closure_outcome_to_str(*outcome));
                encoded.closure_actor = Some(lifecycle_actor_to_str(*provenance));
                match provenance {
                    LifecycleActor::Core {
                        agency: CoreAgency::Model { turn },
                    } => encoded.closure_actor_turn = Some(turn_id_to_uuid(*turn)),
                    LifecycleActor::Core {
                        agency: CoreAgency::Tool { request },
                    } => {
                        encoded.closure_actor_request = Some(tool_request_id_to_uuid(*request));
                    }
                    LifecycleActor::Module { module } => {
                        encoded.closure_actor_module = Some(dispatching_module_to_str(*module));
                    }
                    LifecycleActor::Core {
                        agency: CoreAgency::Daemon,
                    }
                    | LifecycleActor::Operator
                    | LifecycleActor::Watchdog => {}
                }
            }
        }
        encoded
    }
}

pub(crate) async fn load_goal_from_connection(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<Goal>, GoalRepositoryError> {
    let rows = sqlx::query(
        "SELECT event.event_ordinal, event.generation, event.event_kind,
                event.statement, event.blocked_reason,
                COALESCE(arm.need, event.need) AS need,
                event.guidance, event.report, event.user_command_id,
                model_turn_id, model_tool_request_id, scheduler_turn_id,
                session_outcome_kind, closure_actor_kind, closure_actor_module,
                closure_actor_turn_id, closure_actor_tool_request_id
           FROM goal_event AS event
           LEFT JOIN goal_execution_failure_resumption_arm AS arm
             ON arm.session_id = event.session_id
            AND arm.event_ordinal = event.event_ordinal
          WHERE event.session_id = $1
          ORDER BY event.event_ordinal",
    )
    .bind(session_id_to_uuid(session))
    .fetch_all(&mut *connection)
    .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let events = rows
        .iter()
        .map(decode_event)
        .collect::<Result<Vec<_>, _>>()?;
    GoalReconstitutionInput::new(session, events)
        .reconstitute()
        .map(Some)
        .map_err(|error| GoalCorruption::Domain(error.failure()).into())
}

/// Rebuilds the closure outcome one settled generation recorded.
fn decode_session_closure_outcome(value: String) -> Result<SessionClosureOutcome, GoalCorruption> {
    session_closure_outcome_from_str(&value).ok_or(GoalCorruption::Unsupported {
        field: "session closure outcome",
        value,
    })
}

/// Rebuilds the actor classification, and the exact agency behind a core closure.
///
/// The classification and the agency are one value, so a stored row that
/// carries a turn identity under an operator classification is corrupt rather
/// than silently reclassified.
fn decode_closure_actor(
    kind: String,
    module: Option<String>,
    model_turn: Option<Uuid>,
    model_tool_request: Option<Uuid>,
) -> Result<LifecycleActor, GoalCorruption> {
    match (kind.as_str(), module, model_turn, model_tool_request) {
        ("core", None, None, None) => Ok(LifecycleActor::Core {
            agency: CoreAgency::Daemon,
        }),
        ("core", None, Some(turn), None) => Ok(LifecycleActor::Core {
            agency: CoreAgency::Model {
                turn: turn_id_from_uuid(turn),
            },
        }),
        ("core", None, None, Some(request)) => Ok(LifecycleActor::Core {
            agency: CoreAgency::Tool {
                request: tool_request_id_from_uuid(request),
            },
        }),
        ("operator", None, None, None) => Ok(LifecycleActor::Operator),
        ("watchdog", None, None, None) => Ok(LifecycleActor::Watchdog),
        ("module", Some(module), None, None) => dispatching_module_from_str(&module)
            .map(|module| LifecycleActor::Module { module })
            .ok_or(GoalCorruption::Unsupported {
                field: "session closure module",
                value: module,
            }),
        ("core" | "operator" | "watchdog" | "module", _, _, _) => Err(
            GoalCorruption::Inconsistent("session closure actor provenance"),
        ),
        _ => Err(GoalCorruption::Unsupported {
            field: "session closure actor",
            value: kind,
        }),
    }
}

fn decode_event(row: &sqlx::postgres::PgRow) -> Result<GoalEvent, GoalCorruption> {
    let ordinal = positive(column(row, "event_ordinal")?)?;
    let generation = positive(column(row, "generation")?)?;
    let kind: String = column(row, "event_kind")?;
    let statement: Option<String> = column(row, "statement")?;
    let blocked_reason: Option<String> = column(row, "blocked_reason")?;
    let need: Option<String> = column(row, "need")?;
    let guidance: Option<String> = column(row, "guidance")?;
    let report: Option<String> = column(row, "report")?;
    let user_command: Option<Uuid> = column(row, "user_command_id")?;
    let model_turn: Option<Uuid> = column(row, "model_turn_id")?;
    let model_tool_request: Option<Uuid> = column(row, "model_tool_request_id")?;
    let scheduler_turn: Option<Uuid> = column(row, "scheduler_turn_id")?;
    let session_outcome: Option<String> = column(row, "session_outcome_kind")?;
    let closure_actor: Option<String> = column(row, "closure_actor_kind")?;
    let closure_actor_module: Option<String> = column(row, "closure_actor_module")?;
    let closure_actor_turn: Option<Uuid> = column(row, "closure_actor_turn_id")?;
    let closure_actor_request: Option<Uuid> = column(row, "closure_actor_tool_request_id")?;
    let discriminator =
        goal_event_kind_from_str(&kind).ok_or_else(|| GoalCorruption::Unsupported {
            field: "event kind",
            value: kind.clone(),
        })?;
    let kind = match discriminator {
        GoalEventDiscriminator::Commissioned => GoalEventKind::Commissioned {
            statement: goal_statement(required(statement, "commission statement")?)?,
            provenance: GoalUserProvenance::new(
                durable_command_id_from_uuid(required(user_command, "commission command")?)
                    .map_err(GoalCorruption::InvalidCommandId)?,
            ),
        },
        GoalEventDiscriminator::Blocked => {
            let reason = required(blocked_reason, "blocked reason")?;
            let reason_kind = goal_blocked_reason_from_str(&reason).ok_or_else(|| {
                GoalCorruption::Unsupported {
                    field: "blocked reason",
                    value: reason.clone(),
                }
            })?;
            let block = match reason_kind {
                GoalBlockedReasonKind::UserInputRequired
                | GoalBlockedReasonKind::ExternalChangeRequired
                | GoalBlockedReasonKind::AuthorizationRequired => GoalBlockProvenance::Model {
                    reason: goal_model_blocked_reason_from_str(&reason).ok_or_else(|| {
                        GoalCorruption::Unsupported {
                            field: "model blocked reason",
                            value: reason.clone(),
                        }
                    })?,
                    provenance: GoalModelProvenance::new(
                        turn_id_from_uuid(required(model_turn, "blocked model turn")?),
                        tool_request_id_from_uuid(required(
                            model_tool_request,
                            "blocked model tool request",
                        )?),
                    ),
                },
                GoalBlockedReasonKind::FinishCheckFailed => GoalBlockProvenance::FinishCheck {
                    provenance: GoalModelProvenance::new(
                        turn_id_from_uuid(required(model_turn, "finish check model turn")?),
                        tool_request_id_from_uuid(required(
                            model_tool_request,
                            "finish check model tool request",
                        )?),
                    ),
                },
                GoalBlockedReasonKind::ExecutionFailure => GoalBlockProvenance::ExecutionFailure {
                    provenance: GoalSchedulerProvenance::new(turn_id_from_uuid(required(
                        scheduler_turn,
                        "failed scheduler turn",
                    )?)),
                },
            };
            GoalEventKind::Blocked {
                block,
                need: goal_need(required(need, "blocked need")?)?,
            }
        }
        GoalEventDiscriminator::Resumed => GoalEventKind::Resumed {
            guidance: guidance.map(goal_guidance).transpose()?,
            provenance: GoalUserProvenance::new(
                durable_command_id_from_uuid(required(user_command, "resume command")?)
                    .map_err(GoalCorruption::InvalidCommandId)?,
            ),
        },
        GoalEventDiscriminator::Achieved => GoalEventKind::Achieved {
            report: goal_report(required(report, "achievement report")?)?,
            provenance: GoalModelProvenance::new(
                turn_id_from_uuid(required(model_turn, "achievement model turn")?),
                tool_request_id_from_uuid(required(
                    model_tool_request,
                    "achievement model tool request",
                )?),
            ),
        },
        GoalEventDiscriminator::UserStopped => GoalEventKind::UserStopped {
            provenance: GoalUserProvenance::new(
                durable_command_id_from_uuid(required(user_command, "stop command")?)
                    .map_err(GoalCorruption::InvalidCommandId)?,
            ),
        },
        GoalEventDiscriminator::Superseded => GoalEventKind::Superseded {
            replacement_statement: goal_statement(required(statement, "replacement statement")?)?,
            provenance: GoalUserProvenance::new(
                durable_command_id_from_uuid(required(user_command, "supersede command")?)
                    .map_err(GoalCorruption::InvalidCommandId)?,
            ),
        },
        GoalEventDiscriminator::SessionClosed => GoalEventKind::SessionClosed {
            outcome: decode_session_closure_outcome(required(
                session_outcome,
                "session closure outcome",
            )?)?,
            provenance: decode_closure_actor(
                required(closure_actor, "session closure actor")?,
                closure_actor_module,
                closure_actor_turn,
                closure_actor_request,
            )?,
        },
    };
    Ok(GoalEvent::from_stored_parts(
        GoalEventOrdinal::new(ordinal),
        GoalGeneration::new(generation),
        kind,
    ))
}

async fn load_command_from_connection(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<ReconstitutedGoalCommand>, GoalRepositoryError> {
    let row = sqlx::query(
        "SELECT session_id, operation_kind, statement, guidance, descendant_scope,
                result_kind, rejection_kind, result_event_ordinal
           FROM goal_command
          WHERE command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let session_uuid: Uuid = column(&row, "session_id")?;
    let session = signalbox_domain::SessionId::from_uuid(session_uuid);
    let operation: String = column(&row, "operation_kind")?;
    let statement: Option<String> = column(&row, "statement")?;
    let guidance: Option<String> = column(&row, "guidance")?;
    let descendant_scope: Option<String> = column(&row, "descendant_scope")?;
    let action = action_from_stored(&operation, statement, guidance, descendant_scope)?;
    let command = GoalUserCommand::new(command_id, session, action);
    let result_kind: String = column(&row, "result_kind")?;
    let rejection: Option<String> = column(&row, "rejection_kind")?;
    let result_ordinal: Option<Decimal> = column(&row, "result_event_ordinal")?;
    let result = match result_kind.as_str() {
        "applied" => {
            let ordinal = positive(required(result_ordinal, "command result event ordinal")?)?;
            let goal = load_goal_from_connection(connection, session)
                .await?
                .ok_or(GoalCorruption::Missing("command result goal"))?;
            let event = goal
                .events()
                .iter()
                .find(|event| event.ordinal() == GoalEventOrdinal::new(ordinal))
                .cloned()
                .ok_or(GoalCorruption::Missing("command result event"))?;
            GoalCommandResult::Applied(event)
        }
        "rejected" => {
            let rejection = required(rejection, "command rejection")?;
            GoalCommandResult::Rejected(goal_command_rejection_from_str(&rejection).ok_or(
                GoalCorruption::Unsupported {
                    field: "command rejection",
                    value: rejection,
                },
            )?)
        }
        _ => {
            return Err(GoalCorruption::Unsupported {
                field: "command result kind",
                value: result_kind,
            }
            .into());
        }
    };
    Ok(Some(ReconstitutedGoalCommand::new(command, result)))
}

fn positive(value: Decimal) -> Result<NonZeroU64, GoalCorruption> {
    let value = positive_u64_from_numeric(value).map_err(GoalCorruption::InvalidOrdinal)?;
    NonZeroU64::new(value).ok_or(GoalCorruption::InvalidOrdinal(
        PositiveOrdinalMappingError::NonPositive,
    ))
}

fn required<T>(value: Option<T>, field: &'static str) -> Result<T, GoalCorruption> {
    value.ok_or(GoalCorruption::Missing(field))
}

fn goal_statement(value: String) -> Result<GoalStatement, GoalCorruption> {
    GoalStatement::try_new(value).map_err(GoalCorruption::InvalidText)
}

fn goal_need(value: String) -> Result<GoalNeed, GoalCorruption> {
    GoalNeed::try_new(value).map_err(GoalCorruption::InvalidText)
}

fn goal_guidance(value: String) -> Result<GoalGuidance, GoalCorruption> {
    GoalGuidance::try_new(value).map_err(GoalCorruption::InvalidText)
}

fn goal_report(value: String) -> Result<GoalReport, GoalCorruption> {
    GoalReport::try_new(value).map_err(GoalCorruption::InvalidText)
}

fn action_from_stored(
    operation: &str,
    statement: Option<String>,
    guidance: Option<String>,
    descendant_scope: Option<String>,
) -> Result<GoalUserAction, GoalCorruption> {
    let operation =
        goal_operation_from_str(operation).ok_or_else(|| GoalCorruption::Unsupported {
            field: "command operation",
            value: operation.to_owned(),
        })?;
    match operation {
        GoalOperationKind::Attach => Ok(GoalUserAction::Attach(goal_statement(required(
            statement,
            "attach statement",
        )?)?)),
        GoalOperationKind::Resume => Ok(GoalUserAction::Resume(
            guidance.map(goal_guidance).transpose()?,
        )),
        GoalOperationKind::Stop => Ok(GoalUserAction::Stop {
            descendant_scope: descendant_scope_from_str(&required(
                descendant_scope,
                "stop descendant scope",
            )?)?,
        }),
        GoalOperationKind::Supersede => Ok(GoalUserAction::Supersede(goal_statement(required(
            statement,
            "supersede statement",
        )?)?)),
    }
}

const fn descendant_scope_to_str(value: DescendantTerminationScope) -> &'static str {
    match value {
        DescendantTerminationScope::ParentAlone => "parent_alone",
        DescendantTerminationScope::ParentAndDescendants => "parent_and_descendants",
    }
}

fn descendant_scope_from_str(value: &str) -> Result<DescendantTerminationScope, GoalCorruption> {
    match value {
        "parent_alone" => Ok(DescendantTerminationScope::ParentAlone),
        "parent_and_descendants" => Ok(DescendantTerminationScope::ParentAndDescendants),
        value => Err(GoalCorruption::Unsupported {
            field: "stop descendant scope",
            value: value.to_owned(),
        }),
    }
}

fn column<'row, T>(
    row: &'row sqlx::postgres::PgRow,
    field: &'static str,
) -> Result<T, GoalCorruption>
where
    T: sqlx::Decode<'row, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get(field)
        .map_err(|_| GoalCorruption::Column(field))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inv048_scheduler_event_ordinal_exhaustion_is_a_typed_continuation_outcome() {
        let outcome = scheduler_failure_rejection(GoalTransitionFailure::EventOrdinalExhausted)
            .expect("event ordinal exhaustion is typed, not corruption");

        assert_eq!(outcome, GoalTurnContinuationOutcome::EventOrdinalExhausted);
    }

    #[test]
    fn successful_continuation_reuses_frozen_alias_when_catalog_entry_is_absent() {
        let alias = ModelAlias::from_uuid(Uuid::from_u128(0xa11));
        let frozen = FrozenAliasDefinition::selecting(
            signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(0xa12)),
        );

        assert_eq!(
            select_definition_with_frozen_fallback(alias, None, Some((alias, frozen))),
            Some(frozen)
        );
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn goal_corruption_forwards_its_typed_session_source() {
        let session = SessionCorruption::Missing("current defaults");
        let corruption = GoalCorruption::Session(session.clone());

        assert_eq!(
            corruption.source().map(ToString::to_string),
            Some(session.to_string())
        );
    }

    #[test]
    fn changed_unknown_alias_does_not_reuse_an_unrelated_frozen_definition() {
        let requested = ModelAlias::from_uuid(Uuid::from_u128(0xa21));
        let prior = ModelAlias::from_uuid(Uuid::from_u128(0xa22));
        let frozen = FrozenAliasDefinition::selecting(
            signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(0xa23)),
        );

        assert_eq!(
            select_definition_with_frozen_fallback(requested, None, Some((prior, frozen))),
            None
        );
    }
}
