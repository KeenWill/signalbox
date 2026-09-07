use super::*;

impl GoalRepository {
    /// Queues the fixed compaction input in the failed current turn's goal lineage.
    ///
    /// The session lock and predecessor uniqueness make admission atomic and
    /// replay-safe. Only repository-watch continuation headroom failures qualify.
    pub async fn continue_after_context_exhaustion<SelectDefinition>(
        &self,
        session: SessionId,
        predecessor: TurnId,
        candidates: GoalTurnCandidates,
        content: &str,
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
            return Ok(GoalTurnContinuationOutcome::NotCurrentGoalTurn);
        };
        if goal.current().state() != &GoalState::Pursuing {
            transaction.rollback().await?;
            return Ok(GoalTurnContinuationOutcome::NotPursuing);
        }
        let generation = goal.current().generation();
        if current_goal_turn(&mut transaction, session, generation).await? != Some(predecessor) {
            transaction.rollback().await?;
            return Ok(GoalTurnContinuationOutcome::AlreadyScheduled);
        }
        let eligible = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                SELECT 1 FROM turn_lifecycle AS failed
                JOIN session AS owner ON owner.session_id = failed.session_id
                JOIN tool_continuation_context_headroom AS headroom
                  ON headroom.terminal_attempt_id = failed.terminal_attempt_id
                 AND headroom.turn_id = failed.turn_id
                 AND headroom.session_id = failed.session_id
                WHERE failed.session_id = $1 AND failed.turn_id = $2
                  AND owner.creation_cause = 'module_dispatched'
                  AND owner.dispatching_module = 'repo_watch'
                  AND NOT EXISTS (
                      SELECT 1 FROM turn_lifecycle AS later
                      WHERE later.session_id = failed.session_id
                        AND later.acceptance_position > failed.acceptance_position))",
        )
        .bind(session_id_to_uuid(session))
        .bind(turn_id_to_uuid(predecessor))
        .fetch_one(&mut *transaction)
        .await?;
        if !eligible || !session_owned(&mut transaction, session).await? {
            transaction.rollback().await?;
            return Ok(GoalTurnContinuationOutcome::NotPursuing);
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
            GoalTurnSource::PredecessorTurn(predecessor),
            content,
            &configuration,
            GoalTurnInsertion::new(position, candidates),
        )
        .await?;
        commit(transaction).await?;
        Ok(GoalTurnContinuationOutcome::Scheduled {
            turn: candidates.turn(),
        })
    }
}
