use super::reread::cancelled_terminal_closure_matches;
use super::{ModelCallCorruption, ModelCallRepositoryError};
use crate::mapping::{
    delegation_outcome_kind_to_str, delegation_outcome_reason_to_str, session_id_to_uuid,
    turn_id_to_uuid,
};
use signalbox_domain::{
    AssistantResponsePart, CorrelatedModelCallTerminalObservation, DelegationOutcomeKind,
    DelegationOutcomeReason, ModelCallTerminalObservation, SessionId, TurnId,
};
use sqlx::PgConnection;

pub(super) enum ExpectedDelegatedChildResult {
    Returned(String),
    Failed,
    Cancelled,
    ResultUnavailable,
}

struct StoredDelegatedChildResultFacts<'a> {
    outcome: DelegationOutcomeKind,
    reason: DelegationOutcomeReason,
    content: Option<&'a str>,
}

impl ExpectedDelegatedChildResult {
    fn stored_facts(&self) -> StoredDelegatedChildResultFacts<'_> {
        match self {
            Self::Returned(content) => StoredDelegatedChildResultFacts {
                outcome: DelegationOutcomeKind::ResultReturned,
                reason: DelegationOutcomeReason::ChildCompleted,
                content: Some(content.as_str()),
            },
            Self::Failed => StoredDelegatedChildResultFacts {
                outcome: DelegationOutcomeKind::ChildFailed,
                reason: DelegationOutcomeReason::ChildExecutionFailed,
                content: None,
            },
            Self::Cancelled => StoredDelegatedChildResultFacts {
                outcome: DelegationOutcomeKind::ChildCancelled,
                reason: DelegationOutcomeReason::ChildCancelled,
                content: None,
            },
            Self::ResultUnavailable => StoredDelegatedChildResultFacts {
                outcome: DelegationOutcomeKind::ChildFailed,
                reason: DelegationOutcomeReason::ChildResultUnavailable,
                content: None,
            },
        }
    }
}

pub(super) async fn delegated_observation_result_matches(
    connection: &mut PgConnection,
    session: SessionId,
    observation: &CorrelatedModelCallTerminalObservation,
) -> Result<bool, ModelCallRepositoryError> {
    let expected = match observation.observation() {
        ModelCallTerminalObservation::Completed { assistant_text } => {
            match signalbox_domain::DelegationContent::from_assistant_text(assistant_text) {
                Ok(content) => ExpectedDelegatedChildResult::Returned(content.as_str().to_owned()),
                Err(_) => ExpectedDelegatedChildResult::ResultUnavailable,
            }
        }
        ModelCallTerminalObservation::CompletedWithProviderCompaction { response, .. }
        | ModelCallTerminalObservation::CompletedWithProviderReasoning { response } => {
            let assistant_text = response
                .iter()
                .filter_map(|part| match part {
                    AssistantResponsePart::Text(text) => Some(text.clone()),
                    AssistantResponsePart::ProviderCompaction(_)
                    | AssistantResponsePart::ProviderReasoning(_) => None,
                    AssistantResponsePart::ToolCall(_) => None,
                })
                .collect::<Vec<_>>();
            match signalbox_domain::DelegationContent::from_assistant_text(&assistant_text) {
                Ok(content) => ExpectedDelegatedChildResult::Returned(content.as_str().to_owned()),
                Err(_) => ExpectedDelegatedChildResult::ResultUnavailable,
            }
        }
        ModelCallTerminalObservation::KnownFailed
        | ModelCallTerminalObservation::Refused
        | ModelCallTerminalObservation::RefusedWithProviderCompaction { .. } => {
            ExpectedDelegatedChildResult::Failed
        }
        ModelCallTerminalObservation::Cancelled => {
            if cancelled_terminal_closure_matches(connection, session, observation).await? {
                ExpectedDelegatedChildResult::Cancelled
            } else {
                ExpectedDelegatedChildResult::Failed
            }
        }
        ModelCallTerminalObservation::CompletedWithTools { .. }
        | ModelCallTerminalObservation::Ambiguous => {
            return delegated_nonterminal_result_absent(
                connection,
                session,
                observation.correlation().turn(),
            )
            .await;
        }
    };
    delegated_terminal_result_matches(
        connection,
        session,
        observation.correlation().turn(),
        &expected,
    )
    .await
}

pub(super) async fn delegated_terminal_result_matches(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    expected: &ExpectedDelegatedChildResult,
) -> Result<bool, ModelCallRepositoryError> {
    let stored = expected.stored_facts();
    let outcome_kind = delegation_outcome_kind_to_str(stored.outcome);
    let reason_kind = delegation_outcome_reason_to_str(stored.reason).ok_or(
        ModelCallCorruption::Inconsistent("delegated child result reason"),
    )?;
    Ok(sqlx::query_scalar::<_, bool>(
        "WITH delegated AS (
            SELECT task.spawning_tool_request_id,
                   task.child_session_id,
                   task.turn_id,
                   relation.parent_session_id
              FROM session_delegation_initial_task AS task
              JOIN session_delegation AS relation
                ON relation.spawning_tool_request_id = task.spawning_tool_request_id
               AND relation.child_session_id = task.child_session_id
             WHERE task.child_session_id = $1
               AND task.turn_id = $2
        ),
        origin AS (
            SELECT EXISTS (
                       SELECT 1
                         FROM turn_lifecycle
                        WHERE session_id = $1
                          AND turn_id = $2
                          AND origin_kind = 'delegation'
                   ) AS delegated,
                   EXISTS (
                       SELECT 1
                         FROM session_delegation_initial_task
                        WHERE child_session_id = $1
                          AND turn_id = $2
                   ) AS initial_task,
                   EXISTS (
                       SELECT 1
                         FROM session_delegation_wake_turn_origin
                        WHERE recipient_session_id = $1
                          AND turn_id = $2
                   ) AS wake
        )
        SELECT NOT origin.delegated
            OR (origin.wake AND NOT origin.initial_task)
            OR (
                origin.initial_task
                AND NOT origin.wake
                AND (
                    SELECT count(*) = 1
                      FROM delegated
                      JOIN session_child_result AS result
                    ON result.spawning_tool_request_id =
                       delegated.spawning_tool_request_id
                   AND result.event_kind = 'outcome_recorded'
                   AND result.outcome_kind = $3
                   AND result.content_text IS NOT DISTINCT FROM $5
                  JOIN session_delegation_event AS outcome
                    ON outcome.spawning_tool_request_id =
                       result.spawning_tool_request_id
                   AND outcome.event_ordinal = result.event_ordinal
                   AND outcome.event_kind = result.event_kind
                   AND outcome.outcome_kind = result.outcome_kind
                   AND outcome.reason_kind = $4
                   AND outcome.provenance_kind = 'child_turn'
                   AND outcome.provenance_session_id = delegated.child_session_id
                   AND outcome.provenance_turn_id = delegated.turn_id
                  JOIN delegation_update_outbox_event AS parent_update
                    ON parent_update.result_spawning_request_id =
                       delegated.spawning_tool_request_id
                   AND parent_update.session_id = delegated.parent_session_id
                   AND parent_update.update_kind = 'child_result'
                   AND parent_update.spawning_tool_request_id =
                       delegated.spawning_tool_request_id
                   AND parent_update.child_session_id = delegated.child_session_id
                   AND parent_update.outcome_kind = $3
                   AND parent_update.reason_kind = $4
                   AND parent_update.provenance_kind = 'child_turn'
                   AND parent_update.provenance_session_id =
                       delegated.child_session_id
                   AND parent_update.provenance_turn_id = delegated.turn_id
                   AND parent_update.content_text IS NOT DISTINCT FROM $5
                   AND parent_update.event_kind = 'delegation_update'
                   AND parent_update.storage_version = 1
                  JOIN delegation_outbox_event AS update_header
                    ON update_header.event_sequence = parent_update.event_sequence
                   AND update_header.event_kind = parent_update.event_kind
                   AND update_header.storage_version = parent_update.storage_version
                   AND update_header.session_id = parent_update.session_id
                   AND update_header.event_kind = 'delegation_update'
                   AND update_header.storage_version = 1
                  JOIN delegation_wake_outbox_event AS parent_wake
                    ON parent_wake.result_spawning_request_id =
                       delegated.spawning_tool_request_id
                   AND parent_wake.session_id = delegated.parent_session_id
                   AND parent_wake.spawning_tool_request_id =
                       delegated.spawning_tool_request_id
                   AND parent_wake.subject_kind = 'result'
                   AND parent_wake.awaiting_tool_request_id IS NULL
                   AND parent_wake.message_id IS NULL
                   AND parent_wake.event_kind = 'delegation_wake'
                   AND parent_wake.storage_version = 1
                  JOIN delegation_outbox_event AS wake_header
                    ON wake_header.event_sequence = parent_wake.event_sequence
                   AND wake_header.event_kind = parent_wake.event_kind
                   AND wake_header.storage_version = parent_wake.storage_version
                   AND wake_header.session_id = parent_wake.session_id
                   AND wake_header.event_kind = 'delegation_wake'
                   AND wake_header.storage_version = 1
                 WHERE NOT EXISTS (
                    SELECT 1
                      FROM session_delegation_wait AS wait
                      LEFT JOIN session_child_result_delivery AS delivery
                        ON delivery.awaiting_tool_request_id =
                           wait.awaiting_tool_request_id
                       AND delivery.spawning_tool_request_id =
                           wait.spawning_tool_request_id
                       AND delivery.parent_session_id = wait.parent_session_id
                      LEFT JOIN session_pending_delivery AS pending
                        ON pending.recipient_session_id =
                           delivery.parent_session_id
                       AND pending.delivery_sequence = delivery.delivery_sequence
                       AND pending.delivery_kind = delivery.delivery_kind
                     WHERE wait.spawning_tool_request_id =
                           delegated.spawning_tool_request_id
                       AND (
                            delivery.awaiting_tool_request_id IS NULL
                            OR wait.wait_mode IS NULL
                            OR wait.wait_mode NOT IN ('foreground', 'background')
                            OR (
                                wait.wait_mode = 'foreground'
                                AND (
                                    delivery.delivery_sequence IS NOT NULL
                                    OR delivery.delivery_kind IS NOT NULL
                                )
                            )
                            OR (
                                wait.wait_mode = 'background'
                                AND (
                                    delivery.delivery_sequence IS NULL
                                    OR delivery.delivery_kind <>
                                       'background_result'
                                    OR pending.delivery_sequence IS NULL
                                )
                            )
                       )
                 )
                   AND NOT EXISTS (
                    SELECT 1
                      FROM session_child_result_delivery AS delivery
                      LEFT JOIN session_delegation_wait AS wait
                        ON wait.awaiting_tool_request_id =
                           delivery.awaiting_tool_request_id
                       AND wait.spawning_tool_request_id =
                           delivery.spawning_tool_request_id
                       AND wait.parent_session_id = delivery.parent_session_id
                     WHERE delivery.spawning_tool_request_id =
                           delegated.spawning_tool_request_id
                       AND wait.awaiting_tool_request_id IS NULL
                 )
                )
            )
          FROM origin",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(outcome_kind)
    .bind(reason_kind)
    .bind(stored.content)
    .fetch_one(connection)
    .await?)
}

async fn delegated_nonterminal_result_absent(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<bool, ModelCallRepositoryError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "WITH delegated AS (
            SELECT relation.spawning_tool_request_id
              FROM session_delegation AS relation
              JOIN session_delegation_initial_task AS task
                ON task.spawning_tool_request_id = relation.spawning_tool_request_id
               AND task.child_session_id = relation.child_session_id
               AND task.turn_id = $2
             WHERE relation.child_session_id = $1
        ),
        origin AS (
            SELECT EXISTS (
                       SELECT 1
                         FROM turn_lifecycle
                        WHERE session_id = $1
                          AND turn_id = $2
                          AND origin_kind = 'delegation'
                   ) AS delegated,
                   EXISTS (
                       SELECT 1
                         FROM session_delegation_initial_task
                        WHERE child_session_id = $1
                          AND turn_id = $2
                   ) AS initial_task,
                   EXISTS (
                       SELECT 1
                         FROM session_delegation_wake_turn_origin
                        WHERE recipient_session_id = $1
                          AND turn_id = $2
                   ) AS wake
        )
        SELECT NOT origin.delegated
            OR (origin.wake AND NOT origin.initial_task)
            OR (
                origin.initial_task
                AND NOT origin.wake
                AND (SELECT count(*) = 1 FROM delegated)
                AND NOT EXISTS (
                    SELECT 1
                      FROM delegated
                      JOIN session_child_result AS result
                        ON result.spawning_tool_request_id =
                           delegated.spawning_tool_request_id
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM delegated
                      JOIN delegation_update_outbox_event AS parent_update
                        ON parent_update.result_spawning_request_id =
                           delegated.spawning_tool_request_id
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM delegated
                      JOIN delegation_wake_outbox_event AS parent_wake
                        ON parent_wake.result_spawning_request_id =
                           delegated.spawning_tool_request_id
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM delegated
                      JOIN session_child_result_delivery AS delivery
                        ON delivery.spawning_tool_request_id =
                           delegated.spawning_tool_request_id
                )
            )
          FROM origin",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_one(connection)
    .await?)
}
