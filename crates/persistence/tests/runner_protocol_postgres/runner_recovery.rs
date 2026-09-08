//! Runner recovery coverage.

use super::*;

pub(crate) async fn stored_side_effecting_pin_fixture(
    pool: &PgPool,
) -> Result<
    (
        RunnerProtocolStore,
        RunnerEnrollment,
        StoredValidatedRunnerRegistration,
        SessionRunnerPin,
    ),
    Box<dyn Error>,
> {
    stored_pin_fixture_with_authorization(
        pool,
        external_authorized,
        side_effecting_catalog(),
        permission_overrides(RunnerToolPermissionOverride::Auto),
        "external_effect",
    )
    .await
}

pub(crate) async fn stored_side_effecting_later_lease_fixture(
    pool: &PgPool,
) -> Result<
    (
        RunnerProtocolStore,
        RunnerEnrollment,
        StoredValidatedRunnerRegistration,
        SessionRunnerPin,
        RunnerLease,
    ),
    Box<dyn Error>,
> {
    stored_later_lease_fixture_with_authorization(
        pool,
        external_authorized,
        side_effecting_catalog(),
        permission_overrides(RunnerToolPermissionOverride::Auto),
        "external_effect",
    )
    .await
}

pub(crate) async fn record_no_execution_lease_loss(
    pool: &PgPool,
    lease: &RunnerLease,
) -> Result<(), sqlx::Error> {
    let correlation = lease.correlation();
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, $2, 2, 'lost_unclaimed')",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE runner_current_lease_event
            SET event_ordinal = 2
          WHERE lease_id = $1 AND generation = $2",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_lease_no_execution_proof
            (lease_id, generation, attempt_id, session_id,
             runner_id, tool_name, turn_id,
             issuing_turn_attempt_id, request_id, dispatch_generation)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .bind(correlation.dispatch.attempt().into_uuid())
    .bind(correlation.dispatch.session().into_uuid())
    .bind(correlation.runner.into_uuid())
    .bind(correlation.tool.as_str())
    .bind(correlation.dispatch.turn().into_uuid())
    .bind(correlation.dispatch.issuing_attempt().into_uuid())
    .bind(correlation.dispatch.request().into_uuid())
    .bind(Decimal::from(correlation.dispatch.generation().as_u64()))
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

pub(crate) async fn insert_runner_recovery_turn(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    runner: RunnerId,
    placement_revision: RunnerGeneration,
    interrupted_tool_attempt: Option<ToolAttemptId>,
    active_tool_round_call: Option<ModelCallId>,
) -> Result<(), sqlx::Error> {
    let starting_frontier =
        ContextFrontierId::from_uuid(uuid(turn.into_uuid().as_u128() + RELATED_IDENTITY_OFFSET));
    let yielded_attempt = uuid(turn.into_uuid().as_u128() + RELATED_IDENTITY_OFFSET);
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(session.into_uuid())
    .bind(starting_frontier.into_uuid())
    .execute(pool)
    .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("ALTER TABLE turn_attempt DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "ALTER TABLE turn_lifecycle
         ENABLE TRIGGER turn_lifecycle_runner_recovery_is_complete",
    )
    .execute(&mut *transaction)
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_kind, origin_accepted_input_id,
             acceptance_position, state_kind, start_lineage_kind,
             starting_frontier_id, active_phase_kind,
             active_tool_round_call_id, runner_recovery_runner_id,
             runner_recovery_placement_revision,
             runner_recovery_tool_attempt_id)
         VALUES ($1, $2, 'delegation', NULL, 1, 'active',
                 'first_in_session', $3, 'awaiting_runner_recovery',
                 $4, $5, $6, $7)",
    )
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .bind(starting_frontier.into_uuid())
    .bind(active_tool_round_call.map(ModelCallId::into_uuid))
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement_revision.get()))
    .bind(interrupted_tool_attempt.map(ToolAttemptId::into_uuid))
    .execute(&mut *transaction)
    .await;
    inserted?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'ended', 'without_stop',
                 'yielded_to_durable_wait')",
    )
    .bind(yielded_attempt)
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query("ALTER TABLE turn_attempt ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) async fn append_denied_request_to_continuing_tool_round_projection(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    producing_call: ModelCallId,
    request: ToolRequestId,
    boundary: ContextFrontierId,
) -> Result<(), sqlx::Error> {
    let member_count: Decimal = sqlx::query_scalar(
        "SELECT member_count
           FROM context_frontier
          WHERE owning_session_id = $1 AND context_frontier_id = $2",
    )
    .bind(session.into_uuid())
    .bind(boundary.into_uuid())
    .fetch_one(pool)
    .await?;
    let assistant_entry = uuid(request.into_uuid().as_u128() + RELATED_IDENTITY_OFFSET);
    sqlx::raw_sql(
        "ALTER TABLE tool_round DISABLE TRIGGER ALL;
         ALTER TABLE tool_request DISABLE TRIGGER ALL;
         ALTER TABLE decide_tool_request_command DISABLE TRIGGER ALL;
         ALTER TABLE tool_approval_decision DISABLE TRIGGER ALL;
         ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL;
         ALTER TABLE context_frontier DISABLE TRIGGER ALL;
         ALTER TABLE context_frontier_delta DISABLE TRIGGER ALL;",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE tool_round
            SET response_part_count = 2, request_count = 2
          WHERE producing_model_call_id = $1",
    )
    .bind(producing_call.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text)
         VALUES ($1, $2, $3, $4, 1, 'inspect', 'json', '{}')",
    )
    .bind(request.into_uuid())
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(producing_call.into_uuid())
    .execute(pool)
    .await?;
    let command = uuid(request.into_uuid().as_u128() + (RELATED_IDENTITY_OFFSET * 2));
    let mut decision = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'decide_tool_request', 1, transaction_timestamp(), 'operator')",
    )
    .bind(command)
    .execute(&mut *decision)
    .await?;
    sqlx::query(
        "INSERT INTO decide_tool_request_command
            (command_id, command_kind, storage_version, request_id,
             decision_kind, denial_reason, result_kind, rejection_kind,
             result_earliest_undecided_request_id)
         VALUES ($1, 'decide_tool_request', 1, $2, 'deny', NULL,
                 'applied', NULL, NULL)",
    )
    .bind(command)
    .bind(request.into_uuid())
    .execute(&mut *decision)
    .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, denial_reason,
             user_command_id)
         VALUES ($1, 'deny', 'user_command', NULL, $2)",
    )
    .bind(request.into_uuid())
    .bind(command)
    .execute(&mut *decision)
    .await?;
    decision.commit().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             producing_model_call_id, assistant_tool_request_id,
             assistant_response_part_ordinal,
             assistant_response_text_start_bytes)
         VALUES ($1, $2, 'assistant_tool_use', $3, $4, 1, NULL)",
    )
    .bind(session.into_uuid())
    .bind(assistant_entry)
    .bind(producing_call.into_uuid())
    .bind(request.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE context_frontier
            SET member_count = member_count + 1
          WHERE owning_session_id = $1 AND context_frontier_id = $2",
    )
    .bind(session.into_uuid())
    .bind(boundary.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, $3 + 1, $1, $4)",
    )
    .bind(session.into_uuid())
    .bind(boundary.into_uuid())
    .bind(member_count)
    .bind(assistant_entry)
    .execute(pool)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE tool_round ENABLE TRIGGER ALL;
         ALTER TABLE tool_request ENABLE TRIGGER ALL;
         ALTER TABLE decide_tool_request_command ENABLE TRIGGER ALL;
         ALTER TABLE tool_approval_decision ENABLE TRIGGER ALL;
         ALTER TABLE semantic_transcript_entry ENABLE TRIGGER ALL;
         ALTER TABLE context_frontier ENABLE TRIGGER ALL;
         ALTER TABLE context_frontier_delta ENABLE TRIGGER ALL;",
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn convert_running_turn_to_delegated_runner_recovery(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
    runner: RunnerId,
    placement_revision: RunnerGeneration,
) -> Result<ContextFrontierId, sqlx::Error> {
    let spawning_request = uuid(0xa130);
    let parent_session = uuid(FOREIGN_SESSION);
    let parent_turn = uuid(0xa132);
    let task_entry = uuid(0xa133);
    let starting_frontier = ContextFrontierId::from_uuid(uuid(0xa134));
    let selection = uuid(0xa101);
    insert_session_for(pool, parent_session).await?;
    let accepted_starting_frontier: Uuid = sqlx::query_scalar(
        "SELECT starting_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(pool)
    .await?;
    let mut transaction = pool.begin().await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event DISABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;
         ALTER TABLE queued_input_origin DISABLE TRIGGER ALL;
         ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL;
         ALTER TABLE context_frontier DISABLE TRIGGER ALL;
         ALTER TABLE context_frontier_delta DISABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "DELETE FROM context_frontier_delta
          WHERE owning_session_id = $1 AND context_frontier_id = $2",
    )
    .bind(session.into_uuid())
    .bind(accepted_starting_frontier)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "DELETE FROM context_frontier
          WHERE owning_session_id = $1 AND context_frontier_id = $2",
    )
    .bind(session.into_uuid())
    .bind(accepted_starting_frontier)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "DELETE FROM semantic_transcript_entry
          WHERE source_session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id)
         VALUES ($1, 1, 'spawned', 'tool_request', $2, $3, $1)",
    )
    .bind(spawning_request)
    .bind(parent_session)
    .bind(parent_turn)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind,
             on_parent_stopped, on_parent_cancelled)
         VALUES ($1, $2, $3, $4, 'background', NULL, NULL)",
    )
    .bind(spawning_request)
    .bind(parent_session)
    .bind(parent_turn)
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             delegated_task_spawning_tool_request_id)
         VALUES ($1, $2, 'delegated_task', $3)",
    )
    .bind(session.into_uuid())
    .bind(task_entry)
    .bind(spawning_request)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 1)",
    )
    .bind(session.into_uuid())
    .bind(starting_frontier.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, 1, $1, $3)",
    )
    .bind(session.into_uuid())
    .bind(starting_frontier.into_uuid())
    .bind(task_entry)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET origin_kind = 'delegation', origin_accepted_input_id = NULL,
                starting_frontier_id = $1,
                active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(starting_frontier.into_uuid())
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement_revision.get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "DELETE FROM queued_input_origin
          WHERE turn_id = $1 AND session_id = $2",
    )
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_initial_task
            (spawning_tool_request_id, child_session_id, turn_id,
             semantic_entry_id, admission_position, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             frozen_model_kind, frozen_direct_model_selection_id, task_content)
         VALUES ($1, $2, $3, $4, 1, 1,
                 'direct', $5, 'direct', $5, $6)",
    )
    .bind(spawning_request)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(task_entry)
    .bind(selection)
    .bind("delegated runner recovery fixture")
    .execute(&mut *transaction)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE queued_input_origin ENABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation ENABLE TRIGGER ALL;
         ALTER TABLE context_frontier_delta ENABLE TRIGGER ALL;
         ALTER TABLE context_frontier ENABLE TRIGGER ALL;
         ALTER TABLE semantic_transcript_entry ENABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(starting_frontier)
}

pub(crate) async fn make_accepted_turn_direct_root(
    pool: &PgPool,
    command: DurableCommandId,
    accepted_input: AcceptedInputId,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::raw_sql(
        "ALTER TABLE submit_input_command DISABLE TRIGGER ALL;
         ALTER TABLE accepted_input DISABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE submit_input_command
            SET delivery_kind = 'start_when_no_active_turn',
                expected_active_turn_id = NULL
          WHERE command_id = $1",
    )
    .bind(command.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE accepted_input
            SET delivery_kind = 'start_when_no_active_turn',
                expected_active_turn_id = NULL
          WHERE accepted_input_id = $1",
    )
    .bind(accepted_input.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE accepted_input ENABLE TRIGGER ALL;
         ALTER TABLE submit_input_command ENABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

pub(crate) async fn append_pre_pin_replacement_without_advancing_head(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
    successor: RunnerId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, selector_capability_class,
             directory_selection_kind, requested_working_directory,
             requested_credential_profile_name, workspace_requirement_kind,
             requested_repository_key, requested_sandbox_profile,
             permission_override_count, state_kind, lost_runner_id,
             loss_source_kind, pinned_runner_id,
             pinned_working_directory, pinned_credential_profile_name,
             registration_enrollment_id, registration_revision,
             pinned_tool_count, workspace_repository_key,
             workspace_working_directory, workspace_manifest_id,
             workspace_placement_revision,
             workspace_clone_url_digest, workspace_credential_profile_name,
             workspace_sandbox_profile, workspace_relative_path,
             workspace_recovery_kind, workspace_branch_name, workspace_revision,
             credential_grant_runner_id,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision)
         SELECT session_id, event_ordinal + 1, placement_revision + 1,
                'pre_pin_replaced', 'identity', $2, NULL,
                directory_selection_kind, requested_working_directory,
                requested_credential_profile_name,
                workspace_requirement_kind, requested_repository_key,
                requested_sandbox_profile, permission_override_count,
                'unpinned', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0,
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                NULL, NULL, NULL, NULL
           FROM runner_session_placement_record
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .bind(successor.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_permission_override
         SELECT session_id, event_ordinal + 1, tool_name, permission_kind
           FROM runner_session_placement_permission_override
          WHERE session_id = $1
            AND event_ordinal = (
                SELECT event_ordinal
                  FROM runner_current_session_placement
                 WHERE session_id = $1
            )",
    )
    .bind(session.into_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// the ordinary all-trigger lifecycle transition admits an
/// active running turn at the exact pre-pin runner-loss boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn running_turn_enters_runner_recovery_with_all_triggers() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let loaded = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the all-trigger transition stores its runner recovery wait");

    assert_eq!(loaded.turn(), turn);
    assert_eq!(loaded.runner(), runner);
    assert_eq!(loaded.placement_revision(), placement.revision());
    assert_eq!(loaded.interrupted_tool_attempt(), None);
    drop(pool);
    Ok(())
}

/// runner recovery is available only after the exact live
/// turn attempt has yielded to its durable loss boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_rejects_non_yielded_turn_boundary() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    let rejected = sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await
    .expect_err("a non-yielded attempt cannot become a runner recovery wait");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

/// a retained continuing tool round must have been
/// produced by the unique yielded chain-tip turn attempt.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_rejects_stale_tool_round_boundary() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let producing_call = ModelCallId::from_uuid(uuid(0xa16a));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        ToolRequestId::from_uuid(uuid(0xa16b)),
        ContextFrontierId::from_uuid(uuid(0xa16c)),
    )
    .await?;
    let chain_tip = TurnAttemptId::from_uuid(uuid(0xa16d));
    sqlx::raw_sql(
        "ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;
         ALTER TABLE turn_attempt DISABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    let mut corrupted_source = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *corrupted_source)
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, $4, 'ended', 'without_stop',
                 'yielded_to_durable_wait')",
    )
    .bind(chain_tip.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .bind(turn_attempt.into_uuid())
    .execute(&mut *corrupted_source)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET current_attempt_id = $1
          WHERE turn_id = $2 AND session_id = $3",
    )
    .bind(chain_tip.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *corrupted_source)
    .await?;
    corrupted_source.commit().await?;
    sqlx::raw_sql(
        "ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL;
         ALTER TABLE turn_attempt ENABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(producing_call.into_uuid())
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    let rejected = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("runner recovery cannot retain an older attempt's tool round");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

/// a nullable runner-recovery wait can retain only a
/// continuing tool round, never a round already closed by turn end.
#[tokio::test]
#[ignore = "requires Docker"]
async fn nullable_runner_wait_rejects_closed_tool_round_boundary() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let producing_call = ModelCallId::from_uuid(uuid(0xa16e));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        ToolRequestId::from_uuid(uuid(0xa16f)),
        ContextFrontierId::from_uuid(uuid(0xa170)),
    )
    .await?;
    sqlx::query("ALTER TABLE tool_round DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_round
            SET boundary_kind = 'closed_by_turn_end'
          WHERE producing_model_call_id = $1",
    )
    .bind(producing_call.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_round ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(producing_call.into_uuid())
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    let rejected = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("runner recovery cannot retain a closed tool round");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

/// a runner-recovery wait naming the interrupted physical
/// attempt also requires that attempt's tool round to remain continuing.
#[tokio::test]
#[ignore = "requires Docker"]
async fn interrupted_runner_wait_rejects_closed_tool_round_boundary() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss_boundary(
        &pool,
        InterruptedLossRecoveryFacts {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: producing_call,
        },
        "closed_by_turn_end",
    )
    .await
    .expect_err("runner recovery cannot retain an interrupted attempt from a closed tool round");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a nullable runner-recovery wait cannot erase the
/// continuing tool-round boundary produced by its yielded chain-tip attempt.
#[tokio::test]
#[ignore = "requires Docker"]
async fn nullable_runner_wait_rejects_hidden_tool_round() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        ModelCallId::from_uuid(uuid(0xa17c)),
        ToolRequestId::from_uuid(uuid(0xa17d)),
        ContextFrontierId::from_uuid(uuid(0xa17e)),
    )
    .await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    let rejected = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("runner recovery cannot erase its yielded tool-round boundary");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

/// a tool round inserted after a nullable runner wait
/// rechecks and rejects the now-hidden yielded boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn late_tool_round_rechecks_nullable_runner_wait() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let producing_call = ModelCallId::from_uuid(uuid(0xa17f));
    let request = ToolRequestId::from_uuid(uuid(0xa180));
    let boundary = ContextFrontierId::from_uuid(uuid(0xa181));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        request,
        boundary,
    )
    .await?;
    sqlx::query("ALTER TABLE tool_round DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM tool_round WHERE producing_model_call_id = $1")
        .bind(producing_call.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE tool_round ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = sqlx::query(
        "INSERT INTO tool_round
            (producing_model_call_id, session_id, turn_id, boundary_kind,
             boundary_frontier_id, response_part_count, request_count)
         VALUES ($1, $2, $3, 'continuing', $4, 1, 1)",
    )
    .bind(producing_call.into_uuid())
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(boundary.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a late continuing round must expose the hidden runner-wait boundary");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a tool-round writer takes the scheduler rendezvous
/// before inserting a round that would invalidate a nullable runner wait.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn runner_recovery_serializes_tool_round_inserts() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let producing_call = ModelCallId::from_uuid(uuid(0xa182));
    let request = ToolRequestId::from_uuid(uuid(0xa183));
    let boundary = ContextFrontierId::from_uuid(uuid(0xa184));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        request,
        boundary,
    )
    .await?;
    sqlx::query("ALTER TABLE tool_round DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM tool_round WHERE producing_model_call_id = $1")
        .bind(producing_call.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE tool_round ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut stop = pool.begin().await?;
    sqlx::query(
        "SELECT session_id
           FROM session_scheduler
          WHERE session_id = $1
          FOR UPDATE",
    )
    .bind(session.into_uuid())
    .fetch_one(&mut *stop)
    .await?;
    let mut late_round = Box::pin(
        sqlx::query(
            "INSERT INTO tool_round
                (producing_model_call_id, session_id, turn_id, boundary_kind,
                 boundary_frontier_id, response_part_count, request_count)
             VALUES ($1, $2, $3, 'continuing', $4, 1, 1)",
        )
        .bind(producing_call.into_uuid())
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .bind(boundary.into_uuid())
        .execute(&pool),
    );
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut late_round)
        .await
        .expect_err("tool-round insertion must wait for the scheduler rendezvous");
    stop.rollback().await?;
    let rejected = tokio::time::timeout(Duration::from_secs(10), &mut late_round)
        .await
        .expect("tool-round admission finishes after the scheduler is released")
        .expect_err("the hidden runner-wait boundary still rejects the tool round");

    assert_check_violation(rejected);
    drop(late_round);
    drop(pool);
    Ok(())
}

/// a nullable runner-recovery wait cannot hide a live
/// physical attempt in its retained tool round.
#[tokio::test]
#[ignore = "requires Docker"]
async fn nullable_runner_wait_rejects_unrecorded_physical_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        ToolRequestId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.request)),
        ContextFrontierId::from_uuid(uuid(0xa16e)),
    )
    .await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(producing_call.into_uuid())
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    let rejected = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("nullable runner recovery cannot omit a live physical attempt");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

/// a nullable runner-recovery wait cannot retain a
/// prepared physical attempt that stop handling would classify as current.
#[tokio::test]
#[ignore = "requires Docker"]
async fn nullable_runner_wait_rejects_prepared_physical_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        ToolRequestId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.request)),
        ContextFrontierId::from_uuid(uuid(0xa16f)),
    )
    .await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'prepared'
          WHERE attempt_id = $1",
    )
    .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(producing_call.into_uuid())
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    let rejected = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *malformed)
        .await
        .expect_err("nullable runner recovery cannot retain a prepared attempt");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

/// a retired claimed-retry predecessor is historical
/// inventory and does not make a resolved current round ambiguous.
#[tokio::test]
#[ignore = "requires Docker"]
async fn nullable_runner_wait_ignores_retired_claimed_retry_attempt() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        ToolRequestId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.request)),
        ContextFrontierId::from_uuid(uuid(0xa170)),
    )
    .await?;
    insert_external_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), idempotent_catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        session,
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: permission_overrides(RunnerToolPermissionOverride::Auto),
        },
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/idempotent".to_owned())
                .expect("the idempotent fixture directory is valid"),
            None,
            authorized_with_effect(INITIAL_PHYSICAL_ATTEMPT, ToolEffectClass::ExternalEffect),
            offer_request(),
        )
        .expect("the idempotent registration pins its external-effect attempt");
    store.store_pin(&pin, &registration).await?;
    let claimed = duplicate_lease(&pin.lease, registration.registration())
        .claim(pin.lease.correlation())
        .expect("the exact idempotent lease fence claims");
    store.store_lease(&claimed).await?;
    let loss = claimed
        .lose()
        .expect("claimed idempotent work admits a checked retry");
    store_fixture_retryable_loss(&store, &pool, &loss).await?;
    let replacement =
        authorize_fixture_claimed_retry(&store, &loss, ToolEffectClass::ExternalEffect).await?;
    let (_batch, retired, retry_authorization) = replacement.into_parts();
    let retry = pin
        .placement
        .offer_retry(
            &expected_enrollment,
            registration.registration(),
            pin.grant.as_ref(),
            loss,
            retry_authorization,
        )
        .expect("claimed idempotent work re-leases at the successor generation");
    store_fixture_claimed_retry_replacement(&store, &pool, &retired, &retry).await?;
    terminalize_physical_attempt(&pool, RETRY_PHYSICAL_ATTEMPT).await?;
    let mut runner_loss = pool.begin().await?;
    append_runner_lost_without_advancing_head(
        &mut runner_loss,
        session,
        Some("connection"),
        None,
        None,
    )
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *runner_loss)
    .await?;
    runner_loss.commit().await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3,
                runner_recovery_tool_attempt_id = NULL
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(producing_call.into_uuid())
    .bind(expected_enrollment.runner().into_uuid())
    .bind(Decimal::from(pin.placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *recovery)
        .await?;
    recovery.commit().await?;
    let loaded = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the nullable wait ignores the retired predecessor");

    assert_eq!(loaded.interrupted_tool_attempt(), None);
    drop(pool);
    Ok(())
}

/// an accepted-input interrupt terminalizes a runner-loss
/// wait and leaves the placement's runner-effect evidence untouched.
#[tokio::test]
#[ignore = "requires Docker"]
async fn stop_terminalizes_runner_recovery_wait() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let interrupt = SubmitInput::new(
        DurableCommandId::from_uuid(uuid(0xa120)),
        session,
        UserContent::try_text(String::from("stop runner recovery"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa121)),
            Some(TurnId::from_uuid(uuid(0xa122))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa123)),
                ContextFrontierId::from_uuid(uuid(0xa124)),
            ),
            |_| TurnId::from_uuid(uuid(0xa125)),
            |_| (Vec::new(), ContextFrontierId::from_uuid(uuid(0xa126))),
        )
        .await?;
    let reload = StartEligibleTurnRepository::new(pool.clone())
        .preview(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa127)),
                SemanticTranscriptEntryId::from_uuid(uuid(0xa128)),
                ContextFrontierId::from_uuid(uuid(0xa129)),
                TurnAttemptId::from_uuid(uuid(0xa12a)),
            ),
        )
        .await?;
    let terminal: (String, Option<String>, Option<Uuid>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind,
                runner_recovery_runner_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    let retained_loss: String = sqlx::query_scalar(
        "SELECT record.state_kind
           FROM runner_current_session_placement AS head
           JOIN runner_session_placement_record AS record
             ON record.session_id = head.session_id
            AND record.event_ordinal = head.event_ordinal
          WHERE head.session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    let persisted_effect: (Uuid, Uuid, Decimal, Uuid) = sqlx::query_as(
        "SELECT turn_id, runner_id, placement_revision,
                yielded_turn_attempt_id
           FROM turn_runner_recovery_interrupt_effect
          WHERE command_id = $1",
    )
    .bind(uuid(0xa120))
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        terminal,
        (
            String::from("terminal"),
            Some(String::from("cancelled")),
            None
        )
    );
    assert_eq!(retained_loss, "runner_lost_before_pin");
    assert!(
        reload.is_none(),
        "the successor remains queued while placement is lost"
    );
    assert_eq!(
        persisted_effect,
        (
            turn.into_uuid(),
            runner.into_uuid(),
            Decimal::from(placement.revision().get()),
            turn_attempt.into_uuid(),
        )
    );
    assert_eq!(store.load_runner_recovery_wait(session).await?, None);
    drop(pool);
    Ok(())
}

/// stopping a recovery wait with an active tool round uses
/// the round's yielded frontier instead of the ordinary active-batch decoder.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_stop_uses_tool_round_boundary() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let producing_call = ModelCallId::from_uuid(uuid(0xa150));
    let request = ToolRequestId::from_uuid(uuid(0xa15b));
    let boundary = ContextFrontierId::from_uuid(uuid(0xa151));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        request,
        boundary,
    )
    .await?;
    let denied_request = ToolRequestId::from_uuid(uuid(0xa15c));
    append_denied_request_to_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        producing_call,
        denied_request,
        boundary,
    )
    .await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(producing_call.into_uuid())
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let command = DurableCommandId::from_uuid(uuid(0xa152));
    let tool_closure = SemanticTranscriptEntryId::from_uuid(uuid(0xa15a));
    let denied_result = SemanticTranscriptEntryId::from_uuid(uuid(0xa15e));
    let interrupt = SubmitInput::new(
        command,
        session,
        UserContent::try_text(String::from("stop tool-round runner recovery"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa153)),
            Some(TurnId::from_uuid(uuid(0xa154))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa155)),
                ContextFrontierId::from_uuid(uuid(0xa156)),
            ),
            |_| TurnId::from_uuid(uuid(0xa157)),
            |_| {
                (
                    vec![tool_closure, denied_result],
                    ContextFrontierId::from_uuid(uuid(0xa158)),
                )
            },
        )
        .await?;
    let persisted_source: Uuid = sqlx::query_scalar(
        "SELECT source_frontier_id
           FROM turn_runner_recovery_interrupt_effect
          WHERE command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_one(&pool)
    .await?;
    let closure: (String, Uuid) = sqlx::query_as(
        "SELECT payload_kind, tool_result_request_id
           FROM semantic_transcript_entry
          WHERE source_session_id = $1 AND semantic_entry_id = $2",
    )
    .bind(session.into_uuid())
    .bind(tool_closure.into_uuid())
    .fetch_one(&pool)
    .await?;
    let denied: (String, Uuid) = sqlx::query_as(
        "SELECT payload_kind, tool_result_request_id
           FROM semantic_transcript_entry
          WHERE source_session_id = $1 AND semantic_entry_id = $2",
    )
    .bind(session.into_uuid())
    .bind(denied_result.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(persisted_source, boundary.into_uuid());
    assert_eq!(closure.0, "tool_closed_by_turn_end");
    assert_eq!(closure.1, request.into_uuid());
    assert_eq!(denied.0, "tool_denied");
    assert_eq!(denied.1, denied_request.into_uuid());
    sqlx::query("ALTER TABLE context_frontier_delta DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corrupted_member = sqlx::query(
        "UPDATE context_frontier_delta
            SET semantic_entry_id = $1
          WHERE owning_session_id = $2
            AND semantic_entry_id = $3",
    )
    .bind(uuid(0xa15f))
    .bind(session.into_uuid())
    .bind(tool_closure.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE context_frontier_delta ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    assert_eq!(corrupted_member.rows_affected(), 1);
    let malformed = sqlx::query("SELECT assert_cancelled_turn_final_state($1)")
        .bind(turn.into_uuid())
        .execute(&pool)
        .await
        .expect_err("runner recovery cancellation authenticates every tool-result suffix member");

    assert_check_violation(malformed);
    drop(pool);
    Ok(())
}

/// stopping a runner wait preserves an interrupted
/// external-effect attempt as reconciliation-required at the round boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_stop_preserves_tool_ambiguity() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    insert_external_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), side_effecting_catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        session,
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::Exact(
                RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                    .expect("the exact fixture directory is valid"),
            ),
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            permission_overrides: no_permission_overrides(),
        },
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture working directory is valid"),
            None,
            authorized_with_effect(INITIAL_PHYSICAL_ATTEMPT, ToolEffectClass::ExternalEffect),
            offer_request(),
        )
        .expect("the external-effect fixture pins the placement");
    store.store_pin(&pin, &registration).await?;
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    let request = ToolRequestId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.request));
    let boundary = ContextFrontierId::from_uuid(uuid(0xa160));
    attach_continuing_tool_round_projection(
        &pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        request,
        boundary,
    )
    .await?;
    let mut recovery = pool.begin().await?;
    append_runner_lost_without_advancing_head(
        &mut recovery,
        session,
        Some("connection"),
        None,
        Some(interrupted_attempt),
    )
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3,
                runner_recovery_tool_attempt_id = $4
          WHERE turn_id = $5 AND session_id = $6",
    )
    .bind(producing_call.into_uuid())
    .bind(expected_enrollment.runner().into_uuid())
    .bind(Decimal::from(pin.placement.revision().get()))
    .bind(interrupted_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let command = DurableCommandId::from_uuid(uuid(0xa161));
    let terminal_frontier = ContextFrontierId::from_uuid(uuid(0xa162));
    let tool_closure = SemanticTranscriptEntryId::from_uuid(uuid(0xa163));
    let interrupt = SubmitInput::new(
        command,
        session,
        UserContent::try_text(String::from("stop interrupted runner attempt"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa164)),
            Some(TurnId::from_uuid(uuid(0xa165))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa166)),
                ContextFrontierId::from_uuid(uuid(0xa167)),
            ),
            |_| TurnId::from_uuid(uuid(0xa168)),
            |_| (vec![tool_closure], terminal_frontier),
        )
        .await?;
    let reload = StartEligibleTurnRepository::new(pool.clone())
        .preview(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa169)),
                SemanticTranscriptEntryId::from_uuid(uuid(0xa16a)),
                ContextFrontierId::from_uuid(uuid(0xa16b)),
                TurnAttemptId::from_uuid(uuid(0xa16c)),
            ),
        )
        .await?;
    let persisted: (String, Uuid, Uuid) = sqlx::query_as(
        "SELECT lifecycle.terminal_disposition_kind,
                lifecycle.terminal_tool_attempt_id,
                effect.source_frontier_id
           FROM turn_lifecycle AS lifecycle
           JOIN turn_runner_recovery_interrupt_effect AS effect
             ON effect.turn_id = lifecycle.turn_id
            AND effect.session_id = lifecycle.session_id
          WHERE effect.command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_one(&pool)
    .await?;
    let closure: (String, Uuid) = sqlx::query_as(
        "SELECT payload_kind, tool_result_request_id
           FROM semantic_transcript_entry
          WHERE source_session_id = $1 AND semantic_entry_id = $2",
    )
    .bind(session.into_uuid())
    .bind(tool_closure.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(persisted.0, "reconciliation_required");
    assert_eq!(persisted.1, interrupted_attempt.into_uuid());
    assert_eq!(persisted.2, boundary.into_uuid());
    assert!(
        reload.is_none(),
        "the successor remains queued while placement is lost"
    );
    assert_eq!(closure.0, "tool_closed_by_turn_end");
    assert_eq!(closure.1, request.into_uuid());
    drop(pool);
    Ok(())
}

pub(crate) struct RunnerRecoveryToolRoundFacts {
    pub(crate) session: SessionId,
    pub(crate) turn: TurnId,
    pub(crate) turn_attempt: TurnAttemptId,
    pub(crate) interrupted_attempt: ToolAttemptId,
    pub(crate) boundary: ContextFrontierId,
    pub(crate) request: ToolRequestId,
    pub(crate) lease: RunnerLease,
    pub(crate) runner: RunnerId,
    pub(crate) placement_revision: RunnerGeneration,
    pub(crate) producing_call: ModelCallId,
}

pub(crate) async fn prepare_runner_recovery_tool_round(
    pool: &PgPool,
    authorize: fn(PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization,
    fixture_catalog: RunnerCatalog,
    fixture_effect_kind: &'static str,
) -> Result<RunnerRecoveryToolRoundFacts, Box<dyn Error>> {
    let (session, turn, turn_attempt) = insert_running_turn(pool).await?;
    insert_physical_attempt(pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    set_fixture_physical_attempt_effect(pool, INITIAL_PHYSICAL_ATTEMPT, fixture_effect_kind)
        .await?;
    let store = RunnerProtocolStore::new(pool.clone(), fixture_catalog);
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        session,
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::Exact(
                RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                    .expect("the exact fixture directory is valid"),
            ),
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            permission_overrides: no_permission_overrides(),
        },
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture working directory is valid"),
            None,
            authorize(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the retryable fixture pins the placement");
    store.store_pin(&pin, &registration).await?;
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    let request = ToolRequestId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.request));
    let boundary = ContextFrontierId::from_uuid(uuid(0xa170));
    attach_continuing_tool_round_projection(
        pool,
        session,
        turn,
        turn_attempt,
        producing_call,
        request,
        boundary,
    )
    .await?;
    Ok(RunnerRecoveryToolRoundFacts {
        session,
        turn,
        turn_attempt,
        interrupted_attempt,
        boundary,
        request,
        lease: pin.lease,
        runner: expected_enrollment.runner(),
        placement_revision: pin.placement.revision(),
        producing_call,
    })
}

pub(crate) async fn park_runner_recovery_tool_round(
    pool: &PgPool,
    facts: &RunnerRecoveryToolRoundFacts,
) -> Result<(), Box<dyn Error>> {
    let mut recovery = pool.begin().await?;
    append_runner_lost_without_advancing_head(
        &mut recovery,
        facts.session,
        Some("connection"),
        None,
        Some(facts.interrupted_attempt),
    )
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = $1",
    )
    .bind(facts.session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(facts.turn_attempt.into_uuid())
    .bind(facts.turn.into_uuid())
    .bind(facts.session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL, active_tool_round_call_id = $1,
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3,
                runner_recovery_tool_attempt_id = $4
          WHERE turn_id = $5 AND session_id = $6",
    )
    .bind(facts.producing_call.into_uuid())
    .bind(facts.runner.into_uuid())
    .bind(Decimal::from(facts.placement_revision.get()))
    .bind(facts.interrupted_attempt.into_uuid())
    .bind(facts.turn.into_uuid())
    .bind(facts.session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    Ok(())
}

pub(crate) async fn prepare_execution_possible_retryable_runner_recovery(
    pool: &PgPool,
    authorize: fn(PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization,
    fixture_catalog: RunnerCatalog,
    fixture_effect_kind: &'static str,
) -> Result<RunnerRecoveryToolRoundFacts, Box<dyn Error>> {
    let facts =
        prepare_runner_recovery_tool_round(pool, authorize, fixture_catalog, fixture_effect_kind)
            .await?;
    record_execution_possible_lease_loss(pool, &facts.lease).await?;
    park_runner_recovery_tool_round(pool, &facts).await?;
    Ok(facts)
}

pub(crate) async fn prepare_unclaimed_retryable_runner_recovery(
    pool: &PgPool,
) -> Result<
    (
        SessionId,
        TurnId,
        ToolAttemptId,
        ContextFrontierId,
        ToolRequestId,
        RunnerLeaseCorrelation,
    ),
    Box<dyn Error>,
> {
    let facts = prepare_runner_recovery_tool_round(
        pool,
        external_authorized,
        side_effecting_catalog(),
        "external_effect",
    )
    .await?;
    record_no_execution_lease_loss(pool, &facts.lease).await?;
    park_runner_recovery_tool_round(pool, &facts).await?;
    Ok((
        facts.session,
        facts.turn,
        facts.interrupted_attempt,
        facts.boundary,
        facts.request,
        facts.lease.correlation(),
    ))
}

/// stopping a retryable no-execution runner wait retires
/// its dispatch authority before cancelling and releasing the active slot.
#[tokio::test]
#[ignore = "requires Docker"]
async fn stop_retires_retryable_runner_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, interrupted_attempt, boundary, request, lease) =
        prepare_unclaimed_retryable_runner_recovery(&pool).await?;
    let command = DurableCommandId::from_uuid(uuid(0xa171));
    let result_entry = SemanticTranscriptEntryId::from_uuid(uuid(0xa172));
    let terminal_frontier = ContextFrontierId::from_uuid(uuid(0xa173));
    let interrupt = SubmitInput::new(
        command,
        session,
        UserContent::try_text(String::from("stop retryable runner attempt"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa174)),
            Some(TurnId::from_uuid(uuid(0xa175))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa176)),
                terminal_frontier,
            ),
            |_| TurnId::from_uuid(uuid(0xa177)),
            |_| (vec![result_entry], terminal_frontier),
        )
        .await?;
    let reload = StartEligibleTurnRepository::new(pool.clone())
        .preview(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa178)),
                SemanticTranscriptEntryId::from_uuid(uuid(0xa179)),
                ContextFrontierId::from_uuid(uuid(0xa17a)),
                TurnAttemptId::from_uuid(uuid(0xa17b)),
            ),
        )
        .await?;
    let lifecycle: (String, Uuid) = sqlx::query_as(
        "SELECT terminal_disposition_kind, terminal_attempt_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    let stopped_attempt: (String, String, String) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, error_kind
           FROM tool_attempt
          WHERE attempt_id = $1",
    )
    .bind(interrupted_attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    let result: (String, Uuid, Uuid) = sqlx::query_as(
        "SELECT entry.payload_kind, entry.tool_result_attempt_id,
                effect.source_frontier_id
           FROM semantic_transcript_entry AS entry
           JOIN turn_runner_recovery_interrupt_effect AS effect
             ON effect.session_id = entry.source_session_id
            AND effect.command_id = $1
          WHERE entry.source_session_id = $2
            AND entry.semantic_entry_id = $3",
    )
    .bind(command.into_uuid())
    .bind(session.into_uuid())
    .bind(result_entry.into_uuid())
    .fetch_one(&pool)
    .await?;
    let closure_request: Uuid =
        sqlx::query_scalar("SELECT request_id FROM tool_attempt WHERE attempt_id = $1")
            .bind(interrupted_attempt.into_uuid())
            .fetch_one(&pool)
            .await?;
    let consumed_loss = RunnerProtocolStore::new(pool.clone(), side_effecting_catalog())
        .load_lease_loss(lease.lease, lease.generation)
        .await?
        .expect("the stopped source lease remains loadable");
    let retry = consumed_loss
        .retry()
        .expect("the stopped no-execution loss retains checked lineage");
    let consumed_retry = retry
        .prepare_unclaimed_attempt(claimed_batch_with_effect(
            INITIAL_PHYSICAL_ATTEMPT,
            ToolEffectClass::ExternalEffect,
        ))
        .expect_err("the stop durably consumes retry preparation authority");

    assert_eq!(lifecycle.0, "cancelled");
    assert_eq!(stopped_attempt.0, "terminal");
    assert_eq!(stopped_attempt.1, "known_failed");
    assert_eq!(stopped_attempt.2, "crash_lost");
    assert_eq!(result.0, "tool_execution_result");
    assert_eq!(result.1, interrupted_attempt.into_uuid());
    assert_eq!(result.2, boundary.into_uuid());
    assert_eq!(closure_request, request.into_uuid());
    assert_eq!(consumed_retry, RunnerDomainError::InvalidState);
    assert!(
        reload.is_none(),
        "the successor remains queued while placement is lost"
    );
    drop(pool);
    Ok(())
}

/// stopping a pure execution-possible runner wait reloads
/// its named terminal attempt and emits the correlated crash-lost result.
#[tokio::test]
#[ignore = "requires Docker"]
async fn stop_reloads_pure_attempt_from_terminal_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let facts = prepare_execution_possible_retryable_runner_recovery(
        &pool,
        authorized,
        catalog(),
        "effect_free",
    )
    .await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let resumable_loss = store
        .load_lease_loss(facts.lease.correlation().lease, facts.lease.generation())
        .await?
        .expect("the execution-possible loss remains retryable before stop");
    let reserved =
        authorize_fixture_claimed_retry(&store, &resumable_loss, ToolEffectClass::EffectFree)
            .await?;
    let command = DurableCommandId::from_uuid(uuid(0xa184));
    let result_entry = SemanticTranscriptEntryId::from_uuid(uuid(0xa185));
    let terminal_frontier = ContextFrontierId::from_uuid(uuid(0xa186));
    let interrupt = SubmitInput::new(
        command,
        facts.session,
        UserContent::try_text(String::from("stop pure lost runner attempt"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: facts.turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa187)),
            Some(TurnId::from_uuid(uuid(0xa188))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa189)),
                terminal_frontier,
            ),
            |_| TurnId::from_uuid(uuid(0xa18b)),
            |_| (vec![result_entry], terminal_frontier),
        )
        .await?;
    let lifecycle: String = sqlx::query_scalar(
        "SELECT terminal_disposition_kind
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(facts.session.into_uuid())
    .bind(facts.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    let attempt: (String, String, String) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, error_kind
           FROM tool_attempt
          WHERE attempt_id = $1",
    )
    .bind(facts.interrupted_attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    let result: (String, Uuid, Uuid) = sqlx::query_as(
        "SELECT entry.payload_kind, entry.tool_result_attempt_id,
                effect.source_frontier_id
           FROM semantic_transcript_entry AS entry
           JOIN turn_runner_recovery_interrupt_effect AS effect
             ON effect.session_id = entry.source_session_id
            AND effect.command_id = $1
          WHERE entry.source_session_id = $2
            AND entry.semantic_entry_id = $3",
    )
    .bind(command.into_uuid())
    .bind(facts.session.into_uuid())
    .bind(result_entry.into_uuid())
    .fetch_one(&pool)
    .await?;
    let stopped_reservation = store
        .load_claimed_retry_attempt_reservation(
            facts.lease.correlation().lease,
            facts.lease.generation(),
        )
        .await?;

    assert_eq!(reserved.source(), &facts.lease.correlation());
    assert_eq!(lifecycle, "cancelled");
    assert_eq!(attempt.0, "terminal");
    assert_eq!(attempt.1, "known_failed");
    assert_eq!(attempt.2, "crash_lost");
    assert_eq!(result.0, "tool_execution_result");
    assert_eq!(result.1, facts.interrupted_attempt.into_uuid());
    assert_eq!(result.2, facts.boundary.into_uuid());
    assert_eq!(stopped_reservation, None);
    drop(pool);
    Ok(())
}

/// stopping an idempotent execution-possible runner wait
/// reloads its named ambiguity and retains reconciliation authority.
#[tokio::test]
#[ignore = "requires Docker"]
async fn stop_reloads_idempotent_ambiguity_from_terminal_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let facts = prepare_execution_possible_retryable_runner_recovery(
        &pool,
        idempotent_authorized,
        idempotent_catalog(),
        "external_effect",
    )
    .await?;
    let command = DurableCommandId::from_uuid(uuid(0xa18c));
    let tool_closure = SemanticTranscriptEntryId::from_uuid(uuid(0xa18d));
    let terminal_frontier = ContextFrontierId::from_uuid(uuid(0xa18e));
    let interrupt = SubmitInput::new(
        command,
        facts.session,
        UserContent::try_text(String::from("stop idempotent lost runner attempt"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: facts.turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa18f)),
            Some(TurnId::from_uuid(uuid(0xa190))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa191)),
                ContextFrontierId::from_uuid(uuid(0xa192)),
            ),
            |_| TurnId::from_uuid(uuid(0xa193)),
            |_| (vec![tool_closure], terminal_frontier),
        )
        .await?;
    let lifecycle: (String, Uuid) = sqlx::query_as(
        "SELECT terminal_disposition_kind, terminal_tool_attempt_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(facts.session.into_uuid())
    .bind(facts.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    let attempt: (String, String) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM tool_attempt
          WHERE attempt_id = $1",
    )
    .bind(facts.interrupted_attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    let closure: (String, Uuid) = sqlx::query_as(
        "SELECT payload_kind, tool_result_request_id
           FROM semantic_transcript_entry
          WHERE source_session_id = $1 AND semantic_entry_id = $2",
    )
    .bind(facts.session.into_uuid())
    .bind(tool_closure.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(lifecycle.0, "reconciliation_required");
    assert_eq!(lifecycle.1, facts.interrupted_attempt.into_uuid());
    assert_eq!(attempt.0, "terminal");
    assert_eq!(attempt.1, "ambiguous");
    assert_eq!(closure.0, "tool_closed_by_turn_end");
    assert_eq!(closure.1, facts.request.into_uuid());
    drop(pool);
    Ok(())
}

/// a corrupted stop cannot turn a no-execution source into
/// reconciliation-required ambiguity merely by terminalizing its attempt.
#[tokio::test]
#[ignore = "requires Docker"]
async fn stop_rejects_unclaimed_side_effecting_ambiguity() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, interrupted_attempt, _, _, _) =
        prepare_unclaimed_retryable_runner_recovery(&pool).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let interrupt = SubmitInput::new(
        DurableCommandId::from_uuid(uuid(0xa17c)),
        session,
        UserContent::try_text(String::from("invalid ambiguous stop"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    let rejected = SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa17d)),
            Some(TurnId::from_uuid(uuid(0xa17e))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa17f)),
                ContextFrontierId::from_uuid(uuid(0xa180)),
            ),
            |_| TurnId::from_uuid(uuid(0xa181)),
            |_| {
                (
                    vec![SemanticTranscriptEntryId::from_uuid(uuid(0xa182))],
                    ContextFrontierId::from_uuid(uuid(0xa183)),
                )
            },
        )
        .await
        .expect_err("no-execution loss cannot become reconciliation-required");

    assert!(
        rejected
            .to_string()
            .contains("incomplete or cross-wired effect"),
        "the deferred effect correlation must reject the wrong lease ambiguity: {rejected}"
    );
    drop(pool);
    Ok(())
}

/// an interrupt terminalizes a delegated runner-loss wait
/// through its delegation projection and retains the exact loss evidence.
#[tokio::test]
#[ignore = "requires Docker"]
async fn stop_terminalizes_delegated_runner_recovery_wait() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let older_ordinary_turn = TurnId::from_uuid(uuid(0xa140));
    let older_ordinary_input = AcceptedInputId::from_uuid(uuid(0xa141));
    let older_ordinary_command = DurableCommandId::from_uuid(uuid(0xa142));
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            SubmitInput::new(
                older_ordinary_command,
                session,
                UserContent::try_text(String::from("older queued work"))
                    .expect("the fixture input is valid"),
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: turn,
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::try_from_u64(1)
                            .expect("the fixture defaults version is positive"),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
            ),
            older_ordinary_input,
            Some(older_ordinary_turn),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa143)),
                ContextFrontierId::from_uuid(uuid(0xa144)),
            ),
            |_| TurnId::from_uuid(uuid(0xa145)),
            |_| (Vec::new(), ContextFrontierId::from_uuid(uuid(0xa146))),
        )
        .await
        .expect("older ordinary work is accepted while the turn is active");
    make_accepted_turn_direct_root(&pool, older_ordinary_command, older_ordinary_input).await?;
    let stored_older_ordinary: Uuid = sqlx::query_scalar(
        "SELECT turn_id
           FROM queued_input_origin
          WHERE session_id = $1 AND accepted_input_id = $2",
    )
    .bind(session.into_uuid())
    .bind(older_ordinary_input.into_uuid())
    .fetch_one(&pool)
    .await?;
    let starting_frontier = convert_running_turn_to_delegated_runner_recovery(
        &pool,
        session,
        turn,
        turn_attempt,
        runner,
        placement.revision(),
    )
    .await?;
    let command = DurableCommandId::from_uuid(uuid(0xa135));
    let interrupt_successor = TurnId::from_uuid(uuid(0xa137));
    let interrupt = SubmitInput::new(
        command,
        session,
        UserContent::try_text(String::from("stop delegated runner recovery"))
            .expect("the fixture input is valid"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("the fixture defaults version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(uuid(0xa136)),
            Some(interrupt_successor),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa138)),
                ContextFrontierId::from_uuid(uuid(0xa139)),
            ),
            |_| TurnId::from_uuid(uuid(0xa13a)),
            |_| (Vec::new(), ContextFrontierId::from_uuid(uuid(0xa13b))),
        )
        .await?;
    let recorded_command = SubmitInputRepository::new(pool.clone())
        .load(command)
        .await?;
    let reload = StartEligibleTurnRepository::new(pool.clone())
        .preview(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(uuid(0xa13c)),
                SemanticTranscriptEntryId::from_uuid(uuid(0xa13d)),
                ContextFrontierId::from_uuid(uuid(0xa13e)),
                TurnAttemptId::from_uuid(uuid(0xa13f)),
            ),
        )
        .await?;
    assert!(
        reload.is_none(),
        "the delegated successor remains queued while placement is lost"
    );
    let reloaded_turn = TurnId::from_uuid(
        sqlx::query_scalar(
            "SELECT turn_id FROM turn_lifecycle WHERE session_id = $1 AND turn_id = $2 AND state_kind = 'queued'",
        )
        .bind(session.into_uuid())
        .bind(interrupt_successor.into_uuid())
        .fetch_one(&pool)
        .await?,
    );
    let terminal: (String, Option<String>, Option<Uuid>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind,
                runner_recovery_runner_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    let retained_loss: String = sqlx::query_scalar(
        "SELECT record.state_kind
           FROM runner_current_session_placement AS head
           JOIN runner_session_placement_record AS record
             ON record.session_id = head.session_id
            AND record.event_ordinal = head.event_ordinal
          WHERE head.session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    let persisted_effect: (Uuid, Uuid, Decimal, Uuid, Uuid) = sqlx::query_as(
        "SELECT turn_id, runner_id, placement_revision,
                yielded_turn_attempt_id, source_frontier_id
           FROM turn_runner_recovery_interrupt_effect
          WHERE command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        terminal,
        (
            String::from("terminal"),
            Some(String::from("cancelled")),
            None
        )
    );
    assert_eq!(retained_loss, "runner_lost_before_pin");
    assert_eq!(
        persisted_effect,
        (
            turn.into_uuid(),
            runner.into_uuid(),
            Decimal::from(placement.revision().get()),
            turn_attempt.into_uuid(),
            starting_frontier.into_uuid(),
        )
    );
    assert_eq!(store.load_runner_recovery_wait(session).await?, None);
    assert_eq!(stored_older_ordinary, older_ordinary_turn.into_uuid());
    assert_eq!(reloaded_turn, interrupt_successor);
    assert!(
        recorded_command.is_some(),
        "the interrupt receipt must reload with its non-accepted predecessor"
    );
    assert!(
        reload.is_none(),
        "the delegated successor remains queued while its runner placement is lost"
    );
    drop(pool);
    Ok(())
}

/// placement advance and runner-recovery parking rendezvous
/// on the scheduler row, so the stale placement transaction cannot commit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn runner_recovery_serializes_with_placement_advance() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *recovery)
        .await?;
    let replacement_pool = pool.clone();
    let replacement_runner = successor.runner();
    let replacement_commit = tokio::spawn(async move {
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, async {
            let mut replacement = replacement_pool.begin().await?;
            append_pre_pin_replacement_without_advancing_head(
                &mut replacement,
                session,
                replacement_runner,
            )
            .await?;
            sqlx::query(
                "UPDATE runner_current_session_placement
                    SET event_ordinal = event_ordinal + 1
                  WHERE session_id = $1",
            )
            .bind(session.into_uuid())
            .execute(&mut *replacement)
            .await?;
            replacement.commit().await
        })
        .await
    });
    let blocked = tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 1))
        .await
        .expect("placement scheduler-lock observation must remain bounded")?;

    assert!(blocked, "placement advance must wait on the scheduler lock");
    recovery.commit().await?;
    let rejected = replacement_commit
        .await
        .expect("the replacement commit task remains joinable")
        .expect("the replacement commit must finish within its task-owned timeout")
        .expect_err("the stale placement advance cannot commit after runner recovery");
    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a queued turn cannot fabricate a runner-recovery slot.
#[tokio::test]
#[ignore = "requires Docker"]
async fn queued_turn_cannot_enter_runner_recovery() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let session = SessionId::from_uuid(uuid(SESSION));
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let frontier = ContextFrontierId::from_uuid(uuid(0xa141));
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(session.into_uuid())
    .bind(frontier.into_uuid())
    .execute(&pool)
    .await?;
    let mut malformed = pool.begin().await?;
    sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_kind, origin_accepted_input_id,
             acceptance_position, state_kind)
         VALUES ($1, $2, 'delegation', NULL, 1, 'queued')",
    )
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await?;
    let rejected = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active', start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                active_phase_kind = 'awaiting_runner_recovery',
                runner_recovery_runner_id = $2,
                runner_recovery_placement_revision = $3
          WHERE turn_id = $4 AND session_id = $5",
    )
    .bind(frontier.into_uuid())
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *malformed)
    .await
    .expect_err("queued work cannot fabricate a runner recovery wait");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

/// a delegated recovery wait may release its runtime slot
/// without mutating the retained physical lifecycle.
#[tokio::test]
#[ignore = "requires Docker"]
async fn delegated_runner_recovery_releases_runtime_slot_and_wait() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let session = SessionId::from_uuid(uuid(SESSION));
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    insert_runner_recovery_turn(
        &pool,
        session,
        turn,
        runner,
        placement.revision(),
        None,
        None,
    )
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut released = pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE turn_lifecycle
            SET delegation_runtime_terminal = true
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&mut *released)
    .await?;

    assert_eq!(updated.rows_affected(), 1);
    released.commit().await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let loaded_wait = store.load_runner_recovery_wait(session).await?;
    append_pre_pin_replacement_projection(
        &pool,
        session,
        RunnerId::from_uuid(uuid(REPLACEMENT_RUNNER)),
    )
    .await?;
    let current_event: String = sqlx::query_scalar(
        "SELECT record.event_kind
           FROM runner_current_session_placement AS head
           JOIN runner_session_placement_record AS record
             ON record.session_id = head.session_id
            AND record.event_ordinal = head.event_ordinal
          WHERE head.session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(loaded_wait, None);
    assert_eq!(current_event, "pre_pin_replaced");
    drop(pool);
    Ok(())
}

/// an interrupted physical attempt must be leased to the
/// exact runner and placement revision named by the loss wait.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_rejects_cross_wired_lease_runner() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    sqlx::query("ALTER TABLE runner_lease_generation DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_lease_generation
            SET runner_id = $1
          WHERE attempt_id = $2",
    )
    .bind(uuid(REPLACEMENT_RUNNER))
    .bind(interrupted_attempt.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_lease_generation ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner recovery cannot claim another runner's leased attempt");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// runner recovery may retain only an ambiguous physical
/// attempt; a known terminal result cannot be reclassified as runner loss.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_rejects_non_ambiguous_tool_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                error_kind = 'execution_failed'
          WHERE attempt_id = $1",
    )
    .bind(interrupted_attempt.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner recovery cannot retain a known terminal tool attempt");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// later tool-attempt mutation cannot invalidate the exact
/// physical attempt retained by an active runner-recovery wait.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_rechecks_changed_tool_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await?;
    let rejected = sqlx::query(
        "UPDATE tool_attempt
            SET terminal_disposition_kind = 'known_failed',
                error_kind = 'execution_failed'
          WHERE attempt_id = $1",
    )
    .bind(interrupted_attempt.into_uuid())
    .execute(&pool)
    .await
    .expect_err("tool-attempt changes must preserve the exact runner recovery wait");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a continuation written after the wait must not leave its
/// predecessor masquerading as the yielded chain-tip recovery boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_rechecks_turn_attempt_continuations() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let rejected = sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, $4, 'ended', 'without_stop', 'known_failure')",
    )
    .bind(uuid(0xa16e))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .bind(turn_attempt.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a later continuation must invalidate the stale yielded boundary");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a continuation writer takes the scheduler rendezvous
/// before inserting a successor to the yielded runner-recovery attempt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn runner_recovery_serializes_turn_attempt_continuations() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let mut recovery = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3",
    )
    .bind(turn_attempt.into_uuid())
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $1,
                runner_recovery_placement_revision = $2
          WHERE turn_id = $3 AND session_id = $4",
    )
    .bind(runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *recovery)
    .await?;
    recovery.commit().await?;
    let mut stop = pool.begin().await?;
    sqlx::query(
        "SELECT session_id
           FROM session_scheduler
          WHERE session_id = $1
          FOR UPDATE",
    )
    .bind(session.into_uuid())
    .fetch_one(&mut *stop)
    .await?;
    let mut continuation = Box::pin(
        sqlx::query(
            "INSERT INTO turn_attempt
                (turn_attempt_id, turn_id, session_id,
                 continued_from_attempt_id, state_kind,
                 end_variant, end_disposition)
             VALUES ($1, $2, $3, $4, 'ended', 'without_stop',
                     'known_failure')",
        )
        .bind(uuid(0xa16e))
        .bind(turn.into_uuid())
        .bind(session.into_uuid())
        .bind(turn_attempt.into_uuid())
        .execute(&pool),
    );
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut continuation)
        .await
        .expect_err("continuation insertion must wait for the scheduler rendezvous");
    stop.rollback().await?;
    let rejected = tokio::time::timeout(Duration::from_secs(10), &mut continuation)
        .await
        .expect("continuation admission finishes after the scheduler is released")
        .expect_err("the stale yielded boundary still rejects the continuation");

    assert_check_violation(rejected);
    drop(continuation);
    drop(pool);
    Ok(())
}

/// a lease writer takes the scheduler rendezvous before
/// advancing the lease head retained by an active runner-recovery wait.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn runner_recovery_serializes_lease_head_advances() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, pin) =
        stored_side_effecting_pin_fixture(&pool).await?;
    let claimed = duplicate_lease(&pin.lease, registration.registration())
        .claim(pin.lease.correlation())
        .expect("the exact fixture lease correlation claims");
    store.store_lease(&claimed).await?;
    let stale_completion = duplicate_lease(&claimed, registration.registration())
        .complete(pin.lease.correlation())
        .expect("the pre-loss claimed snapshot admits its exact completion");
    let loss = claimed
        .lose()
        .expect("claimed side-effecting work admits execution-possible loss");
    store.store_lease_loss(&loss).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await?;
    let mut stop = pool.begin().await?;
    sqlx::query(
        "SELECT session_id
           FROM session_scheduler
          WHERE session_id = $1
          FOR UPDATE",
    )
    .bind(session.into_uuid())
    .fetch_one(&mut *stop)
    .await?;
    let mut lease_store = Box::pin(store.store_lease(&stale_completion));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut lease_store)
        .await
        .expect_err("lease admission must wait for the scheduler rendezvous");
    stop.rollback().await?;
    let rejected = tokio::time::timeout(Duration::from_secs(10), &mut lease_store)
        .await
        .expect("lease admission finishes after the scheduler is released")
        .expect_err("the stale lease snapshot cannot advance the retained loss");

    assert_store_check_violation(rejected);
    drop(lease_store);
    drop(pool);
    Ok(())
}

/// a lease-head rewrite after wait admission must recheck
/// the exact loss event that authorized runner recovery.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_rechecks_changed_lease_head() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await?;
    let correlation = pin.lease.correlation();
    sqlx::query(
        "ALTER TABLE runner_current_lease_event
         DISABLE TRIGGER runner_current_lease_event_advances",
    )
    .execute(&pool)
    .await?;
    let rejected = sqlx::query(
        "UPDATE runner_current_lease_event
            SET event_ordinal = 1
          WHERE lease_id = $1 AND generation = $2",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&pool)
    .await
    .expect_err("a lease-head rewrite must preserve runner-recovery loss authority");
    sqlx::query(
        "ALTER TABLE runner_current_lease_event
         ENABLE TRIGGER runner_current_lease_event_advances",
    )
    .execute(&pool)
    .await?;

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// mutating the lease event under an unchanged head must
/// also recheck the execution-loss classification retained by the wait.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_rechecks_changed_lease_event() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await?;
    let correlation = pin.lease.correlation();
    sqlx::query(
        "ALTER TABLE runner_lease_event
         DISABLE TRIGGER runner_lease_event_is_append_only",
    )
    .execute(&pool)
    .await?;
    let rejected = sqlx::query(
        "UPDATE runner_lease_event
            SET state_kind = 'claimed'
          WHERE lease_id = $1 AND generation = $2 AND event_ordinal = 2",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&pool)
    .await
    .expect_err("lease-event mutation must preserve runner-recovery loss authority");
    sqlx::query(
        "ALTER TABLE runner_lease_event
         ENABLE TRIGGER runner_lease_event_is_append_only",
    )
    .execute(&pool)
    .await?;

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a completed lease cannot be reclassified as the
/// physical execution interrupted by a later runner loss.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_rejects_completed_lease_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, pin) =
        stored_side_effecting_pin_fixture(&pool).await?;
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    let claimed = duplicate_lease(&pin.lease, registration.registration())
        .claim(pin.lease.correlation())
        .expect("the exact fixture lease correlation claims");
    store.store_lease(&claimed).await?;
    let completed = claimed
        .complete(pin.lease.correlation())
        .expect("the exact claimed lease correlation completes");
    store.store_lease(&completed).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner recovery cannot retain an attempt whose lease completed");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// an offered lease is not evidence that runner loss
/// interrupted execution.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_rejects_offered_lease_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner recovery cannot retain an attempt whose lease is only offered");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a claimed lease without a durable loss event is not
/// evidence that runner loss interrupted execution.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_rejects_claimed_lease_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, pin) =
        stored_side_effecting_pin_fixture(&pool).await?;
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    let claimed = duplicate_lease(&pin.lease, registration.registration())
        .claim(pin.lease.correlation())
        .expect("the exact fixture lease correlation claims");
    store.store_lease(&claimed).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner recovery cannot retain an attempt whose lease is only claimed");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a no-execution loss cannot be reclassified as an
/// execution-possible interrupted attempt.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_rejects_no_execution_lease_loss() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_no_execution_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner recovery cannot retain a proven no-execution lease loss");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// an older ambiguous attempt under the same placement
/// revision cannot impersonate the operation interrupted at a later loss.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_attempt_matches_exact_active_round() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin, later_lease) =
        stored_side_effecting_later_lease_fixture(&pool).await?;
    store.store_lease(&later_lease).await?;
    let stale_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, stale_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(LATER_LEASE_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: stale_attempt,
            recovery_interrupted_tool_attempt: Some(stale_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                LATER_LEASE_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner loss cannot retain an older round's ambiguous attempt");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a non-null runner-recovery wait names the only current
/// live or ambiguous physical attempt in its retained tool round.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_rejects_additional_round_ambiguity() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    let producing_call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    sqlx::query("ALTER TABLE tool_request DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text)
         VALUES ($1, $2, $3, $4, 1, 'inspect', 'json', '{}')",
    )
    .bind(uuid(PROFILELESS_PHYSICAL_ATTEMPT.request))
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(producing_call.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_request ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO tool_attempt
            (attempt_id, request_id, session_id, turn_id,
             issuing_turn_attempt_id, effect_class, dispatch_generation,
             state_kind, terminal_disposition_kind)
         VALUES ($1, $2, $3, $4, $5, 'external_effect', 1,
                 'terminal', 'ambiguous')",
    )
    .bind(uuid(PROFILELESS_PHYSICAL_ATTEMPT.attempt))
    .bind(uuid(PROFILELESS_PHYSICAL_ATTEMPT.request))
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(uuid(
        INITIAL_PHYSICAL_ATTEMPT.turn + RELATED_IDENTITY_OFFSET,
    ))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: producing_call,
        },
    )
    .await
    .expect_err("runner recovery cannot retain a second ambiguous round attempt");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a runner-loss wait reads back only from the exact
/// current lost placement and retains the interrupted physical attempt.
#[tokio::test]
#[ignore = "requires Docker"]
async fn pinned_runner_recovery_wait_round_trips_exact_loss() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await?;
    let loaded_placement = store
        .load_placement(session)
        .await?
        .expect("the lost placement is present");
    let loaded_wait = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the exact runner recovery wait is present");

    assert_eq!(
        loaded_placement.interrupted_tool_attempt(),
        Some(interrupted_attempt)
    );
    assert_eq!(loaded_wait.turn(), turn);
    assert_eq!(loaded_wait.runner(), expected_enrollment.runner());
    assert_eq!(loaded_wait.placement_revision(), pin.placement.revision());
    assert_eq!(
        loaded_wait.interrupted_tool_attempt(),
        Some(interrupted_attempt)
    );
    let (_, _, _, _, consumed_interrupted_attempt) = loaded_placement.into_parts();
    assert_eq!(consumed_interrupted_attempt, Some(interrupted_attempt));
    drop(pool);
    Ok(())
}

/// the immutable runner-recovery interrupt effect rejects
/// statement-level truncation as well as row-level mutation.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_interrupt_effect_rejects_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let rejected = sqlx::query("TRUNCATE turn_runner_recovery_interrupt_effect")
        .execute(&pool)
        .await
        .expect_err("immutable runner recovery effects cannot be truncated");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// durable no-execution proof keeps even side-effecting
/// work retryable and parks the turn with its exact in-flight source attempt.
#[tokio::test]
#[ignore = "requires Docker"]
async fn unclaimed_loss_wait_retains_in_flight_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_no_execution_lease_loss(&pool, &pin.lease).await?;
    insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn,
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: Some(interrupted_attempt),
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await?;
    let loaded_wait = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the unclaimed loss retains its runner recovery wait");

    assert_eq!(loaded_wait.turn(), turn);
    assert_eq!(loaded_wait.runner(), expected_enrollment.runner());
    assert_eq!(loaded_wait.placement_revision(), pin.placement.revision());
    assert_eq!(
        loaded_wait.interrupted_tool_attempt(),
        Some(interrupted_attempt)
    );
    drop(pool);
    Ok(())
}

/// a pre-pin loss may park the turn without fabricating a
/// physical attempt, and that nullable arm reads back distinctly.
#[tokio::test]
#[ignore = "requires Docker"]
async fn pre_pin_runner_recovery_wait_round_trips_without_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let session = SessionId::from_uuid(uuid(SESSION));
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    insert_runner_recovery_turn(
        &pool,
        session,
        turn,
        runner,
        placement.revision(),
        None,
        None,
    )
    .await?;
    let loaded = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the pre-pin runner recovery wait is present");

    assert_eq!(loaded.turn(), turn);
    assert_eq!(loaded.runner(), runner);
    assert_eq!(loaded.placement_revision(), placement.revision());
    assert_eq!(loaded.interrupted_tool_attempt(), None);
    drop(pool);
    Ok(())
}

/// the discriminator alone cannot authenticate a runner
/// recovery wait against another runner's loss.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_wait_rejects_cross_wired_runner() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let session = SessionId::from_uuid(uuid(SESSION));
    let lost_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(lost_runner));
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let rejected = insert_runner_recovery_turn(
        &pool,
        session,
        TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
        RunnerId::from_uuid(uuid(REPLACEMENT_RUNNER)),
        placement.revision(),
        None,
        None,
    )
    .await
    .expect_err("runner recovery must name the current placement's exact lost runner");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a runner wait cannot name a placement revision other
/// than the exact current loss revision.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_wait_rejects_cross_wired_revision() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let session = SessionId::from_uuid(uuid(SESSION));
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let unrelated_revision =
        RunnerGeneration::try_from_u64(2).expect("the unrelated placement revision is positive");
    let rejected = insert_runner_recovery_turn(
        &pool,
        session,
        TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
        runner,
        unrelated_revision,
        None,
        None,
    )
    .await
    .expect_err("runner recovery must name the current placement's exact revision");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a runner wait cannot omit the physical attempt retained
/// by the exact placement-loss record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_wait_requires_loss_recorded_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, expected_enrollment, _, pin) = stored_side_effecting_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let interrupted_attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    record_execution_possible_lease_loss(&pool, &pin.lease).await?;
    mark_interrupted_attempt_ambiguous(&pool, interrupted_attempt).await?;
    let rejected = insert_runner_recovery_turn_with_interrupted_loss(
        &pool,
        InterruptedLossRecoveryFacts {
            session,
            turn: TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
            runner: expected_enrollment.runner(),
            placement_revision: pin.placement.revision(),
            placement_interrupted_tool_attempt: interrupted_attempt,
            recovery_interrupted_tool_attempt: None,
            active_tool_round_call: ModelCallId::from_uuid(uuid(
                INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
            )),
        },
    )
    .await
    .expect_err("runner recovery must retain the loss-recorded tool attempt");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// generic active-phase mutation cannot reopen a runner-recovery
/// wait without the future checked replacement transaction.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_recovery_wait_rejects_generic_active_reopen() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let session = SessionId::from_uuid(uuid(SESSION));
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let turn = TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn));
    insert_runner_recovery_turn(
        &pool,
        session,
        turn,
        runner,
        placement.revision(),
        None,
        None,
    )
    .await?;
    let rejected = sqlx::query(
        "UPDATE turn_lifecycle
            SET runner_recovery_runner_id = runner_recovery_runner_id
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&pool)
    .await
    .expect_err("generic mutation cannot reopen a runner recovery wait");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// an established epoch publishes recovery only when its
/// immediate durable predecessor is the suspicion that it supersedes.
#[tokio::test]
#[ignore = "requires Docker"]
async fn initial_connection_cannot_publish_recovery() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "pinned").await?;
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let source = connection_outbox_source(
        &pool,
        placement_event_ordinal,
        expected_enrollment.enrollment(),
        "established",
    )
    .await?;
    let rejected = append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Connected,
            source,
        ),
    )
    .await
    .expect_err("an initial established connection is not a recovery boundary");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// an established recovery source starts a successor epoch;
/// a cause-valid established event in the suspect epoch is not a reconnect.
#[tokio::test]
#[ignore = "requires Docker"]
async fn same_epoch_established_event_cannot_publish_recovery() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "pinned").await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let mut same_epoch = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_connection_event
            (enrollment_id, connection_epoch, event_ordinal,
             state_kind, cause_kind)
         VALUES ($1, $2, 3, 'connected', 'established')",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(connection.epoch().get()))
    .execute(&mut *same_epoch)
    .await?;
    sqlx::query(
        "UPDATE runner_connection_authority_head
            SET connection_event_ordinal = 3
          WHERE enrollment_id = $1",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .execute(&mut *same_epoch)
    .await?;
    same_epoch.commit().await?;
    let rejected = append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Connected,
            RunnerStateTransitionOutboxTestSource::connection(
                placement_event_ordinal,
                expected_enrollment.enrollment(),
                connection.epoch(),
                NonZeroU64::new(3).expect("the fixture event ordinal is positive"),
            ),
        ),
    )
    .await
    .expect_err("same-epoch established evidence is not a reconnect recovery");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn delegated_runner_recovery_charges_its_retained_attachment() -> Result<(), Box<dyn Error>> {
    use signalbox_domain::{
        AttachmentKind, BlobDigest, DeclaredMediaType, SubmitInputAppliedResult, SubmitInputResult,
        UserContentPart,
    };
    use signalbox_persistence::submit_input::SubmitInputHandlingOutcome;
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, turn_attempt) = insert_running_turn(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(session, exact_runner_request(runner));
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    convert_running_turn_to_delegated_runner_recovery(
        &pool,
        session,
        turn,
        turn_attempt,
        runner,
        placement.revision(),
    )
    .await?;
    let retained = BlobDigest::digest(b"runner recovery retained attachment");
    let later = BlobDigest::digest(b"runner recovery later attachment");
    sqlx::query("INSERT INTO blob_store_binding (store_name, namespace_id) VALUES ('runner_attachments', $1)")
        .bind(Uuid::now_v7()).execute(&pool).await?;
    let mut catalog = pool.begin().await?;
    for (digest, object) in [(retained, "retained"), (later, "later")] {
        sqlx::query("INSERT INTO blob (digest, byte_length) VALUES ($1, 7)")
            .bind(digest.as_bytes().as_slice())
            .execute(&mut *catalog)
            .await?;
        sqlx::query("INSERT INTO blob_replica (digest, store_name, object_key) VALUES ($1, 'runner_attachments', $2)")
            .bind(digest.as_bytes().as_slice()).bind(object).execute(&mut *catalog).await?;
    }
    catalog.commit().await?;
    let content = |digest| {
        UserContent::try_parts(vec![UserContentPart::Attachment {
            digest,
            kind: AttachmentKind::File,
            media_type: DeclaredMediaType::try_new("application/octet-stream".to_owned())
                .expect("fixture media type is valid"),
            display_filename: None,
        }])
        .expect("fixture content is valid")
    };
    let repository = SubmitInputRepository::new(pool.clone()).with_attachment_maximum_bytes(10);
    let retained_outcome = repository
        .handle_with_candidates(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::now_v7()),
                session,
                content(retained),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn,
                },
            ),
            AcceptedInputId::from_uuid(Uuid::now_v7()),
            None,
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            |_| panic!("safe-point steering does not reclassify inputs"),
            |_| panic!("safe-point steering does not cancel tools"),
        )
        .await?;
    assert!(
        matches!(
            retained_outcome,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::PendingSteering(_)
            ))
        ),
        "one seven-byte attachment fits the ten-byte bound"
    );
    let later_outcome = repository
        .handle_with_candidates(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::now_v7()),
                session,
                content(later),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn,
                },
            ),
            AcceptedInputId::from_uuid(Uuid::now_v7()),
            None,
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            |_| panic!("safe-point steering does not reclassify inputs"),
            |_| panic!("safe-point steering does not cancel tools"),
        )
        .await?;
    assert_eq!(
        later_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            signalbox_domain::SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
                maximum_bytes: 10
            }
        ))
    );
    Ok(())
}
