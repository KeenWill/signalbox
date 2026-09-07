use super::continuation::ToolContinuationHeadroomEvidence;
use super::persist_disposition::{
    EndedCallEvidence, encode_token_usage, encode_tool_approval, encode_tool_decision_source,
    insert_snapshot, persist_ended_attempt, persist_ended_call_with_provider_failure_cause,
    persist_ended_call_with_retained_usage, persist_tool_round_authority,
};
use super::persist_terminal::persist_failed_with_delegated_child_result;
use super::prepared::map_tool_evidence_error;
use super::{
    ModelCallCorruption, ModelCallRepositoryError, append_terminal_call_event,
    encode_provider_failure_cause, require_single,
};
use crate::mapping::{session_id_to_uuid, tool_request_id_to_uuid, turn_id_to_uuid};
use crate::outbox;
use crate::outbox::{OutboxEvent, ToolBatchOutboxState};
use rust_decimal::Decimal;
use signalbox_domain::{
    ActiveTurnPhase, AvailabilitySuccessorModelCallTurn, ContextHeadroomExhaustedModelCallTurn,
    CredentialPoolExhaustedModelCallTurn, ModelCallId, ProviderModelCallFailureCause,
    ProviderReportedTokenUsage, SessionId, ToolDecisionSource, ToolRoundModelCallTurn,
    TurnAttemptId, TurnId, TurnTerminalCause,
};
use sqlx::PgConnection;
use std::time::Duration;

pub(super) async fn persist_tool_round(
    connection: &mut PgConnection,
    round: &ToolRoundModelCallTurn,
    usage: ProviderReportedTokenUsage,
    retained_input_tokens: Option<u64>,
    retained_output_tokens: Option<u64>,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call_with_retained_usage(
        connection,
        round.session(),
        round.turn(),
        round.call(),
        usage,
        retained_input_tokens,
        retained_output_tokens,
    )
    .await?;
    persist_ended_attempt(connection, round.session(), round.turn(), round.attempt()).await?;
    persist_tool_round_authority(
        connection,
        round.session(),
        round.turn(),
        round.call().id(),
        "continuing",
        round.yielded_snapshot().frontier().snapshot(),
        round.assistant_entries(),
        round.requests(),
    )
    .await?;
    crate::tool_loop::close_lost_runner_requests(connection, round.session(), round.call().id())
        .await
        .map_err(map_tool_evidence_error)?;
    for approval in round.automatic_approvals() {
        let inadmissible: bool = sqlx::query_scalar(
            "SELECT inadmissible_reason IS NOT NULL FROM tool_request WHERE request_id = $1",
        )
        .bind(approval.request().into_uuid())
        .fetch_one(&mut *connection)
        .await?;
        if inadmissible {
            continue;
        }
        let (decision_kind, denial_reason) = encode_tool_approval(approval.decision());
        let source = encode_tool_decision_source(approval.source())?;
        let override_denied_request = match (approval.source(), approval.decider()) {
            (
                ToolDecisionSource::UserOverride,
                Some(signalbox_domain::ToolApprovalDecider::UserOverride {
                    denied_request, ..
                }),
            ) => Some(tool_request_id_to_uuid(*denied_request)),
            (ToolDecisionSource::UserOverride, _) => {
                return Err(
                    ModelCallCorruption::Inconsistent("user-override decision provenance").into(),
                );
            }
            _ => None,
        };
        sqlx::query(
            "INSERT INTO tool_approval_decision
                (request_id, decision_kind, decision_source, denial_reason,
                 user_command_id, override_denied_request_id)
             VALUES ($1, $2, $3, $4, NULL, $5)",
        )
        .bind(tool_request_id_to_uuid(approval.request()))
        .bind(decision_kind)
        .bind(source)
        .bind(denial_reason)
        .bind(override_denied_request)
        .execute(&mut *connection)
        .await?;
        if approval.source() == ToolDecisionSource::UserOverride {
            outbox::append(
                connection,
                OutboxEvent::ToolApprovalDecided {
                    session: round.session(),
                    turn: round.turn(),
                    request: approval.request(),
                },
            )
            .await?;
        }
    }
    insert_snapshot(connection, round.yielded_snapshot()).await?;

    match round.next_phase() {
        ActiveTurnPhase::AwaitingApproval { request } => {
            let rows = sqlx::query(
                "UPDATE turn_lifecycle
                    SET active_phase_kind = 'awaiting_tool_approval',
                        current_attempt_id = NULL,
                        active_tool_round_call_id = $1,
                        approval_tool_request_id = $2,
                        recovery_tool_attempt_id = NULL
                  WHERE turn_id = $3
                    AND session_id = $4
                    AND state_kind = 'active'
                    AND active_phase_kind = 'running'
                    AND current_attempt_id = $5
                    AND active_tool_round_call_id IS NULL
                    AND approval_tool_request_id IS NULL
                    AND recovery_tool_attempt_id IS NULL",
            )
            .bind(round.call().id().into_uuid())
            .bind(tool_request_id_to_uuid(*request))
            .bind(turn_id_to_uuid(round.turn()))
            .bind(session_id_to_uuid(round.session()))
            .bind(round.attempt().id().into_uuid())
            .execute(&mut *connection)
            .await?
            .rows_affected();
            require_single(rows, "tool-round approval wait")?;
        }
        ActiveTurnPhase::Running { current_attempt } => {
            if !matches!(
                current_attempt.state(),
                signalbox_domain::CurrentTurnAttemptState::Prepared
            ) {
                return Err(
                    ModelCallCorruption::Inconsistent("tool continuation attempt state").into(),
                );
            }
            sqlx::query(
                "INSERT INTO turn_attempt
                    (turn_attempt_id, turn_id, session_id,
                     continued_from_attempt_id, state_kind)
                 VALUES ($1, $2, $3, $4, 'prepared')",
            )
            .bind(current_attempt.id().into_uuid())
            .bind(turn_id_to_uuid(round.turn()))
            .bind(session_id_to_uuid(round.session()))
            .bind(round.attempt().id().into_uuid())
            .execute(&mut *connection)
            .await?;
            let rows = sqlx::query(
                "UPDATE turn_lifecycle
                    SET active_phase_kind = 'running',
                        current_attempt_id = $1,
                        active_tool_round_call_id = $2,
                        approval_tool_request_id = NULL,
                        recovery_tool_attempt_id = NULL
                  WHERE turn_id = $3
                    AND session_id = $4
                    AND state_kind = 'active'
                    AND active_phase_kind = 'running'
                    AND current_attempt_id = $5
                    AND active_tool_round_call_id IS NULL
                    AND approval_tool_request_id IS NULL
                    AND recovery_tool_attempt_id IS NULL",
            )
            .bind(current_attempt.id().into_uuid())
            .bind(round.call().id().into_uuid())
            .bind(turn_id_to_uuid(round.turn()))
            .bind(session_id_to_uuid(round.session()))
            .bind(round.attempt().id().into_uuid())
            .execute(&mut *connection)
            .await?
            .rows_affected();
            require_single(rows, "auto-approved tool execution phase")?;
        }
        ActiveTurnPhase::AwaitingChild { .. }
        | ActiveTurnPhase::AwaitingRecoveryDecision { .. }
        | ActiveTurnPhase::AwaitingRunnerRecovery { .. } => {
            return Err(
                ModelCallCorruption::Inconsistent("fresh tool round recovery phase").into(),
            );
        }
    }
    crate::tool_loop::resolve_lost_runner_batch(connection, round.session())
        .await
        .map_err(map_tool_evidence_error)?;
    outbox::append(
        connection,
        OutboxEvent::ToolBatchTransition {
            session: round.session(),
            turn: round.turn(),
            producing_call: round.call().id(),
            state: ToolBatchOutboxState::Proposed(round.yielded_snapshot().frontier().snapshot()),
        },
    )
    .await?;
    append_terminal_call_event(connection, round.session(), round.turn(), round.call()).await
}

pub(super) async fn persist_availability_successor(
    connection: &mut PgConnection,
    successor: &AvailabilitySuccessorModelCallTurn,
    usage: ProviderReportedTokenUsage,
    cause: ProviderModelCallFailureCause,
    backoff: Duration,
) -> Result<(), ModelCallRepositoryError> {
    let backoff_milliseconds = i64::try_from(backoff.as_millis()).map_err(|_| {
        ModelCallRepositoryError::InvalidTransition("availability backoff overflow")
    })?;
    persist_ended_call_with_provider_failure_cause(
        connection,
        successor.session(),
        successor.turn(),
        successor.predecessor_call(),
        EndedCallEvidence {
            usage,
            provider_failure_cause: Some(cause),
            attachment_failure: None,
            retained_input_tokens: None,
            retained_output_tokens: None,
        },
    )
    .await?;
    persist_ended_attempt(
        connection,
        successor.session(),
        successor.turn(),
        successor.predecessor_attempt(),
    )
    .await?;
    if successor.successor_attempt().state() != &signalbox_domain::CurrentTurnAttemptState::Prepared
    {
        return Err(
            ModelCallCorruption::Inconsistent("availability successor attempt state").into(),
        );
    }
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id,
             continued_from_attempt_id, state_kind)
         VALUES ($1, $2, $3, $4, 'prepared')",
    )
    .bind(successor.successor_attempt().id().into_uuid())
    .bind(turn_id_to_uuid(successor.turn()))
    .bind(session_id_to_uuid(successor.session()))
    .bind(successor.predecessor_attempt().id().into_uuid())
    .execute(&mut *connection)
    .await?;
    if is_same_credential_retry_cause(cause) {
        let rows = sqlx::query(
            "INSERT INTO credential_pool_transient_exclusion
                (observation_model_call_id, credential_reference,
                 cause_kind, reset_at)
             SELECT model_call_id, credential_reference, $2,
                    transaction_timestamp() + ($3 * interval '1 millisecond')
               FROM model_call
              WHERE model_call_id = $1",
        )
        .bind(successor.predecessor_call().id().into_uuid())
        .bind(encode_provider_failure_cause(cause))
        .bind(backoff_milliseconds)
        .execute(&mut *connection)
        .await?
        .rows_affected();
        require_single(rows, "credential transient exclusion")?;
    }
    sqlx::query(
        "INSERT INTO credential_pool_availability_successor
            (predecessor_model_call_id, successor_turn_attempt_id, cause_kind,
             retry_backoff_milliseconds, retry_not_before)
         VALUES ($1, $2, $3, $4,
                 transaction_timestamp() + ($4 * interval '1 millisecond'))",
    )
    .bind(successor.predecessor_call().id().into_uuid())
    .bind(successor.successor_attempt().id().into_uuid())
    .bind(encode_provider_failure_cause(cause))
    .bind(backoff_milliseconds)
    .execute(&mut *connection)
    .await?;
    let rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET current_attempt_id = $1
          WHERE turn_id = $2
            AND session_id = $3
            AND state_kind = 'active'
            AND active_phase_kind = 'running'
            AND current_attempt_id = $4",
    )
    .bind(successor.successor_attempt().id().into_uuid())
    .bind(turn_id_to_uuid(successor.turn()))
    .bind(session_id_to_uuid(successor.session()))
    .bind(successor.predecessor_attempt().id().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "availability successor attempt")?;
    append_terminal_call_event(
        connection,
        successor.session(),
        successor.turn(),
        successor.predecessor_call(),
    )
    .await
}

/// Writes the single terminal-exhaustion row shared by both exhaustion shapes.
///
/// Pre-call exhaustion has no failing call to name; post-call exhaustion pins
/// the member call that consumed the last admissible member and its cause.
pub(super) async fn insert_credential_pool_terminal_exhaustion(
    connection: &mut PgConnection,
    attempt: TurnAttemptId,
    session: SessionId,
    turn: TurnId,
    pool_name: &str,
    last_call: Option<ModelCallId>,
    last_cause: Option<ProviderModelCallFailureCause>,
) -> Result<(), ModelCallRepositoryError> {
    sqlx::query(
        "INSERT INTO credential_pool_terminal_exhaustion
            (terminal_attempt_id, terminal_model_call_id,
             session_id, turn_id, pool_name, cause_kind)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(attempt.into_uuid())
    .bind(last_call.map(ModelCallId::into_uuid))
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(pool_name)
    .bind(last_cause.map(encode_provider_failure_cause))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(super) async fn persist_credential_pool_exhaustion(
    connection: &mut PgConnection,
    exhausted: &CredentialPoolExhaustedModelCallTurn,
) -> Result<(), ModelCallRepositoryError> {
    persist_failed_with_delegated_child_result(
        connection,
        exhausted.failed(),
        TurnTerminalCause::CredentialPoolExhausted,
        ProviderReportedTokenUsage::unreported(),
        None,
        None,
    )
    .await?;
    insert_credential_pool_terminal_exhaustion(
        connection,
        exhausted.failed().attempt().id(),
        exhausted.failed().session(),
        exhausted.failed().turn(),
        exhausted.pool_name(),
        None,
        None,
    )
    .await
}

pub(super) async fn persist_tool_continuation_headroom_exhaustion(
    connection: &mut PgConnection,
    required: &ContextHeadroomExhaustedModelCallTurn,
    evidence: ToolContinuationHeadroomEvidence,
) -> Result<(), ModelCallRepositoryError> {
    let usage = encode_token_usage(evidence.usage);
    sqlx::query(
        "INSERT INTO tool_continuation_context_headroom
            (terminal_attempt_id, producing_model_call_id, session_id, turn_id,
             usage_input_includes_cache_tokens, usage_input_tokens,
             usage_output_tokens, usage_cache_creation_input_tokens,
             usage_cache_read_input_tokens, projected_result_content_bytes,
             max_output_tokens, context_window_tokens)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(required.failed().attempt().id().into_uuid())
    .bind(required.producing_call().into_uuid())
    .bind(session_id_to_uuid(required.failed().session()))
    .bind(turn_id_to_uuid(required.failed().turn()))
    .bind(evidence.input_includes_cache_tokens)
    .bind(usage.input_tokens)
    .bind(usage.output_tokens)
    .bind(usage.cache_creation_input_tokens)
    .bind(usage.cache_read_input_tokens)
    .bind(Decimal::from(evidence.projected_result_content_bytes))
    .bind(Decimal::from(evidence.limit.max_output_tokens()))
    .bind(Decimal::from(evidence.limit.context_window_tokens()))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(super) const MAX_AVAILABILITY_BACKOFF: Duration = Duration::from_secs(300);
const MAX_EXPONENTIAL_BACKOFF: Duration = Duration::from_secs(60);

pub(super) const fn is_same_credential_retry_cause(cause: ProviderModelCallFailureCause) -> bool {
    matches!(
        cause,
        ProviderModelCallFailureCause::RateLimited
            | ProviderModelCallFailureCause::Overloaded
            | ProviderModelCallFailureCause::ProviderInternal
    )
}

pub(super) async fn count_turn_credential_attempts(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    credential_reference: &str,
) -> Result<usize, ModelCallRepositoryError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM model_call
          WHERE session_id = $1
            AND turn_id = $2
            AND credential_reference = $3",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(credential_reference)
    .fetch_one(&mut *connection)
    .await?;
    usize::try_from(count)
        .map_err(|_| ModelCallCorruption::Inconsistent("same-credential attempt count").into())
}

pub(super) fn availability_retry_backoff(
    cause: ProviderModelCallFailureCause,
    retry_after: Option<Duration>,
    failed_attempts: usize,
    call: ModelCallId,
) -> Duration {
    if !is_same_credential_retry_cause(cause) {
        return Duration::ZERO;
    }
    let exponent = u32::try_from(failed_attempts.saturating_sub(1).min(6)).unwrap_or(6);
    let ceiling = Duration::from_secs(1_u64 << exponent).min(MAX_EXPONENTIAL_BACKOFF);
    let sample = u64::try_from(call.as_uuid().as_u128() & 1023).unwrap_or(0);
    let ceiling_millis = u64::try_from(ceiling.as_millis()).unwrap_or(u64::MAX);
    let jittered_millis = ceiling_millis * (512 + sample) / 1024;
    let jittered = Duration::from_millis(jittered_millis);
    jittered
        .max(retry_after.unwrap_or(Duration::ZERO))
        .min(MAX_AVAILABILITY_BACKOFF)
}
