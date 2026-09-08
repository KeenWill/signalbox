use super::decode::{decode_complete, map_tool_loop_error};
use super::handle::handle_in_transaction;
use super::load::{
    load_complete_rows, load_existing_interrupt, load_from_connection, load_turn_origin_graph,
    non_accepted_predecessor, related_turn_origin_key,
};
use super::scheduling_projection::load_scheduling_projection;
use super::{
    PreparedAgainstLockedState, SubmitInputCorruption, SubmitInputHandlingOutcome,
    SubmitInputRepositoryError, TransactionDecision, required,
};
use crate::mapping::{
    durable_command_id_from_uuid, durable_command_id_to_uuid, input_position_from_numeric,
    session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid,
};
use crate::session::{SessionCorruption, SessionRepositoryError, load_session_from_connection};
use crate::tool_loop::deny_awaiting_approvals_for_interrupt;
use rust_decimal::Decimal;
use signalbox_application::SubmitInputIdGenerator;
use signalbox_domain::{
    AcceptedInputId, CancelledModelCallTurnIdentities, CommandPrincipal, ContextFrontierId,
    DeliveryRequest, DurableCommandId, FrozenAliasDefinition, ModelAlias, ModelCapabilityCatalog,
    OriginModelSettingsError, ParentTerminationKind, ReconstitutedSubmitInput,
    SemanticTranscriptEntryId, SubmitInput, SubmitInputAppliedResult,
    SubmitInputPreparationFailure, SubmitInputResult, TurnAttemptId, TurnId,
};
use sqlx::types::Uuid;
use sqlx::{PgConnection, Row};
use std::collections::{BTreeMap, BTreeSet};

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
        Some(principal),
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

pub(super) async fn require_recorded(
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
        )
        .await?;
        if recorded.insert(command_id, reconstructed).is_some() {
            return Err(
                SubmitInputCorruption::Inconsistent("duplicate batched command row").into(),
            );
        }
    }
    Ok(recorded)
}

pub(super) fn existing_outcome(
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
pub(super) async fn prepare_against_locked_state<NextClosureDecision, NextClosureAttempt>(
    connection: &mut PgConnection,
    command: SubmitInput,
    principal: Option<CommandPrincipal>,
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
    principal: Option<CommandPrincipal>,
) -> Result<bool, SubmitInputRepositoryError> {
    if principal != Some(CommandPrincipal::Core)
        || matches!(command.actor(), signalbox_domain::Actor::Program { .. })
    {
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

pub(super) fn map_model_settings_resolution_error(
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
