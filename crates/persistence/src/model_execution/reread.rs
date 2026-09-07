use super::{ModelCallIdentityCollision, ModelCallRepositoryError, encode_disposition, required};
use crate::mapping::{session_id_to_uuid, turn_id_to_uuid};
use rust_decimal::Decimal;
use signalbox_application::ModelCallTerminalIdentityCandidates;
use signalbox_domain::{
    AcceptedInputId, AssistantResponsePart, AuthorizedModelCall,
    CorrelatedModelCallTerminalObservation, ModelCallExecution, ModelCallTerminalIdentities,
    ModelCallTerminalObservation, PendingSteeringReclassificationIdentity,
    PreparedModelCallRequest, SemanticTranscriptEntryPayload, SessionId,
    StopRequestedModelCallTurn, TurnId,
};
use sqlx::types::Uuid;
use sqlx::{PgConnection, Row};
use std::collections::BTreeSet;

pub(super) async fn terminal_observation_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    if !terminal_observation_transition_events_match(connection, session, observation).await? {
        return Ok(false);
    }
    match observation.observation() {
        ModelCallTerminalObservation::Completed { assistant_text } => {
            let response = assistant_text
                .iter()
                .cloned()
                .map(AssistantResponsePart::Text)
                .collect::<Vec<_>>();
            completed_terminal_closure_matches(connection, session, observation, &response).await
        }
        ModelCallTerminalObservation::CompletedWithProviderCompaction { response, .. }
        | ModelCallTerminalObservation::CompletedWithProviderReasoning { response } => {
            completed_terminal_closure_matches(connection, session, observation, response).await
        }
        ModelCallTerminalObservation::CompletedWithTools { response, .. } => {
            tool_round_terminal_closure_matches(connection, session, observation, response).await
        }
        ModelCallTerminalObservation::KnownFailed => {
            failed_terminal_closure_matches(connection, session, observation).await
        }
        ModelCallTerminalObservation::Cancelled => {
            if cancelled_terminal_closure_matches(connection, session, observation).await? {
                Ok(true)
            } else {
                failed_terminal_closure_matches(connection, session, observation).await
            }
        }
        ModelCallTerminalObservation::Refused => {
            refused_terminal_closure_matches(connection, session, observation, &[]).await
        }
        ModelCallTerminalObservation::RefusedWithProviderCompaction {
            provider_compaction,
            ..
        } => {
            refused_terminal_closure_matches(connection, session, observation, provider_compaction)
                .await
        }
        ModelCallTerminalObservation::Ambiguous => {
            ambiguous_terminal_closure_matches(connection, session, observation).await
        }
    }
}

async fn tool_round_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
    response: &signalbox_domain::ToolUsingAssistantResponse,
) -> Result<bool, ModelCallRepositoryError> {
    let row = sqlx::query(
        "SELECT boundary_frontier_id, response_part_count
           FROM tool_round
          WHERE producing_model_call_id = $1
            AND session_id = $2
            AND turn_id = $3",
    )
    .bind(observation.call().into_uuid())
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(observation.correlation().turn()))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let boundary_frontier: Uuid = required(&row, "boundary_frontier_id")?;
    let response_part_count: Decimal = required(&row, "response_part_count")?;
    if response_part_count != Decimal::from(response.parts().len()) {
        return Ok(false);
    }

    let source_members = load_frontier_members(
        connection,
        session,
        observation.correlation().frontier().into_uuid(),
    )
    .await?;
    let boundary_members = load_frontier_members(connection, session, boundary_frontier).await?;
    if boundary_members.len() < source_members.len() + response.parts().len()
        || boundary_members
            .iter()
            .zip(&source_members)
            .any(|(stored, expected)| stored != expected)
    {
        return Ok(false);
    }

    let response_start = Decimal::from(source_members.len() + 1);
    let response_end = Decimal::from(source_members.len() + response.parts().len() + 1);
    let stored_parts = sqlx::query(
        "SELECT entry.payload_kind, entry.assistant_text_value,
                entry.producing_model_call_id,
                entry.assistant_tool_request_id,
                request.tool_name, request.arguments_kind,
                request.arguments_text
           FROM resolve_context_frontier_members($1, $2) AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
           LEFT JOIN tool_request AS request
             ON request.request_id = entry.assistant_tool_request_id
          WHERE member.member_position >= $3
            AND member.member_position < $4
          ORDER BY member.member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(boundary_frontier)
    .bind(response_start)
    .bind(response_end)
    .fetch_all(&mut *connection)
    .await?;
    if stored_parts.len() != response.parts().len() {
        return Ok(false);
    }
    let call = observation.call().into_uuid();
    let matches = stored_parts
        .into_iter()
        .zip(response.parts())
        .all(|(stored, expected)| {
            let payload_kind = stored.try_get::<String, _>("payload_kind").ok();
            let assistant_text = stored
                .try_get::<Option<String>, _>("assistant_text_value")
                .ok()
                .flatten();
            let producing_call = stored
                .try_get::<Option<Uuid>, _>("producing_model_call_id")
                .ok()
                .flatten();
            let request = stored
                .try_get::<Option<Uuid>, _>("assistant_tool_request_id")
                .ok()
                .flatten();
            let tool_name = stored
                .try_get::<Option<String>, _>("tool_name")
                .ok()
                .flatten();
            let arguments_kind = stored
                .try_get::<Option<String>, _>("arguments_kind")
                .ok()
                .flatten();
            let arguments_text = stored
                .try_get::<Option<String>, _>("arguments_text")
                .ok()
                .flatten();
            match expected {
                AssistantResponsePart::Text(expected) => {
                    payload_kind.as_deref() == Some("assistant_text")
                        && assistant_text.as_deref() == Some(expected.as_str())
                        && producing_call == Some(call)
                        && request.is_none()
                        && tool_name.is_none()
                        && arguments_kind.is_none()
                        && arguments_text.is_none()
                }
                AssistantResponsePart::ProviderCompaction(expected) => {
                    payload_kind.as_deref() == Some("provider_compaction")
                        && assistant_text.as_deref() == Some(expected.as_json())
                        && producing_call == Some(call)
                        && request.is_none()
                        && tool_name.is_none()
                        && arguments_kind.is_none()
                        && arguments_text.is_none()
                }
                AssistantResponsePart::ProviderReasoning(expected) => {
                    payload_kind.as_deref() == Some("provider_reasoning")
                        && assistant_text.as_deref() == Some(expected.as_json())
                        && producing_call == Some(call)
                        && request.is_none()
                        && tool_name.is_none()
                        && arguments_kind.is_none()
                        && arguments_text.is_none()
                }
                AssistantResponsePart::ToolCall(expected) => {
                    let expected_kind = match expected.arguments().kind() {
                        signalbox_domain::ToolArgumentsKind::Json => "json",
                        signalbox_domain::ToolArgumentsKind::Undecodable => "undecodable",
                    };
                    payload_kind.as_deref() == Some("assistant_tool_use")
                        && assistant_text.is_none()
                        && producing_call == Some(call)
                        && request.is_some()
                        && tool_name.as_deref() == Some(expected.name().as_str())
                        && arguments_kind.as_deref() == Some(expected_kind)
                        && arguments_text.as_deref() == Some(expected.arguments().as_str())
                }
            }
        });
    Ok(matches)
}

async fn terminal_observation_transition_events_match(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT
            EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND model_call_id = $2
                   AND turn_id = $3
                   AND call_state_kind = 'in_flight'
                   AND terminal_disposition_kind IS NULL
            )
            AND EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND model_call_id = $2
                   AND turn_id = $3
                   AND call_state_kind = 'terminal'
                   AND terminal_disposition_kind = $4
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(observation.call().into_uuid())
    .bind(turn_id_to_uuid(observation.correlation().turn()))
    .bind(encode_disposition(observation.observation().disposition()))
    .fetch_one(&mut *connection)
    .await?)
}

async fn completed_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
    response: &[AssistantResponsePart],
) -> Result<bool, ModelCallRepositoryError> {
    let terminal_frontier = sqlx::query_scalar::<_, Uuid>(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'completed'
            AND terminal_model_call_id = $3",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(observation.correlation().turn()))
    .bind(observation.call().into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(terminal_frontier) = terminal_frontier else {
        return Ok(false);
    };
    let source_frontier = load_frontier_members(
        connection,
        session,
        observation.correlation().frontier().into_uuid(),
    )
    .await?;
    let terminal_members = load_terminal_frontier(connection, session, terminal_frontier).await?;
    if !completed_terminal_frontier_matches(
        &source_frontier,
        &terminal_members,
        session_id_to_uuid(session),
        turn_id_to_uuid(observation.correlation().turn()),
        observation.call().into_uuid(),
        response,
    ) {
        return Ok(false);
    }
    let Some(completion_entry) = terminal_members.last().map(|member| member.entry) else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM turn_terminal_outbox_event
             WHERE disposition_kind = 'completed'
             AND session_id = $1
               AND turn_id = $2
               AND model_call_id = $3
               AND completion_entry_id = $4
               AND terminal_frontier_id = $5
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(observation.correlation().turn()))
    .bind(observation.call().into_uuid())
    .bind(completion_entry)
    .bind(terminal_frontier)
    .fetch_one(&mut *connection)
    .await?)
}

async fn failed_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    let correlation = observation.correlation();
    failed_turn_closure_matches(
        connection,
        session,
        turn_id_to_uuid(correlation.turn()),
        correlation.attempt().into_uuid(),
        observation.call().into_uuid(),
        correlation.frontier().into_uuid(),
    )
    .await
}

pub(super) async fn cancelled_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    let correlation = observation.correlation();
    let terminal_frontier = sqlx::query_scalar::<_, Uuid>(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'cancelled'
            AND terminal_attempt_id = $3
            AND terminal_model_call_id = $4
            AND EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND turn_attempt_id = $3
                   AND state_kind = 'ended'
                   AND end_variant = 'after_cancellation'
                   AND end_disposition = 'cancelled'
                   AND interrupt_command_id IS NOT NULL
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(correlation.attempt().into_uuid())
    .bind(observation.call().into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(terminal_frontier) = terminal_frontier else {
        return Ok(false);
    };
    let source_frontier =
        load_frontier_members(connection, session, correlation.frontier().into_uuid()).await?;
    let terminal_members = load_terminal_frontier(connection, session, terminal_frontier).await?;
    if terminal_members.len() != source_frontier.len() + 1
        || terminal_members
            .iter()
            .zip(&source_frontier)
            .any(|(stored, expected)| (stored.source_session, stored.entry) != *expected)
    {
        return Ok(false);
    }
    let cancellation = &terminal_members[source_frontier.len()];
    if cancellation.source_session != session_id_to_uuid(session)
        || cancellation.payload_kind != "turn_cancelled"
        || cancellation.assistant_text.is_some()
        || cancellation.producing_call.is_some()
        || cancellation.completed_turn.is_some()
        || cancellation.failed_turn.is_some()
        || cancellation.cancelled_turn != Some(turn_id_to_uuid(correlation.turn()))
    {
        return Ok(false);
    }
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT
            EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND model_call_id = $3
                   AND call_state_kind = 'cancellation_requested'
                   AND terminal_disposition_kind IS NULL
            )
            AND EXISTS (
                SELECT 1
                  FROM turn_terminal_outbox_event
                 WHERE disposition_kind = 'cancelled'
                 AND session_id = $1
                   AND turn_id = $2
                   AND cancellation_entry_id = $4
                   AND terminal_frontier_id = $5
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(observation.call().into_uuid())
    .bind(cancellation.entry)
    .bind(terminal_frontier)
    .fetch_one(&mut *connection)
    .await?)
}

pub(super) async fn prepared_cancellation_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    turn: Uuid,
    attempt: Uuid,
    call: Uuid,
    source_frontier: Uuid,
) -> Result<bool, ModelCallRepositoryError> {
    let terminal_frontier = sqlx::query_scalar::<_, Uuid>(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'cancelled'
            AND terminal_attempt_id = $3
            AND terminal_model_call_id = $4
            AND EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND turn_attempt_id = $3
                   AND state_kind = 'ended'
                   AND end_variant = 'after_cancellation'
                   AND end_disposition = 'cancelled'
                   AND interrupt_command_id IS NOT NULL
            )
            AND EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND model_call_id = $4
                   AND call_state_kind = 'prepared'
                   AND terminal_disposition_kind IS NULL
            )
            AND NOT EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND model_call_id = $4
                   AND call_state_kind IN ('in_flight', 'cancellation_requested')
            )
            AND EXISTS (
                SELECT 1
                  FROM model_call_transition_outbox_event
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND model_call_id = $4
                   AND call_state_kind = 'terminal'
                   AND terminal_disposition_kind = 'cancelled'
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn)
    .bind(attempt)
    .bind(call)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(terminal_frontier) = terminal_frontier else {
        return Ok(false);
    };
    let source_frontier = load_frontier_members(connection, session, source_frontier).await?;
    let terminal_members = load_terminal_frontier(connection, session, terminal_frontier).await?;
    if terminal_members.len() != source_frontier.len() + 1
        || terminal_members
            .iter()
            .zip(&source_frontier)
            .any(|(stored, expected)| (stored.source_session, stored.entry) != *expected)
    {
        return Ok(false);
    }
    let cancellation = &terminal_members[source_frontier.len()];
    if cancellation.source_session != session_id_to_uuid(session)
        || cancellation.payload_kind != "turn_cancelled"
        || cancellation.assistant_text.is_some()
        || cancellation.producing_call.is_some()
        || cancellation.completed_turn.is_some()
        || cancellation.failed_turn.is_some()
        || cancellation.cancelled_turn != Some(turn)
    {
        return Ok(false);
    }
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM turn_terminal_outbox_event
             WHERE disposition_kind = 'cancelled'
             AND session_id = $1
               AND turn_id = $2
               AND cancellation_entry_id = $3
               AND terminal_frontier_id = $4
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn)
    .bind(cancellation.entry)
    .bind(terminal_frontier)
    .fetch_one(&mut *connection)
    .await?)
}

pub(super) async fn failed_turn_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    turn: Uuid,
    attempt: Uuid,
    call: Uuid,
    source_frontier: Uuid,
) -> Result<bool, ModelCallRepositoryError> {
    let terminal_frontier = sqlx::query_scalar::<_, Uuid>(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'failed'
            AND terminal_attempt_id = $3
            AND terminal_model_call_id = $4
            AND active_phase_kind IS NULL
            AND current_attempt_id IS NULL
            AND recovery_model_call_id IS NULL
            AND EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND turn_attempt_id = $3
                   AND state_kind = 'ended'
                   AND end_disposition = 'known_failure'
                   AND (
                        end_variant = 'without_stop'
                        OR (
                            end_variant = 'after_cancellation'
                            AND interrupt_command_id IS NOT NULL
                            AND interrupt_predecessor_turn_id = $2
                        )
                   )
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn)
    .bind(attempt)
    .bind(call)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(terminal_frontier) = terminal_frontier else {
        return Ok(false);
    };
    let source_frontier = load_frontier_members(connection, session, source_frontier).await?;
    let terminal_members = load_terminal_frontier(connection, session, terminal_frontier).await?;
    if !failed_terminal_frontier_matches(
        &source_frontier,
        &terminal_members,
        session_id_to_uuid(session),
        turn,
    ) {
        return Ok(false);
    }
    let Some(failure_entry) = terminal_members.last().map(|member| member.entry) else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM turn_terminal_outbox_event
             WHERE disposition_kind = 'failed'
             AND session_id = $1
               AND turn_id = $2
               AND failure_entry_id = $3
               AND terminal_frontier_id = $4
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn)
    .bind(failure_entry)
    .bind(terminal_frontier)
    .fetch_one(&mut *connection)
    .await?)
}

async fn refused_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
    provider_compaction: &[signalbox_domain::ProviderCompactionBlock],
) -> Result<bool, ModelCallRepositoryError> {
    let correlation = observation.correlation();
    let terminal_frontier = sqlx::query_scalar::<_, Uuid>(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'refused'
            AND terminal_attempt_id = $3
            AND terminal_model_call_id = $4
            AND active_phase_kind IS NULL
            AND current_attempt_id IS NULL
            AND recovery_model_call_id IS NULL
            AND EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND turn_attempt_id = $3
                   AND state_kind = 'ended'
                   AND end_disposition = 'turn_refused'
                   AND (
                        end_variant = 'without_stop'
                        OR (
                            end_variant = 'after_cancellation'
                            AND interrupt_command_id IS NOT NULL
                            AND interrupt_predecessor_turn_id = $2
                        )
                   )
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(correlation.attempt().into_uuid())
    .bind(observation.call().into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(terminal_frontier) = terminal_frontier else {
        return Ok(false);
    };
    let source_frontier =
        load_frontier_members(connection, session, correlation.frontier().into_uuid()).await?;
    let terminal_members = load_terminal_frontier(connection, session, terminal_frontier).await?;
    if terminal_members.len() != source_frontier.len() + provider_compaction.len()
        || terminal_members
            .iter()
            .zip(&source_frontier)
            .any(|(stored, expected)| (stored.source_session, stored.entry) != *expected)
    {
        return Ok(false);
    }
    let session_uuid = session_id_to_uuid(session);
    let call = observation.call().into_uuid();
    if terminal_members[source_frontier.len()..]
        .iter()
        .zip(provider_compaction)
        .any(|(stored, expected)| {
            stored.source_session != session_uuid
                || stored.payload_kind != "provider_compaction"
                || stored.assistant_text.as_deref() != Some(expected.as_json())
                || stored.producing_call != Some(call)
                || stored.completed_turn.is_some()
                || stored.failed_turn.is_some()
                || stored.cancelled_turn.is_some()
        })
    {
        return Ok(false);
    }
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM turn_terminal_outbox_event
             WHERE disposition_kind = 'refused'
             AND session_id = $1
               AND turn_id = $2
               AND model_call_id = $3
               AND terminal_frontier_id = $4
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(observation.call().into_uuid())
    .bind(terminal_frontier)
    .fetch_one(&mut *connection)
    .await?)
}

async fn ambiguous_terminal_closure_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    let correlation = observation.correlation();
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT
            EXISTS (
                SELECT 1
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND state_kind = 'active'
                   AND terminal_disposition_kind IS NULL
                   AND terminal_frontier_id IS NULL
                   AND terminal_attempt_id IS NULL
                   AND terminal_model_call_id IS NULL
                   AND active_phase_kind = 'awaiting_model_call_recovery'
                   AND current_attempt_id = $3
                   AND recovery_model_call_id = $4
                   AND EXISTS (
                        SELECT 1
                          FROM turn_attempt
                         WHERE session_id = $1
                           AND turn_id = $2
                           AND turn_attempt_id = $3
                           AND state_kind = 'ended'
                           AND end_variant = 'without_stop'
                           AND end_disposition = 'ambiguous'
                   )
            )
            OR EXISTS (
                SELECT 1
                  FROM turn_lifecycle AS lifecycle
                 WHERE lifecycle.session_id = $1
                   AND lifecycle.turn_id = $2
                   AND lifecycle.state_kind = 'terminal'
                   AND lifecycle.terminal_disposition_kind =
                       'reconciliation_required'
                   AND lifecycle.terminal_attempt_id = $3
                   AND lifecycle.terminal_model_call_id = $4
                   AND lifecycle.active_phase_kind IS NULL
                   AND lifecycle.current_attempt_id IS NULL
                   AND lifecycle.recovery_model_call_id IS NULL
                   AND (
                        EXISTS (
                            SELECT 1
                              FROM turn_attempt
                             WHERE session_id = $1
                               AND turn_id = $2
                               AND turn_attempt_id = $3
                               AND state_kind = 'ended'
                               AND end_variant = 'after_cancellation'
                               AND end_disposition = 'ambiguous'
                               AND interrupt_command_id IS NOT NULL
                               AND interrupt_predecessor_turn_id = $2
                        )
                        OR (
                            EXISTS (
                                SELECT 1
                                  FROM turn_attempt
                                 WHERE session_id = $1
                                   AND turn_id = $2
                                   AND turn_attempt_id = $3
                                   AND state_kind = 'ended'
                                   AND end_variant = 'without_stop'
                                   AND end_disposition = 'ambiguous'
                                   AND interrupt_command_id IS NULL
                                   AND interrupt_predecessor_turn_id IS NULL
                            )
                            AND EXISTS (
                                SELECT 1
                                  FROM submit_input_command AS command
                                  JOIN accepted_input AS accepted
                                    ON accepted.accepting_command_id =
                                        command.command_id
                                   AND accepted.accepted_input_id =
                                        command.result_accepted_input_id
                                   AND accepted.session_id =
                                        command.result_session_id
                                   AND accepted.origin_turn_id =
                                        command.result_turn_id
                                  JOIN queued_input_origin AS successor
                                    ON successor.accepted_input_id =
                                        accepted.accepted_input_id
                                   AND successor.turn_id =
                                        accepted.origin_turn_id
                                   AND successor.session_id =
                                        accepted.session_id
                                   AND successor.priority_kind =
                                        'interrupt_immediately_after'
                                   AND successor.interrupt_predecessor_turn_id =
                                        $2
                                 WHERE command.session_id = $1
                                   AND command.delivery_kind = 'interrupt'
                                   AND command.expected_active_turn_id = $2
                                   AND command.result_kind = 'applied'
                                   AND command.rejection_kind IS NULL
                                   AND accepted.disposition_kind = 'origin_of'
                            )
                        )
                   )
                   AND EXISTS (
                        SELECT 1
                          FROM turn_terminal_outbox_event
                         WHERE disposition_kind = 'reconciliation_required'
                         AND session_id = $1
                           AND turn_id = $2
                           AND model_call_id = $4
                           AND terminal_frontier_id =
                               lifecycle.terminal_frontier_id
                   )
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(correlation.turn()))
    .bind(correlation.attempt().into_uuid())
    .bind(observation.call().into_uuid())
    .fetch_one(&mut *connection)
    .await?)
}

pub(super) async fn load_frontier_members(
    connection: &mut PgConnection,
    session: SessionId,
    frontier: Uuid,
) -> Result<Vec<(Uuid, Uuid)>, ModelCallRepositoryError> {
    Ok(sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT source_session_id, semantic_entry_id
           FROM resolve_context_frontier_members($1, $2)
          ORDER BY member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier)
    .fetch_all(&mut *connection)
    .await?)
}

async fn load_terminal_frontier(
    connection: &mut PgConnection,
    session: SessionId,
    frontier: Uuid,
) -> Result<Vec<StoredTerminalFrontierMember>, ModelCallRepositoryError> {
    sqlx::query(
        "SELECT member.source_session_id, member.semantic_entry_id,
                entry.payload_kind, entry.assistant_text_value,
                entry.producing_model_call_id, entry.completed_turn_id,
                entry.failed_turn_id, entry.cancelled_turn_id
           FROM resolve_context_frontier_members($1, $2) AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
          ORDER BY member.member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier)
    .fetch_all(&mut *connection)
    .await?
    .into_iter()
    .map(|row| {
        Ok(StoredTerminalFrontierMember {
            source_session: required(&row, "source_session_id")?,
            entry: required(&row, "semantic_entry_id")?,
            payload_kind: required(&row, "payload_kind")?,
            assistant_text: row.try_get("assistant_text_value")?,
            producing_call: row.try_get("producing_model_call_id")?,
            completed_turn: row.try_get("completed_turn_id")?,
            failed_turn: row.try_get("failed_turn_id")?,
            cancelled_turn: row.try_get("cancelled_turn_id")?,
        })
    })
    .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StoredTerminalFrontierMember {
    pub(super) source_session: Uuid,
    pub(super) entry: Uuid,
    pub(super) payload_kind: String,
    pub(super) assistant_text: Option<String>,
    pub(super) producing_call: Option<Uuid>,
    pub(super) completed_turn: Option<Uuid>,
    pub(super) failed_turn: Option<Uuid>,
    pub(super) cancelled_turn: Option<Uuid>,
}

pub(super) fn completed_terminal_frontier_matches(
    source_frontier: &[(Uuid, Uuid)],
    terminal_frontier: &[StoredTerminalFrontierMember],
    session: Uuid,
    turn: Uuid,
    call: Uuid,
    response: &[AssistantResponsePart],
) -> bool {
    if terminal_frontier.len() != source_frontier.len() + response.len() + 1 {
        return false;
    }
    if terminal_frontier
        .iter()
        .zip(source_frontier)
        .any(|(stored, expected)| (stored.source_session, stored.entry) != *expected)
    {
        return false;
    }
    let assistant_start = source_frontier.len();
    if terminal_frontier[assistant_start..assistant_start + response.len()]
        .iter()
        .zip(response)
        .any(|(stored, expected)| {
            let content_matches = match expected {
                AssistantResponsePart::Text(text) => {
                    stored.payload_kind == "assistant_text"
                        && stored.assistant_text.as_deref() == Some(text.as_str())
                }
                AssistantResponsePart::ProviderCompaction(block) => {
                    stored.payload_kind == "provider_compaction"
                        && stored.assistant_text.as_deref() == Some(block.as_json())
                }
                AssistantResponsePart::ProviderReasoning(block) => {
                    stored.payload_kind == "provider_reasoning"
                        && stored.assistant_text.as_deref() == Some(block.as_json())
                }
                AssistantResponsePart::ToolCall(_) => false,
            };
            stored.source_session != session
                || !content_matches
                || stored.producing_call != Some(call)
                || stored.completed_turn.is_some()
                || stored.failed_turn.is_some()
                || stored.cancelled_turn.is_some()
        })
    {
        return false;
    }
    let completion = &terminal_frontier[assistant_start + response.len()];
    completion.source_session == session
        && completion.payload_kind == "turn_completed"
        && completion.assistant_text.is_none()
        && completion.producing_call.is_none()
        && completion.completed_turn == Some(turn)
        && completion.failed_turn.is_none()
        && completion.cancelled_turn.is_none()
}

pub(super) fn failed_terminal_frontier_matches(
    source_frontier: &[(Uuid, Uuid)],
    terminal_frontier: &[StoredTerminalFrontierMember],
    session: Uuid,
    turn: Uuid,
) -> bool {
    if terminal_frontier.len() != source_frontier.len() + 1
        || terminal_frontier
            .iter()
            .zip(source_frontier)
            .any(|(stored, expected)| (stored.source_session, stored.entry) != *expected)
    {
        return false;
    }
    let failure = &terminal_frontier[source_frontier.len()];
    failure.source_session == session
        && failure.payload_kind == "turn_failed"
        && failure.assistant_text.is_none()
        && failure.producing_call.is_none()
        && failure.completed_turn.is_none()
        && failure.failed_turn == Some(turn)
        && failure.cancelled_turn.is_none()
}

pub(super) fn pending_reclassification_candidates(
    execution: &ModelCallExecution,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<Vec<PendingSteeringReclassificationIdentity>, ModelCallRepositoryError> {
    pending_reclassification_candidates_from_parts(
        execution.active_turn().turn(),
        execution.active_turn().pending_steering(),
        next_turn,
    )
}

fn pending_reclassification_candidates_for_active(
    active_turn: &signalbox_domain::ActivatedAcceptedInputTurn,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<Vec<PendingSteeringReclassificationIdentity>, ModelCallRepositoryError> {
    pending_reclassification_candidates_from_parts(
        active_turn.turn(),
        active_turn.pending_steering(),
        next_turn,
    )
}

fn pending_reclassification_candidates_for_activated(
    active_turn: &signalbox_domain::ActivatedTurn,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<Vec<PendingSteeringReclassificationIdentity>, ModelCallRepositoryError> {
    pending_reclassification_candidates_from_parts(
        active_turn.turn(),
        active_turn.pending_steering(),
        next_turn,
    )
}

fn pending_reclassification_candidates_from_parts(
    source_turn: TurnId,
    pending_steering: &[signalbox_domain::PendingSteeringInput],
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<Vec<PendingSteeringReclassificationIdentity>, ModelCallRepositoryError> {
    let mut proposed_turns = BTreeSet::new();
    let mut reclassifications = Vec::new();
    for pending in pending_steering {
        let accepted_input = pending.accepted_input();
        let proposed_turn = next_turn(accepted_input);
        record_reclassified_turn_candidate(source_turn, proposed_turn, &mut proposed_turns)?;
        reclassifications.push(PendingSteeringReclassificationIdentity::new(
            accepted_input,
            proposed_turn,
        ));
    }
    Ok(reclassifications)
}

pub(crate) fn attach_interrupt_reclassification_candidates(
    identities: signalbox_domain::CancelledModelCallTurnIdentities,
    execution: &ModelCallExecution,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<signalbox_domain::CancelledModelCallTurnIdentities, ModelCallRepositoryError> {
    Ok(
        identities.with_pending_steering_reclassifications(pending_reclassification_candidates(
            execution, next_turn,
        )?),
    )
}

pub(crate) fn attach_interrupt_reclassification_candidates_for_active(
    identities: signalbox_domain::CancelledModelCallTurnIdentities,
    active_turn: &signalbox_domain::ActivatedAcceptedInputTurn,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<signalbox_domain::CancelledModelCallTurnIdentities, ModelCallRepositoryError> {
    Ok(identities.with_pending_steering_reclassifications(
        pending_reclassification_candidates_for_active(active_turn, next_turn)?,
    ))
}

pub(crate) fn attach_interrupt_reclassification_candidates_for_activated(
    identities: signalbox_domain::CancelledModelCallTurnIdentities,
    active_turn: &signalbox_domain::ActivatedTurn,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<signalbox_domain::CancelledModelCallTurnIdentities, ModelCallRepositoryError> {
    Ok(identities.with_pending_steering_reclassifications(
        pending_reclassification_candidates_for_activated(active_turn, next_turn)?,
    ))
}

pub(crate) fn attach_recovery_interrupt_reclassification_candidates(
    identities: signalbox_domain::AmbiguousModelCallTurnIdentities,
    active_turn: &signalbox_domain::ActivatedAcceptedInputTurn,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<signalbox_domain::AmbiguousModelCallTurnIdentities, ModelCallRepositoryError> {
    Ok(identities.with_pending_steering_reclassifications(
        pending_reclassification_candidates_for_active(active_turn, next_turn)?,
    ))
}

pub(crate) fn attach_recovery_interrupt_reclassification_candidates_for_activated(
    identities: signalbox_domain::AmbiguousModelCallTurnIdentities,
    active_turn: &signalbox_domain::ActivatedTurn,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<signalbox_domain::AmbiguousModelCallTurnIdentities, ModelCallRepositoryError> {
    Ok(identities.with_pending_steering_reclassifications(
        pending_reclassification_candidates_for_activated(active_turn, next_turn)?,
    ))
}

pub(super) fn record_reclassified_turn_candidate(
    source_turn: TurnId,
    proposed_turn: TurnId,
    proposed_turns: &mut BTreeSet<TurnId>,
) -> Result<(), ModelCallRepositoryError> {
    if proposed_turn == source_turn || !proposed_turns.insert(proposed_turn) {
        return Err(ModelCallRepositoryError::IdentityCollision(
            ModelCallIdentityCollision::ReclassifiedTurn,
        ));
    }
    Ok(())
}

pub(super) fn select_terminal_identity_candidates(
    identities: ModelCallTerminalIdentityCandidates,
    execution: &ModelCallExecution,
) -> ModelCallTerminalIdentityCandidates {
    match identities {
        ModelCallTerminalIdentityCandidates::Exact(identities) => {
            ModelCallTerminalIdentityCandidates::Exact(identities)
        }
        ModelCallTerminalIdentityCandidates::ToolRound {
            continuing,
            stopped,
        } => {
            if matches!(
                execution.current_attempt().state(),
                signalbox_domain::CurrentTurnAttemptState::StopRequested { .. }
            ) {
                ModelCallTerminalIdentityCandidates::Exact(
                    ModelCallTerminalIdentities::StoppedToolRound(stopped),
                )
            } else {
                ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::ToolRound(
                    continuing,
                ))
            }
        }
        // A pending stop is handled inside the availability branch rather
        // than by downgrading here. Converting to `Exact` closed the turn
        // correctly but skipped the frozen policy entirely, so a racing stop
        // silently dropped a configured switch_next_turn, avoid_new_sessions,
        // or quarantine and left the failed credential selectable. The branch
        // suppresses only successor creation, which is what the stop forbids.
        ModelCallTerminalIdentityCandidates::Availability {
            failed,
            successor_attempt,
        } => ModelCallTerminalIdentityCandidates::Availability {
            failed,
            successor_attempt,
        },
    }
}

pub(super) fn attach_pending_reclassification_candidates(
    identities: ModelCallTerminalIdentityCandidates,
    execution: &ModelCallExecution,
    next_turn: &mut impl FnMut(AcceptedInputId) -> TurnId,
) -> Result<ModelCallTerminalIdentityCandidates, ModelCallRepositoryError> {
    let reclassifications = pending_reclassification_candidates(execution, next_turn)?;
    let identities = match identities {
        ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Completed(
            identities,
        )) => ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Completed(
            identities.with_pending_steering_reclassifications(reclassifications),
        )),
        ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::ToolRound(
            identities,
        )) => ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::ToolRound(
            identities,
        )),
        ModelCallTerminalIdentityCandidates::Exact(
            ModelCallTerminalIdentities::StoppedToolRound(identities),
        ) => ModelCallTerminalIdentityCandidates::Exact(
            ModelCallTerminalIdentities::StoppedToolRound(
                identities.with_pending_steering_reclassifications(reclassifications),
            ),
        ),
        ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Failed(
            identities,
        )) => ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Failed(
            identities.with_pending_steering_reclassifications(reclassifications),
        )),
        ModelCallTerminalIdentityCandidates::Exact(
            ModelCallTerminalIdentities::PhysicalCancellation(identities),
        ) => ModelCallTerminalIdentityCandidates::Exact(
            ModelCallTerminalIdentities::PhysicalCancellation(
                identities.with_pending_steering_reclassifications(reclassifications),
            ),
        ),
        ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Refused(
            identities,
        )) => ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Refused(
            identities.with_pending_steering_reclassifications(reclassifications),
        )),
        ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Ambiguous(
            identities,
        )) => ModelCallTerminalIdentityCandidates::Exact(ModelCallTerminalIdentities::Ambiguous(
            identities.with_pending_steering_reclassifications(reclassifications),
        )),
        ModelCallTerminalIdentityCandidates::Availability {
            failed,
            successor_attempt,
        } => ModelCallTerminalIdentityCandidates::Availability {
            failed: failed.with_pending_steering_reclassifications(reclassifications),
            successor_attempt,
        },
        ModelCallTerminalIdentityCandidates::ToolRound { .. } => {
            return Err(ModelCallRepositoryError::InvalidTransition(
                "tool terminal candidates were not selected before reclassification",
            ));
        }
    };
    Ok(identities)
}

pub(super) fn prepared_matches_authorized(
    prepared: &PreparedModelCallRequest,
    authorized: &AuthorizedModelCall,
) -> bool {
    prepared.session() == authorized.session()
        && prepared.turn() == authorized.turn()
        && prepared.attempt() == authorized.attempt().id()
        && prepared.call().id() == authorized.call().id()
        && prepared.call().selection() == authorized.call().selection()
        && prepared.call().target() == authorized.call().target()
        && prepared.call().frontier() == authorized.call().frontier()
        && prepared
            .frontier_entries()
            .eq(authorized.frontier_entries())
        && prepared
            .frontier_entries()
            .all(|entry| match entry.payload() {
                SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
                | SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input, ..
                } => {
                    prepared.origin_content(*accepted_input)
                        == authorized.origin_content(*accepted_input)
                }
                _ => true,
            })
}

pub(super) fn prepared_matches_stopped(
    prepared: &PreparedModelCallRequest,
    execution: &ModelCallExecution,
    stopped: &StopRequestedModelCallTurn,
) -> bool {
    prepared.session() == stopped.session()
        && prepared.turn() == stopped.turn()
        && prepared.attempt() == stopped.attempt().id()
        && prepared.call().id() == stopped.call().id()
        && prepared.call().selection() == stopped.call().selection()
        && prepared.call().target() == stopped.call().target()
        && prepared.call().frontier() == stopped.call().frontier()
        && prepared.frontier_entries().eq(execution.frontier_entries())
        && prepared
            .frontier_entries()
            .all(|entry| match entry.payload() {
                SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
                | SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input, ..
                } => {
                    prepared.origin_content(*accepted_input)
                        == execution.origin_content(*accepted_input)
                }
                _ => true,
            })
}
