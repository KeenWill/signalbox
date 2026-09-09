use super::*;
use signalbox_domain::SessionConfigurationDefaultsVersion;

/// The physical settlement retained by a user-stopped goal's closure event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GoalStopSettlement {
    /// The closure event this settlement belongs to.
    pub event: GoalEventOrdinal,
    /// The turn asked to stop, absent when no turn was active.
    pub turn: Option<TurnId>,
    /// The defaults epoch frozen for the stop interrupt.
    pub defaults_version: SessionConfigurationDefaultsVersion,
    /// The retained identity for issuing or replaying the interrupt.
    pub interrupt_command: DurableCommandId,
    /// Approved requests closed without execution; absent until the turn settles.
    pub abandoned_actions: Option<u64>,
}

impl GoalRepository {
    /// Loads the durable settlement of each user-stopped generation.
    pub async fn load_stop_settlements(
        &self,
        session: SessionId,
    ) -> Result<Vec<GoalStopSettlement>, GoalRepositoryError> {
        let rows = sqlx::query(
            "SELECT event_ordinal, turn_id, defaults_version, interrupt_command_id,
                    abandoned_actions
               FROM goal_stop_settlement WHERE session_id = $1 ORDER BY event_ordinal",
        )
        .bind(session_id_to_uuid(session))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let abandoned: Option<i64> = row.try_get("abandoned_actions")?;
                Ok(GoalStopSettlement {
                    event: GoalEventOrdinal::new(positive(row.try_get("event_ordinal")?)?),
                    turn: row
                        .try_get::<Option<Uuid>, _>("turn_id")?
                        .map(turn_id_from_uuid),
                    defaults_version: crate::mapping::defaults_version_from_numeric(
                        row.try_get("defaults_version")?,
                    )
                    .map_err(|_| GoalCorruption::Inconsistent("goal stop defaults version"))?,
                    interrupt_command: durable_command_id_from_uuid(
                        row.try_get("interrupt_command_id")?,
                    )
                    .map_err(GoalCorruption::InvalidCommandId)?,
                    abandoned_actions: abandoned
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| GoalCorruption::Inconsistent("goal stop abandoned count"))?,
                })
            })
            .collect()
    }
}

pub(super) async fn active_turn(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<TurnId>, GoalRepositoryError> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT turn_id FROM turn_lifecycle
          WHERE session_id = $1 AND state_kind = 'active'
            AND NOT delegation_runtime_terminal",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(connection)
    .await?
    .map(turn_id_from_uuid))
}

pub(super) async fn admit_interrupt(
    connection: &mut PgConnection,
    session: SessionId,
    event: GoalEventOrdinal,
) -> Result<(), GoalRepositoryError> {
    use crate::submit_input::{
        SubmitInputHandlingOutcome, SubmitInputRepositoryError, TransactionDecision,
        handle_in_transaction,
    };
    use signalbox_domain::{
        CancelledModelCallTurnIdentities, ContextFrontierId, DirectModelSelection,
        ModelSelectionRequest, ParentTerminationKind, PerInputConfigurationChoices,
        SemanticTranscriptEntryId, SubmitInput, SubmitInputResult, TurnAttemptId, UserContent,
    };

    let row = sqlx::query(
        "SELECT turn_id, defaults_version, interrupt_command_id
           FROM goal_stop_settlement WHERE session_id = $1 AND event_ordinal = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(Decimal::from(event.get()))
    .fetch_one(&mut *connection)
    .await?;
    let Some(turn) = row.try_get::<Option<Uuid>, _>("turn_id")? else {
        return Ok(());
    };
    let selected_model = sqlx::query_scalar::<_, Uuid>(
        "SELECT direct_selection_id FROM turn_origin_effective_model_configuration($1, $2)",
    )
    .bind(turn)
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut *connection)
    .await?;
    let command = SubmitInput::new_core_interrupt(
        durable_command_id_from_uuid(row.try_get("interrupt_command_id")?)
            .map_err(GoalCorruption::InvalidCommandId)?,
        session,
        UserContent::try_text("The goal was stopped.".to_owned())
            .map_err(|_| GoalCorruption::Inconsistent("goal stop content"))?,
        turn_id_from_uuid(turn),
        DescendantTerminationScope::ParentAlone,
        PerInputConfigurationChoices::new(
            crate::mapping::defaults_version_from_numeric(row.try_get("defaults_version")?)
                .map_err(|_| GoalCorruption::Inconsistent("goal stop defaults version"))?,
            ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(selected_model),
            )),
        ),
    );
    let decision = Box::pin(handle_in_transaction(
        connection,
        command,
        Some(CommandPrincipal::Core),
        ParentTerminationKind::Stopped,
        AcceptedInputId::from_uuid(Uuid::now_v7()),
        Some(TurnId::from_uuid(Uuid::now_v7())),
        CancelledModelCallTurnIdentities::new(
            SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
            ContextFrontierId::from_uuid(Uuid::now_v7()),
        ),
        |_| TurnId::from_uuid(Uuid::now_v7()),
        |requests| {
            (
                requests
                    .iter()
                    .map(|_| SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()))
                    .collect(),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            )
        },
        || DurableCommandId::from_uuid(Uuid::now_v7()),
        || TurnAttemptId::from_uuid(Uuid::now_v7()),
        |_| None,
        None,
        None,
    ))
    .await
    .map_err(|error| match error {
        SubmitInputRepositoryError::Database(error) => GoalRepositoryError::Database(error),
        _ => GoalCorruption::Inconsistent("goal stop interrupt admission").into(),
    })?;
    match decision {
        TransactionDecision::Commit(SubmitInputHandlingOutcome::Recorded(
            SubmitInputResult::Applied(_),
        )) => Ok(()),
        _ => Err(GoalCorruption::Inconsistent("goal stop interrupt was not applied").into()),
    }
}
