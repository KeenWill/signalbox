//! Durable delegation transactions: wait, message, and relationship commits with their replay authentication.

use crate::*;

/// S17: a background wait, its completed receipt, and its update are
/// one replay-idempotent commit.
/// S18: equal replay still requires its exact dispatch.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_delegation_repository_commits_background_wait_atomically() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_BACKGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let repository = SessionDelegationRepository::new(pool.clone());
    let ended = repository
        .record_process_wait(
            fixture.parent,
            fixture.parent_turn,
            fixture.awaiting_request,
            fixture.child,
            DelegationWaitMode::Background,
        )
        .await?;
    let dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let (request, recorded) = process_wait(
        repository
            .record_process_wait(
                fixture.parent,
                fixture.parent_turn,
                fixture.awaiting_request,
                fixture.child,
                DelegationWaitMode::Background,
            )
            .await?,
    );
    let (replayed_request, replayed) = process_wait(
        repository
            .record_process_wait(
                fixture.parent,
                fixture.parent_turn,
                fixture.awaiting_request,
                fixture.child,
                DelegationWaitMode::Background,
            )
            .await?,
    );
    let conflict = repository
        .record_process_wait(
            fixture.parent,
            fixture.parent_turn,
            fixture.awaiting_request,
            fixture.child,
            DelegationWaitMode::Foreground,
        )
        .await?;
    let second_seed = DELEGATION_REPOSITORY_SECOND_BACKGROUND_WAIT_SEED;
    let second_fixture =
        prepare_delegation_repository_fixture(&pool, second_seed, "background").await?;
    let second_dispatch = repository_wait_dispatch(&pool, second_fixture, second_seed).await?;
    let cross_wired = repository
        .record_wait(
            DelegationAwaitRequest::parse(
                dispatch.request().clone(),
                fixture.child,
                DelegationWaitMode::Background,
            )?,
            &second_dispatch,
        )
        .await?;
    let evidence: BackgroundWaitAtomicityEvidence = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM session_delegation_wait
              WHERE awaiting_tool_request_id = $1) AS wait_count,
            (SELECT count(*) FROM delegation_update_outbox_event
              WHERE awaiting_tool_request_id = $1) AS update_count,
            (SELECT count(*) FROM tool_attempt
              WHERE request_id = $1 AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed') AS completed_attempt_count,
            (SELECT result_text FROM tool_attempt WHERE request_id = $1) AS result_text",
    )
    .bind(fixture.awaiting_request.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(recorded, replayed);
    assert_eq!(request, replayed_request);
    assert_eq!(
        ended,
        ProcessDelegationOutcome::Rejected(ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::StaleDispatch {
                state: DelegationRequestExecutionState::AttemptEnded,
            },
        ))
    );
    assert_eq!(
        conflict,
        ProcessDelegationOutcome::Rejected(ProcessDelegationRequestRejection::AwaitConflict)
    );
    assert_eq!(recorded.wait().mode(), DelegationWaitMode::Background);
    assert_eq!(
        cross_wired,
        RecordDelegationWaitOutcome::Rejected(DelegationOperationRejection::StaleDispatch {
            state: DelegationRequestExecutionState::AttemptEnded,
        })
    );
    assert_eq!(evidence.wait_count, 1);
    assert_eq!(evidence.update_count, 1);
    assert_eq!(evidence.completed_attempt_count, 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&evidence.result_text)?,
        serde_json::json!({
            "result": "session_await_registered",
            "tool_request_id": fixture.awaiting_request.as_uuid().to_string(),
            "child_session_id": fixture.child.as_uuid().to_string(),
            "mode": "background",
        })
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: a reconstituted process request observes a prepared
/// physical attempt as nonterminal rather than claiming terminal evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_process_wait_reports_prepared_attempt_without_ending_it() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_PREPARED_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let absent_child = SessionId::from_uuid(Uuid::from_u128(seed + 0x501));
    let arguments = serde_json::json!({
        "child_session_id": absent_child.as_uuid().to_string(),
        "mode": "background",
    })
    .to_string();
    sqlx::query("ALTER TABLE tool_request DISABLE TRIGGER tool_request_is_append_only")
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE tool_request SET arguments_text = $1 WHERE request_id = $2")
        .bind(arguments)
        .bind(fixture.awaiting_request.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE tool_request ENABLE TRIGGER tool_request_is_append_only")
        .execute(&pool)
        .await?;
    prepare_repository_wait_attempt(&pool, fixture, seed).await?;
    let outcome = SessionDelegationRepository::new(pool.clone())
        .record_process_wait(
            fixture.parent,
            fixture.parent_turn,
            fixture.awaiting_request,
            absent_child,
            DelegationWaitMode::Background,
        )
        .await?;

    assert_eq!(
        outcome,
        ProcessDelegationOutcome::Rejected(ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::StaleDispatch {
                state: DelegationRequestExecutionState::Prepared,
            },
        ))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: a background process wait reserves its future
/// result delivery or terminalizes the executable attempt with typed evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_background_wait_delivery_exhaustion_terminalizes_attempt() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_BACKGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    sqlx::query("ALTER TABLE session_pending_delivery DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO session_pending_delivery
            (recipient_session_id, delivery_sequence, delivery_kind)
         VALUES ($1, $2, 'message')",
    )
    .bind(fixture.parent.into_uuid())
    .bind(Decimal::from(u64::MAX))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_pending_delivery ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let logical = DelegationAwaitRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        DelegationWaitMode::Background,
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    let outcome = repository
        .record_process_wait(
            fixture.parent,
            fixture.parent_turn,
            fixture.awaiting_request,
            fixture.child,
            DelegationWaitMode::Background,
        )
        .await?;
    let replay = repository
        .record_process_wait(
            fixture.parent,
            fixture.parent_turn,
            fixture.awaiting_request,
            fixture.child,
            DelegationWaitMode::Background,
        )
        .await?;
    let model_replay = repository.record_wait(logical, &dispatch).await?;
    let durable_completion = PostgresToolLoopRepository::new(pool.clone())
        .reread_durable_completion(dispatch.correlation())
        .await?;
    let attempt_state: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM tool_attempt
          WHERE request_id = $1",
    )
    .bind(fixture.awaiting_request.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        outcome,
        ProcessDelegationOutcome::Rejected(ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::DeliverySequenceExhausted,
        ))
    );
    assert_eq!(replay, outcome);
    assert_eq!(
        model_replay,
        RecordDelegationWaitOutcome::DurablyRejected(
            DelegationOperationRejection::DeliverySequenceExhausted,
        )
    );
    assert!(durable_completion);
    assert_eq!(
        attempt_state,
        (String::from("terminal"), Some(String::from("known_failed")))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: an approved proposal-ordered request remains nonterminal
/// before the tool loop prepares its physical attempt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_process_wait_reports_approved_request_before_attempt() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_APPROVED_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    remove_repository_pending_attempts(&pool, seed).await?;
    let outcome = SessionDelegationRepository::new(pool.clone())
        .record_process_wait(
            fixture.parent,
            fixture.parent_turn,
            fixture.awaiting_request,
            fixture.child,
            DelegationWaitMode::Background,
        )
        .await?;

    assert_eq!(
        outcome,
        ProcessDelegationOutcome::Rejected(ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::StaleDispatch {
                state: DelegationRequestExecutionState::Approved,
            },
        ))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: process delegation validates the named session before the
/// request identity for both await and message operations.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_process_delegation_rejects_absent_session_first() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionDelegationRepository::new(pool.clone());
    let session = SessionId::from_uuid(Uuid::from_u128(0xd7f1));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xd7f2));
    let request = ToolRequestId::from_uuid(Uuid::from_u128(0xd7f3));
    let peer = SessionId::from_uuid(Uuid::from_u128(0xd7f4));
    let await_outcome = repository
        .record_process_wait(session, turn, request, peer, DelegationWaitMode::Background)
        .await?;
    let message_outcome = repository
        .record_process_message(
            session,
            turn,
            request,
            peer,
            String::from("message"),
            DelegationMessageId::from_uuid(Uuid::from_u128(0xd7f5)),
        )
        .await?;

    assert_eq!(
        await_outcome,
        ProcessDelegationOutcome::Rejected(ProcessDelegationRequestRejection::SessionNotFound)
    );
    assert_eq!(
        message_outcome,
        ProcessDelegationOutcome::Rejected(ProcessDelegationRequestRejection::SessionNotFound)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: replay validates every immutable wait-row
/// correlation instead of deriving over malformed stored endpoint facts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_wait_replay_rejects_cross_wired_stored_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_BACKGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let _dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository
        .record_process_wait(
            fixture.parent,
            fixture.parent_turn,
            fixture.awaiting_request,
            fixture.child,
            DelegationWaitMode::Background,
        )
        .await?;
    sqlx::query("ALTER TABLE session_delegation_wait DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE session_delegation_wait
            SET parent_turn_id = $1
          WHERE awaiting_tool_request_id = $2",
    )
    .bind(Uuid::from_u128(seed + 0x700))
    .bind(fixture.awaiting_request.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_delegation_wait ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = repository
        .record_process_wait(
            fixture.parent,
            fixture.parent_turn,
            fixture.awaiting_request,
            fixture.child,
            DelegationWaitMode::Background,
        )
        .await
        .expect_err("cross-wired stored wait turn is corruption");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Inconsistent("stored wait row")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: relationship reconstitution rejects stored spawn
/// provenance carrying a field outside the tool-request variant.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_spawn_reconstitution_rejects_contradictory_provenance() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_BACKGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let request = DelegationAwaitRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        DelegationWaitMode::Background,
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    sqlx::query(
        "ALTER TABLE session_delegation_event
         DROP CONSTRAINT session_delegation_event_provenance_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_delegation_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE session_delegation_event
            SET provenance_goal_generation = 1
          WHERE spawning_tool_request_id = $1
            AND event_kind = 'spawned'",
    )
    .bind(fixture.spawning_request.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_delegation_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = repository
        .record_wait(request, &dispatch)
        .await
        .expect_err("spawn reconstitution rejects contradictory provenance fields");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Inconsistent("spawn event provenance")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: a spawn event cannot carry a child-result
/// satellite belonging only to a terminal outcome event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_spawn_reconstitution_rejects_result_satellite() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_BACKGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let request = DelegationAwaitRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        DelegationWaitMode::Background,
    )?;
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
    let error = SessionDelegationRepository::new(pool.clone())
        .record_wait(request, &dispatch)
        .await
        .expect_err("spawn replay rejects a cross-kind child-result satellite");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Inconsistent("spawn event provenance")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: relationship reconstitution rejects stored outcome
/// provenance carrying a field outside the selected provenance variant.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_outcome_reconstitution_rejects_contradictory_provenance() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_BACKGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let request = DelegationAwaitRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        DelegationWaitMode::Background,
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    sqlx::query(
        "ALTER TABLE session_delegation_event
         DROP CONSTRAINT session_delegation_event_provenance_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_delegation_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE session_child_result DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id,
             provenance_goal_generation)
         VALUES ($1, 2, 'outcome_recorded', 'child_failed',
                 'child_execution_failed', 'child_turn', $2, $3, 1)",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.child.into_uuid())
    .bind(fixture.initial_turn.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_child_result
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, content_text)
         VALUES ($1, 2, 'outcome_recorded', 'child_failed', NULL)",
    )
    .bind(fixture.spawning_request.into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    sqlx::query("ALTER TABLE session_delegation_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE session_child_result ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = repository
        .record_wait(request, &dispatch)
        .await
        .expect_err("outcome replay rejects contradictory provenance fields");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Inconsistent("outcome provenance shape")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: background-wait replay authenticates the exact
/// completed effect-free attempt and normalized registration receipt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_background_wait_replay_requires_exact_terminal_attempt() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_BACKGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let request = DelegationAwaitRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        DelegationWaitMode::Background,
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository.record_wait(request.clone(), &dispatch).await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET result_text = '{}'
          WHERE attempt_id = $1",
    )
    .bind(dispatch.attempt().attempt().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = repository
        .record_process_wait(
            fixture.parent,
            fixture.parent_turn,
            fixture.awaiting_request,
            fixture.child,
            DelegationWaitMode::Background,
        )
        .await
        .expect_err("background wait replay requires its normalized terminal receipt");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Inconsistent("stored wait attempt")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: equal wait replay requires the durable parent
/// update emitted with the original registration.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_wait_replay_requires_update_outbox_satellite() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_BACKGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let request = DelegationAwaitRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        DelegationWaitMode::Background,
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository.record_wait(request.clone(), &dispatch).await?;
    sqlx::query("ALTER TABLE delegation_update_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "DELETE FROM delegation_update_outbox_event
          WHERE update_kind = 'child_waiting'
            AND awaiting_tool_request_id = $1",
    )
    .bind(fixture.awaiting_request.into_uuid())
    .execute(&pool)
    .await?;
    let error = repository
        .record_wait(request, &dispatch)
        .await
        .expect_err("wait replay requires its update outbox satellite");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Missing("wait update outbox satellite")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: equal wait replay authenticates the global outbox
/// header paired with its durable parent update.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_wait_replay_requires_update_outbox_header() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_BACKGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let request = DelegationAwaitRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        DelegationWaitMode::Background,
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository.record_wait(request.clone(), &dispatch).await?;
    sqlx::query("ALTER TABLE delegation_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "DELETE FROM delegation_outbox_event
          WHERE event_sequence = (
                SELECT event_sequence
                  FROM delegation_update_outbox_event
                 WHERE update_kind = 'child_waiting'
                   AND awaiting_tool_request_id = $1
          )",
    )
    .bind(fixture.awaiting_request.into_uuid())
    .execute(&pool)
    .await?;
    let error = repository
        .record_wait(request, &dispatch)
        .await
        .expect_err("wait replay requires its update outbox header");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Missing("header_event_sequence")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: equal wait replay rejects subject payloads that do
/// not belong to a child-waiting update.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_wait_replay_rejects_unused_update_payload() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_BACKGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let request = DelegationAwaitRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        DelegationWaitMode::Background,
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository.record_wait(request.clone(), &dispatch).await?;
    sqlx::query(
        "ALTER TABLE delegation_update_outbox_event
         DROP CONSTRAINT delegation_update_subject_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE delegation_update_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE delegation_update_outbox_event
            SET outcome_kind = 'child_failed'
          WHERE update_kind = 'child_waiting'
            AND awaiting_tool_request_id = $1",
    )
    .bind(fixture.awaiting_request.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE delegation_update_outbox_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = repository
        .record_wait(request, &dispatch)
        .await
        .expect_err("wait replay rejects an unused outcome payload");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Inconsistent("stored wait update")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: foreground-wait replay authenticates the exact
/// effect-free attempt's typed child-wait terminal evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_foreground_wait_replay_requires_exact_terminal_attempt() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_FOREGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "foreground").await?;
    let dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository
        .record_process_wait(
            fixture.parent,
            fixture.parent_turn,
            fixture.awaiting_request,
            fixture.child,
            DelegationWaitMode::Foreground,
        )
        .await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET wait_child_session_id = $1
          WHERE attempt_id = $2",
    )
    .bind(fixture.parent.into_uuid())
    .bind(dispatch.attempt().attempt().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = repository
        .record_process_wait(
            fixture.parent,
            fixture.parent_turn,
            fixture.awaiting_request,
            fixture.child,
            DelegationWaitMode::Foreground,
        )
        .await
        .expect_err("foreground wait replay requires its exact child-wait evidence");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Inconsistent("stored wait attempt")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: foreground-wait replay authenticates the delivery
/// satellite required by an already-recorded child result.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_foreground_wait_replay_requires_result_delivery() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_FOREGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "foreground").await?;
    let dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let request = DelegationAwaitRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        DelegationWaitMode::Foreground,
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository.record_wait(request.clone(), &dispatch).await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation_event DISABLE TRIGGER ALL;
         ALTER TABLE session_child_result DISABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "WITH event AS (
            INSERT INTO session_delegation_event
                (spawning_tool_request_id, event_ordinal, event_kind,
                 outcome_kind, reason_kind, provenance_kind,
                 provenance_session_id, provenance_turn_id)
            VALUES ($1, 2, 'outcome_recorded', 'child_failed',
                    'child_execution_failed', 'child_turn', $2, $3)
            RETURNING spawning_tool_request_id, event_ordinal, event_kind,
                      outcome_kind
         )
         INSERT INTO session_child_result
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, content_text)
         SELECT spawning_tool_request_id, event_ordinal, event_kind,
                outcome_kind, NULL FROM event",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.child.into_uuid())
    .bind(fixture.initial_turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation_event ENABLE TRIGGER ALL;
         ALTER TABLE session_child_result ENABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    let error = repository
        .record_wait(request, &dispatch)
        .await
        .expect_err("foreground replay requires its result-delivery satellite");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Inconsistent("stored wait delivery")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: background-wait replay rejects a delivery
/// satellite cross-wired to a different recipient and missing its pending row.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_background_wait_replay_requires_exact_result_delivery() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_BACKGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let request = DelegationAwaitRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        DelegationWaitMode::Background,
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository.record_wait(request.clone(), &dispatch).await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation_event DISABLE TRIGGER ALL;
         ALTER TABLE session_child_result DISABLE TRIGGER ALL;
         ALTER TABLE session_child_result_delivery DISABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "WITH event AS (
            INSERT INTO session_delegation_event
                (spawning_tool_request_id, event_ordinal, event_kind,
                 outcome_kind, reason_kind, provenance_kind,
                 provenance_session_id, provenance_turn_id)
            VALUES ($1, 2, 'outcome_recorded', 'child_failed',
                    'child_execution_failed', 'child_turn', $2, $3)
            RETURNING spawning_tool_request_id, event_ordinal, event_kind,
                      outcome_kind
         ), result AS (
            INSERT INTO session_child_result
                (spawning_tool_request_id, event_ordinal, event_kind,
                 outcome_kind, content_text)
            SELECT spawning_tool_request_id, event_ordinal, event_kind,
                   outcome_kind, NULL FROM event
         )
         INSERT INTO session_child_result_delivery
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, delivery_sequence, delivery_kind)
         VALUES ($4, $1, $2, 1, 'background_result')",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.child.into_uuid())
    .bind(fixture.initial_turn.into_uuid())
    .bind(fixture.awaiting_request.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation_event ENABLE TRIGGER ALL;
         ALTER TABLE session_child_result ENABLE TRIGGER ALL;
         ALTER TABLE session_child_result_delivery ENABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    let error = repository
        .record_wait(request, &dispatch)
        .await
        .expect_err("background replay requires exact result-delivery satellites");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Inconsistent("stored wait delivery")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: relationship reconstitution rejects action payloads that a
/// stored background policy is not permitted to carry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_background_policy_reconstitution_rejects_action_payloads() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_BACKGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let request = DelegationAwaitRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        DelegationWaitMode::Background,
    )?;
    sqlx::query("ALTER TABLE session_delegation DROP CONSTRAINT session_delegation_policy_shape")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE session_delegation DISABLE TRIGGER session_delegation_is_append_only")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE session_delegation
            SET on_parent_stopped = 'stop'
          WHERE spawning_tool_request_id = $1",
    )
    .bind(fixture.spawning_request.into_uuid())
    .execute(&pool)
    .await?;
    let error = SessionDelegationRepository::new(pool.clone())
        .record_wait(request, &dispatch)
        .await
        .expect_err("background action payload is durable corruption");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Inconsistent("background policy shape")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: foreground registration ends the physical await
/// attempt and parks the same turn without retaining a live attempt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_delegation_repository_parks_foreground_wait_atomically() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_FOREGROUND_WAIT_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "foreground").await?;
    let dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let request = DelegationAwaitRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        DelegationWaitMode::Foreground,
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    let recorded = repository.record_wait(request.clone(), &dispatch).await?;
    let recorded = recorded_wait(recorded);
    let durable_wait = CorrelatedDurableChildWait::try_new(dispatch.correlation(), recorded.wait())
        .expect("recorded foreground wait matches its dispatch");
    let authenticated = PostgresToolLoopRepository::new(pool.clone())
        .reread_durable_child_wait(durable_wait)
        .await?;
    let replayed = repository.record_wait(request, &dispatch).await?;
    let replayed = recorded_wait(replayed);
    let evidence: ForegroundWaitAtomicityEvidence = sqlx::query_as(
        "SELECT lifecycle.active_phase_kind AS active_phase,
                lifecycle.current_attempt_id AS current_attempt,
                attempt.state_kind AS attempt_state,
                attempt.terminal_disposition_kind AS terminal_disposition,
                issuing.end_disposition AS issuing_disposition,
                (SELECT count(*) FROM delegation_update_outbox_event
                  WHERE awaiting_tool_request_id = $1) AS update_count
           FROM turn_lifecycle AS lifecycle
           JOIN tool_attempt AS attempt ON attempt.request_id = $1
           JOIN turn_attempt AS issuing
             ON issuing.turn_attempt_id = attempt.issuing_turn_attempt_id
          WHERE lifecycle.turn_id = $2 AND lifecycle.session_id = $3",
    )
    .bind(fixture.awaiting_request.into_uuid())
    .bind(fixture.parent_turn.into_uuid())
    .bind(fixture.parent.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(recorded, replayed);
    assert!(authenticated);
    assert_eq!(recorded.wait().mode(), DelegationWaitMode::Foreground);
    assert_eq!(evidence.active_phase, "awaiting_child");
    assert_eq!(evidence.current_attempt, None);
    assert_eq!(evidence.attempt_state, "terminal");
    assert_eq!(evidence.terminal_disposition, "awaiting_child");
    assert_eq!(evidence.issuing_disposition, "yielded_to_durable_wait");
    assert_eq!(evidence.update_count, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: a message, recipient delivery, completed receipt, update,
/// and wake are committed once, while physical replay returns the stored ID.
/// S18: equal replay still requires its exact dispatch.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_delegation_repository_commits_message_and_wake_atomically()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_MESSAGE_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_message_dispatch(&pool, fixture, seed).await?;
    let message = DelegationMessageId::from_uuid(fixture.message_id);
    let repository = SessionDelegationRepository::new(pool.clone());
    let (request, recorded) = process_message(
        repository
            .record_process_message(
                fixture.parent,
                fixture.parent_turn,
                fixture.message_request,
                fixture.child,
                RAW_DELEGATED_MESSAGE.to_owned(),
                message,
            )
            .await?,
    );
    let (replayed_request, replayed) = process_message(
        repository
            .record_process_message(
                fixture.parent,
                fixture.parent_turn,
                fixture.message_request,
                fixture.child,
                RAW_DELEGATED_MESSAGE.to_owned(),
                DelegationMessageId::from_uuid(Uuid::from_u128(seed + 0x401)),
            )
            .await?,
    );
    let conflict = repository
        .record_process_message(
            fixture.parent,
            fixture.parent_turn,
            fixture.message_request,
            fixture.child,
            RAW_DELEGATED_MESSAGE.to_uppercase(),
            DelegationMessageId::from_uuid(Uuid::from_u128(seed + 0x402)),
        )
        .await?;
    let empty_content = repository
        .record_process_message(
            fixture.parent,
            fixture.parent_turn,
            fixture.message_request,
            fixture.child,
            String::new(),
            DelegationMessageId::from_uuid(Uuid::from_u128(seed + 0x403)),
        )
        .await?;
    let nul_content = repository
        .record_process_message(
            fixture.parent,
            fixture.parent_turn,
            fixture.message_request,
            fixture.child,
            String::from("contains\0nul"),
            DelegationMessageId::from_uuid(Uuid::from_u128(seed + 0x404)),
        )
        .await?;
    let oversized_content = repository
        .record_process_message(
            fixture.parent,
            fixture.parent_turn,
            fixture.message_request,
            fixture.child,
            "m".repeat(DelegationContent::MAX_UTF8_BYTES + 1),
            DelegationMessageId::from_uuid(Uuid::from_u128(seed + 0x405)),
        )
        .await?;
    let durable_completion = PostgresToolLoopRepository::new(pool.clone())
        .reread_durable_completion(dispatch.correlation())
        .await?;
    let second_seed = DELEGATION_REPOSITORY_MESSAGE_RACE_SECOND_SEED;
    let second_fixture =
        prepare_delegation_repository_fixture(&pool, second_seed, "background").await?;
    let second_dispatch = repository_message_dispatch(&pool, second_fixture, second_seed).await?;
    let cross_wired = repository
        .record_message(
            DelegationMessageRequest::parse(
                dispatch.request().clone(),
                fixture.child,
                RAW_DELEGATED_MESSAGE.to_owned(),
            )?,
            message,
            &second_dispatch,
        )
        .await?;
    let evidence: MessageAtomicityEvidence = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM session_delegation_event
              WHERE provenance_tool_request_id = $1) AS event_count,
            (SELECT count(*) FROM session_message WHERE message_id = $2) AS message_count,
            (SELECT count(*) FROM session_message_delivery WHERE message_id = $2) AS delivery_count,
            (SELECT count(*) FROM delegation_update_outbox_event WHERE message_id = $2)
                AS update_count,
            (SELECT count(*) FROM delegation_wake_outbox_event WHERE message_id = $2)
                AS wake_count,
            (SELECT count(*) FROM tool_attempt
              WHERE request_id = $1 AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed') AS completed_attempt_count",
    )
    .bind(fixture.message_request.into_uuid())
    .bind(fixture.message_id)
    .fetch_one(&pool)
    .await?;

    assert_eq!(recorded, replayed);
    assert_eq!(request, replayed_request);
    assert_eq!(
        conflict,
        ProcessDelegationOutcome::Rejected(ProcessDelegationRequestRejection::MessageConflict)
    );
    assert_eq!(empty_content, ProcessDelegationOutcome::InvalidRequest);
    assert_eq!(nul_content, ProcessDelegationOutcome::InvalidRequest);
    assert_eq!(oversized_content, ProcessDelegationOutcome::InvalidRequest);
    assert!(durable_completion);
    assert_eq!(recorded.message(), message);
    assert_eq!(
        recorded.direction(),
        DelegationMessageDirection::ParentToChild
    );
    assert_eq!(recorded.ordinal().get(), 2);
    assert_eq!(recorded.delivery_sequence().get(), 1);
    assert_eq!(
        cross_wired,
        RecordDelegationMessageOutcome::Rejected(DelegationOperationRejection::StaleDispatch {
            state: DelegationRequestExecutionState::AttemptEnded,
        })
    );
    assert_eq!(evidence.event_count, 1);
    assert_eq!(evidence.message_count, 1);
    assert_eq!(evidence.delivery_count, 1);
    assert_eq!(evidence.update_count, 1);
    assert_eq!(evidence.wake_count, 1);
    assert_eq!(evidence.completed_attempt_count, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: message replay validates the direction-derived
/// recipient and its pending-delivery correlation before returning a receipt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_message_replay_rejects_cross_wired_recipient() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_MESSAGE_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let _dispatch = repository_message_dispatch(&pool, fixture, seed).await?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository
        .record_process_message(
            fixture.parent,
            fixture.parent_turn,
            fixture.message_request,
            fixture.child,
            RAW_DELEGATED_MESSAGE.to_owned(),
            DelegationMessageId::from_uuid(fixture.message_id),
        )
        .await?;
    sqlx::query("ALTER TABLE session_message_delivery DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE session_message_delivery
            SET recipient_session_id = $1
          WHERE message_id = $2",
    )
    .bind(fixture.parent.into_uuid())
    .bind(fixture.message_id)
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_message_delivery ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = repository
        .record_process_message(
            fixture.parent,
            fixture.parent_turn,
            fixture.message_request,
            fixture.child,
            RAW_DELEGATED_MESSAGE.to_owned(),
            DelegationMessageId::from_uuid(Uuid::from_u128(seed + 0x401)),
        )
        .await
        .expect_err("cross-wired delivery recipient is corruption");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Missing("pending_recipient_session_id")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: message replay authenticates the exact completed
/// external-effect attempt and normalized durable receipt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_message_replay_requires_exact_terminal_attempt() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_MESSAGE_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_message_dispatch(&pool, fixture, seed).await?;
    let request = DelegationMessageRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        RAW_DELEGATED_MESSAGE.to_owned(),
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository
        .record_message(
            request.clone(),
            DelegationMessageId::from_uuid(fixture.message_id),
            &dispatch,
        )
        .await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET result_text = '{}'
          WHERE attempt_id = $1",
    )
    .bind(dispatch.attempt().attempt().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = repository
        .record_process_message(
            fixture.parent,
            fixture.parent_turn,
            fixture.message_request,
            fixture.child,
            RAW_DELEGATED_MESSAGE.to_owned(),
            DelegationMessageId::from_uuid(Uuid::from_u128(seed + 0x401)),
        )
        .await
        .expect_err("message replay requires its normalized terminal receipt");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Inconsistent("stored message attempt")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: message replay authenticates the complete stored
/// tool-request provenance instead of trusting the request identifier alone.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_message_replay_requires_complete_tool_provenance() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_MESSAGE_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_message_dispatch(&pool, fixture, seed).await?;
    let request = DelegationMessageRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        RAW_DELEGATED_MESSAGE.to_owned(),
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository
        .record_message(
            request.clone(),
            DelegationMessageId::from_uuid(fixture.message_id),
            &dispatch,
        )
        .await?;
    sqlx::query("ALTER TABLE session_delegation_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE session_delegation_event
            SET provenance_turn_id = $1
          WHERE provenance_tool_request_id = $2",
    )
    .bind(fixture.initial_turn.into_uuid())
    .bind(fixture.message_request.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_delegation_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = repository
        .record_message(
            request,
            DelegationMessageId::from_uuid(Uuid::from_u128(seed + 0x401)),
            &dispatch,
        )
        .await
        .expect_err("message replay requires exact tool-request provenance");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Inconsistent("message provenance")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: a message event cannot authenticate a cross-kind
/// child-result satellite attached to its ordinal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_message_event_rejects_child_result_satellite() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_MESSAGE_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let message_dispatch = repository_message_dispatch(&pool, fixture, seed).await?;
    let message_request = DelegationMessageRequest::parse(
        message_dispatch.request().clone(),
        fixture.child,
        RAW_DELEGATED_MESSAGE.to_owned(),
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository
        .record_message(
            message_request,
            DelegationMessageId::from_uuid(fixture.message_id),
            &message_dispatch,
        )
        .await?;
    let wait_dispatch = repository_wait_dispatch(&pool, fixture, seed).await?;
    let wait_request = DelegationAwaitRequest::parse(
        wait_dispatch.request().clone(),
        fixture.child,
        DelegationWaitMode::Background,
    )?;
    sqlx::query("ALTER TABLE session_child_result DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO session_child_result
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, content_text)
         VALUES ($1, 2, 'outcome_recorded', 'child_failed', NULL)",
    )
    .bind(fixture.spawning_request.into_uuid())
    .execute(&pool)
    .await?;
    let error = repository
        .record_wait(wait_request, &wait_dispatch)
        .await
        .expect_err("message event rejects a child-result satellite");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Inconsistent("message event result payload")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: reciprocal relationship rows cannot make peer lookup choose
/// one direction nondeterministically.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_message_lookup_rejects_reciprocal_relationships() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_MESSAGE_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_message_dispatch(&pool, fixture, seed).await?;
    let request = DelegationMessageRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        RAW_DELEGATED_MESSAGE.to_owned(),
    )?;
    sqlx::query("ALTER TABLE session_delegation DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind)
         VALUES ($1, $2, $3, $4, 'background')",
    )
    .bind(Uuid::from_u128(seed + 0x900))
    .bind(fixture.child.into_uuid())
    .bind(fixture.initial_turn.into_uuid())
    .bind(fixture.parent.into_uuid())
    .execute(&pool)
    .await?;
    let error = SessionDelegationRepository::new(pool.clone())
        .record_message(
            request,
            DelegationMessageId::from_uuid(fixture.message_id),
            &dispatch,
        )
        .await
        .expect_err("reciprocal relationships are ambiguous corruption");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Inconsistent("ambiguous delegation message endpoints")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: equal message replay requires the durable update
/// satellite emitted by the original transaction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_message_replay_requires_update_outbox_satellite() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_MESSAGE_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_message_dispatch(&pool, fixture, seed).await?;
    let request = DelegationMessageRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        RAW_DELEGATED_MESSAGE.to_owned(),
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository
        .record_message(
            request.clone(),
            DelegationMessageId::from_uuid(fixture.message_id),
            &dispatch,
        )
        .await?;
    sqlx::query("ALTER TABLE delegation_update_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM delegation_update_outbox_event WHERE message_id = $1")
        .bind(fixture.message_id)
        .execute(&pool)
        .await?;
    let error = repository
        .record_message(
            request,
            DelegationMessageId::from_uuid(Uuid::from_u128(seed + 0x401)),
            &dispatch,
        )
        .await
        .expect_err("message replay requires its update outbox satellite");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Missing("update_event_kind")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: equal message replay authenticates the global
/// outbox header paired with its recipient update.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_message_replay_requires_update_outbox_header() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_MESSAGE_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_message_dispatch(&pool, fixture, seed).await?;
    let request = DelegationMessageRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        RAW_DELEGATED_MESSAGE.to_owned(),
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository
        .record_message(
            request.clone(),
            DelegationMessageId::from_uuid(fixture.message_id),
            &dispatch,
        )
        .await?;
    sqlx::query("ALTER TABLE delegation_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "DELETE FROM delegation_outbox_event
          WHERE event_sequence = (
                SELECT event_sequence
                  FROM delegation_update_outbox_event
                 WHERE update_kind = 'session_message'
                   AND message_id = $1
          )",
    )
    .bind(fixture.message_id)
    .execute(&pool)
    .await?;
    let error = repository
        .record_message(
            request,
            DelegationMessageId::from_uuid(Uuid::from_u128(seed + 0x401)),
            &dispatch,
        )
        .await
        .expect_err("message replay requires its update outbox header");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Missing("update_header_event_sequence")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: equal message replay requires the durable wake
/// satellite emitted by the original transaction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_message_replay_requires_wake_outbox_satellite() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_MESSAGE_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_message_dispatch(&pool, fixture, seed).await?;
    let request = DelegationMessageRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        RAW_DELEGATED_MESSAGE.to_owned(),
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository
        .record_message(
            request.clone(),
            DelegationMessageId::from_uuid(fixture.message_id),
            &dispatch,
        )
        .await?;
    sqlx::query("ALTER TABLE delegation_wake_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM delegation_wake_outbox_event WHERE message_id = $1")
        .bind(fixture.message_id)
        .execute(&pool)
        .await?;
    let error = repository
        .record_message(
            request,
            DelegationMessageId::from_uuid(Uuid::from_u128(seed + 0x401)),
            &dispatch,
        )
        .await
        .expect_err("message replay requires its wake outbox satellite");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Missing("wake_event_kind")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: equal message replay authenticates the global
/// outbox header paired with its recipient wake.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_message_replay_requires_wake_outbox_header() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_MESSAGE_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let dispatch = repository_message_dispatch(&pool, fixture, seed).await?;
    let request = DelegationMessageRequest::parse(
        dispatch.request().clone(),
        fixture.child,
        RAW_DELEGATED_MESSAGE.to_owned(),
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    repository
        .record_message(
            request.clone(),
            DelegationMessageId::from_uuid(fixture.message_id),
            &dispatch,
        )
        .await?;
    sqlx::query("ALTER TABLE delegation_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "DELETE FROM delegation_outbox_event
          WHERE event_sequence = (
                SELECT event_sequence
                  FROM delegation_wake_outbox_event
                 WHERE subject_kind = 'message'
                   AND message_id = $1
          )",
    )
    .bind(fixture.message_id)
    .execute(&pool)
    .await?;
    let error = repository
        .record_message(
            request,
            DelegationMessageId::from_uuid(Uuid::from_u128(seed + 0x401)),
            &dispatch,
        )
        .await
        .expect_err("message replay requires its wake outbox header");

    assert_eq!(
        delegation_corruption(error),
        SessionDelegationCorruption::Missing("wake_header_event_sequence")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: concurrent relationships cannot claim one global
/// message identity; exactly one records and the loser is a typed rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_concurrent_message_identity_collision_is_typed() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_seed = DELEGATION_REPOSITORY_MESSAGE_SEED;
    let second_seed = DELEGATION_REPOSITORY_MESSAGE_RACE_SECOND_SEED;
    let first_fixture =
        prepare_delegation_repository_fixture(&pool, first_seed, "background").await?;
    let second_fixture =
        prepare_delegation_repository_fixture(&pool, second_seed, "background").await?;
    let first_dispatch = repository_message_dispatch(&pool, first_fixture, first_seed).await?;
    let second_dispatch = repository_message_dispatch(&pool, second_fixture, second_seed).await?;
    let first_request = DelegationMessageRequest::parse(
        first_dispatch.request().clone(),
        first_fixture.child,
        RAW_DELEGATED_MESSAGE.to_owned(),
    )?;
    let second_request = DelegationMessageRequest::parse(
        second_dispatch.request().clone(),
        second_fixture.child,
        RAW_DELEGATED_MESSAGE.to_owned(),
    )?;
    let shared_message = DelegationMessageId::from_uuid(first_fixture.message_id);
    let repository = SessionDelegationRepository::new(pool.clone());
    let mut provisional_claim = pool.begin().await?;
    sqlx::query(
        "INSERT INTO session_message
            (message_id, spawning_tool_request_id, event_ordinal,
             event_kind, direction, content_text)
         VALUES ($1, $2, 2, 'message_delivered', 'parent_to_child', 'provisional claim')",
    )
    .bind(shared_message.into_uuid())
    .bind(first_fixture.spawning_request.into_uuid())
    .execute(&mut *provisional_claim)
    .await?;
    let mut race = tokio::spawn(async move {
        tokio::join!(
            repository.record_message(first_request, shared_message, &first_dispatch),
            repository.record_message(second_request, shared_message, &second_dispatch),
        )
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(250), &mut race)
            .await
            .is_err(),
        "both message claims wait behind the uncommitted global identity"
    );
    provisional_claim.rollback().await?;
    let (first, second) = race.await?;
    let mut dispositions = [
        message_race_disposition(first?),
        message_race_disposition(second?),
    ];
    dispositions.sort();

    assert_eq!(
        dispositions,
        [
            MessageRaceDisposition::IdentityCollision,
            MessageRaceDisposition::Recorded,
        ]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: a definitive process-message collision
/// retains the exact minted identity, terminalizes its executable attempt as
/// known failed, and replays the typed rejection from durable evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_process_message_collision_replays_typed_rejection() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_seed = DELEGATION_REPOSITORY_MESSAGE_SEED;
    let second_seed = DELEGATION_REPOSITORY_MESSAGE_RACE_SECOND_SEED;
    let first_fixture =
        prepare_delegation_repository_fixture(&pool, first_seed, "background").await?;
    let second_fixture =
        prepare_delegation_repository_fixture(&pool, second_seed, "background").await?;
    let _first_dispatch = repository_message_dispatch(&pool, first_fixture, first_seed).await?;
    let _second_dispatch = repository_message_dispatch(&pool, second_fixture, second_seed).await?;
    let message = DelegationMessageId::from_uuid(first_fixture.message_id);
    let repository = SessionDelegationRepository::new(pool.clone());
    let recorded = repository
        .record_process_message(
            second_fixture.parent,
            second_fixture.parent_turn,
            second_fixture.message_request,
            second_fixture.child,
            RAW_DELEGATED_MESSAGE.to_owned(),
            message,
        )
        .await?;
    let collision = repository
        .record_process_message(
            first_fixture.parent,
            first_fixture.parent_turn,
            first_fixture.message_request,
            first_fixture.child,
            RAW_DELEGATED_MESSAGE.to_owned(),
            message,
        )
        .await?;
    let replay_candidate = DelegationMessageId::from_uuid(Uuid::from_u128(first_seed + 0x780));
    let replay = repository
        .record_process_message(
            first_fixture.parent,
            first_fixture.parent_turn,
            first_fixture.message_request,
            first_fixture.child,
            RAW_DELEGATED_MESSAGE.to_owned(),
            replay_candidate,
        )
        .await?;
    let attempt_state: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM tool_attempt
          WHERE request_id = $1",
    )
    .bind(first_fixture.message_request.into_uuid())
    .fetch_one(&pool)
    .await?;

    let _ = process_message(recorded);
    assert_eq!(
        collision,
        ProcessDelegationOutcome::Rejected(
            ProcessDelegationRequestRejection::MessageIdentityCollision { message }
        )
    );
    assert_eq!(replay, collision);
    assert_eq!(
        attempt_state,
        (String::from("terminal"), Some(String::from("known_failed")))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: an executable process message naming an absent
/// peer terminalizes its attempt with the typed relationship rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_process_message_absent_peer_terminalizes_attempt() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = DELEGATION_REPOSITORY_MESSAGE_SEED;
    let fixture = prepare_delegation_repository_fixture(&pool, seed, "background").await?;
    let absent_peer = SessionId::from_uuid(Uuid::from_u128(seed + 0x500));
    let arguments = serde_json::json!({
        "content": RAW_DELEGATED_MESSAGE,
        "peer_session_id": absent_peer.as_uuid().to_string(),
    })
    .to_string();
    sqlx::query("ALTER TABLE tool_request DISABLE TRIGGER tool_request_is_append_only")
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE tool_request SET arguments_text = $1 WHERE request_id = $2")
        .bind(arguments)
        .bind(fixture.message_request.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE tool_request ENABLE TRIGGER tool_request_is_append_only")
        .execute(&pool)
        .await?;
    let dispatch = repository_message_dispatch(&pool, fixture, seed).await?;
    let logical = DelegationMessageRequest::parse(
        dispatch.request().clone(),
        absent_peer,
        RAW_DELEGATED_MESSAGE.to_owned(),
    )?;
    let repository = SessionDelegationRepository::new(pool.clone());
    let outcome = repository
        .record_process_message(
            fixture.parent,
            fixture.parent_turn,
            fixture.message_request,
            absent_peer,
            RAW_DELEGATED_MESSAGE.to_owned(),
            DelegationMessageId::from_uuid(fixture.message_id),
        )
        .await?;
    let model_replay = repository
        .record_message(
            logical,
            DelegationMessageId::from_uuid(Uuid::from_u128(seed + 0x501)),
            &dispatch,
        )
        .await?;
    let attempt_state: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM tool_attempt
          WHERE request_id = $1",
    )
    .bind(fixture.message_request.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        outcome,
        ProcessDelegationOutcome::Rejected(ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::RelationshipNotFound,
        ))
    );
    assert_eq!(
        attempt_state,
        (String::from("terminal"), Some(String::from("known_failed")))
    );
    assert_eq!(
        model_replay,
        RecordDelegationMessageOutcome::DurablyRejected(
            DelegationOperationRejection::RelationshipNotFound,
        )
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_outbox_records_exact_update_and_wake_inventory() -> Result<(), Box<dyn Error>> {
    let (container, pool, _fixture) =
        prepared_complete_delegation_outbox(DELEGATION_OUTBOX_FIXTURE_SEED).await?;
    let update_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delegation_update_outbox_event")
            .fetch_one(&pool)
            .await?;
    let wake_count: i64 = sqlx::query_scalar("SELECT count(*) FROM delegation_wake_outbox_event")
        .fetch_one(&pool)
        .await?;

    assert_eq!(update_count, 5);
    assert_eq!(wake_count, 2);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_result_update_requires_its_subject() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_complete_delegation_outbox(DELEGATION_OUTBOX_FIXTURE_SEED).await?;
    let mut forged = pool.begin().await?;
    let shape_error = append_raw_delegation_update(
        &mut forged,
        fixture,
        RawDelegationUpdate {
            session: fixture.parent,
            kind: "child_result",
            awaiting_request: None,
            event_ordinal: None,
            event_kind: None,
            result_request: None,
            message_id: None,
        },
    )
    .await
    .expect_err("a child-result update requires its correlated result");
    forged.rollback().await?;

    assert_eq!(
        constraint_name(&shape_error),
        Some("delegation_update_subject_shape")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_result_update_rejects_a_duplicate() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_complete_delegation_outbox(DELEGATION_OUTBOX_FIXTURE_SEED).await?;
    let mut duplicate = pool.begin().await?;
    let duplicate_error = append_raw_delegation_update(
        &mut duplicate,
        fixture,
        RawDelegationUpdate {
            session: fixture.parent,
            kind: "child_result",
            awaiting_request: None,
            event_ordinal: None,
            event_kind: None,
            result_request: Some(fixture.spawning_request.into_uuid()),
            message_id: None,
        },
    )
    .await
    .expect_err("one stream cannot receive a result update twice");
    duplicate.rollback().await?;

    assert_eq!(
        constraint_name(&duplicate_error),
        Some("delegation_child_result_update_once")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_relation_requires_spawn_update() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = prepare_canonical_raw_delegation(&pool, DELEGATION_RELATION_FIXTURE_SEED).await?;
    let mut relation_only = pool.begin().await?;
    insert_raw_delegation(&mut relation_only, fixture).await?;
    let error = relation_only
        .commit()
        .await
        .expect_err("a delegation relation cannot commit without its spawn update");

    assert_eq!(
        constraint_name(&error),
        Some("delegation_child_spawned_update_required")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_wait_requires_parent_update() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_recipient_delivery_fixture(DELEGATION_WAIT_FIXTURE_SEED).await?;
    let mut transaction = pool.begin().await?;
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
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a wait cannot commit without its parent-stream update");

    assert_eq!(
        constraint_name(&error),
        Some("delegation_child_waiting_update_required")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_parent_lifecycle_requires_update() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_recipient_delivery_fixture(DELEGATION_LIFECYCLE_FIXTURE_SEED).await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE session_delegation_event
         DISABLE TRIGGER session_delegation_event_requires_payload",
    )
    .execute(&mut *transaction)
    .await?;
    insert_raw_parent_lifecycle_without_update(
        &mut transaction,
        fixture,
        Uuid::from_u128(DELEGATION_LIFECYCLE_COMMAND_ID),
    )
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("an outcome cannot commit without its parent-stream lifecycle update");

    assert_eq!(
        constraint_name(&error),
        Some("delegation_lifecycle_update_required")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_message_requires_recipient_update() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_recipient_delivery_fixture(DELEGATION_MESSAGE_UPDATE_FIXTURE_SEED).await?;
    let mut transaction = pool.begin().await?;
    insert_raw_message(&mut transaction, fixture, "parent_to_child", fixture.child).await?;
    append_raw_message_wake(&mut transaction, fixture, fixture.child).await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a message cannot commit without its recipient-stream update");

    assert_eq!(
        constraint_name(&error),
        Some("delegation_session_message_update_required")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_result_requires_parent_update() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_recipient_delivery_fixture(DELEGATION_RESULT_UPDATE_FIXTURE_SEED).await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE session_delegation_event
         DISABLE TRIGGER session_delegation_event_requires_payload",
    )
    .execute(&mut *transaction)
    .await?;
    insert_raw_wait_with_update(&mut transaction, fixture).await?;
    insert_raw_failed_outcome(
        &mut transaction,
        fixture,
        fixture.initial_turn,
        DELEGATION_WAIT_ONLY_OUTCOME_ORDINAL,
    )
    .await?;
    append_raw_result_wake(&mut transaction, fixture).await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a result cannot commit without its parent-stream result update");

    assert_eq!(
        constraint_name(&error),
        Some("delegation_child_result_update_required")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_message_requires_recipient_wake() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_recipient_delivery_fixture(DELEGATION_MESSAGE_WAKE_FIXTURE_SEED).await?;
    let mut transaction = pool.begin().await?;
    insert_raw_message(&mut transaction, fixture, "parent_to_child", fixture.child).await?;
    append_raw_message_update(
        &mut transaction,
        fixture,
        RawMessageRoute {
            stream: fixture.child,
            sender: fixture.parent,
            recipient: fixture.child,
        },
    )
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("every message requires its distinct recipient wake");

    assert_eq!(
        constraint_name(&error),
        Some("delegation_message_wake_required")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_result_requires_parent_wake() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_recipient_delivery_fixture(DELEGATION_RESULT_WAKE_FIXTURE_SEED).await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE session_delegation_event
         DISABLE TRIGGER session_delegation_event_requires_payload",
    )
    .execute(&mut *transaction)
    .await?;
    insert_raw_wait_with_update(&mut transaction, fixture).await?;
    insert_raw_failed_outcome(
        &mut transaction,
        fixture,
        fixture.initial_turn,
        DELEGATION_WAIT_ONLY_OUTCOME_ORDINAL,
    )
    .await?;
    append_raw_delegation_update(
        &mut transaction,
        fixture,
        RawDelegationUpdate {
            session: fixture.parent,
            kind: "child_result",
            awaiting_request: None,
            event_ordinal: None,
            event_kind: None,
            result_request: Some(fixture.spawning_request.into_uuid()),
            message_id: None,
        },
    )
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("every result requires its distinct parent wake");

    assert_eq!(
        constraint_name(&error),
        Some("delegation_result_wake_required")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn parent_to_child_update_requires_child_stream() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_recipient_delivery_fixture(DELEGATION_CHILD_STREAM_FIXTURE_SEED).await?;
    let mut transaction = pool.begin().await?;
    insert_raw_message(&mut transaction, fixture, "parent_to_child", fixture.child).await?;
    append_raw_message_update(
        &mut transaction,
        fixture,
        RawMessageRoute {
            stream: fixture.parent,
            sender: fixture.parent,
            recipient: fixture.child,
        },
    )
    .await?;
    append_raw_message_wake(&mut transaction, fixture, fixture.child).await?;
    let error = sqlx::query("SET CONSTRAINTS delegation_update_subject IMMEDIATE")
        .execute(&mut *transaction)
        .await
        .expect_err("a parent-to-child update belongs only to the child stream");
    transaction.rollback().await?;

    assert_eq!(constraint_name(&error), Some("delegation_update_subject"));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn child_to_parent_update_requires_parent_stream() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_recipient_delivery_fixture(DELEGATION_PARENT_STREAM_FIXTURE_SEED).await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE session_delegation_event
         DISABLE TRIGGER session_delegation_event_requires_payload",
    )
    .execute(&mut *transaction)
    .await?;
    insert_raw_message(&mut transaction, fixture, "child_to_parent", fixture.parent).await?;
    append_raw_message_update(
        &mut transaction,
        fixture,
        RawMessageRoute {
            stream: fixture.child,
            sender: fixture.child,
            recipient: fixture.parent,
        },
    )
    .await?;
    append_raw_message_wake(&mut transaction, fixture, fixture.parent).await?;
    let error = sqlx::query("SET CONSTRAINTS delegation_update_subject IMMEDIATE")
        .execute(&mut *transaction)
        .await
        .expect_err("a child-to-parent update belongs only to the parent stream");
    transaction.rollback().await?;

    assert_eq!(constraint_name(&error), Some("delegation_update_subject"));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn message_update_rejects_cross_endpoint_duplicate() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_recipient_delivery_fixture(DELEGATION_DUPLICATE_MESSAGE_FIXTURE_SEED).await?;
    let mut transaction = pool.begin().await?;
    insert_raw_message(&mut transaction, fixture, "parent_to_child", fixture.child).await?;
    append_raw_message_update(
        &mut transaction,
        fixture,
        RawMessageRoute {
            stream: fixture.child,
            sender: fixture.parent,
            recipient: fixture.child,
        },
    )
    .await?;
    let error = append_raw_message_update(
        &mut transaction,
        fixture,
        RawMessageRoute {
            stream: fixture.parent,
            sender: fixture.parent,
            recipient: fixture.child,
        },
    )
    .await
    .expect_err("one message cannot be duplicated onto another endpoint");
    transaction.rollback().await?;

    assert_eq!(
        constraint_name(&error),
        Some("delegation_session_message_update_once")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn message_delivery_admits_reverse_insert_order() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_recipient_delivery_fixture(DELEGATION_REVERSE_INSERT_FIXTURE_SEED).await?;
    let mut transaction = pool.begin().await?;
    append_raw_message_update(
        &mut transaction,
        fixture,
        RawMessageRoute {
            stream: fixture.child,
            sender: fixture.parent,
            recipient: fixture.child,
        },
    )
    .await?;
    append_raw_message_wake(&mut transaction, fixture, fixture.child).await?;
    insert_raw_message(&mut transaction, fixture, "parent_to_child", fixture.child).await?;
    transaction.commit().await?;

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_delegation_history_rejects_initial_task_deletion() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_recipient_delivery_fixture(DELEGATION_HISTORY_FIXTURE_SEED).await?;
    let mut history = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE session_delegation_initial_task
         DISABLE TRIGGER session_delegation_initial_task_is_append_only",
    )
    .execute(&mut *history)
    .await?;
    sqlx::query(
        "ALTER TABLE semantic_transcript_entry
         DISABLE TRIGGER semantic_transcript_entry_is_append_only",
    )
    .execute(&mut *history)
    .await?;
    sqlx::query(
        "DELETE FROM semantic_transcript_entry
          WHERE semantic_entry_id = $1",
    )
    .bind(fixture.initial_semantic_entry.into_uuid())
    .execute(&mut *history)
    .await?;
    sqlx::query(
        "DELETE FROM session_delegation_initial_task
          WHERE spawning_tool_request_id = $1",
    )
    .bind(fixture.spawning_request.into_uuid())
    .execute(&mut *history)
    .await?;
    let history_error = history
        .commit()
        .await
        .expect_err("a relation cannot outlive its initial task");

    assert_eq!(
        constraint_name(&history_error),
        Some("session_delegation_initial_task_history")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_delegation_outcome_rejects_a_later_child_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_delegation_with_wait(DELEGATION_HISTORY_FIXTURE_SEED).await?;
    let later_turn = TurnId::from_uuid(Uuid::from_u128(DELEGATION_HISTORY_FIXTURE_SEED + 0x500));
    let mut outcome = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE turn_lifecycle
         DISABLE TRIGGER turn_lifecycle_requires_typed_origin",
    )
    .execute(&mut *outcome)
    .await?;
    sqlx::query("DROP INDEX turn_lifecycle_one_queued_delegation_origin_per_session")
        .execute(&mut *outcome)
        .await?;
    sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_kind, origin_accepted_input_id,
             acceptance_position, state_kind)
         VALUES ($1, $2, 'delegation', NULL, 2, 'queued')",
    )
    .bind(later_turn.into_uuid())
    .bind(fixture.child.into_uuid())
    .execute(&mut *outcome)
    .await?;
    insert_raw_failed_outcome(
        &mut outcome,
        fixture,
        later_turn,
        DELEGATION_WAIT_ONLY_OUTCOME_ORDINAL,
    )
    .await?;
    let turn_error = outcome
        .commit()
        .await
        .expect_err("a later child turn cannot terminate the delegation");

    assert_eq!(
        constraint_name(&turn_error),
        Some("session_delegation_event_semantics")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_result_wake_requires_its_subject_shape() -> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) =
        prepared_recipient_delivery_fixture(DELEGATION_HISTORY_FIXTURE_SEED).await?;
    let wake_error = sqlx::query(
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
                $2, 'result', NULL, NULL FROM header",
    )
    .bind(fixture.parent.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a result wake requires its correlated result pointer");

    assert_eq!(
        constraint_name(&wake_error),
        Some("delegation_wake_subject_shape")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_delegation_spawn_purpose_requires_exact_json() -> Result<(), Box<dyn Error>> {
    let extra_spawn = serde_json::json!({
        "relationship": { "kind": "background" },
        "task": RAW_DELEGATED_TASK,
        "unexpected": true,
    })
    .to_string();
    let child = SessionId::from_uuid(Uuid::from_u128(
        DELEGATION_SPAWN_PURPOSE_FIXTURE_SEED + 0x200,
    ));
    let canonical_message = serde_json::json!({
        "content": RAW_DELEGATED_MESSAGE,
        "peer_session_id": child.as_uuid().to_string(),
    })
    .to_string();
    let (container, pool, _database_url) = migrated_postgres().await?;
    let spawn_fixture = prepare_raw_delegation(
        &pool,
        DELEGATION_SPAWN_PURPOSE_FIXTURE_SEED,
        RawDelegationPurposes {
            spawn_arguments: &extra_spawn,
            message_arguments: &canonical_message,
            wait_mode: "background",
        },
    )
    .await?;
    let mut spawn = pool.begin().await?;
    insert_raw_delegation_with_update(&mut spawn, spawn_fixture).await?;
    let spawn_error = spawn
        .commit()
        .await
        .expect_err("extra spawn fields are not canonical purpose");

    assert_eq!(
        constraint_name(&spawn_error),
        Some("session_delegation_initial_task_purpose")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_delegation_message_purpose_requires_exact_json() -> Result<(), Box<dyn Error>> {
    let canonical_spawn = serde_json::json!({
        "relationship": { "kind": "background" },
        "task": RAW_DELEGATED_TASK,
    })
    .to_string();
    let message_child = SessionId::from_uuid(Uuid::from_u128(
        DELEGATION_MESSAGE_PURPOSE_FIXTURE_SEED + 0x200,
    ));
    let extra_message = serde_json::json!({
        "content": RAW_DELEGATED_MESSAGE,
        "peer_session_id": message_child.as_uuid().to_string(),
        "unexpected": true,
    })
    .to_string();
    let (container, pool, _database_url) = migrated_postgres().await?;
    let message_fixture = prepare_raw_delegation(
        &pool,
        DELEGATION_MESSAGE_PURPOSE_FIXTURE_SEED,
        RawDelegationPurposes {
            spawn_arguments: &canonical_spawn,
            message_arguments: &extra_message,
            wait_mode: "background",
        },
    )
    .await?;
    let mut message = pool.begin().await?;
    insert_raw_delegation_with_update(&mut message, message_fixture).await?;
    insert_raw_wait_and_message_with_delivery(&mut message, message_fixture).await?;
    let message_error = message
        .commit()
        .await
        .expect_err("extra message fields are not canonical purpose");

    assert_eq!(
        constraint_name(&message_error),
        Some("session_delegation_event_semantics")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s19_delegation_cascade_rejects_unrelated_disposition_source() -> Result<(), Box<dyn Error>>
{
    let spawn_arguments = serde_json::json!({
        "relationship": { "kind": "background" },
        "task": RAW_DELEGATED_TASK,
    })
    .to_string();
    let first_child = SessionId::from_uuid(Uuid::from_u128(
        DELEGATION_CASCADE_SOURCE_FIXTURE_SEED + 0x200,
    ));
    let first_message = serde_json::json!({
        "content": RAW_DELEGATED_MESSAGE,
        "peer_session_id": first_child.as_uuid().to_string(),
    })
    .to_string();
    let second_child = SessionId::from_uuid(Uuid::from_u128(
        DELEGATION_CASCADE_TARGET_FIXTURE_SEED + 0x200,
    ));
    let second_message = serde_json::json!({
        "content": RAW_DELEGATED_MESSAGE,
        "peer_session_id": second_child.as_uuid().to_string(),
    })
    .to_string();
    let (container, pool, _database_url) = migrated_postgres().await?;
    let source = prepare_raw_delegation(
        &pool,
        DELEGATION_CASCADE_SOURCE_FIXTURE_SEED,
        RawDelegationPurposes {
            spawn_arguments: &spawn_arguments,
            message_arguments: &first_message,
            wait_mode: "background",
        },
    )
    .await?;
    let target = prepare_raw_delegation(
        &pool,
        DELEGATION_CASCADE_TARGET_FIXTURE_SEED,
        RawDelegationPurposes {
            spawn_arguments: &spawn_arguments,
            message_arguments: &second_message,
            wait_mode: "background",
        },
    )
    .await?;
    let mut setup = pool.begin().await?;
    insert_raw_delegation_with_update(&mut setup, source).await?;
    insert_raw_delegation_with_update(&mut setup, target).await?;
    setup.commit().await?;
    let root_command =
        DurableCommandId::from_uuid(Uuid::from_u128(DELEGATION_CASCADE_ROOT_COMMAND_ID));
    let mut cascade = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE durable_command
         DISABLE TRIGGER durable_command_requires_typed_record",
    )
    .execute(&mut *cascade)
    .await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'goal', 1, transaction_timestamp(), 'operator')",
    )
    .bind(root_command.into_uuid())
    .execute(&mut *cascade)
    .await?;
    sqlx::query(
        "ALTER TABLE session_delegation_termination_cascade
         DISABLE TRIGGER session_delegation_termination_cascade_command",
    )
    .execute(&mut *cascade)
    .await?;
    sqlx::query(
        "ALTER TABLE session_delegation_event
         DISABLE TRIGGER session_delegation_event_zz_requires_lifecycle_update",
    )
    .execute(&mut *cascade)
    .await?;
    sqlx::query(
        "ALTER TABLE session_child_result
         DISABLE TRIGGER session_child_result_zz_requires_update",
    )
    .execute(&mut *cascade)
    .await?;
    sqlx::query(
        "ALTER TABLE session_child_result
         DISABLE TRIGGER session_child_result_zz_requires_wake",
    )
    .execute(&mut *cascade)
    .await?;
    sqlx::query(
        "ALTER TABLE session_delegation
         DISABLE TRIGGER session_delegation_is_append_only",
    )
    .execute(&mut *cascade)
    .await?;
    sqlx::query(
        "UPDATE session_delegation
            SET policy_kind = 'bound',
                on_parent_stopped = 'stop',
                on_parent_cancelled = 'cancel'
          WHERE spawning_tool_request_id = $1",
    )
    .bind(source.spawning_request.into_uuid())
    .execute(&mut *cascade)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_termination_cascade
            (root_command_id, root_session_id, root_source_kind,
             root_turn_id, root_goal_generation,
             termination_kind, descendant_scope, disposition_count)
         VALUES ($1, $2, 'goal_command', NULL, 1,
                 'stopped', 'parent_and_descendants', 1)",
    )
    .bind(root_command.into_uuid())
    .bind(source.parent.into_uuid())
    .execute(&mut *cascade)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_parent_termination
            (spawning_tool_request_id, root_command_id,
             parent_session_id, command_source_kind, parent_turn_id,
             parent_goal_generation, termination_kind,
             source_kind, source_spawning_tool_request_id)
         VALUES ($1, $2, $3, 'goal_command', NULL, 1,
                 'stopped', 'root', NULL)",
    )
    .bind(source.spawning_request.into_uuid())
    .bind(root_command.into_uuid())
    .bind(source.parent.into_uuid())
    .execute(&mut *cascade)
    .await?;
    sqlx::query(
        "WITH event AS (
            INSERT INTO session_delegation_event
                (spawning_tool_request_id, event_ordinal, event_kind,
                 outcome_kind, reason_kind, provenance_kind,
                 provenance_session_id, provenance_turn_id,
                 provenance_goal_generation,
                 provenance_command_id)
            VALUES ($1, 2, 'outcome_recorded', 'child_stopped',
                    'parent_stopped_parent_and_descendants',
                    'parent_goal_command', $2, NULL, 1, $3)
            RETURNING spawning_tool_request_id, event_ordinal, event_kind,
                      outcome_kind
         )
         INSERT INTO session_child_result
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, content_text)
         SELECT spawning_tool_request_id, event_ordinal, event_kind,
                outcome_kind, NULL FROM event",
    )
    .bind(source.spawning_request.into_uuid())
    .bind(source.parent.into_uuid())
    .bind(root_command.into_uuid())
    .execute(&mut *cascade)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_parent_termination
            (spawning_tool_request_id, root_command_id,
             parent_session_id, command_source_kind, parent_turn_id,
             parent_goal_generation, termination_kind,
             source_kind, source_spawning_tool_request_id)
         VALUES ($1, $2, $3, 'goal_command', NULL, 1,
                 'stopped', 'parent_disposition', $4)",
    )
    .bind(target.spawning_request.into_uuid())
    .bind(root_command.into_uuid())
    .bind(target.parent.into_uuid())
    .bind(source.spawning_request.into_uuid())
    .execute(&mut *cascade)
    .await?;
    let error = cascade
        .commit()
        .await
        .expect_err("an unrelated disposition cannot authorize another edge");

    assert_eq!(
        constraint_name(&error),
        Some("session_delegation_parent_termination_chain")
    );

    pool.close().await;
    drop(container);
    Ok(())
}
