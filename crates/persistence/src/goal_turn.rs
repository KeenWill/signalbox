//! Durable queued turns owned by a commissioned-goal generation.

use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, DirectModelSelection, FrozenAliasDefinition, FrozenModelSelection,
    GoalGeneration, GoalTurnSource, ModelAlias, ModelSelectionRequest, OriginConfiguration,
    SessionId, SessionInputPosition, TurnId, TurnTerminalCause,
};
use sqlx::{FromRow, PgConnection, types::Uuid};

use crate::{
    goal::{GoalCorruption, GoalRepositoryError},
    mapping::{
        accepted_input_id_to_uuid, dangerous_tool_auto_approval_to_str,
        defaults_version_to_numeric, input_position_from_numeric, input_position_to_numeric,
        session_id_to_uuid, turn_id_to_uuid, turn_terminal_cause_to_str,
    },
    model_settings_resolution,
    outbox::{self, OutboxEvent, TurnTerminalOutboxDisposition},
};

/// Fresh identities for one goal-owned accepted-input origin and turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GoalTurnInsertion {
    position: SessionInputPosition,
    candidates: GoalTurnCandidates,
}

impl GoalTurnInsertion {
    pub(crate) const fn new(
        position: SessionInputPosition,
        candidates: GoalTurnCandidates,
    ) -> Self {
        Self {
            position,
            candidates,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GoalTurnCandidates {
    accepted_input: AcceptedInputId,
    turn: TurnId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GoalTurnTerminalState {
    NotTerminal,
    Completed,
    Unsuccessful,
    Retired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GoalTurnAcceptancePosition {
    Available(SessionInputPosition),
    Exhausted { last: SessionInputPosition },
}

#[derive(FromRow)]
struct StoredGoalTurnTerminalState {
    state_kind: String,
    terminal_disposition_kind: Option<String>,
}

#[derive(FromRow)]
struct StoredGoalTurnFrozenModel {
    frozen_model_kind: String,
    frozen_direct_model_selection_id: Option<Uuid>,
    frozen_model_alias_id: Option<Uuid>,
    frozen_alias_selected_direct_id: Option<Uuid>,
}

/// Result of reconciling one goal turn after a daemon execution pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalTurnContinuationOutcome {
    /// A new goal-owned turn was durably queued.
    Scheduled {
        /// The queued successor turn.
        turn: TurnId,
    },
    /// The goal turn remains queued or active and requires no disposition.
    NotTerminal,
    /// An unsuccessfully terminalized goal turn durably blocked pursuit.
    Blocked {
        /// The appended blocked event ordinal.
        event: signalbox_domain::GoalEventOrdinal,
    },
    /// The current goal state is absent or scheduler-terminal.
    NotPursuing,
    /// The completed turn is not owned by the current goal generation.
    NotCurrentGoalTurn,
    /// The selected defaults name an alias with no available definition.
    UnknownModelAlias {
        /// The unavailable alias selected by the current defaults epoch.
        alias: ModelAlias,
    },
    /// The goal event ordinal cannot advance beyond `u64::MAX`.
    EventOrdinalExhausted,
    /// The session's accepted-input position cannot advance beyond `u64::MAX`.
    AcceptancePositionExhausted {
        /// The maximum durable position already occupied by the session.
        last: SessionInputPosition,
    },
    /// This predecessor already has its one durable goal successor.
    AlreadyScheduled,
}

impl GoalTurnCandidates {
    /// Binds independently minted accepted-input and turn candidates.
    pub const fn new(accepted_input: AcceptedInputId, turn: TurnId) -> Self {
        Self {
            accepted_input,
            turn,
        }
    }

    /// Returns the accepted-input candidate.
    pub const fn accepted_input(self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the turn candidate.
    pub const fn turn(self) -> TurnId {
        self.turn
    }
}

pub(crate) async fn next_goal_turn_acceptance_position(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<GoalTurnAcceptancePosition, GoalRepositoryError> {
    let previous = sqlx::query_scalar::<_, Decimal>(
        "SELECT acceptance_position
           FROM accepted_input
          WHERE session_id = $1
          ORDER BY acceptance_position DESC
          LIMIT 1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?
    .map(input_position_from_numeric)
    .transpose()
    .map_err(GoalCorruption::InvalidOrdinal)?;
    Ok(match previous {
        Some(last) => match last.checked_next() {
            Some(position) => GoalTurnAcceptancePosition::Available(position),
            None => GoalTurnAcceptancePosition::Exhausted { last },
        },
        None => GoalTurnAcceptancePosition::Available(SessionInputPosition::first()),
    })
}

pub(crate) async fn insert_goal_turn(
    connection: &mut PgConnection,
    session: SessionId,
    generation: GoalGeneration,
    source: GoalTurnSource,
    content: &str,
    configuration: &OriginConfiguration,
    insertion: GoalTurnInsertion,
) -> Result<(), GoalRepositoryError> {
    let position = insertion.position;
    let candidates = insertion.candidates;
    let requested = encode_selection(configuration.requested().model());
    let frozen = encode_frozen(configuration.effective().model());
    let accepted = accepted_input_id_to_uuid(candidates.accepted_input());
    let turn = turn_id_to_uuid(candidates.turn());
    let session_uuid = session_id_to_uuid(session);

    let scheduler_exists =
        sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::SUBMIT_INPUT_SCHEDULER)
            .bind(session_uuid)
            .fetch_optional(&mut *connection)
            .await?
            .is_some();
    if !scheduler_exists {
        return Err(GoalCorruption::Missing("session scheduler row").into());
    }

    sqlx::query(
        "INSERT INTO accepted_input
            (accepted_input_id, accepting_command_id, session_id,
             delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             acceptance_position, disposition_kind, origin_turn_id)
         VALUES
            ($1, NULL, $2, 'start_when_no_active_turn',
             NULL, $3, 'use_session_default', NULL, NULL, NULL,
             $4, 'origin_of', $5)",
    )
    .bind(accepted)
    .bind(session_uuid)
    .bind(defaults_version_to_numeric(
        configuration.session_defaults_version(),
    ))
    .bind(input_position_to_numeric(position))
    .bind(turn)
    .execute(&mut *connection)
    .await?;

    sqlx::query(
        "INSERT INTO accepted_input_content_part
            (accepted_input_id, position, part_kind, text_value)
         VALUES ($1, 0, 'text', $2)",
    )
    .bind(accepted)
    .bind(content)
    .execute(&mut *connection)
    .await?;

    sqlx::query(
        "INSERT INTO queued_input_origin
            (turn_id, accepted_input_id, session_id, acceptance_position,
             priority_kind, defaults_version, interrupt_predecessor_turn_id,
             requested_model_kind, requested_direct_model_selection_id,
             requested_model_alias_id, frozen_model_kind,
             frozen_direct_model_selection_id, frozen_model_alias_id,
             frozen_alias_selected_direct_id, model_parameters,
             known_provider_failure_retry, model_fallback,
             dangerous_tool_auto_approval)
         VALUES
            ($1, $2, $3, $4, 'ordinary', $5, NULL, $6, $7, $8,
             $9, $10, $11, $12, 'provider_defaults', 'disabled', 'disabled', $13)",
    )
    .bind(turn)
    .bind(accepted)
    .bind(session_uuid)
    .bind(input_position_to_numeric(position))
    .bind(defaults_version_to_numeric(
        configuration.session_defaults_version(),
    ))
    .bind(requested.kind)
    .bind(requested.direct)
    .bind(requested.alias)
    .bind(frozen.kind)
    .bind(frozen.direct)
    .bind(frozen.alias)
    .bind(frozen.alias_selected)
    .bind(dangerous_tool_auto_approval_to_str(
        configuration.effective().dangerous_tool_auto_approval(),
    ))
    .execute(&mut *connection)
    .await?;

    sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_accepted_input_id,
             acceptance_position, state_kind)
         VALUES ($1, $2, $3, $4, 'queued')",
    )
    .bind(turn)
    .bind(session_uuid)
    .bind(accepted)
    .bind(input_position_to_numeric(position))
    .execute(&mut *connection)
    .await?;

    let settings_event = model_settings_resolution::event_from_origin(
        candidates.accepted_input(),
        candidates.turn(),
        configuration,
    )
    .ok_or(GoalCorruption::Inconsistent(
        "goal turn model settings event",
    ))?;
    model_settings_resolution::persist(connection, session, &settings_event).await?;

    bind_goal_turn(
        connection,
        session,
        generation,
        source,
        candidates.accepted_input(),
        candidates.turn(),
    )
    .await?;

    outbox::append(
        connection,
        OutboxEvent::InputAccepted {
            session,
            accepted_input: candidates.accepted_input(),
            turn: candidates.turn(),
            acceptance_position: position,
        },
    )
    .await?;
    Ok(())
}

/// Records an existing queued turn as the goal turn of one generation.
///
/// A repository-watch dispatch submits its tagged context through its own
/// command before the goal it commissions exists, so the generation cannot mint
/// the turn that carries it. Binding writes the `goal_turn` row alone: the
/// accepted input, queued origin, lifecycle, and model-settings resolution the
/// turn already owns are exactly the ones the generation adopts, and writing
/// them again would fabricate a second history for a turn that has one.
///
/// No `InputAccepted` outbox event is appended here. The command that accepted
/// this turn already published one naming the same accepted input and turn, and
/// `input_accepted_outbox_event` is unique on each of them, so a second append
/// is a constraint violation rather than a duplicate a follower could absorb.
///
/// This writes the single row and asserts nothing beyond it, because the
/// deferred `goal_turn_shape` trigger decides the whole shape at commit. The
/// turn-minting path above reaches this function for the same row.
pub(crate) async fn bind_goal_turn(
    connection: &mut PgConnection,
    session: SessionId,
    generation: GoalGeneration,
    source: GoalTurnSource,
    accepted_input: AcceptedInputId,
    turn: TurnId,
) -> Result<(), GoalRepositoryError> {
    let (source_event, predecessor) = match source {
        GoalTurnSource::UserEvent(event) => (Some(Decimal::from(event.get())), None),
        GoalTurnSource::SuccessfulTurn(predecessor) => (None, Some(turn_id_to_uuid(predecessor))),
    };
    sqlx::query(
        "INSERT INTO goal_turn
            (session_id, goal_generation, turn_id, accepted_input_id,
             source_event_ordinal, predecessor_turn_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(session_id_to_uuid(session))
    .bind(Decimal::from(generation.get()))
    .bind(turn_id_to_uuid(turn))
    .bind(accepted_input_id_to_uuid(accepted_input))
    .bind(source_event)
    .bind(predecessor)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(crate) async fn goal_turn_frozen_alias_definition(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Option<(ModelAlias, FrozenAliasDefinition)>, GoalRepositoryError> {
    let stored = sqlx::query_as::<_, StoredGoalTurnFrozenModel>(
        "SELECT
            frozen_model_kind,
            frozen_direct_model_selection_id,
            frozen_model_alias_id,
            frozen_alias_selected_direct_id
           FROM queued_input_origin
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(GoalCorruption::Missing("goal turn frozen model"))?;

    match (
        stored.frozen_model_kind.as_str(),
        stored.frozen_direct_model_selection_id,
        stored.frozen_model_alias_id,
        stored.frozen_alias_selected_direct_id,
    ) {
        ("direct", Some(_), None, None) => Ok(None),
        ("frozen_alias", None, Some(alias), Some(selected)) => Ok(Some((
            ModelAlias::from_uuid(alias),
            FrozenAliasDefinition::selecting(DirectModelSelection::from_uuid(selected)),
        ))),
        ("direct" | "frozen_alias", _, _, _) => {
            Err(GoalCorruption::Inconsistent("goal turn frozen model").into())
        }
        (_, _, _, _) => Err(GoalCorruption::Unsupported {
            field: "goal turn frozen model kind",
            value: stored.frozen_model_kind,
        }
        .into()),
    }
}

pub(crate) async fn goal_turn_generation(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Option<GoalGeneration>, GoalRepositoryError> {
    let value = sqlx::query_scalar::<_, Decimal>(
        "SELECT goal_generation FROM goal_turn
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(value) = value else {
        return Ok(None);
    };
    let value =
        crate::mapping::positive_u64_from_numeric(value).map_err(GoalCorruption::InvalidOrdinal)?;
    let generation = std::num::NonZeroU64::new(value).ok_or(GoalCorruption::Inconsistent(
        "positive goal generation decoded as zero",
    ))?;
    Ok(Some(GoalGeneration::new(generation)))
}

pub(crate) async fn current_goal_turn(
    connection: &mut PgConnection,
    session: SessionId,
    generation: GoalGeneration,
) -> Result<Option<TurnId>, GoalRepositoryError> {
    let turn = sqlx::query_scalar::<_, Uuid>(
        "SELECT goal.turn_id
           FROM goal_turn AS goal
           JOIN accepted_input AS accepted
             ON accepted.accepted_input_id = goal.accepted_input_id
            AND accepted.session_id = goal.session_id
            AND accepted.origin_turn_id = goal.turn_id
          WHERE goal.session_id = $1 AND goal.goal_generation = $2
          ORDER BY accepted.acceptance_position DESC
          LIMIT 1",
    )
    .bind(session_id_to_uuid(session))
    .bind(Decimal::from(generation.get()))
    .fetch_optional(&mut *connection)
    .await?;
    Ok(turn.map(crate::mapping::turn_id_from_uuid))
}

pub(crate) async fn goal_turn_terminal_state(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<GoalTurnTerminalState, GoalRepositoryError> {
    let stored = sqlx::query_as::<_, StoredGoalTurnTerminalState>(
        "SELECT lifecycle.state_kind, lifecycle.terminal_disposition_kind
           FROM goal_turn AS goal
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.session_id = goal.session_id
            AND lifecycle.turn_id = goal.turn_id
          WHERE goal.session_id = $1 AND goal.turn_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(GoalCorruption::Missing("goal turn lifecycle"))?;
    match (
        stored.state_kind.as_str(),
        stored.terminal_disposition_kind.as_deref(),
    ) {
        ("queued" | "active", None) => Ok(GoalTurnTerminalState::NotTerminal),
        ("terminal", Some("completed")) => Ok(GoalTurnTerminalState::Completed),
        ("terminal", Some("refused" | "failed" | "cancelled" | "reconciliation_required")) => {
            Ok(GoalTurnTerminalState::Unsuccessful)
        }
        ("terminal", Some("retired")) => Ok(GoalTurnTerminalState::Retired),
        ("queued" | "active" | "terminal", _) => {
            Err(GoalCorruption::Inconsistent("goal turn terminal shape").into())
        }
        _ => Err(GoalCorruption::Unsupported {
            field: "goal turn lifecycle state",
            value: stored.state_kind,
        }
        .into()),
    }
}

pub(crate) async fn continuation_exists(
    connection: &mut PgConnection,
    session: SessionId,
    predecessor: TurnId,
) -> Result<bool, GoalRepositoryError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1 FROM goal_turn
             WHERE session_id = $1 AND predecessor_turn_id = $2
         )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(predecessor))
    .fetch_one(&mut *connection)
    .await?)
}

/// Retires the queued goal turn that is no longer eligible to run.
///
/// The turn reaches `terminal{retired}` and its `turn_terminal` event appends
/// in the caller's transaction; a session with no such turn changes nothing.
pub(crate) async fn retire_ineligible_queued_goal_turn(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<TurnId>, GoalRepositoryError> {
    let turn = sqlx::query_scalar::<_, Uuid>(
        "UPDATE turn_lifecycle AS lifecycle
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'retired',
                terminal_cause_kind = $2
          WHERE lifecycle.session_id = $1
            AND lifecycle.state_kind = 'queued'
            AND lifecycle.turn_id = (
                SELECT goal.turn_id
                  FROM goal_turn AS goal
                  JOIN accepted_input AS accepted
                    ON accepted.accepted_input_id = goal.accepted_input_id
                   AND accepted.session_id = goal.session_id
                   AND accepted.origin_turn_id = goal.turn_id
                  JOIN turn_lifecycle AS queued
                    ON queued.session_id = goal.session_id
                   AND queued.turn_id = goal.turn_id
                   AND queued.state_kind = 'queued'
                 WHERE goal.session_id = $1
                   AND (
                       NOT goal_turn_is_runtime_relevant(
                           goal.session_id,
                           goal.turn_id
                       )
                       OR NOT EXISTS (
                           SELECT 1
                             FROM session_lifecycle AS session
                            WHERE session.session_id = goal.session_id
                              AND session.owned
                       )
                   )
                 ORDER BY accepted.acceptance_position DESC
                 LIMIT 1
            )
        RETURNING lifecycle.turn_id",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_terminal_cause_to_str(
        TurnTerminalCause::GoalTurnIneligible,
    ))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(turn) = turn else {
        return Ok(None);
    };
    let turn = crate::mapping::turn_id_from_uuid(turn);
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session,
            turn,
            disposition: TurnTerminalOutboxDisposition::Retired,
        },
    )
    .await?;
    Ok(Some(turn))
}

struct EncodedSelection {
    kind: &'static str,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
}

fn encode_selection(selection: ModelSelectionRequest) -> EncodedSelection {
    match selection {
        ModelSelectionRequest::Direct(selection) => EncodedSelection {
            kind: "direct",
            direct: Some(selection.into_uuid()),
            alias: None,
        },
        ModelSelectionRequest::Alias(alias) => EncodedSelection {
            kind: "alias",
            direct: None,
            alias: Some(alias.into_uuid()),
        },
    }
}

struct EncodedFrozen {
    kind: &'static str,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    alias_selected: Option<Uuid>,
}

fn encode_frozen(selection: &FrozenModelSelection) -> EncodedFrozen {
    match selection {
        FrozenModelSelection::Direct(selection) => EncodedFrozen {
            kind: "direct",
            direct: Some(selection.into_uuid()),
            alias: None,
            alias_selected: None,
        },
        FrozenModelSelection::FrozenAlias { alias, definition } => EncodedFrozen {
            kind: "frozen_alias",
            direct: None,
            alias: Some(alias.into_uuid()),
            alias_selected: Some(definition.selected().into_uuid()),
        },
    }
}
