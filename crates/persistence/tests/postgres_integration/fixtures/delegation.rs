//! Delegation rows, waits, messages, and recovery.

use crate::*;

pub(crate) const RAW_DELEGATED_TASK: &str = "inspect delegated work";
pub(crate) const RAW_DELEGATED_MESSAGE: &str = "delegated status";
pub(crate) const DELEGATION_OUTBOX_FIXTURE_SEED: u128 = 0xd600;
pub(crate) const DELEGATION_HISTORY_FIXTURE_SEED: u128 = 0xd610;
pub(crate) const DELEGATION_SPAWN_PURPOSE_FIXTURE_SEED: u128 = 0xd620;
pub(crate) const DELEGATION_MESSAGE_PURPOSE_FIXTURE_SEED: u128 = 0xd630;
pub(crate) const DELEGATION_CASCADE_SOURCE_FIXTURE_SEED: u128 = 0xd640;
pub(crate) const DELEGATION_CASCADE_TARGET_FIXTURE_SEED: u128 = 0xd650;
pub(crate) const DELEGATION_RELATION_FIXTURE_SEED: u128 = 0xd700;
pub(crate) const DELEGATION_WAIT_FIXTURE_SEED: u128 = 0xd710;
pub(crate) const DELEGATION_LIFECYCLE_FIXTURE_SEED: u128 = 0xd720;
pub(crate) const DELEGATION_MESSAGE_UPDATE_FIXTURE_SEED: u128 = 0xd730;
pub(crate) const DELEGATION_RESULT_UPDATE_FIXTURE_SEED: u128 = 0xd740;
pub(crate) const DELEGATION_MESSAGE_WAKE_FIXTURE_SEED: u128 = 0xd750;
pub(crate) const DELEGATION_RESULT_WAKE_FIXTURE_SEED: u128 = 0xd760;
pub(crate) const DELEGATION_CHILD_STREAM_FIXTURE_SEED: u128 = 0xd770;
pub(crate) const DELEGATION_PARENT_STREAM_FIXTURE_SEED: u128 = 0xd780;
pub(crate) const DELEGATION_DUPLICATE_MESSAGE_FIXTURE_SEED: u128 = 0xd790;
pub(crate) const DELEGATION_REVERSE_INSERT_FIXTURE_SEED: u128 = 0xd7a0;
pub(crate) const DELEGATION_REPOSITORY_BACKGROUND_WAIT_SEED: u128 = 0x10000;
pub(crate) const DELEGATION_REPOSITORY_FOREGROUND_WAIT_SEED: u128 = 0x12000;
pub(crate) const DELEGATION_REPOSITORY_SECOND_BACKGROUND_WAIT_SEED: u128 = 0x14000;
pub(crate) const DELEGATION_REPOSITORY_MESSAGE_SEED: u128 = 0x16000;
pub(crate) const DELEGATION_REPOSITORY_MESSAGE_RACE_SECOND_SEED: u128 = 0x18000;
pub(crate) const DELEGATION_REPOSITORY_PREPARED_WAIT_SEED: u128 = 0x1a000;
pub(crate) const DELEGATION_REPOSITORY_APPROVED_WAIT_SEED: u128 = 0x1c000;
pub(crate) const DELEGATION_LIFECYCLE_COMMAND_ID: u128 = 0xdd10;
pub(crate) const DELEGATION_CASCADE_ROOT_COMMAND_ID: u128 = 0xe640;
pub(crate) const DELEGATION_WAIT_ONLY_OUTCOME_ORDINAL: i16 = 2;
pub(crate) struct RawDelegationPurposes<'a> {
    pub(crate) spawn_arguments: &'a str,
    pub(crate) message_arguments: &'a str,
    pub(crate) wait_mode: &'a str,
}

#[derive(Clone, Copy)]
pub(crate) struct RawMessageRoute {
    pub(crate) stream: SessionId,
    pub(crate) sender: SessionId,
    pub(crate) recipient: SessionId,
}

pub(crate) async fn prepare_raw_delegation(
    pool: &PgPool,
    seed: u128,
    purposes: RawDelegationPurposes<'_>,
) -> Result<RawDelegationFixture, Box<dyn Error>> {
    let child = SessionId::from_uuid(Uuid::from_u128(seed + 0x200));
    let await_arguments = serde_json::json!({
        "child_session_id": child.as_uuid().to_string(),
        "mode": purposes.wait_mode,
    })
    .to_string();
    let (parent, _repository, _observation, requests) = checkpoint_confirmed_tool_batch(
        pool,
        seed,
        &[
            ("spawn_session", purposes.spawn_arguments),
            ("await_session", await_arguments.as_str()),
            ("send_session_message", purposes.message_arguments),
        ],
    )
    .await?;
    let [spawning_request, awaiting_request, message_request]: [ToolRequestId; 3] = requests
        .try_into()
        .expect("delegation fixture prepares exactly spawn, await, and message requests");
    let fixture = RawDelegationFixture {
        parent: parent.session,
        parent_turn: parent.turn,
        parent_attempt: parent.attempt,
        child,
        initial_turn: TurnId::from_uuid(Uuid::from_u128(seed + 0x201)),
        initial_semantic_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x202)),
        spawning_request,
        awaiting_request,
        message_request,
        message_id: Uuid::from_u128(seed + 0x400),
    };
    insert_raw_delegation_tool_receipts(pool, fixture, seed).await?;
    Ok(fixture)
}

pub(crate) async fn insert_raw_delegation_tool_receipts(
    pool: &PgPool,
    fixture: RawDelegationFixture,
    seed: u128,
) -> Result<(), sqlx::Error> {
    let spawn_result = serde_json::json!({
        "result": "session_spawned",
        "tool_request_id": fixture.spawning_request.as_uuid().to_string(),
        "child_session_id": fixture.child.as_uuid().to_string(),
        "relationship": { "kind": "background" },
    })
    .to_string();
    let await_result = serde_json::json!({
        "result": "session_await_registered",
        "tool_request_id": fixture.awaiting_request.as_uuid().to_string(),
        "child_session_id": fixture.child.as_uuid().to_string(),
        "mode": "background",
    })
    .to_string();
    let message_result = serde_json::json!({
        "result": "session_message_sent",
        "tool_request_id": fixture.message_request.as_uuid().to_string(),
        "message_id": fixture.message_id.to_string(),
        "direction": "parent_to_child",
        "ordinal": 2,
        "delivery_sequence": 1,
    })
    .to_string();
    let mut transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO tool_attempt
            (attempt_id, request_id, session_id, turn_id,
             issuing_turn_attempt_id, effect_class, dispatch_generation,
             state_kind, terminal_disposition_kind, result_content_kind,
             result_text, context_result_text)
         VALUES
            ($1, $2, $7, $8, $9, 'external_effect', 1,
             'terminal', 'completed', 'text', $10, $10),
            ($3, $4, $7, $8, $9, 'effect_free', 1,
             'terminal', 'completed', 'text', $11, $11),
            ($5, $6, $7, $8, $9, 'external_effect', 1,
             'terminal', 'completed', 'text', $12, $12)",
    )
    .bind(Uuid::from_u128(seed + 0x300))
    .bind(fixture.spawning_request.into_uuid())
    .bind(Uuid::from_u128(seed + 0x301))
    .bind(fixture.awaiting_request.into_uuid())
    .bind(Uuid::from_u128(seed + 0x302))
    .bind(fixture.message_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .bind(fixture.parent_attempt.into_uuid())
    .bind(spawn_result)
    .bind(await_result)
    .bind(message_result)
    .execute(&mut *transaction)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await
}

pub(crate) async fn prepare_canonical_raw_delegation(
    pool: &PgPool,
    seed: u128,
) -> Result<RawDelegationFixture, Box<dyn Error>> {
    let spawn_arguments = serde_json::json!({
        "relationship": { "kind": "background" },
        "task": RAW_DELEGATED_TASK,
    })
    .to_string();
    let child = SessionId::from_uuid(Uuid::from_u128(seed + 0x200));
    let message_arguments = serde_json::json!({
        "content": RAW_DELEGATED_MESSAGE,
        "peer_session_id": child.as_uuid().to_string(),
    })
    .to_string();
    prepare_raw_delegation(
        pool,
        seed,
        RawDelegationPurposes {
            spawn_arguments: &spawn_arguments,
            message_arguments: &message_arguments,
            wait_mode: "background",
        },
    )
    .await
}

pub(crate) async fn insert_raw_delegation(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO session
            (session_id, creation_cause, ancestry_kind, spawning_tool_request_id)
         VALUES ($1, 'delegated', 'none', $2)",
    )
    .bind(fixture.child.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .execute(&mut *connection)
    .await?;
    // Placement owns the delegated default. The parent fixture is pathless, so
    // this test-only creation record preserves that exact existing placement.
    sqlx::query(
        "INSERT INTO session_placement_event
            (session_id, version, prior_version, event_kind, placement_path,
             root_global_read_intent, provenance_command_id, recorded_at)
         SELECT $1, 1, NULL, 'created', placement_path,
                root_global_read_intent, provenance_command_id,
                transaction_timestamp()
           FROM session_placement_event
          WHERE session_id = $2 AND version = 1",
    )
    .bind(fixture.child.into_uuid())
    .bind(fixture.parent.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_placement(session_id, current_version)
         VALUES ($1, 1)",
    )
    .bind(fixture.child.into_uuid())
    .execute(&mut *connection)
    .await?;
    // Delegated creation is owned in production.
    insert_raw_session_lifecycle(&mut *connection, fixture.child.into_uuid(), true).await?;
    sqlx::query("INSERT INTO session_scheduler(session_id) VALUES ($1)")
        .bind(fixture.child.into_uuid())
        .execute(&mut *connection)
        .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind, direct_model_selection_id,
             model_alias_id, dangerous_tool_auto_approval, system_prompt)
         SELECT $1, 1, defaults.model_selection_kind,
                defaults.direct_model_selection_id, defaults.model_alias_id,
                defaults.dangerous_tool_auto_approval, defaults.system_prompt
           FROM turn_origin_effective_model_configuration($2, $3) AS frozen
           JOIN session_defaults_version AS defaults
             ON defaults.session_id = $3
            AND defaults.version = frozen.defaults_version",
    )
    .bind(fixture.child.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .bind(fixture.parent.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults(session_id, current_version)
         VALUES ($1, 1)",
    )
    .bind(fixture.child.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind)
         VALUES ($1, $2, $3, $4, 'background')",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .bind(fixture.child.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "WITH lifecycle AS (
            INSERT INTO turn_lifecycle
                (turn_id, session_id, origin_kind, origin_accepted_input_id,
                 acceptance_position, state_kind)
            VALUES ($1, $2, 'delegation', NULL, 1, 'queued')
            RETURNING turn_id
         ), semantic_entry AS (
            INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 delegated_task_spawning_tool_request_id)
            VALUES ($2, $7, 'delegated_task', $3)
            RETURNING semantic_entry_id
         )
         INSERT INTO session_delegation_initial_task
            (spawning_tool_request_id, child_session_id, turn_id, semantic_entry_id,
             admission_position, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             frozen_model_kind, frozen_direct_model_selection_id, task_content)
         SELECT $3, $2, lifecycle.turn_id, semantic_entry.semantic_entry_id, 1, 1,
                'direct', frozen.direct_selection_id,
                'direct', frozen.direct_selection_id, $4
           FROM lifecycle
           CROSS JOIN semantic_entry
           CROSS JOIN turn_origin_effective_model_configuration($5, $6) AS frozen",
    )
    .bind(fixture.initial_turn.into_uuid())
    .bind(fixture.child.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .bind(RAW_DELEGATED_TASK)
    .bind(fixture.parent_turn.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.initial_semantic_entry.into_uuid())
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id)
         VALUES ($1, 1, 'spawned', 'tool_request', $2, $3, $1)",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(crate) async fn insert_raw_wait(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO session_delegation_wait
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, parent_turn_id, child_session_id, wait_mode)
         VALUES ($1, $2, $3, $4, $5, 'background')",
    )
    .bind(fixture.awaiting_request.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .bind(fixture.child.into_uuid())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(crate) async fn insert_raw_failed_outcome(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
    turn: TurnId,
    event_ordinal: i16,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id)
         VALUES ($1, $4, 'outcome_recorded', 'child_failed',
                 'child_execution_failed', 'child_turn', $2, $3)",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.child.into_uuid())
    .bind(turn.into_uuid())
    .bind(event_ordinal)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_child_result
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, content_text)
         VALUES ($1, $2, 'outcome_recorded', 'child_failed', NULL)",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(event_ordinal)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "WITH pending AS (
            INSERT INTO session_pending_delivery
                (recipient_session_id, delivery_sequence, delivery_kind)
            VALUES ($1, 1, 'background_result')
         )
         INSERT INTO session_child_result_delivery
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, delivery_sequence, delivery_kind)
         VALUES ($2, $3, $1, 1, 'background_result')",
    )
    .bind(fixture.parent.into_uuid())
    .bind(fixture.awaiting_request.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(crate) struct RawDelegationUpdate<'a> {
    pub(crate) session: SessionId,
    pub(crate) kind: &'a str,
    pub(crate) awaiting_request: Option<Uuid>,
    pub(crate) event_ordinal: Option<i64>,
    pub(crate) event_kind: Option<&'a str>,
    pub(crate) result_request: Option<Uuid>,
    pub(crate) message_id: Option<Uuid>,
}

pub(crate) async fn append_raw_delegation_update(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
    update: RawDelegationUpdate<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, child_session_id,
             policy_kind, on_parent_stopped, on_parent_cancelled,
             awaiting_tool_request_id, wait_mode,
             delegation_event_ordinal, delegation_event_kind,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id, provenance_command_id,
             result_spawning_request_id, message_id,
             sender_session_id, recipient_session_id, message_ordinal,
             content_text)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $2, $3,
                CASE WHEN $2 = 'session_message' THEN NULL ELSE $9 END,
                CASE WHEN $2 = 'child_spawned' THEN 'background' END,
                NULL, NULL, $4,
                CASE WHEN $2 = 'child_waiting' THEN 'background' END,
                $5, $6,
                CASE WHEN $2 IN (
                    'child_lifecycle_disposition', 'child_result'
                ) THEN 'child_failed' END,
                CASE WHEN $2 IN (
                    'child_lifecycle_disposition', 'child_result'
                ) THEN 'child_execution_failed' END,
                CASE WHEN $2 IN (
                    'child_lifecycle_disposition', 'child_result'
                ) THEN 'child_turn' END,
                CASE WHEN $2 IN (
                    'child_lifecycle_disposition', 'child_result'
                ) THEN $9 END,
                CASE WHEN $2 IN (
                    'child_lifecycle_disposition', 'child_result'
                ) THEN $10 END,
                NULL, $7, $8,
                CASE WHEN $2 = 'session_message' THEN $11 END,
                CASE WHEN $2 = 'session_message' THEN $9 END,
                CASE WHEN $2 = 'session_message' THEN 2 END,
                CASE WHEN $2 = 'session_message' THEN $12 END
           FROM header",
    )
    .bind(update.session.into_uuid())
    .bind(update.kind)
    .bind(fixture.spawning_request.into_uuid())
    .bind(update.awaiting_request)
    .bind(update.event_ordinal)
    .bind(update.event_kind)
    .bind(update.result_request)
    .bind(update.message_id)
    .bind(fixture.child.into_uuid())
    .bind(fixture.initial_turn.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(RAW_DELEGATED_MESSAGE)
    .execute(connection)
    .await?;
    Ok(())
}

pub(crate) async fn insert_raw_parent_lifecycle_without_update(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
    command_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "ALTER TABLE durable_command
         DISABLE TRIGGER durable_command_requires_typed_record",
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'goal', 1, transaction_timestamp(), 'operator')",
    )
    .bind(command_id)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "ALTER TABLE durable_command
         ENABLE TRIGGER durable_command_requires_typed_record",
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id,
             provenance_command_id)
         VALUES ($1, 2, 'outcome_recorded', 'continue_running',
                 'parent_stopped_parent_and_descendants',
                 'parent_turn_command', $2, $3, $4)",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .bind(command_id)
    .execute(connection)
    .await?;
    Ok(())
}

pub(crate) async fn append_raw_result_wake(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_wake', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_wake_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             spawning_tool_request_id, subject_kind,
             result_spawning_request_id, message_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $2, 'result', $2, NULL FROM header",
    )
    .bind(fixture.parent.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

pub(crate) async fn append_raw_message_wake(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
    recipient: SessionId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_wake', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_wake_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             spawning_tool_request_id, subject_kind,
             result_spawning_request_id, message_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $2, 'message', NULL, $3 FROM header",
    )
    .bind(recipient.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.message_id)
    .execute(connection)
    .await?;
    Ok(())
}

pub(crate) async fn insert_raw_delegation_with_update(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
) -> Result<(), sqlx::Error> {
    insert_raw_delegation(connection, fixture).await?;
    append_raw_delegation_update(
        connection,
        fixture,
        RawDelegationUpdate {
            session: fixture.parent,
            kind: "child_spawned",
            awaiting_request: None,
            event_ordinal: Some(1),
            event_kind: Some("spawned"),
            result_request: None,
            message_id: None,
        },
    )
    .await
}

pub(crate) async fn prepare_delegation_repository_fixture(
    pool: &PgPool,
    seed: u128,
    wait_mode: &str,
) -> Result<RawDelegationFixture, Box<dyn Error>> {
    let spawn_arguments = serde_json::json!({
        "relationship": { "kind": "background" },
        "task": RAW_DELEGATED_TASK,
    })
    .to_string();
    let child = SessionId::from_uuid(Uuid::from_u128(seed + 0x200));
    let message_arguments = serde_json::json!({
        "content": RAW_DELEGATED_MESSAGE,
        "peer_session_id": child.as_uuid().to_string(),
    })
    .to_string();
    let await_arguments = serde_json::json!({
        "child_session_id": child.as_uuid().to_string(),
        "mode": wait_mode,
    })
    .to_string();
    let (parent, _repository, _observation, requests) = checkpoint_tool_batch_with_approval(
        pool,
        seed,
        &[
            ("spawn_session", spawn_arguments.as_str()),
            ("await_session", await_arguments.as_str()),
            ("send_session_message", message_arguments.as_str()),
        ],
        InitialToolApproval::PolicyAuto,
    )
    .await?;
    let fixture = RawDelegationFixture {
        parent: parent.session,
        parent_turn: parent.turn,
        parent_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xc1)),
        child,
        initial_turn: TurnId::from_uuid(Uuid::from_u128(seed + 0x201)),
        initial_semantic_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x202)),
        spawning_request: requests[0],
        awaiting_request: requests[1],
        message_request: requests[2],
        message_id: Uuid::from_u128(seed + 0x400),
    };
    insert_raw_delegation_tool_receipts(pool, fixture, seed).await?;
    let mut transaction = pool.begin().await?;
    insert_raw_delegation_with_update(&mut transaction, fixture).await?;
    transaction.commit().await?;
    Ok(fixture)
}

pub(crate) async fn repository_wait_dispatch(
    pool: &PgPool,
    fixture: RawDelegationFixture,
    seed: u128,
) -> Result<ToolDispatchAuthority, Box<dyn Error>> {
    prepare_repository_wait_attempt(pool, fixture, seed).await?;
    PostgresToolLoopRepository::new(pool.clone())
        .authorize_attempt(
            fixture.parent,
            fixture.parent_turn,
            ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x301)),
        )
        .await
        .map_err(Into::into)
}

pub(crate) async fn prepare_repository_wait_attempt(
    pool: &PgPool,
    fixture: RawDelegationFixture,
    seed: u128,
) -> Result<(), Box<dyn Error>> {
    let message_attempt = Uuid::from_u128(seed + 0x302);
    let wait_attempt = Uuid::from_u128(seed + 0x301);
    let mut transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM tool_attempt WHERE attempt_id IN ($1, $2)")
        .bind(wait_attempt)
        .bind(message_attempt)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    repository
        .prepare_next_attempt(
            fixture.parent,
            fixture.parent_turn,
            ToolAttemptId::from_uuid(wait_attempt),
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the wait fixture prepares its next attempt");
    Ok(())
}

pub(crate) async fn remove_repository_pending_attempts(
    pool: &PgPool,
    seed: u128,
) -> Result<(), sqlx::Error> {
    let wait_attempt = Uuid::from_u128(seed + 0x301);
    let message_attempt = Uuid::from_u128(seed + 0x302);
    let mut transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM tool_attempt WHERE attempt_id IN ($1, $2)")
        .bind(wait_attempt)
        .bind(message_attempt)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await
}

pub(crate) async fn repository_message_dispatch(
    pool: &PgPool,
    fixture: RawDelegationFixture,
    seed: u128,
) -> Result<ToolDispatchAuthority, Box<dyn Error>> {
    let message_attempt = Uuid::from_u128(seed + 0x302);
    let mut transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM tool_attempt WHERE attempt_id = $1")
        .bind(message_attempt)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    repository
        .prepare_next_attempt(
            fixture.parent,
            fixture.parent_turn,
            ToolAttemptId::from_uuid(message_attempt),
            ToolEffectClass::ExternalEffect,
        )
        .await?
        .expect("the message fixture prepares its next attempt");
    repository
        .authorize_attempt(
            fixture.parent,
            fixture.parent_turn,
            ToolAttemptId::from_uuid(message_attempt),
        )
        .await
        .map_err(Into::into)
}

pub(crate) fn recorded_wait(outcome: RecordDelegationWaitOutcome) -> RecordedDelegationWait {
    match outcome {
        RecordDelegationWaitOutcome::Recorded(recorded) => recorded,
        RecordDelegationWaitOutcome::Rejected(rejection) => {
            panic!("fixture wait was rejected: {rejection:?}")
        }
        RecordDelegationWaitOutcome::DurablyRejected(rejection) => {
            panic!("fixture wait was durably rejected: {rejection:?}")
        }
    }
}

pub(crate) fn process_wait(
    outcome: ProcessDelegationOutcome<(DelegationAwaitRequest, RecordedDelegationWait)>,
) -> (DelegationAwaitRequest, RecordedDelegationWait) {
    match outcome {
        ProcessDelegationOutcome::Applied(recorded) => recorded,
        ProcessDelegationOutcome::InvalidRequest | ProcessDelegationOutcome::Rejected(_) => {
            panic!("the exact stored await request reconstitutes")
        }
    }
}

pub(crate) fn process_message(
    outcome: ProcessDelegationOutcome<(DelegationMessageRequest, Box<RecordedDelegationMessage>)>,
) -> (DelegationMessageRequest, Box<RecordedDelegationMessage>) {
    match outcome {
        ProcessDelegationOutcome::Applied(recorded) => recorded,
        ProcessDelegationOutcome::InvalidRequest | ProcessDelegationOutcome::Rejected(_) => {
            panic!("the exact stored message request reconstitutes")
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum MessageRaceDisposition {
    IdentityCollision,
    Recorded,
}

pub(crate) fn message_race_disposition(
    outcome: RecordDelegationMessageOutcome,
) -> MessageRaceDisposition {
    match outcome {
        RecordDelegationMessageOutcome::Recorded(_) => MessageRaceDisposition::Recorded,
        RecordDelegationMessageOutcome::Rejected(
            DelegationOperationRejection::MessageIdentityCollision,
        ) => MessageRaceDisposition::IdentityCollision,
        RecordDelegationMessageOutcome::Rejected(
            DelegationOperationRejection::RelationshipNotFound,
        ) => panic!("message race lost its relationship"),
        RecordDelegationMessageOutcome::Rejected(DelegationOperationRejection::StaleDispatch {
            ..
        }) => {
            panic!("message race lost its dispatch")
        }
        RecordDelegationMessageOutcome::Rejected(
            DelegationOperationRejection::DeliverySequenceExhausted,
        ) => panic!("message race exhausted its delivery sequence"),
        RecordDelegationMessageOutcome::Rejected(DelegationOperationRejection::Transition {
            ..
        }) => {
            panic!("message race reached an invalid transition")
        }
        RecordDelegationMessageOutcome::DurablyRejected(_) => {
            panic!("issued message race cannot observe a process-owned durable rejection")
        }
    }
}

pub(crate) fn delegation_corruption(
    error: SessionDelegationRepositoryError,
) -> SessionDelegationCorruption {
    match error {
        SessionDelegationRepositoryError::Placement(error) => {
            panic!("unexpected placement failure: {error:?}")
        }
        SessionDelegationRepositoryError::Corruption(corruption) => corruption,
        SessionDelegationRepositoryError::Database(_) => {
            panic!("expected typed delegation corruption, found database failure")
        }
        SessionDelegationRepositoryError::CommitAmbiguous(_) => {
            panic!("expected typed delegation corruption, found commit ambiguity")
        }
        SessionDelegationRepositoryError::ToolLoop(_) => {
            panic!("expected typed delegation corruption, found tool-loop failure")
        }
        SessionDelegationRepositoryError::InvalidTransition(_) => {
            panic!("expected typed delegation corruption, found invalid transition")
        }
    }
}

#[derive(sqlx::FromRow)]
pub(crate) struct BackgroundWaitAtomicityEvidence {
    pub(crate) wait_count: i64,
    pub(crate) update_count: i64,
    pub(crate) completed_attempt_count: i64,
    pub(crate) result_text: String,
}

#[derive(sqlx::FromRow)]
pub(crate) struct ForegroundWaitAtomicityEvidence {
    pub(crate) active_phase: String,
    pub(crate) current_attempt: Option<Uuid>,
    pub(crate) attempt_state: String,
    pub(crate) terminal_disposition: String,
    pub(crate) issuing_disposition: String,
    pub(crate) update_count: i64,
}

#[derive(sqlx::FromRow)]
pub(crate) struct MessageAtomicityEvidence {
    pub(crate) event_count: i64,
    pub(crate) message_count: i64,
    pub(crate) delivery_count: i64,
    pub(crate) update_count: i64,
    pub(crate) wake_count: i64,
    pub(crate) completed_attempt_count: i64,
}

#[derive(sqlx::FromRow)]
pub(crate) struct DelegatedResultMaterializationEvidence {
    pub(crate) outcome_kind: String,
    pub(crate) content_text: Option<String>,
    pub(crate) reason_kind: String,
    pub(crate) provenance_kind: String,
    pub(crate) terminal_disposition_kind: String,
    pub(crate) parent_update_count: i64,
    pub(crate) parent_wake_count: i64,
}

pub(crate) async fn insert_raw_wait_and_message_with_delivery(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
) -> Result<(), sqlx::Error> {
    insert_raw_wait_with_update(connection, fixture).await?;
    insert_raw_message(connection, fixture, "parent_to_child", fixture.child).await?;
    append_raw_delegation_update(
        connection,
        fixture,
        RawDelegationUpdate {
            session: fixture.child,
            kind: "session_message",
            awaiting_request: None,
            event_ordinal: None,
            event_kind: None,
            result_request: None,
            message_id: Some(fixture.message_id),
        },
    )
    .await?;
    append_raw_message_wake(connection, fixture, fixture.child).await
}

pub(crate) async fn insert_raw_wait_with_update(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
) -> Result<(), sqlx::Error> {
    insert_raw_wait(connection, fixture).await?;
    append_raw_delegation_update(
        connection,
        fixture,
        RawDelegationUpdate {
            session: fixture.parent,
            kind: "child_waiting",
            awaiting_request: Some(fixture.awaiting_request.into_uuid()),
            event_ordinal: None,
            event_kind: None,
            result_request: None,
            message_id: None,
        },
    )
    .await
}

pub(crate) async fn insert_raw_message(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
    direction: &str,
    recipient: SessionId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH event AS (
            INSERT INTO session_delegation_event
                (spawning_tool_request_id, event_ordinal, event_kind,
                 provenance_kind, provenance_session_id, provenance_turn_id,
                 provenance_tool_request_id)
            VALUES ($1, 2, 'message_delivered', 'tool_request', $2, $3, $4)
            RETURNING spawning_tool_request_id, event_ordinal, event_kind
         )
         INSERT INTO session_message
            (message_id, spawning_tool_request_id, event_ordinal,
             event_kind, direction, content_text)
         SELECT $5, spawning_tool_request_id, event_ordinal, event_kind, $6, $7
           FROM event",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .bind(fixture.message_request.into_uuid())
    .bind(fixture.message_id)
    .bind(direction)
    .bind(RAW_DELEGATED_MESSAGE)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "WITH pending AS (
            INSERT INTO session_pending_delivery
                (recipient_session_id, delivery_sequence, delivery_kind)
            VALUES ($1, 1, 'message')
         )
         INSERT INTO session_message_delivery
            (message_id, spawning_tool_request_id, recipient_session_id,
             delivery_sequence, delivery_kind)
         VALUES ($2, $3, $1, 1, 'message')",
    )
    .bind(recipient.into_uuid())
    .bind(fixture.message_id)
    .bind(fixture.spawning_request.into_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

pub(crate) async fn append_raw_message_update(
    connection: &mut PgConnection,
    fixture: RawDelegationFixture,
    route: RawMessageRoute,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, message_id,
             sender_session_id, recipient_session_id, message_ordinal,
             content_text)
         SELECT event_sequence, event_kind, storage_version, session_id,
                'session_message', $2, $3, $4, $5, 2, $6
           FROM header",
    )
    .bind(route.stream.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.message_id)
    .bind(route.sender.into_uuid())
    .bind(route.recipient.into_uuid())
    .bind(RAW_DELEGATED_MESSAGE)
    .execute(connection)
    .await?;
    Ok(())
}

pub(crate) fn constraint_name(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(|error| error.constraint())
}

pub(crate) async fn prepared_recipient_delivery_fixture(
    seed: u128,
) -> Result<(TestDatabase, PgPool, RawDelegationFixture), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = prepare_canonical_raw_delegation(&pool, seed).await?;
    let mut base = pool.begin().await?;
    insert_raw_delegation_with_update(&mut base, fixture).await?;
    base.commit().await?;
    Ok((container, pool, fixture))
}

pub(crate) async fn prepared_delegation_with_wait(
    seed: u128,
) -> Result<(TestDatabase, PgPool, RawDelegationFixture), Box<dyn Error>> {
    let (container, pool, fixture) = prepared_recipient_delivery_fixture(seed).await?;
    let mut setup = pool.begin().await?;
    insert_raw_wait_with_update(&mut setup, fixture).await?;
    setup.commit().await?;
    Ok((container, pool, fixture))
}

/// Inserts the complete pre-outbox session record family for allocator tests.
///
/// The command and model identities derive from the one session seed.
/// Gives one raw-SQL session fixture the lifecycle row every session owns.
///
/// `session` carries a deferred foreign key to its satellite, so a fixture
/// that inserts a session row by statement owes the same row a creation path
/// writes — including the ownership its creation cause establishes, since an
/// owned fixture that recorded itself unmonitored would run with a posture
/// production never produces.
pub(crate) async fn insert_raw_session_lifecycle(
    connection: &mut sqlx::PgConnection,
    session: Uuid,
    owned: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO session_lifecycle
            (session_id, state_kind, owned, start_gate_held, actor_kind)
         VALUES ($1, 'created', $2, false, 'operator')",
    )
    .bind(session)
    .bind(owned)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_ownership_event
            (session_id, event_ordinal, transition_kind, owned_after, actor_kind)
         VALUES ($1, 1, $2, $3, 'operator')",
    )
    .bind(session)
    .bind(if owned {
        "created_owned"
    } else {
        "created_unmonitored"
    })
    .bind(owned)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(crate) async fn activate_delegated_result_fixture(
    pool: &PgPool,
    seed: u128,
) -> Result<
    (
        SessionId,
        SessionId,
        TurnId,
        ToolRequestId,
        DirectModelSelection,
    ),
    Box<dyn Error>,
> {
    let parent = SessionId::from_uuid(Uuid::from_u128(seed + 1));
    let child = SessionId::from_uuid(Uuid::from_u128(seed + 2));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 3));
    let parent_turn = TurnId::from_uuid(Uuid::from_u128(seed + 4));
    let child_turn = TurnId::from_uuid(Uuid::from_u128(seed + 5));
    let spawning_request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 6));
    let task_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 7));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(seed + 8, seed + 1, direct(seed + 3)))
        .await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(seed + 9, seed + 2, direct(seed + 3)))
        .await?;
    let mut fixture = pool.begin().await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event DISABLE TRIGGER ALL;",
    )
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id)
         VALUES ($1, 1, 'spawned', 'tool_request', $2, $3, $1)",
    )
    .bind(spawning_request.into_uuid())
    .bind(parent.into_uuid())
    .bind(parent_turn.into_uuid())
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind)
         VALUES ($1, $2, $3, $4, 'background')",
    )
    .bind(spawning_request.into_uuid())
    .bind(parent.into_uuid())
    .bind(parent_turn.into_uuid())
    .bind(child.into_uuid())
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_kind, origin_accepted_input_id,
             acceptance_position, state_kind)
         VALUES ($1, $2, 'delegation', NULL, 1, 'queued')",
    )
    .bind(child_turn.into_uuid())
    .bind(child.into_uuid())
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_initial_task
            (spawning_tool_request_id, child_session_id, turn_id,
             semantic_entry_id, admission_position, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             frozen_model_kind, frozen_direct_model_selection_id, task_content)
         VALUES ($1, $2, $3, $4, 1, 1, 'direct', $5, 'direct', $5, $6)",
    )
    .bind(spawning_request.into_uuid())
    .bind(child.into_uuid())
    .bind(child_turn.into_uuid())
    .bind(task_entry.into_uuid())
    .bind(selection.into_uuid())
    .bind("return the delegated result")
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             delegated_task_spawning_tool_request_id)
         VALUES ($1, $2, 'delegated_task', $3)",
    )
    .bind(child.into_uuid())
    .bind(task_entry.into_uuid())
    .bind(spawning_request.into_uuid())
    .execute(&mut *fixture)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event ENABLE TRIGGER ALL;",
    )
    .execute(&mut *fixture)
    .await?;
    fixture.commit().await?;

    let activation = StartEligibleTurnRepository::new(pool.clone());
    let preview = activation
        .preview(
            child,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 10)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 11)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 12)),
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 13)),
            ),
        )
        .await?
        .expect("the delegated result fixture has one activation preview");
    let CommitActivationPreviewOutcome::Activated(_) = activation.commit_preview(preview).await?
    else {
        return Err("the delegated result fixture activation changed".into());
    };
    record_empty_instruction_manifest(pool, child).await?;
    Ok((parent, child, child_turn, spawning_request, selection))
}

pub(crate) struct AuthorizedDelegatedSuccessorFixture {
    pub(crate) child: SessionId,
    pub(crate) selection: DirectModelSelection,
    pub(crate) repository: PostgresModelCallRepository,
    pub(crate) authorized: AuthorizedModelCall,
}

pub(crate) async fn authorize_delegated_successor_model_call_fixture(
    pool: &PgPool,
    seed: u128,
) -> Result<AuthorizedDelegatedSuccessorFixture, Box<dyn Error>> {
    let (_parent, child, _delegated_turn, _spawning_request, selection) =
        activate_delegated_result_fixture(pool, seed).await?;
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 20));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one delegated successor target forms a catalog");
    complete_text_turn(
        pool,
        child,
        targets.clone(),
        model_credential_reference(),
        seed + 0x100,
        "complete the delegated initial turn",
    )
    .await?;
    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 0x201));
    let submitted = SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 0x202,
                child.as_uuid().as_u128(),
                "continue after delegated completion",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x203)),
            Some(successor),
        )
        .await?;
    assert!(matches!(
        submitted,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    activate_earliest_queued_turn(
        pool,
        EarliestQueuedTurnActivation {
            session: child.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 0x204),
            starting_frontier: Uuid::from_u128(seed + 0x205),
            initial_attempt: Uuid::from_u128(seed + 0x206),
        },
    )
    .await?;
    record_empty_instruction_manifest(pool, child).await?;

    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x207));
    assert!(matches!(
        repository
            .prepare_initial_call(
                child,
                call,
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x208)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x209)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x20a)),
                |_| {
                    (
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x20b)),
                        TurnId::from_uuid(Uuid::from_u128(seed + 0x20c)),
                    )
                },
            )
            .await?,
        PrepareInitialModelCallOutcome::Checkpointed(checkpointed) if checkpointed == call
    ));
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        repository.authorize_send(child, call).await?
    else {
        panic!("the accepted-input successor authorizes its exact call")
    };
    Ok(AuthorizedDelegatedSuccessorFixture {
        child,
        selection,
        repository,
        authorized: *authorized,
    })
}

pub(crate) async fn reclassify_successor_as_delegated_wake(
    pool: &PgPool,
    fixture: &AuthorizedDelegatedSuccessorFixture,
) -> Result<(), Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    sqlx::raw_sql(
        "ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;
         ALTER TABLE session_pending_delivery DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wake_turn_origin DISABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET origin_kind = 'delegation', origin_accepted_input_id = NULL
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(fixture.child.into_uuid())
    .bind(fixture.authorized.turn().into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_pending_delivery
            (recipient_session_id, delivery_sequence, delivery_kind)
         VALUES ($1, 1, 'background_result')",
    )
    .bind(fixture.child.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_wake_turn_origin
            (turn_id, recipient_session_id, admission_position,
             first_delivery_sequence, through_delivery_sequence,
             defaults_version, requested_model_kind,
             requested_direct_model_selection_id, frozen_model_kind,
             frozen_direct_model_selection_id)
         SELECT lifecycle.turn_id, lifecycle.session_id,
                lifecycle.acceptance_position, 1, 1, 1,
                'direct', $3, 'direct', $3
           FROM turn_lifecycle AS lifecycle
          WHERE lifecycle.session_id = $1 AND lifecycle.turn_id = $2",
    )
    .bind(fixture.child.into_uuid())
    .bind(fixture.authorized.turn().into_uuid())
    .bind(fixture.selection.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL;
         ALTER TABLE session_pending_delivery ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wake_turn_origin ENABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) struct DelegatedToolCrashFixture {
    pub(crate) parent: SessionId,
    pub(crate) child: SessionId,
    pub(crate) turn: TurnId,
    pub(crate) spawning_request: ToolRequestId,
}

pub(crate) async fn prepare_delegated_tool_crash_fixture(
    pool: &PgPool,
    seed: u128,
) -> Result<DelegatedToolCrashFixture, Box<dyn Error>> {
    let (fixture, _) =
        prepare_delegated_tool_attempt_fixture(pool, seed, ToolEffectClass::EffectFree).await?;
    Ok(fixture)
}

/// An external-effect declaration supplies an ambiguous delegated tool wait.
/// The seed allocates arbitrary distinct fixture identities.
pub(crate) async fn prepare_delegated_tool_recovery_fixture(
    pool: &PgPool,
    seed: u128,
) -> Result<
    (
        DelegatedToolCrashFixture,
        signalbox_domain::EndedToolAttempt,
    ),
    Box<dyn Error>,
> {
    let catalog = ambiguity_fixture_catalog();
    let effect_class = catalog
        .definition(
            &ToolName::try_new(String::from(AMBIGUITY_FIXTURE_TOOL)).expect("fixture tool name"),
        )
        .expect("fixture declaration")
        .effect_class();
    let (fixture, authorized) =
        prepare_delegated_tool_attempt_fixture(pool, seed, effect_class).await?;
    let ended = PostgresToolLoopRepository::new(pool.clone())
        .commit_observation(
            authorized
                .executor_fence()
                .bind(ToolAttemptObservation::Ambiguous),
        )
        .await?;
    Ok((fixture, ended))
}

async fn prepare_delegated_tool_attempt_fixture(
    pool: &PgPool,
    seed: u128,
    effect_class: ToolEffectClass,
) -> Result<
    (
        DelegatedToolCrashFixture,
        signalbox_domain::ToolDispatchAuthority,
    ),
    Box<dyn Error>,
> {
    let fixture = authorize_delegated_model_call_fixture(pool, seed).await?;
    let tool_name = match effect_class {
        ToolEffectClass::EffectFree => "current_time",
        ToolEffectClass::ExternalEffect => AMBIGUITY_FIXTURE_TOOL,
    };
    let request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 0x40));
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new(String::from(tool_name)).expect("valid fixture tool name"),
                NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                    .expect("bounded fixture arguments"),
            ),
        )])
        .expect("the proposal forms a tool-using response");
    let observation = fixture
        .authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
            response,
            retained_input_tokens: None,
            retained_output_tokens: None,
        });
    let outcome = fixture
        .repository
        .apply_terminal_observation(
            fixture.child,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x80)),
                    request,
                    InitialToolApproval::Confirm,
                )],
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xc0)),
                None,
            )),
            |_| panic!("the delegated fixture has no pending steering to reclassify"),
        )
        .await?;
    let ModelCallTerminalOutcome::ToolRound(_) = outcome else {
        panic!("the delegated fixture reaches a tool round")
    };
    let repository = PostgresToolLoopRepository::new(pool.clone());
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x100)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x101)),
        )
        .await?;
    let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x102));
    repository
        .prepare_next_attempt(
            fixture.child,
            fixture.authorized.turn(),
            attempt,
            effect_class,
        )
        .await?;
    let authorized = repository
        .authorize_attempt(fixture.child, fixture.authorized.turn(), attempt)
        .await?;
    Ok((
        DelegatedToolCrashFixture {
            parent: fixture.parent,
            child: fixture.child,
            turn: fixture.authorized.turn(),
            spawning_request: fixture.spawning_request,
        },
        authorized,
    ))
}

pub(crate) struct DelegatedCapabilityFailureFixture {
    pub(crate) repository: PostgresModelCallRepository,
    pub(crate) child: SessionId,
    pub(crate) call: ModelCallId,
    pub(crate) spawning_request: ToolRequestId,
}

pub(crate) async fn delegated_capability_failure_fixture(
    pool: &PgPool,
    seed: u128,
) -> Result<DelegatedCapabilityFailureFixture, Box<dyn Error>> {
    let (_parent, child, _turn, spawning_request, selection) =
        activate_delegated_result_fixture(pool, seed).await?;
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 20));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one delegated capability target forms a catalog");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed + 21));
    let prepared = repository
        .prepare_initial_call(
            child,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 22)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 23)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 24)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 25)),
                    TurnId::from_uuid(Uuid::from_u128(seed + 26)),
                )
            },
        )
        .await?;
    assert_eq!(prepared, PrepareInitialModelCallOutcome::Checkpointed(call));
    repository
        .fail_prepared_call(
            child,
            call,
            PreparedModelCallFailureCause::CapabilityKnownFailure,
            None,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 27)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 28)),
            ),
            |_| panic!("the delegated capability fixture has no steering"),
        )
        .await?;
    Ok(DelegatedCapabilityFailureFixture {
        repository,
        child,
        call,
        spawning_request,
    })
}

#[derive(Clone, Copy)]
pub(crate) enum DelegatedCapabilityResultDamage {
    InitialTask,
    Result,
    Update,
    UpdateHeaderKind,
    Wake,
    WakeHeaderKind,
}

pub(crate) async fn assert_delegated_capability_reread_rejects_damage(
    seed: u128,
    damage: DelegatedCapabilityResultDamage,
) -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = delegated_capability_failure_fixture(&pool, seed).await?;
    assert_eq!(
        fixture
            .repository
            .reread_prepared_failure(fixture.child, fixture.call, None)
            .await?,
        RetainedPreparedFailureStatus::AlreadyCommitted
    );
    match damage {
        DelegatedCapabilityResultDamage::InitialTask => {
            sqlx::query("ALTER TABLE session_delegation_initial_task DISABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
            sqlx::query(
                "DELETE FROM session_delegation_initial_task
                  WHERE spawning_tool_request_id = $1",
            )
            .bind(fixture.spawning_request.into_uuid())
            .execute(&pool)
            .await?;
            sqlx::query("ALTER TABLE session_delegation_initial_task ENABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
        }
        DelegatedCapabilityResultDamage::Result => {
            sqlx::query("ALTER TABLE session_child_result DISABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
            sqlx::query("DELETE FROM session_child_result WHERE spawning_tool_request_id = $1")
                .bind(fixture.spawning_request.into_uuid())
                .execute(&pool)
                .await?;
            sqlx::query("ALTER TABLE session_child_result ENABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
        }
        DelegatedCapabilityResultDamage::Update => {
            sqlx::query("ALTER TABLE delegation_update_outbox_event DISABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
            sqlx::query(
                "DELETE FROM delegation_update_outbox_event
                  WHERE update_kind = 'child_result'
                    AND result_spawning_request_id = $1",
            )
            .bind(fixture.spawning_request.into_uuid())
            .execute(&pool)
            .await?;
            sqlx::query("ALTER TABLE delegation_update_outbox_event ENABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
        }
        DelegatedCapabilityResultDamage::UpdateHeaderKind => {
            sqlx::query("ALTER TABLE delegation_outbox_event DISABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
            sqlx::query(
                "UPDATE delegation_outbox_event AS header
                    SET event_kind = 'delegation_wake'
                   FROM delegation_update_outbox_event AS parent_update
                  WHERE header.event_sequence = parent_update.event_sequence
                    AND parent_update.result_spawning_request_id = $1",
            )
            .bind(fixture.spawning_request.into_uuid())
            .execute(&pool)
            .await?;
            sqlx::query("ALTER TABLE delegation_outbox_event ENABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
        }
        DelegatedCapabilityResultDamage::Wake => {
            sqlx::query("ALTER TABLE delegation_wake_outbox_event DISABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
            sqlx::query(
                "DELETE FROM delegation_wake_outbox_event
                  WHERE subject_kind = 'result'
                    AND result_spawning_request_id = $1",
            )
            .bind(fixture.spawning_request.into_uuid())
            .execute(&pool)
            .await?;
            sqlx::query("ALTER TABLE delegation_wake_outbox_event ENABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
        }
        DelegatedCapabilityResultDamage::WakeHeaderKind => {
            sqlx::query("ALTER TABLE delegation_outbox_event DISABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
            sqlx::query(
                "UPDATE delegation_outbox_event AS header
                    SET event_kind = 'delegation_update'
                   FROM delegation_wake_outbox_event AS parent_wake
                  WHERE header.event_sequence = parent_wake.event_sequence
                    AND parent_wake.result_spawning_request_id = $1",
            )
            .bind(fixture.spawning_request.into_uuid())
            .execute(&pool)
            .await?;
            sqlx::query("ALTER TABLE delegation_outbox_event ENABLE TRIGGER ALL")
                .execute(&pool)
                .await?;
        }
    }
    let error = fixture
        .repository
        .reread_prepared_failure(fixture.child, fixture.call, None)
        .await
        .expect_err("damaged delegated delivery cannot authenticate a capability failure");
    assert!(matches!(
        error,
        ModelCallRepositoryError::InvalidTransition(
            "retained prepared failure durable closure is incomplete"
        )
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) enum DelegatedObservationDisposition {
    Completed,
    KnownFailed,
    Refused,
    Cancelled,
}

pub(crate) async fn assert_delegated_observation_reread_requires_result(
    seed: u128,
    disposition: DelegatedObservationDisposition,
) -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = authorize_delegated_model_call_fixture(&pool, seed).await?;
    let (observation, identities) = match disposition {
        DelegatedObservationDisposition::Completed => (
            fixture
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new(String::from("authenticated delegated result"))
                            .expect("fixture delegated result is admitted"),
                    ],
                }),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 30,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 32)),
            )),
        ),
        DelegatedObservationDisposition::KnownFailed => (
            fixture
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed),
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 30)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 31)),
            )),
        ),
        DelegatedObservationDisposition::Refused => (
            fixture
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Refused),
            ModelCallTerminalIdentities::Refused(
                signalbox_domain::RefusedModelCallTurnIdentities::new(
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30)),
                ),
            ),
        ),
        DelegatedObservationDisposition::Cancelled => (
            fixture
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Cancelled),
            ModelCallTerminalIdentities::PhysicalCancellation(
                PhysicalCancellationModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 30)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 31)),
                ),
            ),
        ),
    };
    fixture
        .repository
        .apply_terminal_observation(fixture.child, observation.clone(), identities, |_| {
            panic!("the delegated observation fixture has no steering")
        })
        .await?;
    assert_eq!(
        fixture
            .repository
            .reread_terminal_observation(fixture.child, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    sqlx::query("ALTER TABLE session_child_result DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM session_child_result WHERE spawning_tool_request_id = $1")
        .bind(fixture.spawning_request.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE session_child_result ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = fixture
        .repository
        .reread_terminal_observation(fixture.child, &observation)
        .await
        .expect_err("a delegated observation reread requires its child result closure");
    assert!(matches!(
        error,
        ModelCallRepositoryError::InvalidTransition(
            "retained observation delegated result closure changed"
        )
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) enum DelegatedNonterminalObservation {
    CompletedWithTools,
    Ambiguous,
}

pub(crate) async fn assert_delegated_nonterminal_reread_rejects_result(
    seed: u128,
    kind: DelegatedNonterminalObservation,
) -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = authorize_delegated_model_call_fixture(&pool, seed).await?;
    let (observation, identities) = match kind {
        DelegatedNonterminalObservation::CompletedWithTools => {
            let request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 30));
            let response =
                ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
                    ToolCallProposal::new(
                        ToolName::try_new(String::from("current_time"))
                            .expect("valid fixture tool name"),
                        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                            .expect("bounded fixture arguments"),
                    ),
                )])
                .expect("the proposal forms a tool-using response");
            (
                fixture
                    .authorized
                    .observation_correlation()
                    .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
                        response,
                        retained_input_tokens: None,
                        retained_output_tokens: None,
                    }),
                ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                    vec![ToolResponsePartIdentity::tool_call(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 31)),
                        request,
                        InitialToolApproval::Confirm,
                    )],
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 32)),
                    None,
                )),
            )
        }
        DelegatedNonterminalObservation::Ambiguous => (
            fixture
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous),
            ModelCallTerminalIdentities::Ambiguous(AmbiguousModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30)),
            )),
        ),
    };
    fixture
        .repository
        .apply_terminal_observation(fixture.child, observation.clone(), identities, |_| {
            panic!("the delegated nonterminal fixture has no steering")
        })
        .await?;
    assert_eq!(
        fixture
            .repository
            .reread_terminal_observation(fixture.child, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    sqlx::query("ALTER TABLE session_child_result DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO session_child_result
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, content_text)
         VALUES ($1, 1, 'outcome_recorded', 'child_failed', NULL)",
    )
    .bind(fixture.spawning_request.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_child_result ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = fixture
        .repository
        .reread_terminal_observation(fixture.child, &observation)
        .await
        .expect_err("a nonterminal observation cannot retain delegated result evidence");

    assert!(matches!(
        error,
        ModelCallRepositoryError::InvalidTransition(
            "retained observation delegated result closure changed"
        )
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

pub(crate) fn delegated_tool_crash_scan_ids(seed: u128) -> FixedStartupScanIds {
    FixedStartupScanIds::new(
        [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
            seed + 0x110,
        ))],
        [ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x111))],
    )
}

pub(crate) fn delegated_tool_crash_failure_ids(seed: u128) -> AcceptedInputTurnFailureIdentities {
    AcceptedInputTurnFailureIdentities::new(
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x112)),
        ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x113)),
    )
}

pub(crate) async fn checkpoint_foreground_child_wait_without_result(
    pool: &PgPool,
    seed: u128,
) -> Result<(RestartModelCallFixture, ToolRequestId, ToolRequestId), Box<dyn Error>> {
    let (fixture, _, _, requests) = checkpoint_confirmed_tool_batch(
        pool,
        seed,
        &[("spawn_session", "{}"), ("await_session", "{}")],
    )
    .await?;
    checkpoint_foreground_child_wait_for_batch(pool, seed, fixture, requests).await
}

async fn checkpoint_foreground_child_wait_for_batch(
    pool: &PgPool,
    seed: u128,
    fixture: RestartModelCallFixture,
    requests: Vec<ToolRequestId>,
) -> Result<(RestartModelCallFixture, ToolRequestId, ToolRequestId), Box<dyn Error>> {
    let [spawning_request, awaiting_request] = requests.as_slice() else {
        panic!("the foreground fixture has spawn and await requests")
    };
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(seed + 0x100, seed + 0x101, direct(seed + 5)))
        .await?;
    let child = Uuid::from_u128(seed + 0x101);
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let issuing_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe0));
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
                *spawning_request,
                ToolApprovalDecision::Approve,
            ),
            || panic!("the first approval does not start execution"),
        )
        .await?;
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd1)),
                *awaiting_request,
                ToolApprovalDecision::Approve,
            ),
            || issuing_attempt,
        )
        .await?;
    sqlx::raw_sql(
        "ALTER TABLE tool_attempt DISABLE TRIGGER ALL;
         ALTER TABLE turn_attempt DISABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;",
    )
    .execute(pool)
    .await?;
    let spawn_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            spawn_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the approved spawn request prepares one attempt");
    let authorized_spawn = repository
        .authorize_attempt(fixture.session, fixture.turn, spawn_attempt)
        .await?;
    repository
        .commit_observation(authorized_spawn.executor_fence().bind(
            ToolAttemptObservation::Completed {
                result: ToolResultContent::Text(
                    ToolResultText::try_new(child.to_string()).expect("bounded child identity"),
                ),
            },
        ))
        .await?;
    let await_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe2));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            await_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the approved await request prepares one attempt");

    sqlx::raw_sql(
        "ALTER TABLE session_delegation DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wait DISABLE TRIGGER ALL;
         ALTER TABLE tool_attempt DISABLE TRIGGER ALL;
         ALTER TABLE turn_attempt DISABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind)
         VALUES ($1, $2, $3, $4, 'background')",
    )
    .bind(spawning_request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(child)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id)
         VALUES ($1, 1, 'spawned', 'tool_request', $2, $3, $1)",
    )
    .bind(spawning_request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_wait
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, parent_turn_id, child_session_id, wait_mode)
         VALUES ($1, $2, $3, $4, $5, 'foreground')",
    )
    .bind(awaiting_request.into_uuid())
    .bind(spawning_request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(child)
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'awaiting_child',
                wait_spawning_request_id = $1,
                wait_child_session_id = $2
          WHERE attempt_id = $3",
    )
    .bind(spawning_request.into_uuid())
    .bind(child)
    .bind(await_attempt.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1",
    )
    .bind(issuing_attempt.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_child', current_attempt_id = NULL,
                child_wait_request_id = $1
          WHERE turn_id = $2",
    )
    .bind(awaiting_request.into_uuid())
    .bind(fixture.turn.into_uuid())
    .execute(pool)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wait ENABLE TRIGGER ALL;
         ALTER TABLE tool_attempt ENABLE TRIGGER ALL;
         ALTER TABLE turn_attempt ENABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL;",
    )
    .execute(pool)
    .await?;

    Ok((fixture, *spawning_request, *awaiting_request))
}

/// A delegated initial task issues spawn and foreground-await in one tool round.
pub(crate) async fn checkpoint_delegated_foreground_child_wait(
    pool: &PgPool,
) -> Result<(RestartModelCallFixture, ToolRequestId, ToolRequestId), Box<dyn Error>> {
    // Arbitrary identity namespace, disjoint from this fixture's generated requests.
    let seed = 0x12ff_8000;
    let delegated = authorize_delegated_model_call_fixture(pool, seed).await?;
    let fixture = RestartModelCallFixture {
        session: delegated.child,
        turn: delegated.authorized.turn(),
        attempt: delegated.authorized.attempt().id(),
        call: delegated.authorized.call().id(),
    };
    let requests = vec![
        ToolRequestId::from_uuid(next_test_submit_uuid()),
        ToolRequestId::from_uuid(next_test_submit_uuid()),
    ];
    let response = ToolUsingAssistantResponse::try_from_parts(
        ["spawn_session", "await_session"]
            .into_iter()
            .map(|name| {
                AssistantResponsePart::ToolCall(ToolCallProposal::new(
                    ToolName::try_new(name.into()).expect("fixture tool"),
                    NormalizedToolArguments::try_from_provider_text("{}".into())
                        .expect("fixture arguments"),
                ))
            })
            .collect(),
    )
    .expect("two calls form a response");
    delegated
        .repository
        .apply_terminal_observation(
            delegated.child,
            delegated
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
                    response,
                    retained_input_tokens: None,
                    retained_output_tokens: None,
                }),
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                requests
                    .iter()
                    .map(|request| {
                        ToolResponsePartIdentity::tool_call(
                            SemanticTranscriptEntryId::from_uuid(next_test_submit_uuid()),
                            *request,
                            InitialToolApproval::Confirm,
                        )
                    })
                    .collect(),
                ContextFrontierId::from_uuid(next_test_submit_uuid()),
                None,
            )),
            |_| panic!("the fixture has no pending steering"),
        )
        .await?;
    checkpoint_foreground_child_wait_for_batch(pool, seed, fixture, requests).await
}
