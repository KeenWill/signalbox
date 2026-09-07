//! Embedded migration idempotence and the delegation schema's decoded outbox and transcript shapes.

use crate::*;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn embedded_migrator_connects_and_is_idempotent() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    migrate(&pool).await?;
    let connected: i32 = sqlx::query_scalar("SELECT 1").fetch_one(&pool).await?;
    assert_eq!(connected, 1);

    pool.close().await;
    drop(container);

    Ok(())
}

/// Every reviewed delegation function, constraint, and trigger the migration
/// commissions is installed with its exact signature and wiring after
/// PostgreSQL parses the migration.
///
/// Each object's own behavior is proved by the delegation lifecycle, delivery,
/// wake, and cascade tests that exercise it; this proves only that the
/// migration installed the reviewed set, which those tests cannot distinguish
/// from an object that was never declared.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_schema_installs_every_reviewed_object() -> Result<(), Box<dyn Error>> {
    const REVIEWED_FUNCTIONS: [&str; 21] = [
        "accepted_input_turn_is_first_nonterminal(uuid,uuid)",
        "assert_model_call_steering_final_state(uuid)",
        "delegation_cascade_expected_frontier(uuid,text)",
        "guard_session_pending_delivery_append()",
        "lock_delegation_parent_for_spawn()",
        "lock_delegation_termination_frontier(uuid,text)",
        "require_applied_goal_command_delegation_cascade()",
        "require_applied_turn_command_delegation_cascade()",
        "require_context_compaction_exact_evidence()",
        "require_delegation_initial_task_purpose()",
        "require_delegation_lifecycle_update()",
        "require_delegation_wait_rejection_attempt()",
        "require_delegation_update_subject()",
        "require_delegation_wait_purpose()",
        "require_delegation_wait_update()",
        "require_delegation_wake_turn_origin()",
        "require_semantic_delegation_result_delivery_mode()",
        "require_session_delegation_event_payload()",
        "require_terminal_delegated_turn_result()",
        "turn_lifecycle_origin_semantic_entry(uuid,uuid,uuid)",
        "turn_start_model_identity_boundary_is_valid(uuid,uuid)",
    ];
    const REVIEWED_CONSTRAINTS: [&str; 3] = [
        "delegation_update_subject_shape",
        "semantic_transcript_entry_payload_shape",
        "session_child_result_delivery_sequence_shape",
    ];
    const NO_MISSING_OBJECTS: [String; 0] = [];
    let (container, pool, _database_url) = migrated_postgres().await?;

    let missing_functions: Vec<String> = sqlx::query_scalar(
        "SELECT signature
           FROM unnest($1::text[]) AS signature
          WHERE to_regprocedure(signature) IS NULL
          ORDER BY signature",
    )
    .bind(REVIEWED_FUNCTIONS)
    .fetch_all(&pool)
    .await?;
    assert_eq!(missing_functions, NO_MISSING_OBJECTS);

    let missing_constraints: Vec<String> = sqlx::query_scalar(
        "SELECT name
           FROM unnest($1::text[]) AS name
          WHERE NOT EXISTS (
                SELECT 1 FROM pg_constraint WHERE conname = name
          )
          ORDER BY name",
    )
    .bind(REVIEWED_CONSTRAINTS)
    .fetch_all(&pool)
    .await?;
    assert_eq!(missing_constraints, NO_MISSING_OBJECTS);

    // A root session with no relationships closes the cascade frontier rather
    // than returning an edge, so an unrelated stop commissions no dispositions.
    let empty_cascade_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM delegation_cascade_expected_frontier($1, 'stopped')",
    )
    .bind(Uuid::nil())
    .fetch_one(&pool)
    .await?;
    assert_eq!(empty_cascade_count, 0);

    let early_cascade_lock_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_trigger
          WHERE tgname IN (
                'goal_command_locks_delegation_frontier',
                'submit_input_command_locks_delegation_frontier'
          )
            AND NOT tgdeferrable
            AND pg_get_triggerdef(oid) LIKE '% BEFORE INSERT ON %'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(early_cascade_lock_count, 2);

    let terminal_result_trigger_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_trigger
          WHERE tgname IN (
                'turn_lifecycle_zz_requires_delegated_result',
                'delegation_initial_task_zz_requires_terminal_result'
          )",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(terminal_result_trigger_count, 2);

    let cascade_chain_trigger_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_trigger
          WHERE tgname = 'session_delegation_cascade_requires_parent_chains'
            AND tgrelid = 'session_delegation_termination_cascade'::regclass",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(cascade_chain_trigger_count, 1);

    let reverse_cascade_trigger_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_trigger
          WHERE tgname IN (
                'applied_goal_command_requires_delegation_cascade',
                'applied_turn_command_requires_delegation_cascade'
          )",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(reverse_cascade_trigger_count, 2);

    pool.close().await;
    drop(container);
    Ok(())
}

/// Delegation outbox headers decode to their closed update and wake variants
/// before the durable delivery cursor advances.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_outbox_dispatch_decodes_update_and_wake_shapes() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = insert_outbox_session_fixture(&pool, 0xd310).await?;
    let spawning_request = Uuid::from_u128(0xd311);
    let child = Uuid::from_u128(0xd312);

    sqlx::query("ALTER TABLE delegation_update_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut update_transaction = pool.begin().await?;
    let update_sequence: Decimal = sqlx::query_scalar(
        "INSERT INTO delegation_outbox_event (event_kind, storage_version, session_id)
         VALUES ('delegation_update', 1, $1)
         RETURNING event_sequence",
    )
    .bind(session)
    .fetch_one(&mut *update_transaction)
    .await?;
    sqlx::query(
        "INSERT INTO delegation_update_outbox_event (
            event_sequence, event_kind, storage_version, session_id,
            update_kind, spawning_tool_request_id, child_session_id,
            policy_kind, delegation_event_ordinal, delegation_event_kind
         ) VALUES (
            $1, 'delegation_update', 1, $2,
            'child_spawned', $3, $4,
            'background', 1, 'spawned'
         )",
    )
    .bind(update_sequence)
    .bind(session)
    .bind(spawning_request)
    .bind(child)
    .execute(&mut *update_transaction)
    .await?;
    update_transaction.commit().await?;

    let update_outcome = OutboxDispatcher::new(pool.clone())
        .dispatch_next(|event| {
            assert_eq!(event.session(), Some(SessionId::from_uuid(session)));
            assert_eq!(
                event.kind(),
                &DispatchedOutboxEventKind::DelegationUpdate(
                    DispatchedDelegationUpdate::ChildSpawned {
                        spawning_request: ToolRequestId::from_uuid(spawning_request),
                        child: SessionId::from_uuid(child),
                        policy: DispatchedDelegationPolicy::Background,
                    }
                )
            );
            OutboxDeliveryDecision::Delivered
        })
        .await?;
    assert_eq!(
        update_outcome,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );

    sqlx::query("ALTER TABLE delegation_wake_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut wake_transaction = pool.begin().await?;
    let wake_sequence: Decimal = sqlx::query_scalar(
        "INSERT INTO delegation_outbox_event (event_kind, storage_version, session_id)
         VALUES ('delegation_wake', 1, $1)
         RETURNING event_sequence",
    )
    .bind(session)
    .fetch_one(&mut *wake_transaction)
    .await?;
    sqlx::query(
        "INSERT INTO delegation_wake_outbox_event (
            event_sequence, event_kind, storage_version, session_id,
            spawning_tool_request_id, subject_kind,
            result_spawning_request_id
         ) VALUES (
            $1, 'delegation_wake', 1, $2,
            $3, 'result', $3
         )",
    )
    .bind(wake_sequence)
    .bind(session)
    .bind(spawning_request)
    .execute(&mut *wake_transaction)
    .await?;
    wake_transaction.commit().await?;

    let wake_outcome = OutboxDispatcher::new(pool.clone())
        .dispatch_next(|event| {
            assert_eq!(event.session(), Some(SessionId::from_uuid(session)));
            assert_eq!(
                event.kind(),
                &DispatchedOutboxEventKind::DelegationWake(DispatchedDelegationWake::Result {
                    spawning_request: ToolRequestId::from_uuid(spawning_request),
                    awaiting_request: None,
                })
            );
            OutboxDeliveryDecision::Delivered
        })
        .await?;
    assert_eq!(
        wake_outcome,
        OutboxDispatchOutcome::Delivered { sequence: 2 }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A foreground delegated result occupies its await request's ordinary
/// tool-result position, so the durable tool-batch outbox event remains
/// dispatchable instead of wedging the global cursor.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn foreground_delegation_result_decodes_in_tool_batch_outbox() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session_uuid = insert_outbox_session_fixture(&pool, 0xd350).await?;
    drain_outbox(&pool, |_| {}).await?;

    let turn_uuid = Uuid::from_u128(0xd351);
    let call_uuid = Uuid::from_u128(0xd352);
    let request_uuid = Uuid::from_u128(0xd353);
    let spawning_request_uuid = Uuid::from_u128(0xd354);
    let boundary_uuid = Uuid::from_u128(0xd355);
    let result_frontier_uuid = Uuid::from_u128(0xd356);
    let result_entry_uuid = Uuid::from_u128(0xd357);

    sqlx::raw_sql(
        "ALTER TABLE tool_round DISABLE TRIGGER ALL;
         ALTER TABLE tool_request DISABLE TRIGGER ALL;
         ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL;
         ALTER TABLE context_frontier DISABLE TRIGGER ALL;
         ALTER TABLE context_frontier_delta DISABLE TRIGGER ALL;
         ALTER TABLE tool_batch_transition_outbox_event DISABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0), ($1, $3, 1)",
    )
    .bind(session_uuid)
    .bind(boundary_uuid)
    .bind(result_frontier_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO tool_round
            (producing_model_call_id, session_id, turn_id, boundary_kind,
             boundary_frontier_id, response_part_count, request_count)
         VALUES ($1, $2, $3, 'continuing', $4, 1, 1)",
    )
    .bind(call_uuid)
    .bind(session_uuid)
    .bind(turn_uuid)
    .bind(boundary_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text)
         VALUES ($1, $2, $3, $4, 0, 'await_session', 'json', '{}')",
    )
    .bind(request_uuid)
    .bind(session_uuid)
    .bind(turn_uuid)
    .bind(call_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             tool_result_request_id,
             delegation_result_awaiting_tool_request_id,
             delegation_result_spawning_tool_request_id)
         VALUES ($1, $2, 'delegation_result', $3, $3, $4)",
    )
    .bind(session_uuid)
    .bind(result_entry_uuid)
    .bind(request_uuid)
    .bind(spawning_request_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, 1, $1, $3)",
    )
    .bind(session_uuid)
    .bind(result_frontier_uuid)
    .bind(result_entry_uuid)
    .execute(&pool)
    .await?;
    let mut outbox_transaction = pool.begin().await?;
    let event_sequence: Decimal = sqlx::query_scalar(
        "INSERT INTO outbox_event (event_kind, storage_version, session_id)
         VALUES ('tool_batch_transition', 1, $1)
         RETURNING event_sequence",
    )
    .bind(session_uuid)
    .fetch_one(&mut *outbox_transaction)
    .await?;
    sqlx::query(
        "INSERT INTO tool_batch_transition_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, producing_model_call_id, transition_kind, frontier_id)
         VALUES ($1, 'tool_batch_transition', 1, $2, $3, $4,
                 'results_projected', $5)",
    )
    .bind(event_sequence)
    .bind(session_uuid)
    .bind(turn_uuid)
    .bind(call_uuid)
    .bind(result_frontier_uuid)
    .execute(&mut *outbox_transaction)
    .await?;
    outbox_transaction.commit().await?;

    let outcome = OutboxDispatcher::new(pool.clone())
        .dispatch_next(|event| {
            assert_eq!(
                event.kind(),
                &DispatchedOutboxEventKind::ToolBatchTransition {
                    turn: TurnId::from_uuid(turn_uuid),
                    producing_call: ModelCallId::from_uuid(call_uuid),
                    state: DispatchedToolBatchState::ResultsProjected {
                        frontier: ContextFrontierId::from_uuid(result_frontier_uuid),
                    },
                }
            );
            OutboxDeliveryDecision::Delivered
        })
        .await?;
    assert_eq!(
        outcome,
        OutboxDispatchOutcome::Delivered {
            sequence: u64::try_from(event_sequence)?
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Delegation semantic entries resolve through their immutable relationship,
/// delivery, wait, result, and lifecycle records instead of degrading to
/// generic transcript markers.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_semantic_entries_decode_exact_delivered_content() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let parent_uuid = Uuid::from_u128(0xd360);
    let child_uuid = Uuid::from_u128(0xd361);
    let parent_turn_uuid = Uuid::from_u128(0xd362);
    let child_turn_uuid = Uuid::from_u128(0xd363);
    let spawning_request_uuid = Uuid::from_u128(0xd364);
    let awaiting_request_uuid = Uuid::from_u128(0xd365);
    let task_entry_uuid = Uuid::from_u128(0xd366);
    let message_entry_uuid = Uuid::from_u128(0xd367);
    let result_entry_uuid = Uuid::from_u128(0xd368);
    let message_uuid = Uuid::from_u128(0xd369);
    let model_selection_uuid = Uuid::from_u128(0xd36a);
    let background_awaiting_request_uuid = Uuid::from_u128(0xd36b);
    let background_result_entry_uuid = Uuid::from_u128(0xd36c);
    let delegated_task_content = "inspect the durable result";
    let delegation_message_content = "continue with the checked input";
    let delegation_result_content = "checked result";

    sqlx::raw_sql(
        "ALTER TABLE session_delegation DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event DISABLE TRIGGER ALL;
         ALTER TABLE session_message DISABLE TRIGGER ALL;
         ALTER TABLE session_message_delivery DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wait DISABLE TRIGGER ALL;
         ALTER TABLE session_child_result DISABLE TRIGGER ALL;
         ALTER TABLE session_child_result_delivery DISABLE TRIGGER ALL;
         ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind)
         VALUES ($1, $2, $3, $4, 'background')",
    )
    .bind(spawning_request_uuid)
    .bind(parent_uuid)
    .bind(parent_turn_uuid)
    .bind(child_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_initial_task
            (spawning_tool_request_id, child_session_id, turn_id,
             semantic_entry_id, admission_position, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             frozen_model_kind, frozen_direct_model_selection_id, task_content)
         VALUES ($1, $2, $3, $4, 1, 1, 'direct', $5, 'direct', $5, $6)",
    )
    .bind(spawning_request_uuid)
    .bind(child_uuid)
    .bind(child_turn_uuid)
    .bind(task_entry_uuid)
    .bind(model_selection_uuid)
    .bind(delegated_task_content)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id)
         VALUES ($1, 2, 'message_delivered', 'tool_request', $2, $3, $4)",
    )
    .bind(spawning_request_uuid)
    .bind(parent_uuid)
    .bind(parent_turn_uuid)
    .bind(awaiting_request_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_message
            (message_id, spawning_tool_request_id, event_ordinal,
             event_kind, direction, content_text)
         VALUES ($1, $2, 2, 'message_delivered', 'parent_to_child', $3)",
    )
    .bind(message_uuid)
    .bind(spawning_request_uuid)
    .bind(delegation_message_content)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_message_delivery
            (message_id, spawning_tool_request_id, recipient_session_id,
             delivery_sequence, delivery_kind)
         VALUES ($1, $2, $3, 1, 'message')",
    )
    .bind(message_uuid)
    .bind(spawning_request_uuid)
    .bind(child_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id)
         VALUES ($1, 3, 'outcome_recorded', 'result_returned',
                 'child_completed', 'child_turn', $2, $3)",
    )
    .bind(spawning_request_uuid)
    .bind(child_uuid)
    .bind(child_turn_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_child_result
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, content_text)
         VALUES ($1, 3, 'outcome_recorded', 'result_returned', $2)",
    )
    .bind(spawning_request_uuid)
    .bind(delegation_result_content)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_wait
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, parent_turn_id, child_session_id, wait_mode)
         VALUES ($1, $2, $3, $4, $5, 'foreground'),
                ($6, $2, $3, $4, $5, 'background')",
    )
    .bind(awaiting_request_uuid)
    .bind(spawning_request_uuid)
    .bind(parent_uuid)
    .bind(parent_turn_uuid)
    .bind(child_uuid)
    .bind(background_awaiting_request_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_child_result_delivery
            (awaiting_tool_request_id, spawning_tool_request_id, parent_session_id,
             delivery_sequence, delivery_kind)
         VALUES ($1, $2, $3, NULL, NULL),
                ($4, $2, $3, 2, 'background_result')",
    )
    .bind(awaiting_request_uuid)
    .bind(spawning_request_uuid)
    .bind(parent_uuid)
    .bind(background_awaiting_request_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             delegated_task_spawning_tool_request_id)
         VALUES ($1, $2, 'delegated_task', $3)",
    )
    .bind(child_uuid)
    .bind(task_entry_uuid)
    .bind(spawning_request_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             delegation_result_awaiting_tool_request_id,
             delegation_result_spawning_tool_request_id)
         VALUES ($1, $2, 'delegation_result', $3, $4)",
    )
    .bind(parent_uuid)
    .bind(background_result_entry_uuid)
    .bind(background_awaiting_request_uuid)
    .bind(spawning_request_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             delegation_message_id)
         VALUES ($1, $2, 'delegation_message', $3)",
    )
    .bind(child_uuid)
    .bind(message_entry_uuid)
    .bind(message_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             tool_result_request_id,
             delegation_result_awaiting_tool_request_id,
             delegation_result_spawning_tool_request_id)
         VALUES ($1, $2, 'delegation_result', $3, $3, $4)",
    )
    .bind(parent_uuid)
    .bind(result_entry_uuid)
    .bind(awaiting_request_uuid)
    .bind(spawning_request_uuid)
    .execute(&pool)
    .await?;

    let parent = SessionId::from_uuid(parent_uuid);
    let child = SessionId::from_uuid(child_uuid);
    let spawning_request = ToolRequestId::from_uuid(spawning_request_uuid);
    let awaiting_request = ToolRequestId::from_uuid(awaiting_request_uuid);
    let background_awaiting_request = ToolRequestId::from_uuid(background_awaiting_request_uuid);
    let task_entry = SemanticTranscriptEntryId::from_uuid(task_entry_uuid);
    let message_entry = SemanticTranscriptEntryId::from_uuid(message_entry_uuid);
    let result_entry = SemanticTranscriptEntryId::from_uuid(result_entry_uuid);
    let background_result_entry =
        SemanticTranscriptEntryId::from_uuid(background_result_entry_uuid);
    let entries = ProcessReadRepository::new(pool.clone())
        .read_selected_transcript_entries(
            &[1, 2, 3, 4],
            &[
                SemanticTranscriptEntryRef::from_source(child, task_entry),
                SemanticTranscriptEntryRef::from_source(child, message_entry),
                SemanticTranscriptEntryRef::from_source(parent, result_entry),
                SemanticTranscriptEntryRef::from_source(parent, background_result_entry),
            ],
        )
        .await?;

    assert_eq!(
        entries.as_ref(),
        &[
            ProcessTranscriptEntry::DelegatedTask {
                entry_index: 0,
                source_session: child,
                entry: task_entry,
                spawning_request,
                parent_session: parent,
                parent_turn: TurnId::from_uuid(parent_turn_uuid),
                content: String::from(delegated_task_content),
            },
            ProcessTranscriptEntry::DelegationMessage {
                entry_index: 1,
                source_session: child,
                entry: message_entry,
                spawning_request,
                message: DelegationMessageId::from_uuid(message_uuid),
                sender: parent,
                recipient: child,
                ordinal: 2,
                delivery_sequence: 1,
                content: String::from(delegation_message_content),
            },
            ProcessTranscriptEntry::DelegationResult {
                entry_index: 2,
                source_session: parent,
                entry: result_entry,
                awaiting_request,
                spawning_request,
                child,
                mode: DispatchedDelegationWaitMode::Foreground,
                delivery_sequence: None,
                outcome: DispatchedDelegationOutcome::ResultReturned,
                content: Some(String::from(delegation_result_content)),
                reason: DispatchedDelegationReason::ChildCompleted,
                provenance: DispatchedDelegationProvenance::ChildTurn {
                    session: child,
                    turn: TurnId::from_uuid(child_turn_uuid),
                },
            },
            ProcessTranscriptEntry::DelegationResult {
                entry_index: 3,
                source_session: parent,
                entry: background_result_entry,
                awaiting_request: background_awaiting_request,
                spawning_request,
                child,
                mode: DispatchedDelegationWaitMode::Background,
                delivery_sequence: Some(2),
                outcome: DispatchedDelegationOutcome::ResultReturned,
                content: Some(String::from(delegation_result_content)),
                reason: DispatchedDelegationReason::ChildCompleted,
                provenance: DispatchedDelegationProvenance::ChildTurn {
                    session: child,
                    turn: TurnId::from_uuid(child_turn_uuid),
                },
            },
        ]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A message update recovers its recipient-wide delivery sequence, and its
/// paired internal wake remains a typed dispatcher event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_outbox_dispatch_decodes_message_update_and_wake() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let recipient = insert_outbox_session_fixture(&pool, 0xd320).await?;
    let spawning_request = Uuid::from_u128(0xd321);
    let message = Uuid::from_u128(0xd322);
    let sender = Uuid::from_u128(0xd323);

    sqlx::query("ALTER TABLE delegation_update_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE session_message_delivery DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut update_transaction = pool.begin().await?;
    let update_sequence: Decimal = sqlx::query_scalar(
        "INSERT INTO delegation_outbox_event (event_kind, storage_version, session_id)
         VALUES ('delegation_update', 1, $1)
         RETURNING event_sequence",
    )
    .bind(recipient)
    .fetch_one(&mut *update_transaction)
    .await?;
    sqlx::query(
        "INSERT INTO delegation_update_outbox_event (
            event_sequence, event_kind, storage_version, session_id,
            update_kind, spawning_tool_request_id, message_id,
            sender_session_id, recipient_session_id, message_ordinal,
            content_text
         ) VALUES (
            $1, 'delegation_update', 1, $2,
            'session_message', $3, $4,
            $5, $2, 2, 'status'
         )",
    )
    .bind(update_sequence)
    .bind(recipient)
    .bind(spawning_request)
    .bind(message)
    .bind(sender)
    .execute(&mut *update_transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_message_delivery (
            message_id, spawning_tool_request_id, recipient_session_id,
            delivery_sequence, delivery_kind
         ) VALUES ($1, $2, $3, 7, 'message')",
    )
    .bind(message)
    .bind(spawning_request)
    .bind(recipient)
    .execute(&mut *update_transaction)
    .await?;
    update_transaction.commit().await?;

    let update_outcome = OutboxDispatcher::new(pool.clone())
        .dispatch_next(|event| {
            assert_eq!(
                event.kind(),
                &DispatchedOutboxEventKind::DelegationUpdate(
                    DispatchedDelegationUpdate::SessionMessage {
                        spawning_request: ToolRequestId::from_uuid(spawning_request),
                        message: signalbox_domain::DelegationMessageId::from_uuid(message),
                        sender: SessionId::from_uuid(sender),
                        recipient: SessionId::from_uuid(recipient),
                        message_ordinal: 2,
                        delivery_sequence: 7,
                        content: String::from("status"),
                    }
                )
            );
            OutboxDeliveryDecision::Delivered
        })
        .await?;
    assert_eq!(
        update_outcome,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );

    sqlx::query("ALTER TABLE delegation_wake_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut wake_transaction = pool.begin().await?;
    let wake_sequence: Decimal = sqlx::query_scalar(
        "INSERT INTO delegation_outbox_event (event_kind, storage_version, session_id)
         VALUES ('delegation_wake', 1, $1)
         RETURNING event_sequence",
    )
    .bind(recipient)
    .fetch_one(&mut *wake_transaction)
    .await?;
    sqlx::query(
        "INSERT INTO delegation_wake_outbox_event (
            event_sequence, event_kind, storage_version, session_id,
            spawning_tool_request_id, subject_kind, message_id
         ) VALUES (
            $1, 'delegation_wake', 1, $2,
            $3, 'message', $4
         )",
    )
    .bind(wake_sequence)
    .bind(recipient)
    .bind(spawning_request)
    .bind(message)
    .execute(&mut *wake_transaction)
    .await?;
    wake_transaction.commit().await?;

    let wake_outcome = OutboxDispatcher::new(pool.clone())
        .dispatch_next(|event| {
            assert_eq!(
                event.kind(),
                &DispatchedOutboxEventKind::DelegationWake(DispatchedDelegationWake::Message {
                    spawning_request: ToolRequestId::from_uuid(spawning_request),
                    message: signalbox_domain::DelegationMessageId::from_uuid(message),
                })
            );
            OutboxDeliveryDecision::Delivered
        })
        .await?;
    assert_eq!(
        wake_outcome,
        OutboxDispatchOutcome::Delivered { sequence: 2 }
    );

    pool.close().await;
    drop(container);
    Ok(())
}
