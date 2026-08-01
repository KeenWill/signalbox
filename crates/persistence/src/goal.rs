//! PostgreSQL storage for session-scoped commissioned goals.
//!
//! The event stream is authoritative. Loads decode every durable event and
//! replay it through the domain aggregate; no mutable current-state row exists.

use std::{error::Error, fmt, num::NonZeroU64};

use rust_decimal::Decimal;
use signalbox_domain::{
    DurableCommandId, FrozenAliasDefinition, Goal, GoalBlockProvenance, GoalBlockedReasonKind,
    GoalCommandRejection, GoalCommandResult, GoalEvent, GoalEventKind, GoalEventOrdinal,
    GoalGeneration, GoalGuidance, GoalModelBlockedReasonKind, GoalModelProvenance, GoalNeed,
    GoalReconstitutionFailure, GoalReconstitutionInput, GoalReport, GoalSchedulerProvenance,
    GoalState, GoalStatement, GoalTextError, GoalTransitionError, GoalTransitionFailure,
    GoalTurnSource, GoalUserAction, GoalUserCommand, GoalUserProvenance, ModelAlias,
    ModelSelectionOverride, OriginConfiguration, ReconstitutedGoalCommand, SessionId, TurnId,
};
use sqlx::{PgConnection, PgPool, Row, types::Uuid};

use crate::{
    command_registry::{self, CommandKind, GOAL_KIND, RegistryCorruption, RegistryInspectionError},
    commit_failure_is_ambiguous,
    goal_turn::{
        GoalTurnCandidates, GoalTurnContinuationOutcome, continuation_exists, goal_turn_generation,
        insert_goal_turn,
    },
    mapping::{
        DurableCommandIdMappingError, PositiveOrdinalMappingError, durable_command_id_from_uuid,
        durable_command_id_to_uuid, positive_u64_from_numeric, session_id_to_uuid,
        tool_request_id_from_uuid, tool_request_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid,
    },
};

const STORAGE_VERSION: i16 = 1;

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
}

/// Result of a scheduler- or model-provenance transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalTransitionOutcome {
    /// The transition appended this event.
    Applied(GoalEvent),
    /// The session has no attached goal.
    GoalNotAttached,
    /// The current state rejected the requested transition.
    Rejected(GoalTransitionError),
    /// Scheduler provenance did not name a turn in the current goal generation.
    NotCurrentGoalTurn,
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
    Column(String),
    /// A positive stored ordinal cannot map to the domain.
    InvalidOrdinal(PositiveOrdinalMappingError),
    /// Stored bounded text cannot map to the domain.
    InvalidText(GoalTextError),
    /// Stored user-command provenance uses a sentinel identity.
    InvalidCommandId(DurableCommandIdMappingError),
    /// The complete history failed domain-owned replay.
    Domain(GoalReconstitutionFailure),
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
        }
    }
}

impl Error for GoalCorruption {}

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
        let command_id = command.command_id();
        let mut transaction = self.pool.begin().await?;
        if let Some(kind) = inspect_registry(&mut transaction, command_id).await? {
            let outcome = existing_or_conflicting(&mut transaction, &command, kind).await?;
            transaction.rollback().await?;
            return Ok(outcome);
        }

        let claimed = sqlx::query(
            "INSERT INTO durable_command
                (command_id, command_kind, storage_version, claimed_at)
             VALUES ($1, $2, $3, transaction_timestamp())
             ON CONFLICT DO NOTHING",
        )
        .bind(durable_command_id_to_uuid(command_id))
        .bind(GOAL_KIND)
        .bind(STORAGE_VERSION)
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

        let session_exists = lock_session(&mut transaction, command.session()).await?;
        let result = if !session_exists {
            GoalCommandResult::Rejected(GoalCommandRejection::SessionNotFound)
        } else {
            apply_user_command(&mut transaction, &command).await?
        };
        insert_command(&mut transaction, &command, &result).await?;
        if let GoalCommandResult::Applied(event) = &result {
            insert_event(&mut transaction, command.session(), event).await?;
            if event_starts_pursuit(event) {
                let candidates = candidates.ok_or(GoalCorruption::Missing(
                    "turn candidates for pursuing command",
                ))?;
                let configuration = current_origin_configuration(
                    &mut transaction,
                    command.session(),
                    select_definition,
                )
                .await?;
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
                    candidates,
                )
                .await?;
            }
        }
        commit(transaction).await?;
        Ok(GoalCommandHandlingOutcome::Recorded(result))
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
            Some(_) => Err(GoalRepositoryError::DifferentCommandKind { command_id }),
        }
    }

    /// Loads and domain-replays a session's full goal lineage.
    pub async fn load_goal(&self, session: SessionId) -> Result<Option<Goal>, GoalRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        load_goal_from_connection(&mut connection, session).await
    }

    /// Queues one successor after a successfully completed current goal turn.
    pub async fn continue_after_success<SelectDefinition>(
        &self,
        session: SessionId,
        predecessor: TurnId,
        candidates: GoalTurnCandidates,
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
        if !matches!(goal.current().state(), GoalState::Pursuing) {
            transaction.rollback().await?;
            return Ok(GoalTurnContinuationOutcome::NotPursuing);
        }
        let Some(generation) = goal_turn_generation(&mut transaction, session, predecessor).await?
        else {
            transaction.rollback().await?;
            return Ok(GoalTurnContinuationOutcome::NotCurrentGoalTurn);
        };
        if generation != goal.current().generation() {
            transaction.rollback().await?;
            return Ok(GoalTurnContinuationOutcome::NotCurrentGoalTurn);
        }
        if continuation_exists(&mut transaction, session, predecessor).await? {
            transaction.rollback().await?;
            return Ok(GoalTurnContinuationOutcome::AlreadyScheduled);
        }
        let configuration =
            current_origin_configuration(&mut transaction, session, select_definition).await?;
        insert_goal_turn(
            &mut transaction,
            session,
            generation,
            GoalTurnSource::SuccessfulTurn(predecessor),
            goal.current().statement().as_str(),
            &configuration,
            candidates,
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

    /// Appends a model-declared achievement and final report reference.
    pub async fn declare_achieved(
        &self,
        session: SessionId,
        report: GoalReport,
        provenance: GoalModelProvenance,
    ) -> Result<GoalTransitionOutcome, GoalRepositoryError> {
        self.handle_system_transition(session, SystemTransition::Achieved { report, provenance })
            .await
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
            SystemTransition::ExecutionFailure { need, provenance },
        )
        .await
    }

    async fn handle_system_transition(
        &self,
        session: SessionId,
        transition: SystemTransition,
    ) -> Result<GoalTransitionOutcome, GoalRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        if !lock_session(&mut transaction, session).await? {
            transaction.rollback().await?;
            return Ok(GoalTransitionOutcome::GoalNotAttached);
        }
        let Some(goal) = load_goal_from_connection(&mut transaction, session).await? else {
            transaction.rollback().await?;
            return Ok(GoalTransitionOutcome::GoalNotAttached);
        };
        let generation = goal_turn_generation(&mut transaction, session, transition.turn()).await?;
        if generation != Some(goal.current().generation()) {
            transaction.rollback().await?;
            return Ok(GoalTransitionOutcome::NotCurrentGoalTurn);
        }
        let transitioned = match transition {
            SystemTransition::Blocked {
                reason,
                need,
                provenance,
            } => goal.declare_blocked(reason, need, provenance),
            SystemTransition::Achieved { report, provenance } => {
                goal.declare_achieved(report, provenance)
            }
            SystemTransition::ExecutionFailure { need, provenance } => {
                goal.block_execution_failure(need, provenance)
            }
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
        commit(transaction).await?;
        Ok(GoalTransitionOutcome::Applied(event))
    }
}

fn event_starts_pursuit(event: &GoalEvent) -> bool {
    matches!(
        event.kind(),
        GoalEventKind::Commissioned { .. }
            | GoalEventKind::Resumed { .. }
            | GoalEventKind::Superseded { .. }
    )
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
        | GoalEventKind::UserStopped { .. } => Err(GoalCorruption::Inconsistent(
            "non-pursuing event scheduled a turn",
        )),
    }
}

async fn current_origin_configuration<SelectDefinition>(
    connection: &mut PgConnection,
    session: SessionId,
    select_definition: SelectDefinition,
) -> Result<OriginConfiguration, GoalRepositoryError>
where
    SelectDefinition: FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
{
    let current = match crate::session::load_session_from_connection(connection, session).await {
        Ok(Some(session)) => session,
        Ok(None) => return Err(GoalCorruption::Missing("goal turn session").into()),
        Err(crate::session::SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(crate::session::SessionRepositoryError::Corruption(_)) => {
            return Err(GoalCorruption::Inconsistent("goal turn session configuration").into());
        }
    };
    let defaults = current.current_configuration_defaults();
    let checked = defaults
        .derive_request(
            defaults.version(),
            ModelSelectionOverride::UseSessionDefault,
        )
        .map_err(|_| GoalCorruption::Inconsistent("current goal turn defaults version"))?;
    OriginConfiguration::freeze(checked, select_definition)
        .map_err(|_| GoalCorruption::Inconsistent("current goal turn model alias").into())
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
    },
    ExecutionFailure {
        need: GoalNeed,
        provenance: GoalSchedulerProvenance,
    },
}

impl SystemTransition {
    const fn turn(&self) -> TurnId {
        match self {
            Self::Blocked { provenance, .. } | Self::Achieved { provenance, .. } => {
                provenance.turn()
            }
            Self::ExecutionFailure { provenance, .. } => provenance.turn(),
        }
    }
}

async fn apply_user_command(
    connection: &mut PgConnection,
    command: &GoalUserCommand,
) -> Result<GoalCommandResult, GoalRepositoryError> {
    let existing = load_goal_from_connection(connection, command.session()).await?;
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
        | (GoalUserAction::Stop, None)
        | (GoalUserAction::Supersede(_), None) => {
            return Ok(GoalCommandResult::Rejected(
                GoalCommandRejection::GoalNotAttached,
            ));
        }
        (GoalUserAction::Resume(guidance), Some(goal)) => goal.resume(
            guidance.clone(),
            GoalUserProvenance::new(command.command_id()),
        ),
        (GoalUserAction::Stop, Some(goal)) => {
            goal.stop(GoalUserProvenance::new(command.command_id()))
        }
        (GoalUserAction::Supersede(statement), Some(goal)) => goal.supersede(
            statement.clone(),
            GoalUserProvenance::new(command.command_id()),
        ),
    };
    match transitioned {
        Ok(goal) => Ok(GoalCommandResult::Applied(latest_event(&goal)?)),
        Err(error) => Ok(GoalCommandResult::Rejected(rejection_from_transition(
            error.failure(),
        )?)),
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

async fn lock_session(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT session_id FROM session WHERE session_id = $1 FOR UPDATE",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?
    .is_some())
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
    if kind != CommandKind::Goal {
        return Ok(GoalCommandHandlingOutcome::ConflictingReuse {
            command_id: command.command_id(),
        });
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
    let operation = operation_to_str(command.action());
    let statement = match command.action() {
        GoalUserAction::Attach(value) | GoalUserAction::Supersede(value) => Some(value.as_str()),
        GoalUserAction::Resume(_) | GoalUserAction::Stop => None,
    };
    let guidance = match command.action() {
        GoalUserAction::Resume(value) => value.as_ref().map(GoalGuidance::as_str),
        GoalUserAction::Attach(_) | GoalUserAction::Stop | GoalUserAction::Supersede(_) => None,
    };
    let (result_kind, rejection_kind, result_ordinal) = match result {
        GoalCommandResult::Applied(event) => {
            ("applied", None, Some(Decimal::from(event.ordinal().get())))
        }
        GoalCommandResult::Rejected(reason) => ("rejected", Some(rejection_to_str(*reason)), None),
    };
    sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, statement, guidance, result_kind,
             rejection_kind, result_event_ordinal)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(GOAL_KIND)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(command.session()))
    .bind(operation)
    .bind(statement)
    .bind(guidance)
    .bind(result_kind)
    .bind(rejection_kind)
    .bind(result_ordinal)
    .execute(&mut *connection)
    .await?;
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
             model_turn_id, model_tool_request_id, scheduler_turn_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
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
    .execute(&mut *connection)
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
}

impl<'a> EncodedEvent<'a> {
    fn from_event(event: &'a GoalEvent) -> Self {
        let mut encoded = Self {
            kind: event_kind_to_str(event.kind()),
            statement: None,
            blocked_reason: None,
            need: None,
            guidance: None,
            report: None,
            user_command: None,
            model_turn: None,
            model_tool_request: None,
            scheduler_turn: None,
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
                encoded.blocked_reason = Some(blocked_reason_to_str(block.reason_kind()));
                encoded.need = Some(need.as_str());
                match block {
                    GoalBlockProvenance::Model { provenance, .. } => {
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
        }
        encoded
    }
}

async fn load_goal_from_connection(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<Goal>, GoalRepositoryError> {
    let rows = sqlx::query(
        "SELECT event_ordinal, generation, event_kind, statement,
                blocked_reason, need, guidance, report, user_command_id,
                model_turn_id, model_tool_request_id, scheduler_turn_id
           FROM goal_event
          WHERE session_id = $1
          ORDER BY event_ordinal",
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

fn decode_event(row: &sqlx::postgres::PgRow) -> Result<GoalEvent, GoalCorruption> {
    let ordinal = positive(row.try_get("event_ordinal")?)?;
    let generation = positive(row.try_get("generation")?)?;
    let kind: String = row.try_get("event_kind")?;
    let statement: Option<String> = row.try_get("statement")?;
    let blocked_reason: Option<String> = row.try_get("blocked_reason")?;
    let need: Option<String> = row.try_get("need")?;
    let guidance: Option<String> = row.try_get("guidance")?;
    let report: Option<String> = row.try_get("report")?;
    let user_command: Option<Uuid> = row.try_get("user_command_id")?;
    let model_turn: Option<Uuid> = row.try_get("model_turn_id")?;
    let model_tool_request: Option<Uuid> = row.try_get("model_tool_request_id")?;
    let scheduler_turn: Option<Uuid> = row.try_get("scheduler_turn_id")?;
    let kind = match kind.as_str() {
        "commissioned" => GoalEventKind::Commissioned {
            statement: goal_statement(required(statement, "commission statement")?)?,
            provenance: GoalUserProvenance::new(
                durable_command_id_from_uuid(required(user_command, "commission command")?)
                    .map_err(GoalCorruption::InvalidCommandId)?,
            ),
        },
        "blocked" => {
            let reason = required(blocked_reason, "blocked reason")?;
            let block = match reason.as_str() {
                "user_input_required" | "external_change_required" | "authorization_required" => {
                    GoalBlockProvenance::Model {
                        reason: model_blocked_reason_from_str(&reason)?,
                        provenance: GoalModelProvenance::new(
                            turn_id_from_uuid(required(model_turn, "blocked model turn")?),
                            tool_request_id_from_uuid(required(
                                model_tool_request,
                                "blocked model tool request",
                            )?),
                        ),
                    }
                }
                "execution_failure" => GoalBlockProvenance::ExecutionFailure {
                    provenance: GoalSchedulerProvenance::new(turn_id_from_uuid(required(
                        scheduler_turn,
                        "failed scheduler turn",
                    )?)),
                },
                _ => {
                    return Err(GoalCorruption::Unsupported {
                        field: "blocked reason",
                        value: reason,
                    });
                }
            };
            GoalEventKind::Blocked {
                block,
                need: goal_need(required(need, "blocked need")?)?,
            }
        }
        "resumed" => GoalEventKind::Resumed {
            guidance: guidance.map(goal_guidance).transpose()?,
            provenance: GoalUserProvenance::new(
                durable_command_id_from_uuid(required(user_command, "resume command")?)
                    .map_err(GoalCorruption::InvalidCommandId)?,
            ),
        },
        "achieved" => GoalEventKind::Achieved {
            report: goal_report(required(report, "achievement report")?)?,
            provenance: GoalModelProvenance::new(
                turn_id_from_uuid(required(model_turn, "achievement model turn")?),
                tool_request_id_from_uuid(required(
                    model_tool_request,
                    "achievement model tool request",
                )?),
            ),
        },
        "user_stopped" => GoalEventKind::UserStopped {
            provenance: GoalUserProvenance::new(
                durable_command_id_from_uuid(required(user_command, "stop command")?)
                    .map_err(GoalCorruption::InvalidCommandId)?,
            ),
        },
        "superseded" => GoalEventKind::Superseded {
            replacement_statement: goal_statement(required(statement, "replacement statement")?)?,
            provenance: GoalUserProvenance::new(
                durable_command_id_from_uuid(required(user_command, "supersede command")?)
                    .map_err(GoalCorruption::InvalidCommandId)?,
            ),
        },
        _ => {
            return Err(GoalCorruption::Unsupported {
                field: "event kind",
                value: kind,
            });
        }
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
        "SELECT session_id, operation_kind, statement, guidance,
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
    let session_uuid: Uuid = row.try_get("session_id")?;
    let session = signalbox_domain::SessionId::from_uuid(session_uuid);
    let operation: String = row.try_get("operation_kind")?;
    let statement: Option<String> = row.try_get("statement")?;
    let guidance: Option<String> = row.try_get("guidance")?;
    let action = action_from_stored(&operation, statement, guidance)?;
    let command = GoalUserCommand::new(command_id, session, action);
    let result_kind: String = row.try_get("result_kind")?;
    let rejection: Option<String> = row.try_get("rejection_kind")?;
    let result_ordinal: Option<Decimal> = row.try_get("result_event_ordinal")?;
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
        "rejected" => GoalCommandResult::Rejected(rejection_from_str(&required(
            rejection,
            "command rejection",
        )?)?),
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

fn operation_to_str(value: &GoalUserAction) -> &'static str {
    match value {
        GoalUserAction::Attach(_) => "attach",
        GoalUserAction::Resume(_) => "resume",
        GoalUserAction::Stop => "stop",
        GoalUserAction::Supersede(_) => "supersede",
    }
}

fn action_from_stored(
    operation: &str,
    statement: Option<String>,
    guidance: Option<String>,
) -> Result<GoalUserAction, GoalCorruption> {
    match operation {
        "attach" => Ok(GoalUserAction::Attach(goal_statement(required(
            statement,
            "attach statement",
        )?)?)),
        "resume" => Ok(GoalUserAction::Resume(
            guidance.map(goal_guidance).transpose()?,
        )),
        "stop" => Ok(GoalUserAction::Stop),
        "supersede" => Ok(GoalUserAction::Supersede(goal_statement(required(
            statement,
            "supersede statement",
        )?)?)),
        _ => Err(GoalCorruption::Unsupported {
            field: "command operation",
            value: operation.to_owned(),
        }),
    }
}

fn event_kind_to_str(value: &GoalEventKind) -> &'static str {
    match value {
        GoalEventKind::Commissioned { .. } => "commissioned",
        GoalEventKind::Blocked { .. } => "blocked",
        GoalEventKind::Resumed { .. } => "resumed",
        GoalEventKind::Achieved { .. } => "achieved",
        GoalEventKind::UserStopped { .. } => "user_stopped",
        GoalEventKind::Superseded { .. } => "superseded",
    }
}

fn blocked_reason_to_str(value: GoalBlockedReasonKind) -> &'static str {
    match value {
        GoalBlockedReasonKind::UserInputRequired => "user_input_required",
        GoalBlockedReasonKind::ExternalChangeRequired => "external_change_required",
        GoalBlockedReasonKind::AuthorizationRequired => "authorization_required",
        GoalBlockedReasonKind::ExecutionFailure => "execution_failure",
    }
}

fn model_blocked_reason_from_str(
    value: &str,
) -> Result<GoalModelBlockedReasonKind, GoalCorruption> {
    match value {
        "user_input_required" => Ok(GoalModelBlockedReasonKind::UserInputRequired),
        "external_change_required" => Ok(GoalModelBlockedReasonKind::ExternalChangeRequired),
        "authorization_required" => Ok(GoalModelBlockedReasonKind::AuthorizationRequired),
        _ => Err(GoalCorruption::Unsupported {
            field: "model blocked reason",
            value: value.to_owned(),
        }),
    }
}

fn rejection_to_str(value: GoalCommandRejection) -> &'static str {
    match value {
        GoalCommandRejection::SessionNotFound => "session_not_found",
        GoalCommandRejection::GoalAlreadyAttached => "goal_already_attached",
        GoalCommandRejection::GoalNotAttached => "goal_not_attached",
        GoalCommandRejection::RequiresBlocked => "requires_blocked",
        GoalCommandRejection::RequiresPursuingOrBlocked => "requires_pursuing_or_blocked",
        GoalCommandRejection::GenerationExhausted => "generation_exhausted",
        GoalCommandRejection::EventOrdinalExhausted => "event_ordinal_exhausted",
    }
}

fn rejection_from_str(value: &str) -> Result<GoalCommandRejection, GoalCorruption> {
    match value {
        "session_not_found" => Ok(GoalCommandRejection::SessionNotFound),
        "goal_already_attached" => Ok(GoalCommandRejection::GoalAlreadyAttached),
        "goal_not_attached" => Ok(GoalCommandRejection::GoalNotAttached),
        "requires_blocked" => Ok(GoalCommandRejection::RequiresBlocked),
        "requires_pursuing_or_blocked" => Ok(GoalCommandRejection::RequiresPursuingOrBlocked),
        "generation_exhausted" => Ok(GoalCommandRejection::GenerationExhausted),
        "event_ordinal_exhausted" => Ok(GoalCommandRejection::EventOrdinalExhausted),
        _ => Err(GoalCorruption::Unsupported {
            field: "command rejection",
            value: value.to_owned(),
        }),
    }
}

impl From<sqlx::Error> for GoalCorruption {
    fn from(error: sqlx::Error) -> Self {
        GoalCorruption::Column(error.to_string())
    }
}
