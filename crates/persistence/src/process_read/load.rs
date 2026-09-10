use super::entry::decode_transcript_entry;
use super::reader::ProcessTranscriptReader;
use super::session::{
    PendingSessionSummary, ProcessModelSelection, ProcessRunnerConnectionHealth,
    ProcessRunnerProjection, ProcessRunnerProjectionState,
};
use super::transcript_types::{ProcessSessionAncestry, ProcessTranscriptEntry, ProcessTurnState};
use super::turn_facts::{
    DecodedTurn, load_execution_lineage_tip, load_terminal_model_call_count,
    load_transcript_turn_count,
};
use super::{
    ProcessReadCorruption, ProcessReadError, decode_nonnegative, decode_positive, required,
};
use crate::mapping::{runner_sandbox_from_str, session_id_from_uuid, session_id_to_uuid};
use crate::outbox::{
    DispatchedDelegationOutcome, DispatchedDelegationProvenance, DispatchedDelegationReason,
    decode_delegation_outcome, decode_delegation_provenance, decode_delegation_reason,
};
use rust_decimal::Decimal;
use signalbox_domain::{
    ContextFrontierId, CredentialProfileName, DirectModelSelection, ModelAlias,
    RunnerCapabilityClass, RunnerGeneration, RunnerId, RunnerSelector, RunnerWorkingDirectory,
    SessionId, ToolRequestId, VersionedSessionPlacement, WorkspaceRepositoryKey,
};
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;
use sqlx::{Postgres, Row, Transaction};
use std::collections::HashMap;

pub(super) async fn load_process_session_placement(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<Option<VersionedSessionPlacement>, ProcessReadError> {
    crate::session_placement::load_current(transaction, session)
        .await
        .map_err(map_session_placement_read_error)
}

pub(super) fn map_session_placement_read_error(
    error: crate::session_placement::SessionPlacementRepositoryError,
) -> ProcessReadError {
    use crate::session_placement::SessionPlacementRepositoryError;

    match error {
        SessionPlacementRepositoryError::Database(error)
        | SessionPlacementRepositoryError::CommitAmbiguous(error) => {
            ProcessReadError::Database(error)
        }
        SessionPlacementRepositoryError::InvalidCommandId
        | SessionPlacementRepositoryError::Corruption(_) => {
            ProcessReadCorruption::Inconsistent("session placement").into()
        }
    }
}

pub(super) async fn open_transcript_in_transaction(
    mut transaction: Transaction<'static, Postgres>,
    requested_session: SessionId,
    automatic_reconciliation_attempt_budget: Option<Option<u32>>,
) -> Result<ProcessTranscriptReader, ProcessReadError> {
    let stored_cursor: Option<Decimal> = sqlx::query_scalar(
        "SELECT last_sequence
               FROM outbox_sequence_state
              WHERE singleton",
    )
    .fetch_optional(&mut *transaction)
    .await?;
    let cursor = decode_nonnegative(
        stored_cursor.ok_or(ProcessReadCorruption::Missing("outbox sequence state"))?,
        "outbox cursor",
    )?;
    let runner = load_process_runner_projection(&mut transaction, requested_session).await?;
    let lineage_tip = load_execution_lineage_tip(&mut transaction, requested_session).await?;
    // Imported seed integrity remains fail-closed on every transcript open:
    // native lineage supersedes the seed as the rendered frontier, not as an
    // integrity fact.
    let imported_seed =
        load_checked_imported_seed_frontier(&mut transaction, requested_session).await?;
    let expected_turn_count =
        load_transcript_turn_count(&mut transaction, requested_session).await?;
    let expected_model_call_count =
        load_terminal_model_call_count(&mut transaction, requested_session).await?;
    let workspace_root_kind =
        crate::session_workspace::read_binding(&mut transaction, requested_session).await?;
    Ok(ProcessTranscriptReader {
        workspace_root_kind,
        transaction: Some(transaction),
        session: requested_session,
        cursor,
        runner,
        lineage_tip,
        latest_frontier: if lineage_tip.is_none() {
            imported_seed
        } else {
            None
        },
        expected_turn_count,
        turn_count: 0,
        next_turn_after: None,
        turns_complete: false,
        expected_model_call_count,
        model_call_count: 0,
        next_model_call_after: None,
        model_calls_complete: false,
        entry_count: None,
        next_entry_index: 0,
        summary: None,
        automatic_reconciliation_attempt_budget,
    })
}

pub(crate) async fn load_process_runner_projection(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<Option<ProcessRunnerProjection>, ProcessReadError> {
    Ok(
        load_process_runner_projection_batch(transaction, &[session])
            .await?
            .remove(&session.into_uuid()),
    )
}

pub(super) async fn load_process_runner_projection_batch(
    transaction: &mut Transaction<'static, Postgres>,
    sessions: &[SessionId],
) -> Result<HashMap<Uuid, ProcessRunnerProjection>, ProcessReadError> {
    let sessions: Vec<_> = sessions.iter().map(|session| session.into_uuid()).collect();
    let rows = sqlx::query(
        "SELECT placement.session_id, placement.selector_kind, placement.selector_runner_id,
                placement.selector_capability_class,
                placement.directory_selection_kind,
                placement.requested_working_directory,
                placement.requested_credential_profile_name,
                placement.workspace_requirement_kind,
                placement.requested_repository_key,
                placement.requested_sandbox_profile,
                placement.placement_revision, placement.state_kind,
                placement.pinned_runner_id, placement.lost_runner_id,
                placement.loss_source_kind,
                connection.state_kind AS connection_state_kind
           FROM runner_current_session_placement AS current_placement
           JOIN runner_session_placement_record AS placement
             ON placement.session_id = current_placement.session_id
            AND placement.event_ordinal = current_placement.event_ordinal
           LEFT JOIN LATERAL (
                SELECT state_kind
                  FROM runner_connection_event
                 WHERE enrollment_id = placement.registration_enrollment_id
                 ORDER BY connection_epoch DESC, event_ordinal DESC
                 LIMIT 1
           ) AS connection ON placement.state_kind = 'pinned'
          WHERE current_placement.session_id = ANY($1)",
    )
    .bind(sessions)
    .fetch_all(&mut **transaction)
    .await?;
    rows.iter()
        .map(|row| {
            Ok((
                required(row, "session_id")?,
                decode_process_runner_projection(row)?,
            ))
        })
        .collect()
}

fn decode_process_runner_projection(
    row: &PgRow,
) -> Result<ProcessRunnerProjection, ProcessReadError> {
    let selector_kind: String = required(row, "selector_kind")?;
    let selector_runner: Option<Uuid> = row.try_get("selector_runner_id")?;
    let selector_capability: Option<String> = row.try_get("selector_capability_class")?;
    let selector = match (selector_kind.as_str(), selector_runner, selector_capability) {
        ("identity", Some(runner), None) => RunnerSelector::Identity(RunnerId::from_uuid(runner)),
        ("capability_class", None, Some(capability)) => RunnerSelector::CapabilityClass(
            RunnerCapabilityClass::try_new(capability)
                .map_err(|_| ProcessReadCorruption::Inconsistent("runner selector"))?,
        ),
        _ => return Err(ProcessReadCorruption::Inconsistent("runner selector").into()),
    };

    let directory_kind: String = required(row, "directory_selection_kind")?;
    let requested_directory: Option<String> = row.try_get("requested_working_directory")?;
    let working_directory = match (directory_kind.as_str(), requested_directory) {
        ("runner_default", None) => None,
        ("exact", Some(directory)) => Some(
            RunnerWorkingDirectory::try_new(directory)
                .map_err(|_| ProcessReadCorruption::Inconsistent("runner working directory"))?,
        ),
        _ => {
            return Err(
                ProcessReadCorruption::Inconsistent("runner working directory selection").into(),
            );
        }
    };

    let workspace_kind: String = required(row, "workspace_requirement_kind")?;
    let requested_repository: Option<String> = row.try_get("requested_repository_key")?;
    let repository = match (workspace_kind.as_str(), requested_repository) {
        ("none", None) => None,
        ("repository_worktree", Some(repository)) => Some(
            WorkspaceRepositoryKey::try_new(repository)
                .map_err(|_| ProcessReadCorruption::Inconsistent("runner repository key"))?,
        ),
        _ => return Err(ProcessReadCorruption::Inconsistent("runner workspace request").into()),
    };

    let credential_profile = row
        .try_get::<Option<String>, _>("requested_credential_profile_name")?
        .map(CredentialProfileName::try_new)
        .transpose()
        .map_err(|_| ProcessReadCorruption::Inconsistent("runner credential profile"))?;
    let sandbox_name: String = required(row, "requested_sandbox_profile")?;
    let sandbox =
        runner_sandbox_from_str(&sandbox_name).ok_or(ProcessReadCorruption::Unsupported {
            field: "runner sandbox profile",
            value: sandbox_name,
        })?;
    let placement_revision = RunnerGeneration::try_from_u64(decode_positive(
        required(row, "placement_revision")?,
        "runner placement revision",
    )?)
    .ok_or(ProcessReadCorruption::InvalidOrdinal(
        "runner placement revision",
    ))?;

    let state_kind: String = required(row, "state_kind")?;
    let pinned_runner = row
        .try_get::<Option<Uuid>, _>("pinned_runner_id")?
        .map(RunnerId::from_uuid);
    let lost_runner = row
        .try_get::<Option<Uuid>, _>("lost_runner_id")?
        .map(RunnerId::from_uuid);
    let loss_source: Option<String> = row.try_get("loss_source_kind")?;
    let connection_state: Option<String> = row.try_get("connection_state_kind")?;
    let (runner, state) = match (
        state_kind.as_str(),
        pinned_runner,
        lost_runner,
        loss_source.as_deref(),
    ) {
        ("unpinned", None, None, None) => (None, ProcessRunnerProjectionState::Unpinned),
        ("pinned", Some(runner), None, None) => {
            (Some(runner), ProcessRunnerProjectionState::Pinned)
        }
        ("runner_lost_before_pin", None, Some(runner), None) => (
            Some(runner),
            ProcessRunnerProjectionState::RunnerLostBeforePin,
        ),
        ("runner_lost", Some(pinned), Some(lost), Some("connection" | "registration"))
            if pinned == lost =>
        {
            (Some(lost), ProcessRunnerProjectionState::RunnerLost)
        }
        ("runner_abandoned", None, Some(lost), None) => {
            (Some(lost), ProcessRunnerProjectionState::RunnerAbandoned)
        }
        ("runner_abandoned", Some(pinned), Some(lost), Some("connection" | "registration"))
            if pinned == lost =>
        {
            (Some(lost), ProcessRunnerProjectionState::RunnerAbandoned)
        }
        _ => return Err(ProcessReadCorruption::Inconsistent("runner placement state").into()),
    };
    let connection_health = match (state, connection_state.as_deref()) {
        (ProcessRunnerProjectionState::Pinned, Some("connected")) => {
            Some(ProcessRunnerConnectionHealth::Connected)
        }
        (ProcessRunnerProjectionState::Pinned, Some("suspect")) => {
            Some(ProcessRunnerConnectionHealth::Suspect)
        }
        (ProcessRunnerProjectionState::Pinned, Some("shutdown")) => {
            Some(ProcessRunnerConnectionHealth::Shutdown)
        }
        (ProcessRunnerProjectionState::Pinned, Some("lost")) => {
            Some(ProcessRunnerConnectionHealth::Lost)
        }
        (
            ProcessRunnerProjectionState::Unpinned
            | ProcessRunnerProjectionState::RunnerLostBeforePin
            | ProcessRunnerProjectionState::RunnerLost
            | ProcessRunnerProjectionState::RunnerAbandoned,
            None,
        ) => None,
        _ => return Err(ProcessReadCorruption::Inconsistent("runner connection health").into()),
    };

    Ok(ProcessRunnerProjection {
        selector,
        runner,
        placement_revision,
        sandbox,
        credential_profile,
        repository,
        working_directory,
        connection_health,
        state,
    })
}

pub(super) fn decode_process_session_ancestry(
    row: &PgRow,
) -> Result<ProcessSessionAncestry, ProcessReadError> {
    let ancestry: String = required(row, "ancestry_kind")?;
    match ancestry.as_str() {
        "none" => Ok(ProcessSessionAncestry::UserInitiated),
        "imported_conversation" => Ok(ProcessSessionAncestry::ImportedConversation),
        _ => Err(ProcessReadCorruption::Unsupported {
            field: "session ancestry kind",
            value: ancestry,
        }
        .into()),
    }
}

async fn load_checked_imported_seed_frontier(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<Option<ContextFrontierId>, ProcessReadError> {
    sqlx::query("SELECT assert_imported_session_seed_complete($1)")
        .bind(session_id_to_uuid(session))
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_seed_validation_error)?;

    let row = sqlx::query(
        "SELECT
            session_row.ancestry_kind,
            seed.seed_context_frontier_id
           FROM session AS session_row
           LEFT JOIN imported_session_seed AS seed
             ON seed.session_id = session_row.session_id
          WHERE session_row.session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut **transaction)
    .await?;
    let ancestry = decode_process_session_ancestry(&row)?;
    let seed: Option<Uuid> = row.try_get("seed_context_frontier_id")?;
    match (ancestry, seed) {
        (ProcessSessionAncestry::UserInitiated, None) => Ok(None),
        (ProcessSessionAncestry::ImportedConversation, Some(frontier)) => {
            Ok(Some(ContextFrontierId::from_uuid(frontier)))
        }
        _ => Err(ProcessReadCorruption::Inconsistent("imported session seed shape").into()),
    }
}

fn map_seed_validation_error(error: sqlx::Error) -> ProcessReadError {
    let is_integrity_failure = error.as_database_error().is_some_and(|database| {
        matches!(
            database.code().as_deref(),
            Some("23000" | "23502" | "23503" | "23505" | "23514")
        )
    });
    if is_integrity_failure {
        ProcessReadCorruption::Inconsistent("imported session seed").into()
    } else {
        error.into()
    }
}

pub(super) fn decode_pending_session_summary(
    row: &PgRow,
    placement: VersionedSessionPlacement,
) -> Result<PendingSessionSummary, ProcessReadError> {
    let session = session_id_from_uuid(required(row, "session_id")?);
    let defaults_version = decode_positive(
        required(row, "defaults_version")?,
        "current defaults version",
    )?;
    let kind: String = required(row, "model_selection_kind")?;
    let direct: Option<Uuid> = row.try_get("direct_model_selection_id")?;
    let alias: Option<Uuid> = row.try_get("model_alias_id")?;
    let model_selection = match (kind.as_str(), direct, alias) {
        ("direct", Some(selection), None) => {
            ProcessModelSelection::Direct(DirectModelSelection::from_uuid(selection))
        }
        ("alias", None, Some(alias)) => ProcessModelSelection::Alias(ModelAlias::from_uuid(alias)),
        ("direct" | "alias", _, _) => {
            return Err(ProcessReadCorruption::Inconsistent("model selection shape").into());
        }
        _ => {
            return Err(ProcessReadCorruption::Unsupported {
                field: "model selection kind",
                value: kind,
            }
            .into());
        }
    };
    Ok(PendingSessionSummary {
        session,
        defaults_version,
        model_selection,
        placement,
    })
}

#[derive(Clone, Copy)]
pub(super) struct LogicalDelegationTerminalProjection {
    spawning_request: ToolRequestId,
    terminal_frontier: ContextFrontierId,
    outcome: DispatchedDelegationOutcome,
    reason: DispatchedDelegationReason,
    provenance: DispatchedDelegationProvenance,
}

pub(super) fn decode_logical_delegation_terminal(
    row: &PgRow,
) -> Result<Option<LogicalDelegationTerminalProjection>, ProcessReadError> {
    let spawning_request: Option<Uuid> = row.try_get("logical_terminal_spawning_request_id")?;
    let terminal_frontier: Option<Uuid> = row.try_get("logical_terminal_frontier_id")?;
    let outcome: Option<String> = row.try_get("logical_terminal_outcome_kind")?;
    let reason: Option<String> = row.try_get("logical_terminal_reason_kind")?;
    match (spawning_request, outcome.as_deref(), reason.as_deref()) {
        (None, None, None) => Ok(None),
        (Some(spawning_request), Some(outcome), Some(reason)) => {
            let terminal_frontier = terminal_frontier.ok_or(
                ProcessReadCorruption::Inconsistent("logical delegation terminal frontier"),
            )?;
            let outcome = decode_delegation_outcome(outcome).map_err(|_| {
                ProcessReadCorruption::Inconsistent("logical delegation terminal outcome")
            })?;
            let reason = decode_delegation_reason(reason).map_err(|_| {
                ProcessReadCorruption::Inconsistent("logical delegation terminal reason")
            })?;
            let provenance = decode_delegation_provenance(row).map_err(|_| {
                ProcessReadCorruption::Inconsistent("logical delegation terminal provenance")
            })?;
            if !matches!(
                (outcome, reason),
                (
                    DispatchedDelegationOutcome::ChildStopped,
                    DispatchedDelegationReason::ParentStoppedWithDescendants
                ) | (
                    DispatchedDelegationOutcome::ChildStopped,
                    DispatchedDelegationReason::ParentCancelledWithDescendants
                ) | (
                    DispatchedDelegationOutcome::ChildCancelled,
                    DispatchedDelegationReason::ParentStoppedWithDescendants
                ) | (
                    DispatchedDelegationOutcome::ChildCancelled,
                    DispatchedDelegationReason::ParentCancelledWithDescendants
                )
            ) || !matches!(
                provenance,
                DispatchedDelegationProvenance::ParentTurnCommand { .. }
                    | DispatchedDelegationProvenance::ParentGoalCommand { .. }
                    | DispatchedDelegationProvenance::ParentLifecycleCommand { .. }
            ) {
                return Err(ProcessReadCorruption::Inconsistent(
                    "logical delegation terminal shape",
                )
                .into());
            }
            Ok(Some(LogicalDelegationTerminalProjection {
                spawning_request: ToolRequestId::from_uuid(spawning_request),
                terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                outcome,
                reason,
                provenance,
            }))
        }
        _ => Err(
            ProcessReadCorruption::Inconsistent("logical delegation terminal correlation").into(),
        ),
    }
}

pub(super) fn project_logical_delegation_terminal(
    mut decoded: DecodedTurn,
    logical_terminal: Option<LogicalDelegationTerminalProjection>,
) -> Result<DecodedTurn, ProcessReadError> {
    if let Some(logical_terminal) = logical_terminal {
        decoded.turn.state = ProcessTurnState::DelegationTerminated {
            spawning_request: logical_terminal.spawning_request,
            outcome: logical_terminal.outcome,
            reason: logical_terminal.reason,
            provenance: logical_terminal.provenance,
        };
        // The physical decode observed a mid-execution boundary, but the
        // cascade froze this turn at the logical terminal's frontier and every
        // successor chains from it. Rendering the physical frontier would show
        // execution evidence no successor model call ever saw, and would vanish
        // from the same transcript as soon as a successor activates. A turn
        // terminalized while still queued started no execution lineage at all,
        // so it keeps the absent frontier its start lineage pairs with.
        if decoded.start_lineage.is_some() {
            decoded.latest_frontier = Some(logical_terminal.terminal_frontier);
        }
    }
    Ok(decoded)
}

pub(super) async fn open_transcript_entry_cursor(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    frontier: ContextFrontierId,
) -> Result<u64, ProcessReadError> {
    let stored_member_count: Option<Decimal> = sqlx::query_scalar(
        "SELECT member_count
           FROM context_frontier
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    let member_count = decode_nonnegative(
        stored_member_count.ok_or(ProcessReadCorruption::Missing("context frontier"))?,
        "context frontier member count",
    )?;
    // The transaction-scoped cursor retains this query's execution state, so
    // every later FETCH advances the same single recursive chain resolution.
    sqlx::query(
        "DECLARE signalbox_process_transcript_entries NO SCROLL CURSOR FOR
         SELECT
            member.actual_member_count,
            member.member_position,
            member.source_session_id,
            member.semantic_entry_id,
            entry.payload_kind,
                entry.runner_placement_revision,
                entry.assistant_response_part_ordinal,
            entry.origin_accepted_input_id,
            entry.steering_source_turn_id,
            entry.failed_turn_id,
            entry.assistant_text_value,
            entry.producing_model_call_id,
            entry.assistant_tool_request_id,
            entry.tool_result_request_id,
            entry.tool_result_attempt_id,
            entry.completed_turn_id,
            entry.cancelled_turn_id,
            entry.imported_conversation_id,
            entry.imported_transcript_entry_id,
            entry.model_identity_turn_id,
            entry.model_identity_defaults_version,
            entry.model_identity_direct_selection_id,
            entry.context_summary_value,
            entry.context_summary_producing_call_id,
            entry.context_summary_first_source_session_id,
            entry.context_summary_first_entry_id,
            entry.context_summary_through_source_session_id,
            entry.context_summary_through_entry_id,
            entry.delegated_task_spawning_tool_request_id,
            entry.delegation_message_id,
            entry.delegation_result_awaiting_tool_request_id,
            entry.delegation_result_spawning_tool_request_id,
            delegated_task.task_content AS delegated_task_content,
            task_relation.parent_session_id AS delegated_task_parent_session_id,
            task_relation.parent_turn_id AS delegated_task_parent_turn_id,
            delegated_message.spawning_tool_request_id AS delegation_message_spawning_request_id,
            delegated_message.event_ordinal AS delegation_message_ordinal,
            delegated_message.content_text AS delegation_message_content,
            message_delivery.recipient_session_id AS delegation_message_recipient_session_id,
            message_delivery.delivery_sequence AS delegation_message_delivery_sequence,
            CASE delegated_message.direction
                WHEN 'parent_to_child' THEN message_relation.parent_session_id
                WHEN 'child_to_parent' THEN message_relation.child_session_id
            END AS delegation_message_sender_session_id,
            delegated_wait.child_session_id AS delegation_result_child_session_id,
            delegated_wait.wait_mode AS delegation_result_wait_mode,
            result_delivery.delivery_sequence AS delegation_result_delivery_sequence,
            delegated_result.outcome_kind AS delegation_result_outcome_kind,
            delegated_result.content_text AS delegation_result_content,
            result_event.reason_kind AS delegation_result_reason_kind,
            result_event.provenance_kind,
            result_event.provenance_session_id,
            result_event.provenance_turn_id,
            result_event.provenance_goal_generation,
            result_event.provenance_command_id,
            imported.source_speaker_kind AS imported_source_speaker_kind,
            imported.content_encoding AS imported_content_encoding,
            CASE WHEN accepted.accepted_input_id IS NULL THEN NULL
                 ELSE accepted_input_content_parts_json(
                    accepted.accepted_input_id)
            END AS origin_content,
            accepted.origin_turn_id,
            call.turn_id AS assistant_turn_id,
            result_attempt.request_id AS result_attempt_request_id,
            transcript_request.tool_name AS transcript_tool_name,
                transcript_request.inadmissible_reason AS transcript_inadmissible_reason,
                transcript_request.inadmissible_limit AS transcript_inadmissible_limit,
                transcript_request.inadmissible_argument_bytes AS transcript_inadmissible_argument_bytes,
            transcript_request.arguments_text AS transcript_tool_arguments,
            result_attempt.terminal_disposition_kind AS result_disposition,
            result_attempt.result_text,
            result_attempt.error_kind AS result_error_kind,
            result_attempt.error_detail AS result_error_detail,
            transcript_approval.decision_kind AS transcript_decision_kind,
            EXISTS (
                SELECT 1 FROM tool_approval_user_override AS recorded
                 WHERE recorded.denied_request_id = transcript_request.request_id
            ) AS transcript_override_recorded,
            transcript_approval.decision_source AS transcript_decision_source,
            transcript_approval.denial_reason AS transcript_denial_reason,
            transcript_approval.user_command_id AS transcript_user_command_id,
            transcript_approval.delegate_model_selection_id AS transcript_delegate_model_selection_id,
            transcript_approval.delegate_model_call_id AS transcript_delegate_model_call_id,
            transcript_approval.rationale AS transcript_decision_rationale,
            transcript_approval.override_denied_request_id
                AS transcript_override_denied_request_id,
            transcript_override.command_id AS transcript_override_command_id
           FROM (
                SELECT
                    resolved.*,
                    count(*) OVER () AS actual_member_count
                  FROM resolve_context_frontier_members($1, $2) AS resolved
           ) AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
           LEFT JOIN accepted_input AS accepted
             ON accepted.session_id = entry.source_session_id
            AND accepted.accepted_input_id = entry.origin_accepted_input_id
           LEFT JOIN model_call AS call
             ON call.session_id = entry.source_session_id
            AND call.model_call_id = entry.producing_model_call_id
           LEFT JOIN tool_attempt AS result_attempt
             ON result_attempt.session_id = entry.source_session_id
            AND result_attempt.attempt_id = entry.tool_result_attempt_id
           LEFT JOIN tool_request AS transcript_request
             ON transcript_request.session_id = entry.source_session_id
            AND transcript_request.request_id = COALESCE(
                entry.assistant_tool_request_id,
                entry.tool_result_request_id,
                result_attempt.request_id
            )
           LEFT JOIN tool_approval_decision AS transcript_approval
             ON transcript_approval.request_id = transcript_request.request_id
           LEFT JOIN tool_approval_user_override AS transcript_override
             ON transcript_override.denied_request_id =
                transcript_approval.override_denied_request_id
           LEFT JOIN imported_transcript_entry AS imported
             ON imported.imported_conversation_id =
                    entry.imported_conversation_id
            AND imported.imported_transcript_entry_id =
                    entry.imported_transcript_entry_id
           LEFT JOIN session_delegation_initial_task AS delegated_task
             ON delegated_task.spawning_tool_request_id =
                    entry.delegated_task_spawning_tool_request_id
            AND delegated_task.child_session_id = entry.source_session_id
            AND delegated_task.semantic_entry_id = entry.semantic_entry_id
           LEFT JOIN session_delegation AS task_relation
             ON task_relation.spawning_tool_request_id =
                    delegated_task.spawning_tool_request_id
           LEFT JOIN session_message_delivery AS message_delivery
             ON message_delivery.message_id = entry.delegation_message_id
            AND message_delivery.recipient_session_id = entry.source_session_id
           LEFT JOIN session_message AS delegated_message
             ON delegated_message.message_id = message_delivery.message_id
            AND delegated_message.spawning_tool_request_id =
                    message_delivery.spawning_tool_request_id
           LEFT JOIN session_delegation AS message_relation
             ON message_relation.spawning_tool_request_id =
                    delegated_message.spawning_tool_request_id
           LEFT JOIN session_child_result_delivery AS result_delivery
             ON result_delivery.awaiting_tool_request_id =
                    entry.delegation_result_awaiting_tool_request_id
            AND result_delivery.spawning_tool_request_id =
                    entry.delegation_result_spawning_tool_request_id
            AND result_delivery.parent_session_id = entry.source_session_id
           LEFT JOIN session_delegation_wait AS delegated_wait
             ON delegated_wait.awaiting_tool_request_id =
                    result_delivery.awaiting_tool_request_id
            AND delegated_wait.spawning_tool_request_id =
                    result_delivery.spawning_tool_request_id
            AND delegated_wait.parent_session_id = result_delivery.parent_session_id
           LEFT JOIN session_child_result AS delegated_result
             ON delegated_result.spawning_tool_request_id =
                    result_delivery.spawning_tool_request_id
           LEFT JOIN session_delegation_event AS result_event
             ON result_event.spawning_tool_request_id =
                    delegated_result.spawning_tool_request_id
            AND result_event.event_ordinal = delegated_result.event_ordinal
            AND result_event.event_kind = delegated_result.event_kind
          ORDER BY member.member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(member_count)
}

pub(super) async fn fetch_next_transcript_entry(
    transaction: &mut Transaction<'static, Postgres>,
    entry_index: u64,
    expected_entry_count: u64,
) -> Result<Option<ProcessTranscriptEntry>, ProcessReadError> {
    let row = sqlx::query("FETCH NEXT FROM signalbox_process_transcript_entries")
        .fetch_optional(&mut **transaction)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let actual_entry_count: i64 = required(&row, "actual_member_count")?;
    if u64::try_from(actual_entry_count)
        .map_err(|_| ProcessReadCorruption::InvalidOrdinal("transcript entry count"))?
        != expected_entry_count
    {
        return Err(
            ProcessReadCorruption::Inconsistent("context frontier declared membership").into(),
        );
    }
    let member_position =
        entry_index
            .checked_add(1)
            .ok_or(ProcessReadCorruption::InvalidOrdinal(
                "frontier member position",
            ))?;
    let stored_position = decode_positive(
        required(&row, "member_position")?,
        "frontier member position",
    )?;
    if stored_position != member_position {
        return Err(
            ProcessReadCorruption::Inconsistent("context frontier contiguous membership").into(),
        );
    }
    decode_transcript_entry(&row, entry_index).map(Some)
}
