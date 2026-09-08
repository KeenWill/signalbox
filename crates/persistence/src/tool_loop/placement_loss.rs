//! Request closure before runner dispatch.

use super::*;

/// Closes eligible requests under the caller's session and scheduler locks.
pub(crate) async fn close_lost_runner_requests(
    connection: &mut PgConnection,
    session: SessionId,
    producing_call: signalbox_domain::ModelCallId,
) -> Result<(), ToolLoopRepositoryError> {
    close_lost_runner_requests_after_observation(connection, session, producing_call, None).await
}

async fn close_lost_runner_requests_after_observation(
    connection: &mut PgConnection,
    session: SessionId,
    producing_call: signalbox_domain::ModelCallId,
    observed_judge_request: Option<ToolRequestId>,
) -> Result<(), ToolLoopRepositoryError> {
    let requests: Vec<Uuid> = sqlx::query_scalar(
        "SELECT request.request_id FROM tool_request AS request
         JOIN runner_current_session_placement AS head ON head.session_id = request.session_id
         JOIN runner_session_placement_record AS placement
           ON placement.session_id = head.session_id AND placement.event_ordinal = head.event_ordinal
         WHERE request.session_id = $1 AND request.producing_model_call_id = $2
           AND request.inadmissible_reason IS NULL
           AND placement.state_kind IN ('runner_lost', 'runner_lost_before_pin')
           AND (EXISTS (SELECT 1 FROM runner_session_placement_tool AS declared
                        WHERE declared.session_id = head.session_id AND declared.event_ordinal = head.event_ordinal
                          AND declared.tool_name = request.tool_name AND declared.runner_required)
                OR (placement.state_kind = 'runner_lost_before_pin' AND EXISTS (
                    SELECT 1 FROM runner_enrollment AS enrollment
                    JOIN runner_connection_loss_epoch AS loss ON loss.enrollment_id = enrollment.enrollment_id
                        AND loss.loss_epoch = COALESCE(placement.observed_runner_loss_epoch, 0) + 1
                    JOIN runner_registration_tool AS declared ON declared.enrollment_id = loss.enrollment_id
                        AND declared.registration_revision = COALESCE(loss.registration_revision,
                            (SELECT current.registration_revision FROM runner_current_registration AS current
                             WHERE current.enrollment_id = loss.enrollment_id))
                    WHERE enrollment.runner_id = placement.lost_runner_id
                      AND declared.tool_name = request.tool_name AND declared.loci_kind = 'runner_only')))
           AND NOT EXISTS (SELECT 1 FROM runner_tool_request_lease_binding AS lease WHERE lease.request_id = request.request_id)
           AND NOT EXISTS (SELECT 1 FROM tool_attempt AS attempt WHERE attempt.request_id = request.request_id AND attempt.state_kind <> 'prepared')
           AND (request.request_id = $3 OR NOT EXISTS (SELECT 1 FROM tool_approval_decision AS decision WHERE decision.request_id = request.request_id AND decision.decision_kind = 'deny'))
           AND NOT EXISTS (SELECT 1 FROM tool_approval_judge_model_call AS judge WHERE judge.request_id = request.request_id AND judge.state_kind = 'in_flight')
         ORDER BY request.request_ordinal FOR UPDATE OF request",
    )
    .bind(session.into_uuid()).bind(producing_call.into_uuid())
    .bind(observed_judge_request.map(ToolRequestId::into_uuid))
    .fetch_all(&mut *connection).await?;
    for request in requests {
        if let Some(row) = sqlx::query(
            "SELECT * FROM tool_attempt WHERE request_id = $1 AND state_kind = 'prepared'",
        )
        .bind(request)
        .fetch_optional(&mut *connection)
        .await?
        {
            let ReconstitutedToolAttempt::Current(attempt) = decode_attempt(row)? else {
                return Err(
                    ToolLoopCorruption::Inconsistent("placement loss prepared attempt").into(),
                );
            };
            let ended = attempt
                .end_placement_lost()
                .map_err(|_| ToolLoopCorruption::Inconsistent("placement loss attempt stage"))?;
            persist_ended_attempt(connection, &ended).await?;
        }
        sqlx::query("UPDATE tool_approval_judge_model_call SET state_kind = 'terminal', terminal_disposition_kind = 'cancelled' WHERE request_id = $1 AND state_kind = 'prepared'")
            .bind(request).execute(&mut *connection).await?;
        sqlx::query("UPDATE tool_request SET resolution_kind = 'closed_inadmissible', inadmissible_reason = 'placement_lost' WHERE request_id = $1")
            .bind(request).execute(&mut *connection).await?;
    }
    Ok(())
}

/// Resumes proposal-order evaluation after closing all pre-dispatch runner work.
pub(crate) async fn resolve_lost_runner_batch(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<(), ToolLoopRepositoryError> {
    let Some(row) = sqlx::query("SELECT turn_id, active_tool_round_call_id, active_phase_kind FROM turn_lifecycle WHERE session_id = $1 AND state_kind = 'active' AND active_tool_round_call_id IS NOT NULL")
        .bind(session.into_uuid()).fetch_optional(&mut *connection).await? else { return Ok(()); };
    let producing_call =
        signalbox_domain::ModelCallId::from_uuid(required(&row, "active_tool_round_call_id")?);
    close_lost_runner_requests(connection, session, producing_call).await?;
    let phase: String = required(&row, "active_phase_kind")?;
    if phase != "awaiting_tool_approval" {
        return Ok(());
    }
    let turn: Uuid = required(&row, "turn_id")?;
    let next: Option<Uuid> = sqlx::query_scalar("SELECT request.request_id FROM tool_request AS request WHERE request.producing_model_call_id = $1 AND request.inadmissible_reason IS NULL AND NOT EXISTS (SELECT 1 FROM tool_approval_decision AS approval WHERE approval.request_id = request.request_id) ORDER BY request.request_ordinal LIMIT 1")
        .bind(producing_call.into_uuid()).fetch_optional(&mut *connection).await?;
    if let Some(request) = next {
        sqlx::query("UPDATE turn_lifecycle SET approval_tool_request_id = $1 WHERE turn_id = $2")
            .bind(request)
            .bind(turn)
            .execute(&mut *connection)
            .await?;
    } else {
        let attempt = Uuid::now_v7();
        sqlx::query("INSERT INTO turn_attempt (turn_attempt_id, turn_id, session_id, continued_from_attempt_id, state_kind) SELECT $1, $2, $3, turn_attempt_id, 'prepared' FROM model_call WHERE model_call_id = $4")
            .bind(attempt).bind(turn).bind(session.into_uuid()).bind(producing_call.into_uuid()).execute(&mut *connection).await?;
        sqlx::query("UPDATE turn_lifecycle SET active_phase_kind = 'running', current_attempt_id = $1, approval_tool_request_id = NULL WHERE turn_id = $2")
            .bind(attempt).bind(turn).execute(&mut *connection).await?;
    }
    Ok(())
}

pub(crate) async fn resolve_lost_runner_batch_after_judge(
    connection: &mut PgConnection,
    request: &signalbox_domain::ToolRequest,
) -> Result<bool, ToolLoopRepositoryError> {
    close_lost_runner_requests_after_observation(
        connection,
        request.session(),
        request.producing_call(),
        Some(request.id()),
    )
    .await?;
    resolve_lost_runner_batch(connection, request.session()).await?;
    Ok(sqlx::query_scalar(
        "SELECT inadmissible_reason IS NOT NULL FROM tool_request WHERE request_id = $1",
    )
    .bind(request.id().into_uuid())
    .fetch_one(&mut *connection)
    .await?)
}
