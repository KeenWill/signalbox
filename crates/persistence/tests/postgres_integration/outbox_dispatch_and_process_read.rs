//! Outbox sequencing and dispatch, process read projections, and turn model settings evidence.

use crate::*;

/// S24: the transactional allocator holds its singleton row through
/// commit, so a concurrent event cannot obtain the next sequence and commit
/// ahead of the lower event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_outbox_sequences_follow_concurrent_commit_order() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_session = insert_outbox_session_fixture(&pool, 0xe11).await?;
    let second_session = insert_outbox_session_fixture(&pool, 0xe12).await?;

    let mut first_transaction = pool.begin().await?;
    let first_sequence =
        append_session_created_test_event(&mut first_transaction, first_session).await?;
    let second = tokio::spawn({
        let pool = pool.clone();
        async move {
            let mut transaction = pool.begin().await?;
            let sequence =
                append_session_created_test_event(&mut transaction, second_session).await?;
            transaction.commit().await?;
            Ok::<_, sqlx::Error>(sequence)
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the higher-sequence allocator must wait for the lower transaction"
    );

    first_transaction.commit().await?;
    let second_sequence = second.await??;
    assert_eq!(first_sequence, Decimal::ONE);
    assert_eq!(second_sequence, Decimal::from(2));

    let committed: Vec<(Decimal, Uuid)> = sqlx::query_as(
        "SELECT event_sequence, session_id
           FROM outbox_event
          ORDER BY event_sequence",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        committed,
        vec![
            (first_sequence, first_session),
            (second_sequence, second_session),
        ]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: delivery cannot advance to an uncommitted allocation, and a
/// later concurrent allocation remains a suffix after the committed prefix is
/// marked delivered.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_outbox_delivery_prefix_is_stable() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_session = insert_outbox_session_fixture(&pool, 0xe13).await?;
    let second_session = insert_outbox_session_fixture(&pool, 0xe14).await?;

    let mut first_transaction = pool.begin().await?;
    let first_sequence =
        append_session_created_test_event(&mut first_transaction, first_session).await?;
    let (allocated_sender, allocated_receiver) = tokio::sync::oneshot::channel();
    let (commit_sender, commit_receiver) = tokio::sync::oneshot::channel();
    let second = tokio::spawn({
        let pool = pool.clone();
        async move {
            let mut transaction = pool.begin().await?;
            let sequence =
                append_session_created_test_event(&mut transaction, second_session).await?;
            allocated_sender
                .send(sequence)
                .expect("the prefix test receives the second allocation");
            commit_receiver
                .await
                .expect("the prefix test releases the second commit");
            transaction.commit().await?;
            Ok::<_, sqlx::Error>(sequence)
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the second allocation must wait while the first is uncommitted"
    );

    let invisible_events: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_event")
        .fetch_one(&pool)
        .await?;
    assert_eq!(invisible_events, 0);
    let uncommitted_delivery = sqlx::query(
        "UPDATE outbox_consumer_cursor
            SET delivered_through = $1
          WHERE consumer_name = 'process_protocol'",
    )
    .bind(first_sequence)
    .execute(&pool)
    .await
    .expect_err("an uncommitted sequence is not a deliverable prefix");
    assert_eq!(
        uncommitted_delivery
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    first_transaction.commit().await?;
    let second_sequence = allocated_receiver.await?;
    let visible_sequences: Vec<Decimal> = sqlx::query_scalar(
        "SELECT event_sequence
           FROM outbox_event
          ORDER BY event_sequence",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(visible_sequences, vec![first_sequence]);

    sqlx::query(
        "UPDATE outbox_consumer_cursor
            SET delivered_through = $1
          WHERE consumer_name = 'process_protocol'",
    )
    .bind(first_sequence)
    .execute(&pool)
    .await?;
    commit_sender
        .send(())
        .expect("the prefix test still awaits the second commit");
    assert_eq!(second.await??, second_sequence);

    let undelivered_suffix: Vec<Decimal> = sqlx::query_scalar(
        "SELECT event.event_sequence
           FROM outbox_event AS event
           CROSS JOIN outbox_consumer_cursor AS delivery
          WHERE delivery.consumer_name = 'process_protocol'
            AND event.event_sequence > delivery.delivered_through
          ORDER BY event.event_sequence",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(first_sequence, Decimal::ONE);
    assert_eq!(second_sequence, Decimal::from(2));
    assert_eq!(undelivered_suffix, vec![second_sequence]);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: one summary page batches distinct placement projections while retaining
/// stable session-identity order and each selected defaults row.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_process_session_summary_page_batches_placement_projection()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let earlier_selection = outbox_session_fixture_model_selection(0xe31);
    let earlier_session = insert_outbox_session_fixture(&pool, 0xe31).await?;
    let earlier_placement = SessionPlacement::pathless();
    let later_session = Uuid::from_u128(0xe32);
    let alias = ModelAlias::from_uuid(Uuid::from_u128(0xae32));
    let later_placement = SessionPlacement::scoped(
        SessionPlacementPath::try_new("projects.later".to_owned())
            .expect("the later fixture path is valid"),
    )
    .expect("the later fixture path is non-root");
    let later_creation = CreateSession::new_with_placement(
        DurableCommandId::from_uuid(Uuid::from_u128(0x4e32)),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Alias(alias)),
        later_placement.clone(),
    )
    .prepare(SessionId::from_uuid(later_session))
    .expect("the placed user-initiated fixture is preparable");
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(later_creation)
        .await?;

    let mut summaries = ProcessReadRepository::new(pool.clone())
        .open_session_summaries()
        .await?;
    let earlier = summaries
        .next_summary()
        .await?
        .ok_or("the earlier session summary is present")?;
    let later = summaries
        .next_summary()
        .await?
        .ok_or("the later session summary is present")?;
    assert!(summaries.next_summary().await?.is_none());

    assert_eq!(summaries.summary_count(), Some(2));
    assert_eq!(earlier.session().into_uuid(), earlier_session);
    assert_eq!(earlier.defaults_version(), 1);
    assert_eq!(
        earlier.model_selection(),
        ProcessModelSelection::Direct(earlier_selection)
    );
    assert_eq!(earlier.placement().placement(), &earlier_placement);
    assert_eq!(later.session().into_uuid(), later_session);
    assert_eq!(later.defaults_version(), 1);
    assert_eq!(later.model_selection(), ProcessModelSelection::Alias(alias));
    assert_eq!(later.placement().placement(), &later_placement);

    pool.close().await;
    drop(container);
    Ok(())
}

/// Inserts exactly one full 64-session summary page plus one continuation row,
/// returning their identities in the order the summary cursor must yield them.
async fn insert_session_summary_page_boundary_fixture(
    pool: &PgPool,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let mut sessions = Vec::with_capacity(65);
    for session_seed in 0xf000..=0xf040 {
        sessions.push(insert_outbox_session_fixture(pool, session_seed).await?);
    }
    Ok(sessions)
}

/// S24: a summary catalog one row beyond the 64-session safety ceiling
/// continues onto a second page without skipping or duplicating an identity.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_process_session_summary_page_continues_after_64_sessions() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let expected_sessions = insert_session_summary_page_boundary_fixture(&pool).await?;

    let summaries = ProcessReadRepository::new(pool.clone())
        .list_sessions()
        .await?;
    let actual_sessions = summaries
        .iter()
        .map(|summary| summary.session().into_uuid())
        .collect::<Vec<_>>();

    assert_eq!(summaries.len(), 65);
    assert_eq!(actual_sessions, expected_sessions);
    assert_eq!(summaries[63].session().into_uuid(), expected_sessions[63]);
    assert_eq!(summaries[64].session().into_uuid(), expected_sessions[64]);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: the process transcript read observes the global outbox
/// cursor, ordered turn state, and latest semantic frontier in one
/// repeatable-read snapshot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_process_transcript_is_one_authoritative_snapshot() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8e41));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0xce41));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            0x4e41,
            0x8e41,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9e41));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xae41));
    let defaults_version = SessionConfigurationDefaultsVersion::first();
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x4e42,
                0x8e41,
                "projected user request",
                defaults_version.as_u64(),
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_input,
            Some(turn),
        )
        .await?;
    let repository = ProcessReadRepository::new(pool.clone());
    assert!(
        repository
            .read_transcript(SessionId::from_uuid(Uuid::from_u128(0xffff)))
            .await?
            .is_none()
    );
    let queued_snapshot = repository
        .read_transcript(session)
        .await?
        .expect("the committed session has a transcript projection");

    assert_eq!(queued_snapshot.session(), session);
    assert_eq!(queued_snapshot.cursor(), 4);
    assert_eq!(queued_snapshot.turns().len(), 1);
    assert_eq!(queued_snapshot.turns()[0].turn(), turn);
    assert_eq!(queued_snapshot.turns()[0].acceptance_position(), 1);
    let queued_settings = queued_snapshot.turns()[0]
        .model_settings()
        .expect("the settings-aware turn retains its frozen settings evidence");
    assert_eq!(queued_settings.accepted_input(), accepted_input);
    assert_eq!(queued_settings.turn(), turn);
    assert_eq!(queued_settings.defaults_version(), defaults_version);
    assert_eq!(
        queued_settings.selection(),
        &FrozenModelSelection::Direct(selection)
    );
    assert_eq!(
        queued_settings.per_call_override(),
        ModelSettingsOverlay::inherit_all()
    );
    assert_eq!(
        queued_settings.settings().precedence().per_call(),
        ModelSettingsOverlay::inherit_all()
    );
    assert_eq!(
        queued_snapshot.turns()[0].state(),
        &ProcessTurnState::Queued {
            accepted_input,
            content: user_content("projected user request"),
        }
    );
    assert!(queued_snapshot.entries().is_empty());

    let origin_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde41));
    let starting_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xee41));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbe41));
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: origin_entry.into_uuid(),
            starting_frontier: starting_frontier.into_uuid(),
            initial_attempt: attempt.into_uuid(),
        },
    )
    .await?;

    let snapshot = repository
        .read_transcript(session)
        .await?
        .expect("the committed session has a transcript projection");

    assert_eq!(snapshot.session(), session);
    assert_eq!(snapshot.cursor(), 5);
    assert_eq!(snapshot.turns().len(), 1);
    assert_eq!(snapshot.turns()[0].turn(), turn);
    assert_eq!(snapshot.turns()[0].acceptance_position(), 1);
    assert_eq!(
        snapshot.turns()[0].state(),
        &ProcessTurnState::ActiveRunning {
            current_attempt: attempt,
            current_model_call: None,
        }
    );
    assert_eq!(
        snapshot.entries(),
        [ProcessTranscriptEntry::User {
            entry_index: 0,
            source_session: session,
            entry: origin_entry,
            accepted_input,
            turn,
            content: user_content("projected user request"),
        }]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: a settings-aware turn cannot omit its required
/// resolution event.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_process_read_rejects_missing_turn_settings_evidence() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8e51));
    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9e51));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xae51));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x4e51, 0x8e51, direct(0xce51)))
        .await?;
    let command = start_input(
        0x4e52,
        0x8e51,
        "settings evidence required",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    SubmitInputRepository::new(pool.clone())
        .handle(command.clone(), accepted_input, Some(turn))
        .await?;

    sqlx::query("ALTER TABLE turn_model_settings_resolved_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let deleted_outbox = sqlx::query(
        "DELETE FROM turn_model_settings_resolved_outbox_event
          WHERE accepted_input_id = $1",
    )
    .bind(accepted_input.into_uuid())
    .execute(&pool)
    .await?;
    assert_eq!(deleted_outbox.rows_affected(), 1);
    sqlx::query("ALTER TABLE turn_model_settings_resolved DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let deleted_settings =
        sqlx::query("DELETE FROM turn_model_settings_resolved WHERE accepted_input_id = $1")
            .bind(accepted_input.into_uuid())
            .execute(&pool)
            .await?;
    assert_eq!(deleted_settings.rows_affected(), 1);
    sqlx::query("ALTER TABLE turn_model_settings_resolved ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE turn_model_settings_resolved_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    assert!(matches!(
        ProcessReadRepository::new(pool.clone())
            .read_transcript(session)
            .await,
        Err(ProcessReadError::Corruption(
            ProcessReadCorruption::Missing("turn model settings evidence")
        ))
    ));
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .load(command.command_id())
            .await,
        Err(SubmitInputRepositoryError::Corruption(
            SubmitInputCorruption::Missing("turn model settings evidence")
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: a process transcript snapshot exposes the exact durable
/// Prepared, InFlight, or CancellationRequested state of the current model
/// call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_process_transcript_projects_current_model_call_state() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prepared = checkpoint_restart_model_call(&pool, 0x8e50, false).await?;
    let in_flight = checkpoint_restart_model_call(&pool, 0x8e60, true).await?;
    let repository = ProcessReadRepository::new(pool.clone());
    let prepared_snapshot = repository
        .read_transcript(prepared.session)
        .await?
        .expect("the prepared-call session is committed");
    let in_flight_snapshot = repository
        .read_transcript(in_flight.session)
        .await?
        .expect("the in-flight-call session is committed");

    assert_eq!(prepared_snapshot.turns().len(), 1);
    assert_running_current_model_call(
        prepared_snapshot.turns()[0].state(),
        prepared.attempt,
        prepared.call,
        ProcessCurrentModelCallState::Prepared,
    );
    assert_eq!(in_flight_snapshot.turns().len(), 1);
    assert_running_current_model_call(
        in_flight_snapshot.turns()[0].state(),
        in_flight.attempt,
        in_flight.call,
        ProcessCurrentModelCallState::InFlight,
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: the production dispatcher offers one exact next event before
/// advancing the locked durable prefix. Consumer retry and an injected deferred
/// commit failure after the offer both roll the prefix back, so restart offers
/// the same cursor again before the later committed event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_dispatcher_redelivers_after_cursor_commit_failure_in_order()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_session = insert_outbox_session_fixture(&pool, 0xe17).await?;
    let second_session = insert_outbox_session_fixture(&pool, 0xe18).await?;
    let mut first_transaction = pool.begin().await?;
    append_session_created_test_event(&mut first_transaction, first_session).await?;
    first_transaction.commit().await?;
    let mut second_transaction = pool.begin().await?;
    append_session_created_test_event(&mut second_transaction, second_session).await?;
    second_transaction.commit().await?;
    sqlx::query(
        "CREATE FUNCTION fail_test_outbox_delivery_commit()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             RAISE EXCEPTION 'injected delivery cursor commit failure'
                 USING ERRCODE = '40001';
         END;
         $$",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE CONSTRAINT TRIGGER zz_test_fail_outbox_delivery_commit
         AFTER UPDATE ON outbox_consumer_cursor
         DEFERRABLE INITIALLY DEFERRED
         FOR EACH ROW
         EXECUTE FUNCTION fail_test_outbox_delivery_commit()",
    )
    .execute(&pool)
    .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    let offered = Arc::new(Mutex::new(Vec::new()));
    let retry_offer = Arc::clone(&offered);
    assert_eq!(
        dispatcher
            .dispatch_next(move |event| {
                retry_offer
                    .lock()
                    .expect("offer log lock")
                    .push((event.sequence(), event.session().map(SessionId::into_uuid)));
                OutboxDeliveryDecision::Retry
            })
            .await?,
        OutboxDispatchOutcome::Retry { sequence: 1 }
    );
    let first_offer = Arc::clone(&offered);
    assert!(matches!(
        dispatcher
            .dispatch_next(move |event| {
                first_offer
                    .lock()
                    .expect("offer log lock")
                    .push((event.sequence(), event.session().map(SessionId::into_uuid)));
                OutboxDeliveryDecision::Delivered
            })
            .await,
        Err(OutboxDispatchError::Database(_))
    ));
    assert_eq!(
        sqlx::query_scalar::<_, Decimal>(
            "SELECT delivered_through
               FROM outbox_consumer_cursor
              WHERE consumer_name = 'process_protocol'",
        )
        .fetch_one(&pool)
        .await?,
        Decimal::ZERO
    );

    sqlx::query(
        "DROP TRIGGER zz_test_fail_outbox_delivery_commit
            ON outbox_consumer_cursor",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DROP FUNCTION fail_test_outbox_delivery_commit()")
        .execute(&pool)
        .await?;

    let first_redelivery = Arc::clone(&offered);
    assert_eq!(
        dispatcher
            .dispatch_next(move |event| {
                first_redelivery
                    .lock()
                    .expect("offer log lock")
                    .push((event.sequence(), event.session().map(SessionId::into_uuid)));
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );
    let second_delivery = Arc::clone(&offered);
    assert_eq!(
        dispatcher
            .dispatch_next(move |event| {
                second_delivery
                    .lock()
                    .expect("offer log lock")
                    .push((event.sequence(), event.session().map(SessionId::into_uuid)));
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 2 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Idle
    );
    assert_eq!(
        offered.lock().expect("offer log lock").as_slice(),
        [
            (1, Some(first_session)),
            (1, Some(first_session)),
            (1, Some(first_session)),
            (2, Some(second_session))
        ]
    );
    assert_eq!(
        sqlx::query_scalar::<_, Decimal>(
            "SELECT delivered_through
               FROM outbox_consumer_cursor
              WHERE consumer_name = 'process_protocol'",
        )
        .fetch_one(&pool)
        .await?,
        Decimal::from(2)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24 / INV-032: each compiled-in outbox consumer advances an independent
/// prefix while decoding the same commit-ordered typed events.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_inv032_outbox_consumers_advance_independent_typed_prefixes()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_session = insert_outbox_session_fixture(&pool, 0xe19).await?;
    let second_session = insert_outbox_session_fixture(&pool, 0xe1a).await?;
    let mut first_transaction = pool.begin().await?;
    append_session_created_test_event(&mut first_transaction, first_session).await?;
    first_transaction.commit().await?;
    let mut second_transaction = pool.begin().await?;
    append_session_created_test_event(&mut second_transaction, second_session).await?;
    second_transaction.commit().await?;

    let process = OutboxDispatcher::new(pool.clone());
    let repo_watch = OutboxConsumerReader::new(pool.clone(), OutboxConsumer::RepoWatch);

    assert_eq!(
        process
            .dispatch_next(|event| {
                assert_eq!(event.session(), Some(SessionId::from_uuid(first_session)));
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );
    let first = repo_watch
        .read_next()
        .await?
        .expect("repo-watch has its first typed event");
    assert_eq!(first.sequence(), 1);
    assert_eq!(first.session(), Some(SessionId::from_uuid(first_session)));
    assert!(first.recorded_at().unix_timestamp() > 0);
    assert_eq!(repo_watch.read_next().await?, Some(first.clone()));
    repo_watch.acknowledge(first.sequence()).await?;

    let second = repo_watch
        .read_next()
        .await?
        .expect("repo-watch has its second typed event");
    assert_eq!(second.sequence(), 2);
    assert_eq!(second.session(), Some(SessionId::from_uuid(second_session)));
    repo_watch.acknowledge(second.sequence()).await?;
    repo_watch.acknowledge(first.sequence()).await?;
    assert_eq!(repo_watch.read_next().await?, None);

    let cursors: Vec<(String, Decimal)> = sqlx::query_as(
        "SELECT consumer_name, delivered_through
           FROM outbox_consumer_cursor
          ORDER BY consumer_name",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        cursors,
        vec![
            ("process_protocol".to_owned(), Decimal::ONE),
            ("repo_watch".to_owned(), Decimal::from(2)),
        ]
    );

    assert_eq!(
        process
            .dispatch_next(|event| {
                assert_eq!(event.session(), Some(SessionId::from_uuid(second_session)));
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 2 }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S10: storage independently rejects a restored tool response whose
/// request inventory exceeds the bounded domain vocabulary.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_tool_round_storage_rejects_more_than_32_requests() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let error = sqlx::query(
        "INSERT INTO tool_round
            (producing_model_call_id, session_id, turn_id, boundary_kind,
             boundary_frontier_id, response_part_count, request_count)
         VALUES ($1, $2, $3, 'continuing', $4, 33, 33)",
    )
    .bind(Uuid::from_u128(1))
    .bind(Uuid::from_u128(2))
    .bind(Uuid::from_u128(3))
    .bind(Uuid::from_u128(4))
    .execute(&pool)
    .await
    .expect_err("the request-count constraint rejects the thirty-third request");

    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("tool_round_counts_bounded")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: an allocator cursor beyond the delivered prefix requires its
/// exact committed header; dispatcher idle is reserved for equal cursors.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_dispatcher_reports_a_missing_committed_header() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         DISABLE TRIGGER outbox_sequence_requires_event",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_sequence_state
            SET last_sequence = 1,
                last_allocation_xid = pg_current_xact_id()
          WHERE singleton",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         ENABLE TRIGGER outbox_sequence_requires_event",
    )
    .execute(&pool)
    .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert!(matches!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::MissingCommittedEventHeader
        ))
    ));
    assert_eq!(
        sqlx::query_scalar::<_, Decimal>(
            "SELECT delivered_through
               FROM outbox_consumer_cursor
              WHERE consumer_name = 'process_protocol'",
        )
        .fetch_one(&pool)
        .await?,
        Decimal::ZERO
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: a header restored ahead of the allocator cursor is durable
/// corruption and is never offered to the consumer.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_dispatcher_rejects_a_header_beyond_the_allocator() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = insert_outbox_session_fixture(&pool, 0xe1a).await?;
    let mut producer = pool.begin().await?;
    append_session_created_test_event(&mut producer, session).await?;
    producer.commit().await?;

    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         DISABLE TRIGGER outbox_sequence_requires_event",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_sequence_state
            SET last_sequence = 0,
                last_allocation_xid = NULL
          WHERE singleton",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         ENABLE TRIGGER outbox_sequence_requires_event",
    )
    .execute(&pool)
    .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("an unallocated header must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::EventBeyondAllocatedSequence
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: a restored header above both the allocator and the exact next
/// slot is corruption rather than an idle outbox.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_dispatcher_rejects_a_noncontiguous_header_beyond_the_allocator()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_session = insert_outbox_session_fixture(&pool, 0xe1b).await?;
    let second_session = insert_outbox_session_fixture(&pool, 0xe1c).await?;
    let mut first_producer = pool.begin().await?;
    append_session_created_test_event(&mut first_producer, first_session).await?;
    first_producer.commit().await?;
    let mut second_producer = pool.begin().await?;
    append_session_created_test_event(&mut second_producer, second_session).await?;
    second_producer.commit().await?;

    sqlx::query("ALTER TABLE session_created_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         DISABLE TRIGGER outbox_sequence_requires_event",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM session_created_outbox_event WHERE event_sequence = 1")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM outbox_event WHERE event_sequence = 1")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE outbox_sequence_state
            SET last_sequence = 0,
                last_allocation_xid = NULL
          WHERE singleton",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_created_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         ENABLE TRIGGER outbox_sequence_requires_event",
    )
    .execute(&pool)
    .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("an unallocated header must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::EventBeyondAllocatedSequence
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: exhausted delivery still validates the allocator singleton
/// rather than silently polling forever on missing durable state.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_dispatcher_validates_the_allocator_at_exhaustion() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    sqlx::query(
        "ALTER TABLE outbox_consumer_cursor
         DISABLE TRIGGER outbox_consumer_cursor_advances_prefix",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_consumer_cursor
            SET delivered_through = 18446744073709551615,
                last_delivery_xid = pg_current_xact_id()
          WHERE consumer_name = 'process_protocol'",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_consumer_cursor
         ENABLE TRIGGER outbox_consumer_cursor_advances_prefix",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         DISABLE TRIGGER outbox_sequence_state_cannot_be_deleted",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM outbox_sequence_state WHERE singleton")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE outbox_sequence_state
         ENABLE TRIGGER outbox_sequence_state_cannot_be_deleted",
    )
    .execute(&pool)
    .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("exhausted delivery cannot offer an event"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::MissingSequenceState
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: independently valid same-session terminal identifiers do not
/// form a dispatchable event unless they all describe the event's exact turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_dispatcher_rejects_crosswired_terminal_correlations() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = Uuid::from_u128(0x7e1);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x3e0, 0x7e1, direct(0x8e1)))
        .await?;
    let inputs = SubmitInputRepository::new(pool.clone());

    let first_turn = Uuid::from_u128(0xae1);
    inputs
        .handle(
            start_input(
                0x3e1,
                0x7e1,
                "first failed turn",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9e1)),
            Some(TurnId::from_uuid(first_turn)),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session,
            origin_entry: Uuid::from_u128(0xce1),
            starting_frontier: Uuid::from_u128(0xde1),
            initial_attempt: Uuid::from_u128(0xbe1),
        },
    )
    .await?;
    let mut first_scan = StartupScanService::new(
        FixedStartupScanIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xee1))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xfe1))],
        ),
        PostgresStartupScanRepository::new(pool.clone()),
    );
    assert_eq!(first_scan.execute().await?.recovered_turn_count(), 1);

    let second_turn = Uuid::from_u128(0xae2);
    inputs
        .handle(
            start_input(
                0x3e2,
                0x7e1,
                "second failed turn",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9e2)),
            Some(TurnId::from_uuid(second_turn)),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session,
            origin_entry: Uuid::from_u128(0xce2),
            starting_frontier: Uuid::from_u128(0xde2),
            initial_attempt: Uuid::from_u128(0xbe2),
        },
    )
    .await?;
    let mut second_scan = StartupScanService::new(
        FixedStartupScanIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xee2))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xfe2))],
        ),
        PostgresStartupScanRepository::new(pool.clone()),
    );
    assert_eq!(second_scan.execute().await?.recovered_turn_count(), 1);

    let failures: Vec<(Decimal, Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT event_sequence, turn_id, failure_entry_id, terminal_frontier_id
           FROM turn_terminal_outbox_event
          WHERE disposition_kind = 'failed'
          AND session_id = $1
          ORDER BY event_sequence",
    )
    .bind(session)
    .fetch_all(&pool)
    .await?;
    let [first, second] = failures.as_slice() else {
        return Err(std::io::Error::other("fixture did not produce two failures").into());
    };
    assert_eq!(first.1, first_turn);
    assert_eq!(second.1, second_turn);

    sqlx::query(
        "ALTER TABLE turn_terminal_outbox_event
         DISABLE TRIGGER turn_terminal_outbox_event_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM turn_terminal_outbox_event WHERE disposition_kind = 'failed' AND event_sequence = $1")
        .bind(second.0)
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_terminal_outbox_event
            SET failure_entry_id = $1,
                terminal_frontier_id = $2
          WHERE event_sequence = $3",
    )
    .bind(second.2)
    .bind(second.3)
    .bind(first.0)
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE turn_terminal_outbox_event
         ENABLE TRIGGER turn_terminal_outbox_event_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_consumer_cursor
         DISABLE TRIGGER outbox_consumer_cursor_advances_prefix",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_consumer_cursor
            SET delivered_through = $1 - 1,
                last_delivery_xid = pg_current_xact_id()
          WHERE consumer_name = 'process_protocol'",
    )
    .bind(first.0)
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_consumer_cursor
         ENABLE TRIGGER outbox_consumer_cursor_advances_prefix",
    )
    .execute(&pool)
    .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("a cross-wired terminal event must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidTerminalEventCorrelation
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: the dispatcher observes the allocator and candidate header in
/// one statement snapshot, so an uncommitted allocation is idle rather than
/// false committed-header corruption.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_dispatcher_treats_an_uncommitted_allocation_as_idle() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = insert_outbox_session_fixture(&pool, 0xe19).await?;
    let mut producer = pool.begin().await?;
    let sequence = append_session_created_test_event(&mut producer, session).await?;
    let dispatcher = OutboxDispatcher::new(pool.clone());

    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Idle
    );
    producer.commit().await?;
    assert_eq!(sequence, Decimal::ONE);
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: an event-producing transaction cannot mark its own
/// uncommitted event delivered and thereby make restart recovery skip it.
/// Both append-before-delivery and delivery-before-append orderings are covered.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_outbox_delivery_rejects_event_producing_transaction() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    insert_outbox_session_fixture(&pool, 0xe15).await?;
    insert_outbox_session_fixture(&pool, 0xe16).await?;

    let mut event_transaction = pool.begin().await?;
    let sequence =
        append_session_created_test_event(&mut event_transaction, Uuid::from_u128(0xe15)).await?;
    let same_transaction_delivery = sqlx::query(
        "UPDATE outbox_consumer_cursor
            SET delivered_through = $1
          WHERE consumer_name = 'process_protocol'",
    )
    .bind(sequence)
    .execute(&mut *event_transaction)
    .await
    .expect_err("an event-producing transaction cannot deliver its own event");
    assert_eq!(
        same_transaction_delivery
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    event_transaction.rollback().await?;

    let rolled_back: (Decimal, i64) = sqlx::query_as(
        "SELECT
            (SELECT delivered_through
               FROM outbox_consumer_cursor
              WHERE consumer_name = 'process_protocol'),
            (SELECT count(*)
               FROM outbox_event)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(rolled_back, (Decimal::ZERO, 0));

    let mut committed_event = pool.begin().await?;
    let sequence =
        append_session_created_test_event(&mut committed_event, Uuid::from_u128(0xe15)).await?;
    committed_event.commit().await?;

    let mut delivery_then_event = pool.begin().await?;
    sqlx::query(
        "UPDATE outbox_consumer_cursor
            SET delivered_through = $1
          WHERE consumer_name = 'process_protocol'",
    )
    .bind(sequence)
    .execute(&mut *delivery_then_event)
    .await?;
    let delivery_first_append =
        append_session_created_test_event(&mut delivery_then_event, Uuid::from_u128(0xe16))
            .await
            .expect_err("delivery and later event append cannot share one transaction");
    assert_eq!(
        delivery_first_append
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    delivery_then_event.rollback().await?;

    let after_delivery_first_rollback: (Decimal, i64) = sqlx::query_as(
        "SELECT
            (SELECT delivered_through
               FROM outbox_consumer_cursor
              WHERE consumer_name = 'process_protocol'),
            (SELECT count(*)
               FROM outbox_event)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(after_delivery_first_rollback, (Decimal::ZERO, 1));

    sqlx::query(
        "UPDATE outbox_consumer_cursor
            SET delivered_through = $1
          WHERE consumer_name = 'process_protocol'",
    )
    .bind(sequence)
    .execute(&pool)
    .await?;
    let delivered_through: Decimal = sqlx::query_scalar(
        "SELECT delivered_through
           FROM outbox_consumer_cursor
          WHERE consumer_name = 'process_protocol'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(delivered_through, sequence);

    pool.close().await;
    drop(container);
    Ok(())
}

/// the durable sequence, prefix, header, and typed-record tables cannot
/// bypass their row-level guards through PostgreSQL's statement-level truncate.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn outbox_storage_rejects_truncate() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE hub_fence_state CASCADE").await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE outbox_sequence_state CASCADE").await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE outbox_consumer_cursor CASCADE").await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE outbox_event CASCADE").await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE session_created_outbox_event CASCADE")
        .await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE input_accepted_outbox_event CASCADE")
        .await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE turn_activated_outbox_event CASCADE")
        .await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE turn_terminal_outbox_event CASCADE")
        .await?;
    assert_outbox_truncate_rejected(
        &pool,
        "TRUNCATE TABLE model_call_transition_outbox_event CASCADE",
    )
    .await?;
    assert_outbox_truncate_rejected(
        &pool,
        "TRUNCATE TABLE session_state_changed_outbox_event CASCADE",
    )
    .await?;
    assert_outbox_truncate_rejected(
        &pool,
        "TRUNCATE TABLE session_terminal_outbox_event CASCADE",
    )
    .await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE goal_changed_outbox_event CASCADE")
        .await?;
    assert_outbox_truncate_rejected(&pool, "TRUNCATE TABLE command_settled_outbox_event CASCADE")
        .await?;
    assert_outbox_truncate_rejected(
        &pool,
        "TRUNCATE TABLE injection_settled_outbox_event CASCADE",
    )
    .await?;
    assert_outbox_truncate_rejected(
        &pool,
        "TRUNCATE TABLE session_ownership_changed_outbox_event CASCADE",
    )
    .await?;

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: a deferred failure after the production append rolls the
/// CreateSession state, event, and sequence allocation back together; retry
/// commits all three together.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_create_session_and_outbox_commit_or_roll_back_together() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    sqlx::query(
        "CREATE FUNCTION fail_test_session_created_outbox_commit()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             RAISE EXCEPTION 'injected failure after outbox append'
                 USING ERRCODE = '40001';
         END;
         $$",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE CONSTRAINT TRIGGER zz_test_fail_session_created_outbox_commit
         AFTER INSERT ON session_created_outbox_event
         DEFERRABLE INITIALLY DEFERRED
         FOR EACH ROW
         EXECUTE FUNCTION fail_test_session_created_outbox_commit()",
    )
    .execute(&pool)
    .await?;

    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let creation = prepared(0xe31, 0xe41, direct(0xe51));
    let command_id = creation.command().command_id().into_uuid();
    let session_id = creation.applied_result().session().into_uuid();
    let error = repository
        .handle(creation.clone())
        .await
        .expect_err("the deferred fixture failure must abort commit");
    assert!(matches!(error, CreateSessionRepositoryError::Database(_)));
    let rolled_back: (i64, i64, i64, i64, Decimal) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM durable_command
              WHERE command_id = $1),
            (SELECT count(*)
               FROM session
              WHERE session_id = $2),
            (SELECT count(*)
               FROM outbox_event
              WHERE session_id = $2),
            (SELECT count(*)
               FROM session_created_outbox_event
              WHERE session_id = $2),
            (SELECT last_sequence
               FROM outbox_sequence_state
              WHERE singleton)",
    )
    .bind(command_id)
    .bind(session_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(rolled_back, (0, 0, 0, 0, Decimal::ZERO));

    sqlx::query(
        "DROP TRIGGER zz_test_fail_session_created_outbox_commit
            ON session_created_outbox_event",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DROP FUNCTION fail_test_session_created_outbox_commit()")
        .execute(&pool)
        .await?;

    assert_eq!(
        repository.handle(creation.clone()).await?,
        CreateSessionHandlingOutcome::Applied(creation.applied_result())
    );
    let committed: (i64, i64, i64, i64, Decimal) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM durable_command
              WHERE command_id = $1),
            (SELECT count(*)
               FROM session
              WHERE session_id = $2),
            (SELECT count(*)
               FROM outbox_event
              WHERE session_id = $2),
            (SELECT count(*)
               FROM session_created_outbox_event
              WHERE session_id = $2),
            (SELECT last_sequence
               FROM outbox_sequence_state
              WHERE singleton)",
    )
    .bind(command_id)
    .bind(session_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(committed, (1, 1, 1, 1, Decimal::ONE));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: only first committed handling emits the creation
/// event; equal replay and conflicting identifier reuse append nothing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_create_session_first_handling_appends_exactly_once() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let creation = prepared(0xe32, 0xe42, direct(0xe52));

    assert_eq!(
        repository.handle(creation.clone()).await?,
        CreateSessionHandlingOutcome::Applied(creation.applied_result())
    );
    assert_eq!(
        repository.handle(creation.clone()).await?,
        CreateSessionHandlingOutcome::Applied(creation.applied_result())
    );
    assert_eq!(
        repository
            .handle(prepared(0xe32, 0xe43, direct(0xe53)))
            .await?,
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: creation.command().command_id(),
        }
    );

    let events: Vec<(Decimal, String, i16, Uuid)> = sqlx::query_as(
        "SELECT event_sequence, event_kind, storage_version, session_id
           FROM outbox_event
          ORDER BY event_sequence",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        events,
        vec![(
            Decimal::ONE,
            "session_created".to_owned(),
            2,
            creation.applied_result().session().into_uuid(),
        )]
    );
    let typed_events: i64 = sqlx::query_scalar("SELECT count(*) FROM session_created_outbox_event")
        .fetch_one(&pool)
        .await?;
    assert_eq!(typed_events, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: acceptance and activation append their complete
/// typed process transitions in the same commits, and command replay emits no
/// duplicate before the dispatcher advances the exact ordered prefix.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_scheduling_transitions_dispatch_in_commit_order() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0xe61));
    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0xe62));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xe63));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xe64));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            0xe60,
            0xe61,
            ModelSelectionRequest::Direct(signalbox_domain::DirectModelSelection::from_uuid(
                Uuid::from_u128(0xe65),
            )),
        ))
        .await?;
    let command = start_input(
        0xe66,
        0xe61,
        "durable process input",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let repository = SubmitInputRepository::new(pool.clone());
    let recorded = repository
        .handle(command.clone(), accepted_input, Some(turn))
        .await?;
    assert_eq!(
        repository
            .handle(command, accepted_input, Some(turn))
            .await?,
        recorded
    );
    let activated = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xe67),
            starting_frontier: Uuid::from_u128(0xe68),
            initial_attempt: attempt.into_uuid(),
        },
    )
    .await?;
    assert_eq!(activated.turn(), turn);

    let dispatcher = OutboxDispatcher::new(pool.clone());
    let mut created = None;
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                created = Some(event.clone());
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );
    let created = created.expect("the session creation event was offered");
    assert_eq!(created.session(), Some(session));
    assert!(matches!(
        created.kind(),
        DispatchedOutboxEventKind::SessionCreated(_)
    ));

    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 2 }
    );
    let mut accepted = None;
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                accepted = Some(event.clone());
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 3 }
    );
    let accepted = accepted.expect("the input acceptance event was offered");
    assert_eq!(accepted.session(), Some(session));
    assert_eq!(
        accepted.kind(),
        &DispatchedOutboxEventKind::InputAccepted {
            accepted_input,
            turn,
            acceptance_position: SessionInputPosition::first(),
            content: user_content("durable process input"),
        }
    );
    let mut settled = None;
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                settled = Some(event.clone());
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 4 }
    );
    assert_eq!(
        settled
            .expect("the accepted origin settles its injection")
            .kind(),
        &DispatchedOutboxEventKind::InjectionSettled {
            command: DurableCommandId::from_uuid(Uuid::from_u128(0xe66)),
            outcome: DispatchedInjectionOutcome::Delivered { turn: Some(turn) },
        }
    );

    let mut activation = None;
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                activation = Some(event.clone());
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 5 }
    );
    let activation = activation.expect("the turn activation event was offered");
    assert_eq!(activation.session(), Some(session));
    assert_eq!(
        activation.kind(),
        &DispatchedOutboxEventKind::TurnActivated {
            turn,
            current_attempt: attempt,
        }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Idle
    );

    let durable_counts: (i64, i64, Decimal) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM input_accepted_outbox_event
              WHERE accepted_input_id = $1),
            (SELECT count(*) FROM turn_activated_outbox_event
              WHERE current_attempt_id = $2),
            (SELECT delivered_through FROM outbox_consumer_cursor
              WHERE consumer_name = 'process_protocol')",
    )
    .bind(accepted_input.into_uuid())
    .bind(attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_counts, (1, 1, Decimal::from(5)));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: an activation remains dispatchable after continuation while
/// its exact initial attempt and the lifecycle's current or terminal attempt
/// remain authoritative; cross-wired lifecycle provenance fails closed.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_turn_activation_dispatch_requires_authoritative_attempt() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0xe81));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xe82));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xe83));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0xe80, 0xe81, direct(0xe84)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0xe85,
                0xe81,
                "activation correlation",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xe86)),
            Some(turn),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xe87),
            starting_frontier: Uuid::from_u128(0xe88),
            initial_attempt: attempt.into_uuid(),
        },
    )
    .await?;
    let mut startup = StartupScanService::new(
        FixedStartupScanIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xe89))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xe8a))],
        ),
        PostgresStartupScanRepository::new(pool.clone()),
    );
    assert_eq!(startup.execute().await?.recovered_turn_count(), 1);

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 2 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 3 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 4 }
    );
    let mut activation = None;
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                activation = Some(event.clone());
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 5 }
    );
    assert_eq!(
        activation
            .expect("the retained terminal attempt authorizes dispatch")
            .kind(),
        &DispatchedOutboxEventKind::TurnActivated {
            turn,
            current_attempt: attempt,
        }
    );

    sqlx::query(
        "ALTER TABLE turn_lifecycle
            DROP CONSTRAINT turn_lifecycle_terminal_attempt_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET terminal_attempt_id = $1
          WHERE turn_id = $2",
    )
    .bind(Uuid::from_u128(0xe8b))
    .bind(turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE outbox_consumer_cursor
         DISABLE TRIGGER outbox_consumer_cursor_advances_prefix",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_consumer_cursor
            SET delivered_through = 4,
                last_delivery_xid = pg_current_xact_id()
          WHERE consumer_name = 'process_protocol'",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_consumer_cursor
         ENABLE TRIGGER outbox_consumer_cursor_advances_prefix",
    )
    .execute(&pool)
    .await?;

    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("cross-wired activation must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidLifecycleEventCorrelation
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: historical Prepared and InFlight transition records remain
/// dispatchable after advancement, but a terminal record must carry the
/// authoritative call's exact terminal disposition.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_terminal_model_call_dispatch_requires_exact_disposition() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xe90;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous);
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Ambiguous(AmbiguousModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
            )),
            |_| panic!("Ambiguous creates no pending-steering successors"),
        )
        .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 2 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 3 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 4 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 5 }
    );
    let mut prepared_transition = None;
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                prepared_transition = Some(event.clone());
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 6 }
    );
    assert_eq!(
        prepared_transition
            .expect("historical Prepared transition is offered")
            .kind(),
        &DispatchedOutboxEventKind::ModelCallTransition {
            turn: fixture.turn,
            call: fixture.call,
            state: DispatchedModelCallState::Prepared,
        }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                assert_eq!(
                    event.kind(),
                    &DispatchedOutboxEventKind::ModelCallTransition {
                        turn: fixture.turn,
                        call: fixture.call,
                        state: DispatchedModelCallState::InFlight,
                    }
                );
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 7 }
    );

    sqlx::query("ALTER TABLE model_call_transition_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE model_call_transition_outbox_event
            SET terminal_disposition_kind = 'cancelled'
          WHERE model_call_id = $1
            AND call_state_kind = 'terminal'",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE model_call_transition_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("cross-wired terminal transition must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidTerminalEventCorrelation
        ))
    ));

    let authoritative: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(authoritative, ("terminal".into(), Some("ambiguous".into())));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: a stored nonterminal model-call transition cannot be ahead
/// of the authoritative monotonic call state.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_model_call_dispatch_rejects_an_unreached_transition() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0xe98, false).await?;
    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 2 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 3 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 4 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 5 }
    );

    sqlx::query("ALTER TABLE model_call_transition_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE model_call_transition_outbox_event
            SET call_state_kind = 'in_flight'
          WHERE model_call_id = $1
            AND call_state_kind = 'prepared'",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE model_call_transition_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("an unreached transition must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidModelCallState
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: a completed-turn event is dispatchable only while the
/// lifecycle's terminal attempt retains a completion-compatible disposition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_completed_dispatch_requires_exact_terminal_attempt() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xea0;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Completed {
            assistant_text: vec![
                AssistantText::try_new(String::from("completed response"))
                    .expect("fixture assistant text is admitted"),
            ],
        });
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 22,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 23)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 24)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    let sequence = sqlx::query_scalar(
        "SELECT event_sequence
           FROM turn_terminal_outbox_event
          WHERE disposition_kind = 'completed'
          AND turn_id = $1",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    corrupt_ended_attempt_disposition(&pool, fixture.attempt, "known_failure").await?;
    rewind_outbox_delivery_before(&pool, sequence).await?;

    assert!(matches!(
        OutboxDispatcher::new(pool.clone())
            .dispatch_next(|_| panic!("a completion with a mismatched attempt must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidTerminalEventCorrelation
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: a refused-turn event is dispatchable only while the
/// lifecycle's terminal attempt retains a refusal-compatible disposition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_refused_dispatch_requires_exact_terminal_attempt() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xeb0;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Refused);
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Refused(RefusedModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    let sequence = sqlx::query_scalar(
        "SELECT event_sequence
           FROM turn_terminal_outbox_event
          WHERE disposition_kind = 'refused'
          AND turn_id = $1",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    corrupt_ended_attempt_disposition(&pool, fixture.attempt, "turn_completed").await?;
    rewind_outbox_delivery_before(&pool, sequence).await?;

    assert!(matches!(
        OutboxDispatcher::new(pool.clone())
            .dispatch_next(|_| panic!("a refusal with a mismatched attempt must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidTerminalEventCorrelation
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / S07: a reconciliation-required event is
/// dispatchable only while its terminal attempt retains exact ambiguity and
/// interrupt provenance.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_reconciliation_dispatch_requires_exact_terminal_attempt() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xec0;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 19,
                seed + 1,
                "stop before ambiguous result",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 21))),
        )
        .await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous);
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Ambiguous(AmbiguousModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
            )),
            |_| panic!("Ambiguous creates no pending-steering successors"),
        )
        .await?;

    let sequence = sqlx::query_scalar(
        "SELECT event_sequence
           FROM turn_terminal_outbox_event
          WHERE disposition_kind = 'reconciliation_required'
          AND turn_id = $1",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    corrupt_ended_attempt_disposition(&pool, fixture.attempt, "cancelled").await?;
    rewind_outbox_delivery_before(&pool, sequence).await?;

    assert!(matches!(
        OutboxDispatcher::new(pool.clone())
            .dispatch_next(|_| {
                panic!("reconciliation with a mismatched attempt must not be offered")
            })
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidTerminalEventCorrelation
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: an accepted-input event is dispatchable only when
/// its content still matches the immutable accepting command.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_dispatcher_rejects_crosswired_accepted_content() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0xe72));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xe73));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0xe70, 0xe71, direct(0xe74)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0xe75,
                0xe71,
                "authoritative command content",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_input,
            Some(turn),
        )
        .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 2 }
    );

    sqlx::query("ALTER TABLE accepted_input_content_part DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE accepted_input_content_part
            SET text_value = 'cross-wired accepted content'
          WHERE accepted_input_id = $1",
    )
    .bind(accepted_input.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE accepted_input_content_part ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("cross-wired accepted content must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::MissingTypedRecord
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: replay of a settings-aware defaults replacement
/// fails closed when its required settings-change evidence is absent.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_replacement_replay_requires_settings_change_evidence() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x3750;
    let replacement = record_settings_replacement_fixture(&pool, seed).await?;

    sqlx::query("ALTER TABLE session_model_settings_changed_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let deleted_outbox = sqlx::query(
        "DELETE FROM session_model_settings_changed_outbox_event
          WHERE session_id = $1",
    )
    .bind(replacement.session.into_uuid())
    .execute(&pool)
    .await?;
    assert_eq!(deleted_outbox.rows_affected(), 1);
    sqlx::query("ALTER TABLE session_model_settings_changed DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let deleted_change =
        sqlx::query("DELETE FROM session_model_settings_changed WHERE command_id = $1")
            .bind(replacement.command.into_uuid())
            .execute(&pool)
            .await?;
    assert_eq!(deleted_change.rows_affected(), 1);
    sqlx::query("ALTER TABLE session_model_settings_changed ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE session_model_settings_changed_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    assert!(matches!(
        ReplaceSessionDefaultsRepository::new(pool.clone())
            .load(replacement.command)
            .await,
        Err(ReplaceSessionDefaultsRepositoryError::Corruption(
            ReplaceSessionDefaultsCorruption::Inconsistent("settings change evidence")
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: replay authenticates settings-change evidence
/// against the immutable command and defaults records.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_replacement_replay_rejects_cross_wired_settings_change_evidence()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let replacement = record_settings_replacement_fixture(&pool, 0x3760).await?;

    sqlx::query("ALTER TABLE session_model_settings_changed DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let update = sqlx::query(
        "UPDATE session_model_settings_changed
            SET prior_model_settings = installed_model_settings
          WHERE command_id = $1",
    )
    .bind(replacement.command.into_uuid())
    .execute(&pool)
    .await?;
    assert_eq!(update.rows_affected(), 1);
    sqlx::query("ALTER TABLE session_model_settings_changed ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    assert!(matches!(
        ReplaceSessionDefaultsRepository::new(pool.clone())
            .load(replacement.command)
            .await,
        Err(ReplaceSessionDefaultsRepositoryError::Corruption(
            ReplaceSessionDefaultsCorruption::Inconsistent("settings event evidence")
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: one defaults epoch can source exactly one durable
/// settings-change outbox event.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_settings_change_outbox_is_unique_per_epoch() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let replacement = record_settings_replacement_fixture(&pool, 0x3770).await?;

    let duplicate = sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ('session_model_settings_changed', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO session_model_settings_changed_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             installed_defaults_version)
         SELECT event_sequence, event_kind, storage_version, session_id, 2
           FROM header",
    )
    .bind(replacement.session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("one defaults epoch cannot produce two settings-change events");
    assert_eq!(
        duplicate
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("session_model_settings_changed_outbox_source_key")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// legacy command versions accept only the provider-default
/// settings documents that the migration backfills.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn legacy_command_versions_reject_explicit_model_settings() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let native_selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x3741));
    let native = prepared_with_low_reasoning(0x3742, 0x3743, native_selection);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(native.clone())
        .await?;
    let replacement_seed = 0x3748;
    let replacement = record_settings_replacement_fixture(&pool, replacement_seed).await?;
    sqlx::raw_sql(
        "ALTER TABLE durable_command DISABLE TRIGGER USER;
         ALTER TABLE create_session_command DISABLE TRIGGER USER;
         ALTER TABLE replace_session_defaults_command DISABLE TRIGGER USER;
         ALTER TABLE create_session_command DROP CONSTRAINT create_session_command_registry_fk;
         ALTER TABLE replace_session_defaults_command DROP CONSTRAINT replace_session_defaults_command_registry_fk;",
    )
    .execute(&pool)
    .await?;
    let native_registry_downgrade =
        sqlx::query("UPDATE durable_command SET storage_version = 6 WHERE command_id = $1")
            .bind(native.command().command_id().into_uuid())
            .execute(&pool)
            .await?;
    let native_typed_downgrade =
        sqlx::query("UPDATE create_session_command SET storage_version = 6 WHERE command_id = $1")
            .bind(native.command().command_id().into_uuid())
            .execute(&pool)
            .await?;
    let replacement_registry_downgrade =
        sqlx::query("UPDATE durable_command SET storage_version = 3 WHERE command_id = $1")
            .bind(replacement.command.into_uuid())
            .execute(&pool)
            .await?;
    let replacement_typed_downgrade = sqlx::query(
        "UPDATE replace_session_defaults_command SET storage_version = 3 WHERE command_id = $1",
    )
    .bind(replacement.command.into_uuid())
    .execute(&pool)
    .await?;
    assert_eq!(native_registry_downgrade.rows_affected(), 1);
    assert_eq!(native_typed_downgrade.rows_affected(), 1);
    assert_eq!(replacement_registry_downgrade.rows_affected(), 1);
    assert_eq!(replacement_typed_downgrade.rows_affected(), 1);

    assert!(matches!(
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
            .load(native.command().command_id())
            .await,
        Err(CreateSessionRepositoryError::Corruption(
            CreateSessionCorruption::Inconsistent("storage version without model settings")
        ))
    ));
    assert!(matches!(
        ReplaceSessionDefaultsRepository::new(pool.clone())
            .load(replacement.command)
            .await,
        Err(ReplaceSessionDefaultsRepositoryError::Corruption(
            ReplaceSessionDefaultsCorruption::Inconsistent(
                "storage version without caller model settings"
            )
        ))
    ));
    assert!(matches!(
        SessionRepository::new(pool.clone())
            .load_session(native.applied_result().session())
            .await,
        Err(SessionRepositoryError::Corruption(
            SessionCorruption::Inconsistent("defaults storage version without model settings")
        ))
    ));
    assert!(matches!(
        SessionRepository::new(pool.clone())
            .load_session(replacement.session)
            .await,
        Err(SessionRepositoryError::Corruption(
            SessionCorruption::Inconsistent("defaults storage version without model settings")
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: native session creation retains the
/// caller's settings independently from its effect row, so a cross-wired
/// defaults snapshot cannot authenticate command replay or current reads.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_native_creation_authenticates_command_settings() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x3761));
    let command = prepared_with_low_reasoning(0x3762, 0x3763, selection);
    let provider_defaults = prepared(0x3764, 0x3765, direct(0x3766));
    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    repository.handle(command.clone()).await?;
    repository.handle(provider_defaults.clone()).await?;

    sqlx::query("ALTER TABLE session_defaults_version DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE session_defaults_version AS target
            SET model_settings = source.model_settings
           FROM session_defaults_version AS source
          WHERE target.session_id = $1
            AND target.version = 1
            AND source.session_id = $2
            AND source.version = 1",
    )
    .bind(command.applied_result().session().into_uuid())
    .bind(provider_defaults.applied_result().session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_defaults_version ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    assert!(matches!(
        repository.load(command.command().command_id()).await,
        Err(CreateSessionRepositoryError::Corruption(
            CreateSessionCorruption::Domain(_)
        ))
    ));
    assert!(matches!(
        SessionRepository::new(pool.clone())
            .load_session(command.applied_result().session())
            .await,
        Err(SessionRepositoryError::Corruption(
            SessionCorruption::Inconsistent("defaults model settings disagree with command")
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: the accepted-input settings copy must equal the
/// independently retained submit-command payload.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_accepted_settings_match_submit_command() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x3767));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x3768, 0x3769, direct(0x376a)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x376b,
                0x3769,
                "settings correlation request",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_input,
            Some(TurnId::from_uuid(Uuid::from_u128(0x376c))),
        )
        .await?;

    sqlx::query("ALTER TABLE accepted_input DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let mismatch = sqlx::query(
        "UPDATE accepted_input
            SET model_settings_override = $1
          WHERE accepted_input_id = $2",
    )
    .bind(serde_json::json!({
        "reasoning_level": { "kind": "provider_default" },
        "fast_mode": { "kind": "inherit" },
        "service_tier": { "kind": "inherit" }
    }))
    .bind(accepted_input.into_uuid())
    .execute(&pool)
    .await
    .expect_err("accepted settings must remain command-correlated");
    sqlx::query("ALTER TABLE accepted_input ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert_eq!(
        mismatch
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("accepted_input_command_settings_result_fk")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: turn-settings dispatch authenticates the retained
/// per-call overlay against the accepted origin rather than trusting only the
/// settings event row.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_dispatcher_rejects_crosswired_turn_settings_origin() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x3771));
    let turn = TurnId::from_uuid(Uuid::from_u128(0x3772));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x3773, 0x3774, direct(0x3775)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x3776,
                0x3774,
                "settings-origin request",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_input,
            Some(turn),
        )
        .await?;
    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );

    sqlx::query("ALTER TABLE accepted_input DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE accepted_input
            SET model_settings_override = $1
          WHERE accepted_input_id = $2",
    )
    .bind(serde_json::json!({
        "reasoning_level": { "kind": "provider_default" },
        "fast_mode": { "kind": "inherit" },
        "service_tier": { "kind": "inherit" }
    }))
    .bind(accepted_input.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE accepted_input ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("cross-wired turn settings must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidModelSettingsEvent
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: defaults-settings dispatch compares both event
/// snapshots with their independently retained immutable defaults epochs.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_dispatcher_rejects_crosswired_defaults_settings_event() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = record_settings_replacement(&pool, 0x3780).await?;
    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );

    sqlx::query("ALTER TABLE session_model_settings_changed DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let update = sqlx::query(
        "UPDATE session_model_settings_changed
            SET prior_model_settings = installed_model_settings
          WHERE session_id = $1
            AND installed_defaults_version = 2",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    assert_eq!(update.rows_affected(), 1);
    sqlx::query("ALTER TABLE session_model_settings_changed ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    let outcome = dispatcher
        .dispatch_next(|_| panic!("cross-wired defaults settings must not be offered"))
        .await;
    assert!(
        matches!(
            outcome,
            Err(OutboxDispatchError::Corruption(
                OutboxCorruption::InvalidModelSettingsEvent
            ))
        ),
        "unexpected dispatch outcome: {outcome:?}"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: defaults-settings dispatch authenticates
/// caller provenance against the independently retained replacement command.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_dispatcher_rejects_crosswired_settings_caller() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = record_settings_replacement(&pool, 0x3790).await?;
    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );

    sqlx::query("ALTER TABLE session_model_settings_changed DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let update = sqlx::query(
        "UPDATE session_model_settings_changed
            SET caller_model_settings = $1
          WHERE session_id = $2
            AND installed_defaults_version = 2",
    )
    .bind(serde_json::json!({
        "reasoning_level": { "kind": "inherit" },
        "fast_mode": { "kind": "inherit" },
        "service_tier": { "kind": "inherit" }
    }))
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    assert_eq!(update.rows_affected(), 1);
    sqlx::query("ALTER TABLE session_model_settings_changed ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("cross-wired caller settings must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidModelSettingsEvent
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S24: turn settings retain the exact lower
/// precedence layers from their referenced immutable defaults epoch.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_turn_settings_authenticate_the_defaults_epoch() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x37a1));
    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x37a2));
    let turn = TurnId::from_uuid(Uuid::from_u128(0x37a3));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x37a4));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared_with_low_reasoning(0x37a5, 0x37a1, selection))
        .await?;
    let capabilities =
        ModelCapabilityCatalog::try_from_definitions([ModelCapabilityDefinition::new(
            selection,
            ModelCapabilities::new(
                BTreeSet::from([ReasoningLevel::Low]),
                FastModeSupport::Unsupported,
                BTreeSet::new(),
            ),
        )])
        .expect("one settings capability forms a catalog");
    SubmitInputRepository::with_model_capabilities(pool.clone(), capabilities)
        .handle(
            start_input(
                0x37a6,
                0x37a1,
                "defaults-authenticated settings",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_input,
            Some(turn),
        )
        .await?;
    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert_eq!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );

    sqlx::query("ALTER TABLE turn_model_settings_resolved DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let update = sqlx::query(
        "UPDATE turn_model_settings_resolved
            SET resolved_model_settings = jsonb_set(
                    jsonb_set(
                        jsonb_set(
                            resolved_model_settings,
                            '{precedence,session,reasoning_level}',
                            '{\"kind\":\"inherit\"}'::jsonb
                        ),
                        '{precedence,profile,reasoning_level}',
                        '{\"kind\":\"value\",\"value\":\"low\"}'::jsonb
                    ),
                    '{reasoning_source}',
                    '\"profile\"'::jsonb
                )
          WHERE accepted_input_id = $1",
    )
    .bind(accepted_input.into_uuid())
    .execute(&pool)
    .await?;
    assert_eq!(update.rows_affected(), 1);
    sqlx::query("ALTER TABLE turn_model_settings_resolved ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    assert!(matches!(
        dispatcher
            .dispatch_next(|_| panic!("cross-wired defaults provenance must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidModelSettingsEvent
        ))
    ));
    assert!(matches!(
        ProcessReadRepository::new(pool.clone())
            .read_transcript(session)
            .await,
        Err(ProcessReadError::Corruption(
            ProcessReadCorruption::Inconsistent("turn model settings defaults")
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S37: the process defaults projection decodes the exact
/// self-contained settings document stored with the selected epoch.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s37_process_defaults_read_retains_model_settings_evidence() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x3751));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x3752));
    let precedence = ModelSettingsPrecedence::new(
        ModelSettingsOverlay::inherit_all(),
        ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::Low),
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        ),
        ModelSettingsOverlay::inherit_all(),
        ModelSettingsOverlay::inherit_all(),
    );
    let settings = ModelCapabilities::new(
        BTreeSet::from([ReasoningLevel::Low]),
        FastModeSupport::Unsupported,
        BTreeSet::new(),
    )
    .validate_precedence(selection, precedence)
    .expect("the fixture capability admits the session reasoning level");
    let defaults = SessionConfigurationDefaults::complete_with_model_settings(
        ModelSelectionRequest::Direct(selection),
        signalbox_domain::DangerousToolAutoApproval::Disabled,
        None,
        settings,
    )
    .expect("the fixture settings belong to the direct default");
    let creation = CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0x3753)),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        defaults.clone(),
    )
    .prepare(session)
    .expect("user-initiated creation without ancestry is preparable");
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation)
        .await?;

    let ProcessSessionDefaultsRead::Read(read) = ProcessReadRepository::new(pool.clone())
        .read_session_defaults(session, None)
        .await?
    else {
        panic!("the installed settings epoch must read");
    };

    assert_eq!(read.defaults(), &defaults);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S34: a session system prompt lives on the
/// immutable defaults epoch. Creation stores it, the loaded current session
/// and process defaults read return it, replacement installs a promptless
/// successor without rewriting the prompted epoch, replay preserves the exact
/// recorded payloads, and model-call preparation reads the prompt through the
/// calling turn's frozen epoch rather than the current pointer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s34_system_prompt_rides_the_frozen_defaults_epoch() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0xa41));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xa42));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xa43));
    let call = ModelCallId::from_uuid(Uuid::from_u128(0xa44));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0xa45));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(0xa46));
    let prompt = SessionSystemPrompt::try_new(String::from("exact session instructions"))
        .expect("test prompt is admissible");
    let prompted_defaults = SessionConfigurationDefaults::complete(
        ModelSelectionRequest::Direct(selection),
        signalbox_domain::DangerousToolAutoApproval::Disabled,
        Some(prompt.clone()),
    );
    let creation = CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xa47)),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        prompted_defaults.clone(),
    )
    .prepare(session)
    .expect("user-initiated creation without ancestry is preparable");
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    create_repository.handle(creation.clone()).await?;

    assert_eq!(
        create_repository.handle(creation.clone()).await?,
        CreateSessionHandlingOutcome::Applied(creation.applied_result())
    );
    let promptless_reuse = CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xa47)),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
    )
    .prepare(SessionId::from_uuid(Uuid::from_u128(0xa60)))
    .expect("user-initiated creation without ancestry is preparable");
    assert_eq!(
        create_repository.handle(promptless_reuse).await?,
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: creation.command().command_id(),
        }
    );

    let loaded = SessionRepository::new(pool.clone())
        .load_session(session)
        .await?
        .expect("the prompted session exists");
    assert_eq!(
        loaded.current_configuration_defaults().defaults(),
        &prompted_defaults
    );

    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0xa48,
                0xa41,
                "prompted-epoch request",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xa49)),
            Some(turn),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xa4a),
            starting_frontier: Uuid::from_u128(0xa4b),
            initial_attempt: attempt.into_uuid(),
        },
    )
    .await?;
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one prompted fixture target forms a catalog");
    let call_repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let checkpoint = call_repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xa4c)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xa4d)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xa4e)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xa4f)),
                    TurnId::from_uuid(Uuid::from_u128(0xa50)),
                )
            },
        )
        .await?;
    let PrepareInitialModelCallOutcome::Checkpointed(checkpointed) = checkpoint else {
        panic!("the prompted initial call must checkpoint");
    };
    assert_eq!(checkpointed, call);

    // Replace the defaults with a promptless successor before the prepared
    // call resumes: the call still binds the origin's frozen prompted epoch.
    let promptless_defaults =
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection));
    let defaults_repository = ReplaceSessionDefaultsRepository::new(pool.clone());

    // A caller whose protocol cannot state the prompt member is refused
    // atomically under the compare-and-set lock while the current epoch
    // carries a prompt, and nothing — not even the command identity — is
    // recorded.
    let unstated = ReplaceSessionDefaults::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xa59)),
        session,
        SessionConfigurationDefaultsVersion::try_from_u64(1).expect("positive version"),
        promptless_defaults.clone(),
    );
    assert_eq!(
        defaults_repository
            .handle_where_prompt_member(unstated, PromptMemberStatement::Unstated)
            .await?,
        ReplaceSessionDefaultsHandlingOutcome::PromptRequiresStatedMember
    );
    let unstated_claimed: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM durable_command WHERE command_id = $1)")
            .bind(Uuid::from_u128(0xa59))
            .fetch_one(&pool)
            .await?;
    assert!(!unstated_claimed);

    let replacement = ReplaceSessionDefaults::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xa51)),
        session,
        SessionConfigurationDefaultsVersion::try_from_u64(1).expect("positive version"),
        promptless_defaults.clone(),
    );
    let ReplaceSessionDefaultsHandlingOutcome::Applied(applied) =
        defaults_repository.handle(replacement).await?
    else {
        panic!("the promptless replacement must apply");
    };
    assert_eq!(applied.installed().defaults(), &promptless_defaults);
    let prompted_reuse = ReplaceSessionDefaults::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xa51)),
        session,
        SessionConfigurationDefaultsVersion::try_from_u64(1).expect("positive version"),
        prompted_defaults.clone(),
    );
    assert_eq!(
        defaults_repository.handle(prompted_reuse).await?,
        ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse {
            command_id: DurableCommandId::from_uuid(Uuid::from_u128(0xa51)),
        }
    );

    let PrepareInitialModelCallOutcome::Ready { system_prompt, .. } = call_repository
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::from_u128(0xa52)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xa53)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xa54)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xa55)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xa56)),
                    TurnId::from_uuid(Uuid::from_u128(0xa57)),
                )
            },
        )
        .await?
    else {
        panic!("the checkpointed prompted call must resume as ready");
    };
    assert_eq!(system_prompt.as_ref(), Some(&prompt));

    // The process defaults read selects the current promptless epoch, the
    // exact named prompted epoch, and types both absences.
    let read = ProcessReadRepository::new(pool.clone());
    let ProcessSessionDefaultsRead::Read(current) =
        read.read_session_defaults(session, None).await?
    else {
        panic!("the current defaults epoch must read");
    };
    assert_eq!(current.version(), applied.installed().version());
    assert_eq!(current.defaults(), &promptless_defaults);
    let ProcessSessionDefaultsRead::Read(named) = read
        .read_session_defaults(
            session,
            SessionConfigurationDefaultsVersion::try_from_u64(1),
        )
        .await?
    else {
        panic!("the named prompted epoch must read");
    };
    assert_eq!(
        named.version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(named.defaults(), &prompted_defaults);
    assert_eq!(
        read.read_session_defaults(
            session,
            SessionConfigurationDefaultsVersion::try_from_u64(9),
        )
        .await?,
        ProcessSessionDefaultsRead::VersionNotFound
    );
    assert_eq!(
        read.read_session_defaults(SessionId::from_uuid(Uuid::from_u128(0xa5f)), None)
            .await?,
        ProcessSessionDefaultsRead::SessionNotFound
    );

    // Schema bounds: an installed epoch's prompt column admits at most
    // 1,048,576 UTF-8 bytes and never empty text, and epochs stay immutable.
    let oversized = "y".repeat(1_048_577);
    let oversized_insert = sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind, direct_model_selection_id,
             model_alias_id, dangerous_tool_auto_approval, system_prompt)
         VALUES ($1, 99, 'direct', $2, NULL, 'disabled', $3)",
    )
    .bind(session.into_uuid())
    .bind(selection.into_uuid())
    .bind(&oversized)
    .execute(&pool)
    .await
    .expect_err("an over-bound stored prompt is rejected");
    assert_eq!(
        oversized_insert
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    let empty_insert = sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind, direct_model_selection_id,
             model_alias_id, dangerous_tool_auto_approval, system_prompt)
         VALUES ($1, 99, 'direct', $2, NULL, 'disabled', '')",
    )
    .bind(session.into_uuid())
    .bind(selection.into_uuid())
    .execute(&pool)
    .await
    .expect_err("an empty stored prompt is rejected");
    assert_eq!(
        empty_insert
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    let rewrite = sqlx::query(
        "UPDATE session_defaults_version
         SET system_prompt = 'rewritten'
         WHERE session_id = $1 AND version = 1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("defaults epochs are append-only");
    assert_eq!(
        rewrite
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    // Command/defaults agreement: an applied replacement receipt whose prompt
    // digest disagrees with the installed epoch cannot commit.
    let mut disagreeing = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'replace_session_defaults', 3, now(), 'operator')",
    )
    .bind(Uuid::from_u128(0xa58))
    .execute(&mut *disagreeing)
    .await?;
    sqlx::query(
        "INSERT INTO replace_session_defaults_command
            (command_id, command_kind, storage_version, session_id,
             expected_current_version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             dangerous_tool_auto_approval, system_prompt, result_kind,
             rejection_kind, result_session_id, result_installed_version,
             result_expected_version, result_current_version)
         VALUES ($1, 'replace_session_defaults', 3, $2, 1, 'direct', $3, NULL,
                 'disabled', 'digest disagreement', 'applied', NULL, $2, 2,
                 NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xa58))
    .bind(session.into_uuid())
    .bind(selection.into_uuid())
    .execute(&mut *disagreeing)
    .await?;
    let disagreement = disagreeing
        .commit()
        .await
        .expect_err("a prompt-digest disagreement cannot commit");
    let sqlx::Error::Database(disagreement_error) = &disagreement else {
        panic!("unexpected digest-disagreement failure: {disagreement:?}");
    };
    assert_eq!(disagreement_error.code().as_deref(), Some("23503"));

    // A session that lost its current pointer fails a named historical read
    // closed as corruption rather than serving the surviving epoch.
    sqlx::query(
        "ALTER TABLE session
         DROP CONSTRAINT session_current_defaults_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM session_current_defaults
         WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    let missing_pointer = read
        .read_session_defaults(
            session,
            SessionConfigurationDefaultsVersion::try_from_u64(1),
        )
        .await
        .expect_err("a named read must fail closed without a current pointer");
    let ProcessReadError::Corruption(ProcessReadCorruption::Missing(missing_field)) =
        missing_pointer
    else {
        panic!("the pointerless named read must be typed corruption");
    };
    assert_eq!(missing_field, "current defaults pointer");

    // A surviving pointer that names a missing epoch is equally corruption
    // for a named read of a different, existing epoch.
    sqlx::query(
        "ALTER TABLE session_current_defaults
         DROP CONSTRAINT session_current_defaults_version_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ($1, 77)",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    let dangling_pointer = read
        .read_session_defaults(
            session,
            SessionConfigurationDefaultsVersion::try_from_u64(1),
        )
        .await
        .expect_err("a named read must fail closed on a dangling current pointer");
    let ProcessReadError::Corruption(ProcessReadCorruption::Missing(dangling_field)) =
        dangling_pointer
    else {
        panic!("the dangling-pointer named read must be typed corruption");
    };
    assert_eq!(dangling_field, "current defaults epoch");

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S03 / S08: the operation counted before
/// activation is the exact no-steering Prepared call committed with that
/// activation; steering accepted afterward remains pending for a later call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_s03_s08_counted_activation_checkpoints_exact_call_before_steering()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0xcd01));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0xcd02));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(0xcd03));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            0xcd04,
            0xcd01,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    let origin = AcceptedInputId::from_uuid(Uuid::from_u128(0xcd05));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xcd06));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0xcd07,
                0xcd01,
                "counted origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            origin,
            Some(turn),
        )
        .await?;

    let activation = StartEligibleTurnRepository::new(pool.clone());
    let preview = activation
        .preview(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcd08)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcd09)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xcd0a)),
                TurnAttemptId::from_uuid(Uuid::from_u128(0xcd0b)),
            ),
        )
        .await?
        .expect("the queued origin has one exact activation preview");
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one fixture target forms a catalog");
    let model_calls =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let counted_call = ModelCallId::from_uuid(Uuid::from_u128(0xcd0c));
    let prospective = model_calls
        .preview_activation_operation(preview.prepared(), counted_call)
        .await?
        .expect("an admitted credential previews the activation operation");
    let counted_operation = prospective.render(Box::new([]))?;
    let counted_entries = counted_operation
        .request()
        .frontier_entries()
        .map(signalbox_domain::SemanticTranscriptEntry::reference)
        .collect::<Vec<_>>();
    let instruction_snapshot = signalbox_application::discover_workspace_instructions(Vec::new());
    let instruction_manifest = signalbox_domain::TurnInstructionManifest::empty_turn_start(
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0xcd15)),
        session,
        turn,
    );
    let no_instruction_bundles = [];
    let instruction_placement =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        )
        .observe_session_runner_placement(session)
        .await?;
    let instruction_evidence = CountedActivationInstructionEvidence::new(
        signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0xcd16)),
        &instruction_manifest,
        &instruction_snapshot,
        &no_instruction_bundles,
        &instruction_placement,
    );

    let committed = activation
        .commit_counted_preview(
            preview,
            prospective,
            &model_calls,
            Some(instruction_evidence),
        )
        .await?;
    let CommitActivationPreviewOutcome::Activated(activated) = committed else {
        panic!("the unchanged counted activation must commit");
    };
    assert_eq!(activated.turn(), turn);

    let steering = input_with_delivery(
        0xcd0d,
        0xcd01,
        "later steering",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: turn,
        },
    );
    let steering_outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            steering,
            AcceptedInputId::from_uuid(Uuid::from_u128(0xcd0e)),
            None,
        )
        .await?;
    assert!(matches!(
        steering_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));

    let ready = model_calls
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::from_u128(0xcd0f)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcd10)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xcd11)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xcd12)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcd13)),
                    TurnId::from_uuid(Uuid::from_u128(0xcd14)),
                )
            },
        )
        .await?;
    let PrepareInitialModelCallOutcome::Ready { request, .. } = ready else {
        panic!("the atomically checkpointed counted call must resume Prepared");
    };
    assert_eq!(request.call().id(), counted_call);
    let prepared_entries = request
        .frontier_entries()
        .map(signalbox_domain::SemanticTranscriptEntry::reference)
        .collect::<Vec<_>>();
    assert_eq!(prepared_entries, counted_entries);
    let pending_steering: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM accepted_input
          WHERE session_id = $1
            AND disposition_kind = 'pending_steering'",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(pending_steering, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// authoritative revalidation that rejects a stale counted preview
/// also rejects its prepared instruction evidence without retaining rows.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stale_counted_preview_retains_no_instruction_evidence() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0xcd20));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0xcd21));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(0xcd22));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            0xcd23,
            0xcd20,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    let turn = TurnId::from_uuid(Uuid::from_u128(0xcd24));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0xcd25,
                0xcd20,
                "stale counted origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xcd26)),
            Some(turn),
        )
        .await?;
    let activation = StartEligibleTurnRepository::new(pool.clone());
    let preview = activation
        .preview(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcd27)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcd28)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xcd29)),
                TurnAttemptId::from_uuid(Uuid::from_u128(0xcd2a)),
            ),
        )
        .await?
        .expect("the queued origin has one exact activation preview");
    insert_pending_compact_command(
        &pool,
        Uuid::from_u128(0xcd2e),
        session.into_uuid(),
        Uuid::from_u128(0xcd2f),
        Uuid::from_u128(0xcd30),
    )
    .await?;
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one fixture target forms a catalog");
    let model_calls =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let snapshot = signalbox_application::discover_workspace_instructions(Vec::new());
    let manifest = signalbox_domain::TurnInstructionManifest::empty_turn_start(
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0xcd2b)),
        session,
        turn,
    );
    let no_bundles = [];
    let placement =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        )
        .observe_session_runner_placement(session)
        .await?;
    let evidence = CountedActivationInstructionEvidence::new(
        signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0xcd2c)),
        &manifest,
        &snapshot,
        &no_bundles,
        &placement,
    );

    let prospective = model_calls
        .preview_activation_operation(
            preview.prepared(),
            ModelCallId::from_uuid(Uuid::from_u128(0xcd2d)),
        )
        .await?
        .expect("an admitted credential previews the activation operation");
    let stale = activation
        .commit_counted_preview(preview, prospective, &model_calls, Some(evidence))
        .await?;
    let discovery_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instruction_discovery WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    let manifest_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM turn_instruction_manifest WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(stale, CommitActivationPreviewOutcome::Stale);
    assert_eq!(discovery_rows, 0);
    assert_eq!(manifest_rows, 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03: deferred compaction evidence accepts successor ranges in
/// model-visible order when a predecessor compacts only its logical leading
/// summary and its retained suffix physically precedes that summary, while
/// reverse correlation rejects an orphan summary.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_context_compaction_constraints_use_projected_successor_order()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0xcc01));
    let session_uuid = session.into_uuid();
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0xcc02));
    let mut create_service = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );
    create_service
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0xcc03)),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
        )?)
        .await?;

    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0xcc04));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xcc05));
    let mut submit_service = SubmitInputService::new(
        FixedSubmitInputIds::new([accepted_input], [turn]),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    submit_service
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0xcc06)),
            session,
            UserContent::try_text("synthetic compaction source".to_owned())
                .expect("fixture user content is valid"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )?)
        .await?;

    let origin_entry = Uuid::from_u128(0xcc07);
    let initial_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xcc08));
    let mut activation_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(origin_entry)],
            [initial_frontier],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xcc09))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    activation_service.execute(session).await?;

    let retained_suffix = Uuid::from_u128(0xcc0a);
    let root_source = Uuid::from_u128(0xcc0b);
    let mut startup = StartupScanService::new(
        FixedStartupScanIds::new(
            [SemanticTranscriptEntryId::from_uuid(retained_suffix)],
            [ContextFrontierId::from_uuid(root_source)],
        ),
        PostgresStartupScanRepository::new(pool.clone()),
    );
    startup.execute().await?;

    let root_call = Uuid::from_u128(0xcc0c);
    let root_summary = Uuid::from_u128(0xcc0d);
    let root_result = Uuid::from_u128(0xcc0e);
    let root_compaction = Uuid::from_u128(0xcc0f);
    let target = Uuid::from_u128(0xcc10);
    let mut root_transaction = pool.begin().await?;
    insert_completed_context_compaction_call(
        &mut root_transaction,
        root_call,
        session_uuid,
        selection.into_uuid(),
        target,
        root_source,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             context_summary_value, context_summary_producing_call_id,
             context_summary_first_source_session_id,
             context_summary_first_entry_id,
             context_summary_through_source_session_id,
             context_summary_through_entry_id)
         VALUES ($1, $2, 'context_summary', 'synthetic root summary', $3,
                 $1, $4, $1, $4)",
    )
    .bind(session_uuid)
    .bind(root_summary)
    .bind(root_call)
    .bind(origin_entry)
    .execute(&mut *root_transaction)
    .await?;
    insert_frontier(
        &mut root_transaction,
        session_uuid,
        root_result,
        Decimal::from(3),
        &[
            (Decimal::ONE, session_uuid, origin_entry),
            (Decimal::from(2), session_uuid, retained_suffix),
            (Decimal::from(3), session_uuid, root_summary),
        ],
    )
    .await?;
    sqlx::query(
        "INSERT INTO context_compaction
            (context_compaction_id, session_id, predecessor_compaction_id,
             source_frontier_id, result_frontier_id, producing_call_id,
             first_source_session_id, first_entry_id,
             through_source_session_id, through_entry_id, summary_entry_id)
         VALUES ($1, $2, NULL, $3, $4, $5, $2, $6, $2, $6, $7)",
    )
    .bind(root_compaction)
    .bind(session_uuid)
    .bind(root_source)
    .bind(root_result)
    .bind(root_call)
    .bind(origin_entry)
    .bind(root_summary)
    .execute(&mut *root_transaction)
    .await?;
    root_transaction.commit().await?;

    let successor_call = Uuid::from_u128(0xcc11);
    let successor_summary = Uuid::from_u128(0xcc12);
    let successor_result = Uuid::from_u128(0xcc13);
    let successor_compaction = Uuid::from_u128(0xcc14);
    let mut successor_transaction = pool.begin().await?;
    insert_completed_context_compaction_call(
        &mut successor_transaction,
        successor_call,
        session_uuid,
        selection.into_uuid(),
        target,
        root_result,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             context_summary_value, context_summary_producing_call_id,
             context_summary_first_source_session_id,
             context_summary_first_entry_id,
             context_summary_through_source_session_id,
             context_summary_through_entry_id)
         VALUES ($1, $2, 'context_summary', 'synthetic successor summary', $3,
                 $1, $4, $1, $4)",
    )
    .bind(session_uuid)
    .bind(successor_summary)
    .bind(successor_call)
    .bind(root_summary)
    .execute(&mut *successor_transaction)
    .await?;
    insert_frontier(
        &mut successor_transaction,
        session_uuid,
        successor_result,
        Decimal::from(4),
        &[
            (Decimal::ONE, session_uuid, origin_entry),
            (Decimal::from(2), session_uuid, retained_suffix),
            (Decimal::from(3), session_uuid, root_summary),
            (Decimal::from(4), session_uuid, successor_summary),
        ],
    )
    .await?;
    sqlx::query(
        "INSERT INTO context_compaction
            (context_compaction_id, session_id, predecessor_compaction_id,
             source_frontier_id, result_frontier_id, producing_call_id,
             first_source_session_id, first_entry_id,
             through_source_session_id, through_entry_id, summary_entry_id)
         VALUES ($1, $2, $3, $4, $5, $6, $2, $7, $2, $8, $9)",
    )
    .bind(successor_compaction)
    .bind(session_uuid)
    .bind(root_compaction)
    .bind(root_result)
    .bind(successor_result)
    .bind(successor_call)
    .bind(root_summary)
    .bind(root_summary)
    .bind(successor_summary)
    .execute(&mut *successor_transaction)
    .await?;
    successor_transaction.commit().await?;

    let suffix_call = Uuid::from_u128(0xcc18);
    let suffix_summary = Uuid::from_u128(0xcc19);
    let suffix_result = Uuid::from_u128(0xcc1a);
    let suffix_compaction = Uuid::from_u128(0xcc1b);
    let mut suffix_transaction = pool.begin().await?;
    insert_completed_context_compaction_call(
        &mut suffix_transaction,
        suffix_call,
        session_uuid,
        selection.into_uuid(),
        target,
        successor_result,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             context_summary_value, context_summary_producing_call_id,
             context_summary_first_source_session_id,
             context_summary_first_entry_id,
             context_summary_through_source_session_id,
             context_summary_through_entry_id)
         VALUES ($1, $2, 'context_summary', 'synthetic suffix summary', $3,
                 $1, $4, $1, $5)",
    )
    .bind(session_uuid)
    .bind(suffix_summary)
    .bind(suffix_call)
    .bind(successor_summary)
    .bind(retained_suffix)
    .execute(&mut *suffix_transaction)
    .await?;
    insert_frontier(
        &mut suffix_transaction,
        session_uuid,
        suffix_result,
        Decimal::from(5),
        &[
            (Decimal::ONE, session_uuid, origin_entry),
            (Decimal::from(2), session_uuid, retained_suffix),
            (Decimal::from(3), session_uuid, root_summary),
            (Decimal::from(4), session_uuid, successor_summary),
            (Decimal::from(5), session_uuid, suffix_summary),
        ],
    )
    .await?;
    sqlx::query(
        "INSERT INTO context_compaction
            (context_compaction_id, session_id, predecessor_compaction_id,
             source_frontier_id, result_frontier_id, producing_call_id,
             first_source_session_id, first_entry_id,
             through_source_session_id, through_entry_id, summary_entry_id)
         VALUES ($1, $2, $3, $4, $5, $6, $2, $7, $2, $8, $9)",
    )
    .bind(suffix_compaction)
    .bind(session_uuid)
    .bind(successor_compaction)
    .bind(successor_result)
    .bind(suffix_result)
    .bind(suffix_call)
    .bind(successor_summary)
    .bind(retained_suffix)
    .bind(suffix_summary)
    .execute(&mut *suffix_transaction)
    .await?;
    suffix_transaction.commit().await?;

    let malformed_summary = Uuid::from_u128(0xcc17);
    let malformed_error = sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             context_summary_value, context_summary_producing_call_id,
             context_summary_first_source_session_id,
             context_summary_first_entry_id,
             context_summary_through_source_session_id,
             context_summary_through_entry_id,
             model_identity_defaults_version,
             model_identity_direct_selection_id)
         VALUES ($1, $2, 'context_summary', 'synthetic malformed summary', $3,
                 $1, $4, $1, $4, 1, $5)",
    )
    .bind(session_uuid)
    .bind(malformed_summary)
    .bind(successor_call)
    .bind(successor_summary)
    .bind(selection.into_uuid())
    .execute(&pool)
    .await
    .expect_err("summary payloads cannot carry model-identity fields");
    assert_eq!(
        malformed_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("semantic_transcript_entry_payload_shape")
    );

    let orphan_call = Uuid::from_u128(0xcc15);
    let orphan_summary = Uuid::from_u128(0xcc16);
    let mut orphan_transaction = pool.begin().await?;
    insert_completed_context_compaction_call(
        &mut orphan_transaction,
        orphan_call,
        session_uuid,
        selection.into_uuid(),
        target,
        successor_result,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             context_summary_value, context_summary_producing_call_id,
             context_summary_first_source_session_id,
             context_summary_first_entry_id,
             context_summary_through_source_session_id,
             context_summary_through_entry_id)
         VALUES ($1, $2, 'context_summary', 'synthetic orphan summary', $3,
                 $1, $4, $1, $4)",
    )
    .bind(session_uuid)
    .bind(orphan_summary)
    .bind(orphan_call)
    .bind(successor_summary)
    .execute(&mut *orphan_transaction)
    .await?;
    let orphan_error = orphan_transaction
        .commit()
        .await
        .expect_err("a summary without its exact compaction cannot commit");

    assert_eq!(
        orphan_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}
