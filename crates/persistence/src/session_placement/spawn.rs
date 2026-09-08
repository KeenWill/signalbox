//! Placement-owned creation of a delegated child and its initial task.

use super::*;
use crate::session_delegation::{SessionDelegationRepositoryError, SpawnSessionCandidates};
use signalbox_domain::{
    ChildRelationshipPolicy, DelegatedSpawnRequest, SessionCreationCause, SessionOwnership,
    StartGate,
};

const DELEGATION_OUTBOX_STORAGE_VERSION: i16 = 1;

pub(crate) async fn create_delegated_child(
    connection: &mut PgConnection,
    request: &DelegatedSpawnRequest,
    candidates: SpawnSessionCandidates,
) -> Result<(), SessionDelegationRepositoryError> {
    let parent = request.request().session();
    let parent_placement = load_current(connection, parent).await?.ok_or(
        SessionPlacementRepositoryError::Corruption("spawn parent placement missing"),
    )?;
    let placement = parent_placement
        .placement()
        .delegated_child(candidates.child);
    let child = candidates.child.into_uuid();
    let spawn = request.request().id().into_uuid();
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind, spawning_tool_request_id)
                VALUES ($1, 'delegated', 'none', $2)",
    )
    .bind(child)
    .bind(spawn)
    .execute(&mut *connection)
    .await?;
    let cause = SessionCreationCause::Delegated {
        spawning_request: request.request().id(),
    };
    crate::session_lifecycle::insert_created(
        connection,
        candidates.child,
        &cause,
        SessionOwnership::Owned,
        StartGate::Open,
        None,
    )
    .await?;
    let (path, root) = encode_placement(&placement);
    sqlx::query(
        "INSERT INTO session_placement_event
        (session_id, version, prior_version, event_kind, placement_path,
         root_global_read_intent, provenance_tool_request_id, parent_placement_version, recorded_at)
         VALUES ($1, 1, NULL, 'created', $2, $3, $4, $5, transaction_timestamp())",
    )
    .bind(child)
    .bind(path)
    .bind(root)
    .bind(spawn)
    .bind(Decimal::from(parent_placement.version().as_u64()))
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_placement(session_id, current_version) VALUES ($1, 1)",
    )
    .bind(child)
    .execute(&mut *connection)
    .await?;
    sqlx::query("INSERT INTO session_scheduler(session_id) VALUES ($1)")
        .bind(child)
        .execute(&mut *connection)
        .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
        (session_id, version, model_selection_kind, direct_model_selection_id, model_alias_id,
         dangerous_tool_auto_approval, system_prompt, model_settings)
        SELECT $1, 1, defaults.model_selection_kind, defaults.direct_model_selection_id,
               defaults.model_alias_id, defaults.dangerous_tool_auto_approval,
               defaults.system_prompt, defaults.model_settings
          FROM turn_origin_effective_model_configuration($2, $3) AS frozen
          JOIN session_defaults_version AS defaults ON defaults.session_id = $3
           AND defaults.version = frozen.defaults_version",
    )
    .bind(child)
    .bind(request.request().turn().into_uuid())
    .bind(parent.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query("INSERT INTO session_current_defaults(session_id, current_version) VALUES ($1, 1)")
        .bind(child)
        .execute(&mut *connection)
        .await?;
    sqlx::query("INSERT INTO session_model_credential_record
        (session_id, event_ordinal, event_kind, provenance_kind, provenance_tool_request_id, recorded_at)
        VALUES ($1, 1, 'created', 'delegated_session', $2, transaction_timestamp())")
        .bind(child).bind(spawn).execute(&mut *connection).await?;
    sqlx::query(
        "INSERT INTO session_model_credential_entry
        (session_id, event_ordinal, model_family, credential_reference)
        SELECT $1, 1, entry.model_family, entry.credential_reference
          FROM session_current_model_credentials AS head
          JOIN session_model_credential_entry AS entry ON entry.session_id = head.session_id
           AND entry.event_ordinal = head.current_event_ordinal WHERE head.session_id = $2",
    )
    .bind(child)
    .bind(parent.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query("INSERT INTO session_current_model_credentials(session_id, current_event_ordinal) VALUES ($1, 1)")
        .bind(child).execute(&mut *connection).await?;
    let (policy_kind, stopped, cancelled) = match request.policy() {
        ChildRelationshipPolicy::Background => ("background", None, None),
        ChildRelationshipPolicy::Bound {
            on_parent_stopped,
            on_parent_cancelled,
        } => (
            "bound",
            Some(action(on_parent_stopped)),
            Some(action(on_parent_cancelled)),
        ),
    };
    sqlx::query(
        "INSERT INTO session_delegation
        (spawning_tool_request_id, parent_session_id, parent_turn_id, child_session_id,
         policy_kind, on_parent_stopped, on_parent_cancelled)
        VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(spawn)
    .bind(parent.into_uuid())
    .bind(request.request().turn().into_uuid())
    .bind(child)
    .bind(policy_kind)
    .bind(stopped)
    .bind(cancelled)
    .execute(&mut *connection)
    .await?;
    sqlx::query("INSERT INTO turn_lifecycle
        (turn_id, session_id, origin_kind, origin_accepted_input_id, acceptance_position, state_kind)
        VALUES ($1, $2, 'delegation', NULL, 1, 'queued')")
        .bind(candidates.turn.into_uuid()).bind(child).execute(&mut *connection).await?;
    sqlx::query("INSERT INTO semantic_transcript_entry
        (source_session_id, semantic_entry_id, payload_kind, delegated_task_spawning_tool_request_id)
        VALUES ($1, $2, 'delegated_task', $3)")
        .bind(child).bind(candidates.entry.into_uuid()).bind(spawn).execute(&mut *connection).await?;
    sqlx::query("INSERT INTO session_delegation_initial_task
        (spawning_tool_request_id, child_session_id, turn_id, semantic_entry_id,
         admission_position, defaults_version, requested_model_kind,
         requested_direct_model_selection_id, requested_model_alias_id, frozen_model_kind,
         frozen_direct_model_selection_id, frozen_model_alias_id, frozen_alias_selected_direct_id, task_content)
        SELECT $1, $2, $3, $4, 1, 1, frozen.requested_model_kind,
               frozen.requested_direct_model_selection_id, frozen.requested_model_alias_id,
               frozen.frozen_model_kind, frozen.frozen_direct_model_selection_id,
               frozen.frozen_model_alias_id, frozen.frozen_alias_selected_direct_id, $5
          FROM turn_origin_exact_model_configuration($6, $7) AS frozen")
        .bind(spawn).bind(child).bind(candidates.turn.into_uuid()).bind(candidates.entry.into_uuid())
        .bind(request.task().as_str()).bind(request.request().turn().into_uuid()).bind(parent.into_uuid())
        .execute(&mut *connection).await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
        (spawning_tool_request_id, event_ordinal, event_kind, provenance_kind,
         provenance_session_id, provenance_turn_id, provenance_tool_request_id)
        VALUES ($1, 1, 'spawned', 'tool_request', $2, $3, $1)",
    )
    .bind(spawn)
    .bind(parent.into_uuid())
    .bind(request.request().turn().into_uuid())
    .execute(&mut *connection)
    .await?;
    crate::outbox::append(
        connection,
        crate::outbox::OutboxEvent::SessionCreated {
            session: candidates.child,
            cause,
            ownership: SessionOwnership::Owned,
        },
    )
    .await?;
    sqlx::query(
        "WITH header AS (
        INSERT INTO delegation_outbox_event (event_kind, storage_version, session_id)
        VALUES ('delegation_update', $1, $2)
        RETURNING event_sequence, event_kind, storage_version, session_id)
        INSERT INTO delegation_update_outbox_event
        (event_sequence, event_kind, storage_version, session_id, update_kind,
         spawning_tool_request_id, child_session_id, policy_kind, on_parent_stopped,
         on_parent_cancelled, delegation_event_ordinal, delegation_event_kind)
        SELECT event_sequence, event_kind, storage_version, session_id, 'child_spawned',
               $3, $4, $5, $6, $7, 1, 'spawned' FROM header",
    )
    .bind(DELEGATION_OUTBOX_STORAGE_VERSION)
    .bind(parent.into_uuid())
    .bind(spawn)
    .bind(child)
    .bind(policy_kind)
    .bind(stopped)
    .bind(cancelled)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

const fn action(action: signalbox_domain::BoundChildAction) -> &'static str {
    match action {
        signalbox_domain::BoundChildAction::KeepRunning => "keep_running",
        signalbox_domain::BoundChildAction::Stop => "stop",
        signalbox_domain::BoundChildAction::Cancel => "cancel",
    }
}
