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
