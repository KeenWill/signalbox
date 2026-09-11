use super::delegation_lock::{
    load_delegation_terminal_relation, lock_delegated_child_result_frontier,
};
use super::persist_disposition::{
    insert_snapshot, persist_ambiguous, persist_cancelled, persist_cancelled_tool_round,
    persist_completed, persist_ended_attempt, persist_ended_call, persist_failed,
    persist_reclassified_pending_steering, persist_refused,
};
use super::persist_tool_round::persist_tool_round;
use super::prepared::map_tool_evidence_error;
use super::{
    ModelCallCorruption, ModelCallRepositoryError, append_terminal_call_event, require_single,
    terminalize_lifecycle,
};
use crate::mapping::{
    DelegationUpdateStorageKind, DelegationWakeStorageKind, delegation_outcome_kind_to_str,
    delegation_outcome_reason_to_str, delegation_update_kind_to_str,
    delegation_wake_subject_to_str, durable_command_id_to_uuid, session_id_to_uuid,
    turn_id_to_uuid, turn_terminal_cause_to_str,
};
use crate::outbox;
use crate::outbox::{ModelCallOutboxState, OutboxEvent, TurnTerminalOutboxDisposition};
use rust_decimal::Decimal;
use signalbox_application::AttachmentPreparationFailure;
use signalbox_domain::{
    AuthorizedModelCall, DelegationContent, DelegationOutcome, DelegationOutcomeKind,
    DelegationOutcomeReason, FailedModelCallTurn, ModelCallTerminalOutcome,
    ProviderModelCallFailureCause, ProviderReportedTokenUsage, ReconciliationRequiredModelCallTurn,
    ReconciliationRequiredToolTurn, ResolvedProviderTarget, StopRequestedModelCallTurn,
    TurnTerminalCause,
};
use sqlx::PgConnection;
use sqlx::types::Uuid;

pub(super) async fn persist_authorization(
    connection: &mut PgConnection,
    authorized: &AuthorizedModelCall,
    effective_target: ResolvedProviderTarget,
) -> Result<(), ModelCallRepositoryError> {
    let attempt_rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'running'
          WHERE turn_attempt_id = $1
            AND turn_id = $2
            AND session_id = $3
            AND state_kind IN ('prepared', 'running')
            AND end_variant IS NULL
            AND end_disposition IS NULL",
    )
    .bind(authorized.attempt().id().into_uuid())
    .bind(turn_id_to_uuid(authorized.turn()))
    .bind(session_id_to_uuid(authorized.session()))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    let call_rows = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'in_flight',
                effective_provider_model_identity_id = $5
          WHERE model_call_id = $1
            AND turn_id = $2
            AND session_id = $3
            AND turn_attempt_id = $4
            AND state_kind = 'prepared'
            AND terminal_disposition_kind IS NULL",
    )
    .bind(authorized.call().id().into_uuid())
    .bind(turn_id_to_uuid(authorized.turn()))
    .bind(session_id_to_uuid(authorized.session()))
    .bind(authorized.attempt().id().into_uuid())
    .bind(effective_target.identity().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(attempt_rows, "send-authorization attempt")?;
    require_single(call_rows, "send-authorization call")?;
    outbox::append(
        connection,
        OutboxEvent::ModelCallTransition {
            session: authorized.session(),
            turn: authorized.turn(),
            call: authorized.call().id(),
            state: ModelCallOutboxState::InFlight,
        },
    )
    .await?;
    Ok(())
}

pub(crate) async fn persist_stop_requested(
    connection: &mut PgConnection,
    stopped: &StopRequestedModelCallTurn,
) -> Result<(), ModelCallRepositoryError> {
    let proof = stopped.interrupt();
    let attempt_rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'stop_requested',
                interrupt_command_id = $1,
                interrupt_predecessor_turn_id = $2
          WHERE turn_attempt_id = $3
            AND turn_id = $4
            AND session_id = $5
            AND state_kind = 'running'
            AND end_variant IS NULL
            AND end_disposition IS NULL
            AND interrupt_command_id IS NULL
            AND interrupt_predecessor_turn_id IS NULL",
    )
    .bind(durable_command_id_to_uuid(proof.command()))
    .bind(turn_id_to_uuid(proof.predecessor()))
    .bind(stopped.attempt().id().into_uuid())
    .bind(turn_id_to_uuid(stopped.turn()))
    .bind(session_id_to_uuid(stopped.session()))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(attempt_rows, "stop-requested turn attempt")?;

    let call_rows = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'cancellation_requested'
          WHERE model_call_id = $1
            AND turn_id = $2
            AND session_id = $3
            AND turn_attempt_id = $4
            AND state_kind = 'in_flight'
            AND terminal_disposition_kind IS NULL",
    )
    .bind(stopped.call().id().into_uuid())
    .bind(turn_id_to_uuid(stopped.turn()))
    .bind(session_id_to_uuid(stopped.session()))
    .bind(stopped.attempt().id().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(call_rows, "cancellation-requested model call")?;
    outbox::append(
        connection,
        OutboxEvent::ModelCallTransition {
            session: stopped.session(),
            turn: stopped.turn(),
            call: stopped.call().id(),
            state: ModelCallOutboxState::CancellationRequested,
        },
    )
    .await?;
    Ok(())
}

/// Persists one terminal outcome, recording `failure_cause` when the outcome
/// is `Failed`.
///
/// The cause is optional because two callers construct only non-failing
/// outcomes; a `Failed` outcome that names none is a typed corruption rather
/// than a silently unclassified terminalization.
pub(crate) async fn persist_terminal_outcome(
    connection: &mut PgConnection,
    outcome: &ModelCallTerminalOutcome,
    failure_cause: Option<TurnTerminalCause>,
) -> Result<(), ModelCallRepositoryError> {
    persist_terminal_outcome_with_usage(
        connection,
        outcome,
        failure_cause,
        ProviderReportedTokenUsage::unreported(),
        None,
        None,
        None,
    )
    .await
}

pub(super) async fn persist_terminal_outcome_with_usage(
    connection: &mut PgConnection,
    outcome: &ModelCallTerminalOutcome,
    failure_cause: Option<TurnTerminalCause>,
    usage: ProviderReportedTokenUsage,
    provider_failure_cause: Option<ProviderModelCallFailureCause>,
    retained_input_tokens: Option<u64>,
    retained_output_tokens: Option<u64>,
) -> Result<(), ModelCallRepositoryError> {
    match outcome {
        ModelCallTerminalOutcome::Completed(completed) => {
            lock_delegated_child_result_frontier(connection, completed.session(), completed.turn())
                .await?;
            persist_completed(
                connection,
                completed,
                usage,
                retained_input_tokens,
                retained_output_tokens,
            )
            .await?;
            persist_delegated_child_result(
                connection,
                &DelegationOutcome::from_completed_child(completed),
            )
            .await
        }
        ModelCallTerminalOutcome::ToolRound(round) => {
            persist_tool_round(
                connection,
                round,
                usage,
                retained_input_tokens,
                retained_output_tokens,
            )
            .await
        }
        ModelCallTerminalOutcome::CancelledWithToolResponse(cancelled) => {
            lock_delegated_child_result_frontier(connection, cancelled.session(), cancelled.turn())
                .await?;
            persist_cancelled_tool_round(
                connection,
                cancelled,
                usage,
                retained_input_tokens,
                retained_output_tokens,
            )
            .await?;
            persist_delegated_child_result(
                connection,
                &DelegationOutcome::from_cancelled_tool_round_child(cancelled),
            )
            .await
        }
        ModelCallTerminalOutcome::Failed(failed) => {
            let cause = failure_cause.ok_or(ModelCallCorruption::Inconsistent(
                "failed terminal outcome without a terminal cause",
            ))?;
            persist_failed_with_delegated_child_result(
                connection,
                failed,
                cause,
                usage,
                provider_failure_cause,
                None,
            )
            .await
        }
        ModelCallTerminalOutcome::Cancelled(cancelled) => {
            lock_delegated_child_result_frontier(connection, cancelled.session(), cancelled.turn())
                .await?;
            persist_cancelled(connection, cancelled, usage).await?;
            persist_delegated_child_result(
                connection,
                &DelegationOutcome::from_cancelled_child(cancelled),
            )
            .await
        }
        ModelCallTerminalOutcome::Refused(refused) => {
            lock_delegated_child_result_frontier(connection, refused.session(), refused.turn())
                .await?;
            persist_refused(
                connection,
                refused,
                usage,
                retained_input_tokens,
                retained_output_tokens,
            )
            .await?;
            persist_delegated_child_result(
                connection,
                &DelegationOutcome::from_refused_child(refused),
            )
            .await
        }
        ModelCallTerminalOutcome::ReconciliationRequired(reconciliation) => {
            persist_reconciliation_required(connection, reconciliation, usage).await
        }
        ModelCallTerminalOutcome::AwaitingRecovery(ambiguous) => {
            persist_ambiguous(connection, ambiguous, usage).await
        }
    }
}

pub(super) async fn persist_failed_with_delegated_child_result(
    connection: &mut PgConnection,
    failed: &FailedModelCallTurn,
    cause: TurnTerminalCause,
    usage: ProviderReportedTokenUsage,
    provider_failure_cause: Option<ProviderModelCallFailureCause>,
    attachment_failure: Option<AttachmentPreparationFailure>,
) -> Result<(), ModelCallRepositoryError> {
    lock_delegated_child_result_frontier(connection, failed.session(), failed.turn()).await?;
    persist_failed(
        connection,
        failed,
        cause,
        usage,
        provider_failure_cause,
        attachment_failure,
    )
    .await?;
    persist_delegated_child_result(connection, &DelegationOutcome::from_failed_child(failed)).await
}

/// Encodes the durable evidence a definitive attachment-preparation failure
/// carries into the statement that terminalizes its call.
///
/// `model_call_changes_are_guarded` raises on every update whose OLD row is
/// already terminal, so this evidence is only writable by the same
/// Prepared-to-terminal `UPDATE` that closes the call; a follow-up statement
/// would abort the whole failure transaction and leave the call open.
/// `Unavailable` is retryable and so never terminalizes a prepared call.
pub(super) fn encode_attachment_preparation_failure(
    failure: AttachmentPreparationFailure,
) -> Result<(&'static str, Option<Decimal>), ModelCallRepositoryError> {
    match failure {
        AttachmentPreparationFailure::TooLarge { maximum_bytes } => {
            Ok(("too_large", Some(Decimal::from(maximum_bytes))))
        }
        AttachmentPreparationFailure::Missing => Ok(("missing", None)),
        AttachmentPreparationFailure::Corrupt => Ok(("corrupt", None)),
        AttachmentPreparationFailure::Unavailable => {
            Err(ModelCallRepositoryError::InvalidTransition(
                "retryable attachment unavailability cannot terminalize a prepared call",
            ))
        }
    }
}

pub(crate) async fn persist_automatic_reconciliation(
    connection: &mut PgConnection,
    reconciliation: &ReconciliationRequiredModelCallTurn,
) -> Result<(), ModelCallRepositoryError> {
    lock_delegated_child_result_frontier(
        connection,
        reconciliation.session(),
        reconciliation.turn(),
    )
    .await?;
    persist_reconciliation_required(
        connection,
        reconciliation,
        ProviderReportedTokenUsage::unreported(),
    )
    .await?;
    persist_delegated_child_result(
        connection,
        &DelegationOutcome::from_reconciliation_required_child(reconciliation),
    )
    .await
}

pub(crate) async fn persist_automatic_tool_reconciliation(
    connection: &mut PgConnection,
    reconciliation: &ReconciliationRequiredToolTurn,
) -> Result<(), ModelCallRepositoryError> {
    lock_delegated_child_result_frontier(
        connection,
        reconciliation.session(),
        reconciliation.turn(),
    )
    .await?;
    persist_tool_reconciliation_required(connection, reconciliation).await?;
    persist_delegated_child_result(
        connection,
        &DelegationOutcome::from_tool_reconciliation_required_child(reconciliation),
    )
    .await
}

async fn persist_delegated_child_result(
    connection: &mut PgConnection,
    outcome: &DelegationOutcome,
) -> Result<(), ModelCallRepositoryError> {
    let (child, turn) =
        outcome
            .provenance()
            .child_turn()
            .ok_or(ModelCallCorruption::Inconsistent(
                "delegated child result provenance",
            ))?;
    // The match admits only the outcome/reason pairings a terminal child result
    // may carry; the spellings themselves come from the canonical encoders, so
    // this states the pairing rule without restating the durable vocabulary.
    let (outcome_pairing, reason_pairing) = match (outcome.kind(), outcome.reason()) {
        pairing @ (
            DelegationOutcomeKind::ResultReturned,
            DelegationOutcomeReason::ChildCompleted,
        )
        | pairing @ (
            DelegationOutcomeKind::ChildFailed,
            DelegationOutcomeReason::ChildExecutionFailed,
        )
        | pairing @ (
            DelegationOutcomeKind::ChildFailed,
            DelegationOutcomeReason::ChildResultUnavailable,
        )
        | pairing @ (
            DelegationOutcomeKind::ChildCancelled,
            DelegationOutcomeReason::ChildCancelled,
        ) => pairing,
        _ => {
            return Err(
                ModelCallCorruption::Inconsistent("terminal delegated child outcome").into(),
            );
        }
    };
    let outcome_kind = delegation_outcome_kind_to_str(outcome_pairing);
    let reason_kind = delegation_outcome_reason_to_str(reason_pairing).ok_or(
        ModelCallCorruption::Inconsistent("terminal delegated child outcome"),
    )?;
    let content = outcome.content().map(DelegationContent::as_str);
    let relation = load_delegation_terminal_relation(
        connection,
        crate::lock_inventory::DELEGATION_TERMINAL_RELATION,
        session_id_to_uuid(child),
        turn_id_to_uuid(turn),
    )
    .await?;
    let Some(relation) = relation else {
        return Ok(());
    };
    let spawning_request = relation.spawning_tool_request_id;
    let parent = relation.parent_session_id;
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1 FROM session_child_result
             WHERE spawning_tool_request_id = $1
        )",
    )
    .bind(relation.spawning_tool_request_id)
    .fetch_one(&mut *connection)
    .await?
    {
        return Ok(());
    }
    let event_ordinal = sqlx::query_scalar::<_, Decimal>(
        "SELECT COALESCE(max(event_ordinal), 0) + 1
           FROM session_delegation_event
          WHERE spawning_tool_request_id = $1",
    )
    .bind(spawning_request)
    .fetch_one(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id)
         VALUES ($1, $2, 'outcome_recorded', $3,
                 $4, 'child_turn', $5, $6)",
    )
    .bind(spawning_request)
    .bind(event_ordinal)
    .bind(outcome_kind)
    .bind(reason_kind)
    .bind(session_id_to_uuid(child))
    .bind(turn_id_to_uuid(turn))
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_child_result
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, content_text)
         VALUES ($1, $2, 'outcome_recorded', $3, $4)",
    )
    .bind(spawning_request)
    .bind(event_ordinal)
    .bind(outcome_kind)
    .bind(content)
    .execute(&mut *connection)
    .await?;

    let waits = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT awaiting_tool_request_id, wait_mode
           FROM session_delegation_wait
          WHERE spawning_tool_request_id = $1
          ORDER BY awaiting_tool_request_id",
    )
    .bind(spawning_request)
    .fetch_all(&mut *connection)
    .await?;
    for (awaiting_request, mode) in waits {
        let delivery_sequence = match mode.as_str() {
            "foreground" => None,
            "background" => Some(
                sqlx::query_scalar::<_, Decimal>(
                    "WITH next_delivery AS (
                        SELECT COALESCE(max(delivery_sequence), 0) + 1 AS sequence
                          FROM session_pending_delivery
                         WHERE recipient_session_id = $1
                     )
                     INSERT INTO session_pending_delivery
                        (recipient_session_id, delivery_sequence, delivery_kind)
                     SELECT $1, sequence, 'background_result'
                       FROM next_delivery
                     RETURNING delivery_sequence",
                )
                .bind(parent)
                .fetch_one(&mut *connection)
                .await?,
            ),
            value => {
                return Err(ModelCallCorruption::Unsupported {
                    field: "delegation wait mode",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        sqlx::query(
            "INSERT INTO session_child_result_delivery
                (awaiting_tool_request_id, spawning_tool_request_id,
                 parent_session_id, delivery_sequence, delivery_kind)
             VALUES ($1, $2, $3, $4,
                     CASE WHEN $4::numeric IS NULL
                          THEN NULL ELSE 'background_result' END)",
        )
        .bind(awaiting_request)
        .bind(relation.spawning_tool_request_id)
        .bind(relation.parent_session_id)
        .bind(delivery_sequence)
        .execute(&mut *connection)
        .await?;
    }

    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event
                (event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, child_session_id,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id,
             result_spawning_request_id, content_text)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $8::text, $2, $3, $4, $5,
                'child_turn', $3, $6, $2, $7
           FROM header",
    )
    .bind(parent)
    .bind(spawning_request)
    .bind(session_id_to_uuid(child))
    .bind(outcome_kind)
    .bind(reason_kind)
    .bind(turn_id_to_uuid(turn))
    .bind(content)
    .bind(delegation_update_kind_to_str(
        DelegationUpdateStorageKind::ChildResult,
    ))
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event
                (event_kind, storage_version, session_id)
            VALUES ('delegation_wake', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_wake_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             spawning_tool_request_id, subject_kind,
             result_spawning_request_id, awaiting_tool_request_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $2, $3::text, $2, NULL
           FROM header",
    )
    .bind(parent)
    .bind(spawning_request)
    .bind(delegation_wake_subject_to_str(
        DelegationWakeStorageKind::Result,
    ))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(crate) async fn persist_reconciliation_required(
    connection: &mut PgConnection,
    reconciliation: &ReconciliationRequiredModelCallTurn,
    usage: ProviderReportedTokenUsage,
) -> Result<(), ModelCallRepositoryError> {
    let call_already_ambiguous = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM model_call
             WHERE model_call_id = $1
               AND turn_id = $2
               AND session_id = $3
               AND turn_attempt_id = $4
               AND state_kind = 'terminal'
               AND terminal_disposition_kind = 'ambiguous'
        )",
    )
    .bind(reconciliation.call().id().into_uuid())
    .bind(turn_id_to_uuid(reconciliation.turn()))
    .bind(session_id_to_uuid(reconciliation.session()))
    .bind(reconciliation.attempt().id().into_uuid())
    .fetch_one(&mut *connection)
    .await?;
    if !call_already_ambiguous {
        persist_ended_call(
            connection,
            reconciliation.session(),
            reconciliation.turn(),
            reconciliation.call(),
            usage,
        )
        .await?;
    }
    if !call_already_ambiguous {
        persist_ended_attempt(
            connection,
            reconciliation.session(),
            reconciliation.turn(),
            reconciliation.attempt(),
        )
        .await?;
    }
    insert_snapshot(connection, reconciliation.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        reconciliation.session(),
        reconciliation.turn(),
        reconciliation.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        reconciliation.session(),
        reconciliation.turn(),
        "reconciliation_required",
        TurnTerminalCause::ModelCallAmbiguous,
        reconciliation.terminal_snapshot().frontier().snapshot(),
        Some(reconciliation.attempt().id()),
        Some(reconciliation.call().id()),
    )
    .await?;
    if !call_already_ambiguous {
        append_terminal_call_event(
            connection,
            reconciliation.session(),
            reconciliation.turn(),
            reconciliation.call(),
        )
        .await?;
    }
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session: reconciliation.session(),
            turn: reconciliation.turn(),
            disposition: TurnTerminalOutboxDisposition::ModelCallReconciliationRequired {
                call: reconciliation.call().id(),
                terminal_frontier: reconciliation.terminal_snapshot().frontier().snapshot(),
            },
        },
    )
    .await?;
    Ok(())
}

pub(crate) async fn persist_tool_reconciliation_required(
    connection: &mut PgConnection,
    reconciliation: &ReconciliationRequiredToolTurn,
) -> Result<(), ModelCallRepositoryError> {
    crate::tool_loop::persist_result_entry_slice(connection, reconciliation.tool_result_entries())
        .await
        .map_err(map_tool_evidence_error)?;
    insert_snapshot(connection, reconciliation.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        reconciliation.session(),
        reconciliation.turn(),
        reconciliation.reclassified_pending_steering(),
    )
    .await?;
    super::retire_terminal_batch_replacement(
        connection,
        reconciliation.session(),
        reconciliation.turn(),
    )
    .await?;
    let rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = $1,
                active_phase_kind = NULL,
                current_attempt_id = NULL,
                recovery_model_call_id = NULL,
                active_tool_round_call_id = NULL,
                approval_tool_request_id = NULL,
                recovery_tool_attempt_id = NULL,
                runner_recovery_runner_id = NULL,
                runner_recovery_placement_revision = NULL,
                runner_recovery_tool_attempt_id = NULL,
                terminal_attempt_id = $2,
                terminal_model_call_id = NULL,
                terminal_tool_attempt_id = $3,
                terminal_disposition_kind = 'reconciliation_required',
                terminal_cause_kind = $6
          WHERE turn_id = $4
            AND session_id = $5
            AND state_kind = 'active'
            AND (
                (
                    active_phase_kind = 'awaiting_tool_recovery'
                    AND current_attempt_id = $2
                    AND recovery_tool_attempt_id = $3
                )
                OR (
                    active_phase_kind = 'awaiting_runner_recovery'
                    AND current_attempt_id IS NULL
                    AND runner_recovery_tool_attempt_id = $3
                    AND EXISTS (
                        SELECT 1
                          FROM turn_attempt AS yielded_attempt
                         WHERE yielded_attempt.turn_attempt_id = $2
                           AND yielded_attempt.turn_id = turn_lifecycle.turn_id
                           AND yielded_attempt.session_id = turn_lifecycle.session_id
                           AND yielded_attempt.state_kind = 'ended'
                           AND yielded_attempt.end_variant = 'without_stop'
                           AND yielded_attempt.end_disposition =
                                'yielded_to_durable_wait'
                           AND NOT EXISTS (
                                SELECT 1
                                  FROM turn_attempt AS continuation
                                 WHERE continuation.continued_from_attempt_id =
                                        yielded_attempt.turn_attempt_id
                           )
                    )
                )
            )",
    )
    .bind(
        reconciliation
            .terminal_snapshot()
            .frontier()
            .snapshot()
            .into_uuid(),
    )
    .bind(reconciliation.attempt().id().into_uuid())
    .bind(reconciliation.tool_attempt().attempt().into_uuid())
    .bind(turn_id_to_uuid(reconciliation.turn()))
    .bind(session_id_to_uuid(reconciliation.session()))
    .bind(turn_terminal_cause_to_str(
        TurnTerminalCause::ToolAttemptAmbiguous,
    ))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "terminal tool-reconciliation lifecycle")?;
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session: reconciliation.session(),
            turn: reconciliation.turn(),
            disposition: TurnTerminalOutboxDisposition::ToolAttemptReconciliationRequired {
                attempt: reconciliation.tool_attempt().attempt(),
                terminal_frontier: reconciliation.terminal_snapshot().frontier().snapshot(),
            },
        },
    )
    .await?;
    Ok(())
}
