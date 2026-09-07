use super::{ModelCallRepositoryError, PostgresModelCallRepository};
use crate::mapping::session_id_to_uuid;
use signalbox_application::{
    AttachmentPreparationFailure, AuthorizeModelCallOutcome, AuthorizeModelCallTransaction,
    CommitModelCallObservationTransaction, FailPreparedModelCallTransaction,
    ModelCallAuthorizationReread, ModelCallObservationCommitOutcome,
    ModelCallTerminalIdentityCandidates, PrepareModelCallOutcome, PrepareModelCallTransaction,
    PreparedModelCallFailureCause, RetainedModelCallObservationStatus,
    RetainedPreparedFailureStatus,
};
use signalbox_domain::{
    AcceptedInputId, CorrelatedModelCallTerminalObservation, FailedModelCallTurn,
    FailedModelCallTurnIdentities, ModelCallId, SessionId, TurnId,
};

impl PrepareModelCallTransaction for PostgresModelCallRepository {
    type Error = ModelCallRepositoryError;

    async fn prepare<NextSteeringIdentities>(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        failure_identities: FailedModelCallTurnIdentities,
        steering_frontier: signalbox_domain::ContextFrontierId,
        next_steering_identities: NextSteeringIdentities,
    ) -> Result<PrepareModelCallOutcome, Self::Error>
    where
        NextSteeringIdentities:
            FnMut(AcceptedInputId) -> (signalbox_domain::SemanticTranscriptEntryId, TurnId) + Send,
    {
        match self
            .prepare_initial_call(
                session,
                call,
                failure_identities,
                steering_frontier,
                next_steering_identities,
            )
            .await
        {
            Err(ModelCallRepositoryError::NoLiveExecution) => Ok(PrepareModelCallOutcome::NoWork),
            result => result,
        }
    }
}

impl FailPreparedModelCallTransaction for PostgresModelCallRepository {
    type Error = ModelCallRepositoryError;

    async fn fail_prepared<NextTurn>(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        cause: PreparedModelCallFailureCause,
        attachment_failure: Option<AttachmentPreparationFailure>,
        identities: FailedModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
    ) -> Result<FailedModelCallTurn, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        PostgresModelCallRepository::fail_prepared_call(
            self,
            session,
            call,
            cause,
            attachment_failure,
            identities,
            next_reclassified_turn,
        )
        .await
    }

    async fn reread_failure(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        attachment_failure: Option<AttachmentPreparationFailure>,
    ) -> Result<RetainedPreparedFailureStatus, Self::Error> {
        self.reread_prepared_failure(session, call, attachment_failure)
            .await
    }
}

impl AuthorizeModelCallTransaction for PostgresModelCallRepository {
    type Error = ModelCallRepositoryError;

    async fn authorize(
        &mut self,
        session: SessionId,
        call: ModelCallId,
    ) -> Result<AuthorizeModelCallOutcome, Self::Error> {
        self.authorize_send(session, call).await
    }

    async fn reread_after_ambiguous_commit(
        &mut self,
        session: SessionId,
        prepared: &signalbox_domain::PreparedModelCallRequest,
    ) -> Result<ModelCallAuthorizationReread, Self::Error> {
        self.reread_ambiguous_authorization(session, prepared).await
    }

    fn cancellation_signal(
        &self,
        session: SessionId,
        call: ModelCallId,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let pool = self.pool.clone();
        async move {
            let mut interval = cancellation_poll_interval();
            loop {
                interval.tick().await;
                let cancelled = sqlx::query_scalar::<_, bool>(
                    "SELECT call.state_kind IN ('cancellation_requested', 'terminal')
                            OR EXISTS (
                                SELECT 1
                                  FROM session_delegation_initial_task AS task
                                  JOIN session_delegation_logical_terminal AS terminal
                                    ON terminal.spawning_tool_request_id =
                                       task.spawning_tool_request_id
                                   AND terminal.child_session_id = task.child_session_id
                                   AND terminal.child_turn_id = task.turn_id
                                 WHERE task.child_session_id = call.session_id
                                   AND task.turn_id = call.turn_id
                            )
                       FROM model_call AS call
                      WHERE call.session_id = $1
                        AND call.model_call_id = $2",
                )
                .bind(session_id_to_uuid(session))
                .bind(call.into_uuid())
                .fetch_optional(&pool)
                .await;
                if matches!(cancelled, Ok(Some(true))) {
                    return;
                }
            }
        }
    }
}

pub(super) fn cancellation_poll_interval() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(25));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

impl CommitModelCallObservationTransaction for PostgresModelCallRepository {
    type Error = ModelCallRepositoryError;

    async fn commit_observation<NextTurn>(
        &mut self,
        session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
        identities: ModelCallTerminalIdentityCandidates,
        next_reclassified_turn: NextTurn,
    ) -> Result<Option<ModelCallObservationCommitOutcome>, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        self.apply_terminal_observation_candidates(
            session,
            observation,
            identities,
            next_reclassified_turn,
        )
        .await
    }

    async fn reread_observation(
        &mut self,
        session: SessionId,
        observation: &CorrelatedModelCallTerminalObservation,
    ) -> Result<RetainedModelCallObservationStatus, Self::Error> {
        self.reread_terminal_observation(session, observation).await
    }
}
