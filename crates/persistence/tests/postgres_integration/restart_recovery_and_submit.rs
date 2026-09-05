//! Restart recovery of lost attempts, semantic entry correlation, and submit serialization.

use crate::*;

/// S08: pending-steering acceptance and source terminalization
/// serialize on the source lifecycle row, so racing commits cannot both
/// succeed from snapshots in which the reciprocal effect is not yet visible.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pending_steering_and_source_terminalization_serialize() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x471, 0x871, direct(0xc71)))
        .await?;
    let active_origin_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x971));
    let active_origin_turn = TurnId::from_uuid(Uuid::from_u128(0xa71));
    let repository = SubmitInputRepository::new(pool.clone());
    repository
        .handle(
            start_input(
                0x472,
                0x871,
                "active source",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            active_origin_input,
            Some(active_origin_turn),
        )
        .await?;
    let activated = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: Uuid::from_u128(0x871),
            origin_entry: Uuid::from_u128(0xd71),
            starting_frontier: Uuid::from_u128(0xe71),
            initial_attempt: Uuid::from_u128(0xb71),
        },
    )
    .await?;
    assert_eq!(activated.accepted_input().id(), active_origin_input);
    assert_eq!(activated.turn(), active_origin_turn);

    let mut terminalize_source = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(Uuid::from_u128(0x871))
    .bind(Uuid::from_u128(0xd72))
    .bind(Uuid::from_u128(0xa71))
    .execute(&mut *terminalize_source)
    .await?;
    insert_frontier(
        &mut terminalize_source,
        Uuid::from_u128(0x871),
        Uuid::from_u128(0xe72),
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, Uuid::from_u128(0x871), Uuid::from_u128(0xd71)),
            (
                Decimal::from(2_u64),
                Uuid::from_u128(0x871),
                Uuid::from_u128(0xd72),
            ),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1",
    )
    .bind(Uuid::from_u128(0xb71))
    .execute(&mut *terminalize_source)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                active_phase_kind = NULL,
                terminal_attempt_id = current_attempt_id,
                current_attempt_id = NULL,
                terminal_frontier_id = $1,
                terminal_disposition_kind = 'failed',
                terminal_cause_kind = 'model_call_failed'
          WHERE turn_id = $2",
    )
    .bind(Uuid::from_u128(0xe72))
    .bind(Uuid::from_u128(0xa71))
    .execute(&mut *terminalize_source)
    .await?;

    let pending_acceptance = tokio::spawn(async move {
        repository
            .handle(
                input_with_delivery(
                    0x473,
                    0x871,
                    "racing steering",
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa71)),
                    },
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x972)),
                None,
            )
            .await
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "pending acceptance must remain blocked on the source lifecycle row"
    );

    terminalize_source.commit().await?;
    let pending_database_error = submit_input_database_error(
        pending_acceptance
            .await?
            .expect_err("steering must fail after racing source terminalization commits"),
    );
    assert_eq!(
        pending_database_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    assert_eq!(
        pending_database_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("accepted_input_pending_source_active")
    );

    let durable_effects: (i64, i64, String) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command WHERE command_id = $1),
            (SELECT count(*) FROM accepted_input WHERE accepted_input_id = $2),
            (SELECT state_kind FROM turn_lifecycle WHERE turn_id = $3)",
    )
    .bind(Uuid::from_u128(0x473))
    .bind(Uuid::from_u128(0x972))
    .bind(Uuid::from_u128(0xa71))
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_effects, (0, 0, "terminal".to_owned()));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / S04: after a real pool restart, startup atomically
/// ends the prior-process attempt as Lost, retains it as attempt-only terminal
/// provenance, appends `TurnFailed`, terminalizes Failed, remains idempotent on
/// replay, and exposes the queued successor to the ordinary scheduler path.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_s04_restart_scan_recovers_lost_attempt_once_and_unblocks_successor()
-> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let session_uuid = Uuid::from_u128(0x7b1);
    let first_turn_uuid = Uuid::from_u128(0xab1);
    let second_turn_uuid = Uuid::from_u128(0xab2);
    let attempt_uuid = Uuid::from_u128(0xbb1);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x3b0, 0x7b1, direct(0x8b1)))
        .await?;
    let inputs = SubmitInputRepository::new(pool.clone());
    inputs
        .handle(
            start_input(
                0x3b1,
                0x7b1,
                "prior process",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9b1)),
            Some(TurnId::from_uuid(first_turn_uuid)),
        )
        .await?;
    inputs
        .handle(
            start_input(
                0x3b2,
                0x7b1,
                "queued successor",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9b2)),
            Some(TurnId::from_uuid(second_turn_uuid)),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session_uuid,
            origin_entry: Uuid::from_u128(0xcb1),
            starting_frontier: Uuid::from_u128(0xdb1),
            initial_attempt: attempt_uuid,
        },
    )
    .await?;

    // Restart boundary: the active attempt exists durably, but its creating
    // process and every connection it owned are gone.
    drop(inputs);
    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let failure_entry_uuid = Uuid::from_u128(0xeb1);
    let terminal_frontier_uuid = Uuid::from_u128(0xfb1);
    let mut scan = StartupScanService::new(
        FixedStartupScanIds::new(
            [SemanticTranscriptEntryId::from_uuid(failure_entry_uuid)],
            [ContextFrontierId::from_uuid(terminal_frontier_uuid)],
        ),
        PostgresStartupScanRepository::new(restarted_pool.clone()),
    );

    let first = scan.execute().await?;
    assert_eq!(first.recovered_turn_count(), 1);

    let recovered: (
        String,
        String,
        String,
        String,
        String,
        Option<Uuid>,
        Uuid,
        Option<Uuid>,
    ) = sqlx::query_as(
        "SELECT attempt.state_kind,
                attempt.end_variant,
                attempt.end_disposition,
                turn.state_kind,
                turn.terminal_disposition_kind,
                turn.current_attempt_id,
                turn.terminal_attempt_id,
                turn.terminal_model_call_id
           FROM turn_attempt AS attempt
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = attempt.turn_id
            AND turn.session_id = attempt.session_id
          WHERE attempt.turn_attempt_id = $1",
    )
    .bind(attempt_uuid)
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        recovered,
        (
            "ended".into(),
            "without_stop".into(),
            "lost".into(),
            "terminal".into(),
            "failed".into(),
            None,
            attempt_uuid,
            None,
        )
    );
    let terminal_entries = sqlx::query_scalar::<_, String>(
        "SELECT entry.payload_kind
           FROM context_frontier_member AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
          WHERE member.owning_session_id = $1
            AND member.context_frontier_id = $2
          ORDER BY member.member_position",
    )
    .bind(session_uuid)
    .bind(terminal_frontier_uuid)
    .fetch_all(&restarted_pool)
    .await?;
    assert_eq!(terminal_entries, ["origin_accepted_input", "turn_failed"]);
    let recovery_events: Vec<(String, i16, Uuid, Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT header.event_kind,
                header.storage_version,
                header.session_id,
                failed.turn_id,
                failed.failure_entry_id,
                failed.terminal_frontier_id
           FROM outbox_event AS header
           JOIN turn_terminal_outbox_event AS failed
             ON failed.disposition_kind = 'failed'
             AND failed.event_sequence = header.event_sequence
          ORDER BY header.event_sequence",
    )
    .fetch_all(&restarted_pool)
    .await?;
    assert_eq!(
        recovery_events,
        vec![(
            "turn_terminal".into(),
            1,
            session_uuid,
            first_turn_uuid,
            failure_entry_uuid,
            terminal_frontier_uuid,
        )]
    );
    let committed_counts_before_replay: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE payload_kind = 'turn_failed' AND failed_turn_id = $1),
            (SELECT count(*) FROM context_frontier
              WHERE owning_session_id = $2),
            (SELECT count(*) FROM turn_attempt
              WHERE turn_id = $1),
            (SELECT count(*) FROM outbox_event
              WHERE event_kind = 'turn_terminal' AND turn_disposition = 'failed'
                AND session_id = $2),
            (SELECT count(*) FROM turn_terminal_outbox_event
              WHERE disposition_kind = 'failed'
              AND turn_id = $1)",
    )
    .bind(first_turn_uuid)
    .bind(session_uuid)
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(committed_counts_before_replay, (1, 2, 1, 1, 1));

    let replay = scan.execute().await?;
    assert_eq!(replay.recovered_turn_count(), 0);
    let committed_counts_after_replay: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE payload_kind = 'turn_failed' AND failed_turn_id = $1),
            (SELECT count(*) FROM context_frontier
              WHERE owning_session_id = $2),
            (SELECT count(*) FROM turn_attempt
              WHERE turn_id = $1),
            (SELECT count(*) FROM outbox_event
              WHERE event_kind = 'turn_terminal' AND turn_disposition = 'failed'
                AND session_id = $2),
            (SELECT count(*) FROM turn_terminal_outbox_event
              WHERE disposition_kind = 'failed'
              AND turn_id = $1)",
    )
    .bind(first_turn_uuid)
    .bind(session_uuid)
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        committed_counts_after_replay,
        committed_counts_before_replay
    );

    let (eligible_sessions, _dispatch_starts, continuation) =
        PostgresEligibilitySweep::new(restarted_pool.clone())
            .find_sessions()
            .await?
            .into_parts();
    assert!(!continuation);
    assert_eq!(eligible_sessions, vec![SessionId::from_uuid(session_uuid)]);
    let activated = activate_earliest_queued_turn(
        &restarted_pool,
        EarliestQueuedTurnActivation {
            session: session_uuid,
            origin_entry: Uuid::from_u128(0xcb2),
            starting_frontier: Uuid::from_u128(0xdb2),
            initial_attempt: Uuid::from_u128(0xbb2),
        },
    )
    .await?;
    assert_eq!(activated.turn(), TurnId::from_uuid(second_turn_uuid));
    assert_eq!(
        activated.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: TurnId::from_uuid(first_turn_uuid),
        }
    );

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S03: failure after the typed outbox append rolls the
/// complete Lost recovery back; retry then commits the state and event once.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_startup_recovery_and_outbox_commit_or_roll_back_together() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session_uuid = Uuid::from_u128(0x7d1);
    let turn_uuid = Uuid::from_u128(0xad1);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x3d0, 0x7d1, direct(0x8d1)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x3d1,
                0x7d1,
                "active before failed recovery",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9d1)),
            Some(TurnId::from_uuid(turn_uuid)),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session_uuid,
            origin_entry: Uuid::from_u128(0xcd1),
            starting_frontier: Uuid::from_u128(0xdd1),
            initial_attempt: Uuid::from_u128(0xbd1),
        },
    )
    .await?;
    sqlx::query(
        "CREATE FUNCTION fail_test_turn_failed_outbox_commit()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             RAISE EXCEPTION 'injected failure after recovery outbox append'
                 USING ERRCODE = '40001';
         END;
         $$",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE CONSTRAINT TRIGGER zz_test_fail_turn_failed_outbox_commit
         AFTER INSERT ON turn_terminal_outbox_event
         DEFERRABLE INITIALLY DEFERRED
         FOR EACH ROW
         EXECUTE FUNCTION fail_test_turn_failed_outbox_commit()",
    )
    .execute(&pool)
    .await?;

    let failure_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xed1));
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xfd1));
    let mut failing_scan = StartupScanService::new(
        FixedStartupScanIds::new([failure_entry], [terminal_frontier]),
        PostgresStartupScanRepository::new(pool.clone()),
    );
    failing_scan
        .execute()
        .await
        .expect_err("the deferred outbox fixture must abort recovery commit");

    let rolled_back: (String, String, i64, i64, Decimal) = sqlx::query_as(
        "SELECT turn.state_kind,
                attempt.state_kind,
                (SELECT count(*) FROM semantic_transcript_entry
                  WHERE failed_turn_id = $1),
                (SELECT count(*) FROM turn_terminal_outbox_event
                  WHERE disposition_kind = 'failed'
                  AND turn_id = $1),
                (SELECT last_sequence FROM outbox_sequence_state
                  WHERE singleton)
           FROM turn_lifecycle AS turn
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = turn.current_attempt_id
          WHERE turn.turn_id = $1",
    )
    .bind(turn_uuid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        rolled_back,
        ("active".into(), "prepared".into(), 0, 0, Decimal::from(5))
    );

    sqlx::query(
        "DROP TRIGGER zz_test_fail_turn_failed_outbox_commit
            ON turn_terminal_outbox_event",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DROP FUNCTION fail_test_turn_failed_outbox_commit()")
        .execute(&pool)
        .await?;

    let mut retry_scan = StartupScanService::new(
        FixedStartupScanIds::new([failure_entry], [terminal_frontier]),
        PostgresStartupScanRepository::new(pool.clone()),
    );
    assert_eq!(retry_scan.execute().await?.recovered_turn_count(), 1);
    let committed: (String, String, i64, i64, Decimal) = sqlx::query_as(
        "SELECT turn.state_kind,
                attempt.state_kind,
                (SELECT count(*) FROM semantic_transcript_entry
                  WHERE failed_turn_id = $1),
                (SELECT count(*) FROM turn_terminal_outbox_event
                  WHERE disposition_kind = 'failed'
                  AND turn_id = $1),
                (SELECT last_sequence FROM outbox_sequence_state
                  WHERE singleton)
           FROM turn_lifecycle AS turn
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = $2
          WHERE turn.turn_id = $1",
    )
    .bind(turn_uuid)
    .bind(Uuid::from_u128(0xbd1))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        committed,
        ("terminal".into(), "ended".into(), 1, 1, Decimal::from(6))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S08 / S09: evidence-free restart recovery
/// ends the abandoned source attempt and atomically reclassifies pending
/// steering, leaving no startup blocker on replay.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s08_s09_restart_reclassifies_pending_steering() -> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let session_uuid = Uuid::from_u128(0x7c1);
    let turn_uuid = Uuid::from_u128(0xac1);
    let attempt_uuid = Uuid::from_u128(0xbc1);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x3c0, 0x7c1, direct(0x8c1)))
        .await?;
    let inputs = SubmitInputRepository::new(pool.clone());
    inputs
        .handle(
            start_input(
                0x3c1,
                0x7c1,
                "active source",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9c1)),
            Some(TurnId::from_uuid(turn_uuid)),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session_uuid,
            origin_entry: Uuid::from_u128(0xcc1),
            starting_frontier: Uuid::from_u128(0xdc1),
            initial_attempt: attempt_uuid,
        },
    )
    .await?;
    let pending_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9c2));
    let pending = inputs
        .handle(
            input_with_delivery(
                0x3c2,
                0x7c1,
                "steer later",
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: TurnId::from_uuid(turn_uuid),
                },
            ),
            pending_input,
            None,
        )
        .await?;
    assert!(matches!(
        pending,
        signalbox_persistence::submit_input::SubmitInputHandlingOutcome::Recorded(
            SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
        )
    ));

    drop(inputs);
    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let mut scan = StartupScanService::new(
        FixedStartupScanIds::new(
            [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xec1)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xec2)),
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(0xfc1)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xfc2)),
            ],
        )
        .with_reclassified_turns([TurnId::from_uuid(Uuid::from_u128(0xac2))]),
        PostgresStartupScanRepository::new(restarted_pool.clone()),
    );

    let first = scan.execute().await?;
    assert_eq!(first.recovered_turn_count(), 1);
    let replay = scan.execute().await?;
    assert_eq!(replay.recovered_turn_count(), 0);

    let recovery_events: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM outbox_event
              WHERE event_kind = 'turn_terminal' AND turn_disposition = 'failed'
                AND session_id = $1),
            (SELECT count(*) FROM turn_terminal_outbox_event
              WHERE disposition_kind = 'failed'
              AND session_id = $1)",
    )
    .bind(session_uuid)
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(recovery_events, (1, 1));

    let recovered: (String, String, i64, i64, String, Uuid, String) = sqlx::query_as(
        "SELECT turn.state_kind,
                attempt.state_kind,
                (SELECT count(*) FROM semantic_transcript_entry
                  WHERE payload_kind = 'turn_failed' AND failed_turn_id = $1),
                (SELECT count(*) FROM context_frontier
                  WHERE owning_session_id = $2),
                accepted.disposition_kind,
                accepted.origin_turn_id,
                successor.state_kind
           FROM turn_lifecycle AS turn
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = $4
            JOIN accepted_input AS accepted
              ON accepted.accepted_input_id = $3
            JOIN turn_lifecycle AS successor
              ON successor.turn_id = accepted.origin_turn_id
          WHERE turn.turn_id = $1",
    )
    .bind(turn_uuid)
    .bind(session_uuid)
    .bind(pending_input.into_uuid())
    .bind(attempt_uuid)
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        recovered,
        (
            "terminal".into(),
            "ended".into(),
            1,
            2,
            "reclassified_as_turn_origin".into(),
            Uuid::from_u128(0xac2),
            "queued".into(),
        )
    );
    let receipt: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT outcome_kind, delivered_turn_id
           FROM injection_settled_outbox_event
          WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x3c2))
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        receipt,
        (String::from("delivered"), Some(Uuid::from_u128(0xac2))),
        "reclassification at the restart boundary settles the steering delivered"
    );
    let mut completed_recovery_ids = FixedStartupScanIds::new([], []);
    assert_eq!(
        PostgresStartupScanRepository::new(restarted_pool.clone())
            .recover(
                SessionId::from_uuid(session_uuid),
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xec3)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(0xfc3)),
                ),
                &mut completed_recovery_ids,
            )
            .await?,
        StartupScanSessionOutcome::NoActiveTurn
    );

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S03 / S07 / S08 / S09 /
/// occupied-slot rejection evidence is recorded exactly, generated
/// identities cannot reuse the active origin, and a matching interrupt
/// atomically cancels prepared work while recording and prioritizing its exact
/// immediate successor ahead of previously queued ordinary work.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_s07_prepared_interrupt_is_exact() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let prepared_session = prepared(0x441, 0x841, direct(0xc41));
    let session = prepared_session.session().id();
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared_session)
        .await?;
    let active_origin_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x941));
    let active_origin_turn = TurnId::from_uuid(Uuid::from_u128(0xa41));
    let repository = SubmitInputRepository::new(pool.clone());
    repository
        .handle(
            start_input(
                0x442,
                0x841,
                "active origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            active_origin_input,
            Some(active_origin_turn),
        )
        .await?;
    let activated = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xd41),
            starting_frontier: Uuid::from_u128(0xe41),
            initial_attempt: Uuid::from_u128(0xb41),
        },
    )
    .await?;
    assert_eq!(activated.accepted_input().id(), active_origin_input);
    assert_eq!(activated.turn(), active_origin_turn);

    let active_start = start_input(
        0x443,
        0x841,
        "cannot start",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let active_start_outcome = repository
        .handle(
            active_start.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x942)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa42))),
        )
        .await?;
    assert_eq!(
        active_start_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::ActiveTurnPresent {
                session,
                active_turn: active_origin_turn,
            },
        ))
    );

    let stale_expected_turn = TurnId::from_uuid(Uuid::from_u128(0xaff));
    let stale_after = record_stale_active_input(
        &repository,
        0x444,
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: stale_expected_turn,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
        0x943,
        Some(0xa43),
    )
    .await?;
    let stale_safe_point = record_stale_active_input(
        &repository,
        0x445,
        DeliveryRequest::NextSafePoint {
            expected_active_turn: stale_expected_turn,
        },
        0x944,
        None,
    )
    .await?;
    let stale_interrupt = record_stale_active_input(
        &repository,
        0x446,
        DeliveryRequest::Interrupt {
            expected_active_turn: stale_expected_turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
        0x945,
        Some(0xa45),
    )
    .await?;
    let stale_expected = SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::ActiveTurnMismatch {
            session,
            expected_active_turn: stale_expected_turn,
            actual_active_turn: active_origin_turn,
        },
    ));
    assert_eq!(stale_after.1, stale_expected);
    assert_eq!(stale_safe_point.1, stale_expected);
    assert_eq!(stale_interrupt.1, stale_expected);

    let after_collision_command = DurableCommandId::from_uuid(Uuid::from_u128(0x449));
    let after_collision = active_origin_collision(
        &repository,
        &pool,
        after_collision_command,
        session,
        active_origin_input,
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: active_origin_turn,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
        Some(0xa49),
    )
    .await?;
    let safe_point_collision_command = DurableCommandId::from_uuid(Uuid::from_u128(0x44a));
    let safe_point_collision = active_origin_collision(
        &repository,
        &pool,
        safe_point_collision_command,
        session,
        active_origin_input,
        DeliveryRequest::NextSafePoint {
            expected_active_turn: active_origin_turn,
        },
        None,
    )
    .await?;
    let SubmitInputRepositoryError::AcceptedInputIdentityCollision {
        command_id,
        active_turn,
        accepted_input,
    } = after_collision.0
    else {
        panic!("after-current collision retains exact authority")
    };
    assert_eq!(command_id, after_collision_command);
    assert_eq!(active_turn, active_origin_turn);
    assert_eq!(accepted_input, active_origin_input);
    assert_eq!(after_collision.1, 0);
    let SubmitInputRepositoryError::AcceptedInputIdentityCollision {
        command_id,
        active_turn,
        accepted_input,
    } = safe_point_collision.0
    else {
        panic!("safe-point collision retains exact authority")
    };
    assert_eq!(command_id, safe_point_collision_command);
    assert_eq!(active_turn, active_origin_turn);
    assert_eq!(accepted_input, active_origin_input);
    assert_eq!(safe_point_collision.1, 0);

    let queued_before_interrupt = repository
        .handle(
            input_with_delivery(
                0x44b,
                0x841,
                "ordinary queued before interrupt",
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: active_origin_turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x948)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa48))),
        )
        .await?;
    assert!(matches!(
        queued_before_interrupt,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    let pending_before_interrupt = repository
        .handle(
            input_with_delivery(
                0x44c,
                0x841,
                "pending steering before interrupt",
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active_origin_turn,
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x949)),
            None,
        )
        .await?;
    assert!(matches!(
        pending_before_interrupt,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));

    let matching_interrupt = input_with_delivery(
        0x447,
        0x841,
        "matching interrupt",
        DeliveryRequest::Interrupt {
            expected_active_turn: active_origin_turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let matching_successor_turn = TurnId::from_uuid(Uuid::from_u128(0xa46));
    let outcome = repository
        .handle(
            matching_interrupt.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x946)),
            Some(matching_successor_turn),
        )
        .await
        .expect("matching interrupt applies atomically");
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(applied),
    )) = &outcome
    else {
        panic!("matching interrupt records its successor origin")
    };
    assert_eq!(applied.turn(), matching_successor_turn);
    assert_eq!(
        applied
            .applied_interrupt()
            .expect("matching interrupt retains proof")
            .proof()
            .predecessor(),
        active_origin_turn
    );
    let claimed: (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command WHERE command_id = $1),
            (SELECT count(*) FROM submit_input_command WHERE command_id = $1),
            (SELECT count(*) FROM accepted_input WHERE accepting_command_id = $1),
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE origin_accepted_input_id = $2),
            (SELECT count(*)
               FROM queued_input_origin
              WHERE accepted_input_id = $2
                AND priority_kind = 'interrupt_immediately_after'
                AND interrupt_predecessor_turn_id = $3),
            (SELECT count(*)
               FROM turn_attempt
              WHERE turn_id = $3
                AND state_kind = 'ended'
                AND end_variant = 'after_cancellation'
                AND end_disposition = 'cancelled'
                AND interrupt_command_id = $1
                AND interrupt_predecessor_turn_id = $3),
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE turn_id = $3
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'cancelled')",
    )
    .bind(Uuid::from_u128(0x447))
    .bind(Uuid::from_u128(0x946))
    .bind(Uuid::from_u128(0xa41))
    .fetch_one(&pool)
    .await?;
    assert_eq!(claimed, (1, 1, 1, 1, 1, 1, 1));

    let next = input_with_delivery(
        0x448,
        0x841,
        "safe point after direct cancellation",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: active_origin_turn,
        },
    );
    let next_outcome = repository
        .handle(
            next.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x947)),
            None,
        )
        .await?;
    assert_eq!(
        next_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::NoActiveTurn {
                session,
                expected_active_turn: active_origin_turn,
            },
        ))
    );
    assert_eq!(
        repository
            .handle(
                matching_interrupt,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fd)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xafd))),
            )
            .await?,
        outcome
    );
    assert_eq!(
        repository
            .handle(
                next,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fc)),
                None,
            )
            .await?,
        next_outcome
    );

    let evidence: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            count(*) FILTER (
                WHERE rejection_kind = 'active_turn_present'
                  AND result_actual_active_turn_id = $1
            ),
            count(*) FILTER (
                WHERE rejection_kind = 'active_turn_mismatch'
                  AND result_expected_active_turn_id = $2
                  AND result_actual_active_turn_id = $1
            ),
            count(*) FILTER (
                WHERE rejection_kind IN (
                    'active_turn_present',
                    'active_turn_mismatch'
                )
                  AND result_accepted_input_id IS NULL
                  AND result_turn_id IS NULL
            )
          FROM submit_input_command
         WHERE command_id BETWEEN $3 AND $4",
    )
    .bind(Uuid::from_u128(0xa41))
    .bind(Uuid::from_u128(0xaff))
    .bind(Uuid::from_u128(0x443))
    .bind(Uuid::from_u128(0x446))
    .fetch_one(&pool)
    .await?;
    assert_eq!(evidence, (1, 3, 4));

    assert_eq!(
        repository
            .handle(
                active_start,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9ff)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xaff))),
            )
            .await?,
        active_start_outcome
    );
    assert_eq!(
        repository
            .handle(
                stale_after.0,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fe)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xafe))),
            )
            .await?,
        stale_after.1
    );
    assert_eq!(
        repository
            .handle(
                stale_safe_point.0,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fe)),
                None,
            )
            .await?,
        stale_safe_point.1
    );
    assert_eq!(
        repository
            .handle(
                stale_interrupt.0,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fe)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xafe))),
            )
            .await?,
        stale_interrupt.1
    );

    let interrupt_successor = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xd46),
            starting_frontier: Uuid::from_u128(0xe46),
            initial_attempt: Uuid::from_u128(0xb46),
        },
    )
    .await?;
    assert_eq!(interrupt_successor.turn(), matching_successor_turn);
    assert_eq!(
        interrupt_successor.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: active_origin_turn,
        }
    );
    let remaining_queue: (String, i64) = sqlx::query_as(
        "SELECT
            (SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1),
            (SELECT count(*)
               FROM accepted_input AS accepted
               JOIN turn_lifecycle AS lifecycle
                 ON lifecycle.turn_id = accepted.origin_turn_id
              WHERE accepted.accepted_input_id = $2
                AND accepted.disposition_kind = 'reclassified_as_turn_origin'
                AND lifecycle.state_kind = 'queued')",
    )
    .bind(Uuid::from_u128(0xa48))
    .bind(Uuid::from_u128(0x949))
    .fetch_one(&pool)
    .await?;
    assert_eq!(remaining_queue, ("queued".to_owned(), 1));

    let mut interrupted_recovery_ids = FixedStartupScanIds::new([], []);
    assert!(matches!(
        PostgresStartupScanRepository::new(pool.clone())
            .recover(
                session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd47)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(0xe47)),
                ),
                &mut interrupted_recovery_ids,
            )
            .await?,
        StartupScanSessionOutcome::Recovered { .. }
    ));
    let ordinary_successor = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xd48),
            starting_frontier: Uuid::from_u128(0xe48),
            initial_attempt: Uuid::from_u128(0xb48),
        },
    )
    .await?;
    assert_eq!(
        ordinary_successor.turn(),
        TurnId::from_uuid(Uuid::from_u128(0xa48))
    );
    assert_eq!(
        ordinary_successor.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: matching_successor_turn,
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// an incomplete frontier cannot expose any
/// semantic entry, start binding, slot owner, or attempt after rollback.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn malformed_atomic_start_rolls_back_every_fact() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x411, 0x811, direct(0xc11)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x412,
                0x811,
                "malformed future start",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x911)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa11))),
        )
        .await?;

    let session = Uuid::from_u128(0x811);
    let turn = Uuid::from_u128(0xa11);
    let mut malformed = pool.begin().await?;
    insert_origin_frontier(
        &mut malformed,
        session,
        Uuid::from_u128(0x911),
        Uuid::from_u128(0xd11),
        Uuid::from_u128(0xe11),
        Decimal::from(2_u64),
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb11))
    .bind(turn)
    .bind(session)
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                active_phase_kind = 'running',
                current_attempt_id = $2
          WHERE turn_id = $3",
    )
    .bind(Uuid::from_u128(0xe11))
    .bind(Uuid::from_u128(0xb11))
    .bind(turn)
    .execute(&mut *malformed)
    .await?;
    let incomplete = malformed
        .commit()
        .await
        .expect_err("a gapped one-member frontier must not commit");
    assert_eq!(
        incomplete
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let unchanged: (String, i64, i64, i64) = sqlx::query_as(
        "SELECT
            state_kind,
            (SELECT count(*) FROM semantic_transcript_entry),
            (SELECT count(*) FROM context_frontier),
            (SELECT count(*) FROM turn_attempt)
         FROM turn_lifecycle
         WHERE turn_id = $1",
    )
    .bind(turn)
    .fetch_one(&pool)
    .await?;
    assert_eq!(unchanged, ("queued".to_owned(), 0, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// the initial semantic variants
/// preserve globally unique identities and exact source correlations; eligible
/// failure records origin then failure without putting the later failure
/// marker in the starting frontier.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn initial_semantic_entries_are_turn_correlated() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x421, 0x821, direct(0xc21)))
        .await?;
    let submit = SubmitInputRepository::new(pool.clone());
    submit
        .handle(
            start_input(
                0x422,
                0x821,
                "will fail eligibility",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x921)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa21))),
        )
        .await?;

    let session = Uuid::from_u128(0x821);
    let turn = Uuid::from_u128(0xa21);
    let origin_entry = Uuid::from_u128(0xd21);
    let failure_entry = Uuid::from_u128(0xd22);
    let starting_frontier = Uuid::from_u128(0xe21);
    let terminal_frontier = Uuid::from_u128(0xe22);

    let mut missing_terminal_frontier = pool.begin().await?;
    insert_origin_frontier(
        &mut missing_terminal_frontier,
        session,
        Uuid::from_u128(0x921),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *missing_terminal_frontier)
    .await?;
    let missing_terminal_frontier_error = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_disposition_kind = 'failed',
                terminal_cause_kind = 'model_call_failed'
          WHERE turn_id = $2",
    )
    .bind(starting_frontier)
    .bind(turn)
    .execute(&mut *missing_terminal_frontier)
    .await
    .expect_err("a failed terminal turn must name its terminal frontier");
    assert_eq!(
        missing_terminal_frontier_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_state_payload_shape")
    );
    missing_terminal_frontier.rollback().await?;

    let mut gapped_terminal_frontier = pool.begin().await?;
    insert_origin_frontier(
        &mut gapped_terminal_frontier,
        session,
        Uuid::from_u128(0x921),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *gapped_terminal_frontier)
    .await?;
    insert_frontier(
        &mut gapped_terminal_frontier,
        session,
        terminal_frontier,
        Decimal::from(3_u64),
        &[
            (Decimal::ONE, session, origin_entry),
            (Decimal::from(3_u64), session, failure_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_frontier_id = $2,
                terminal_disposition_kind = 'failed',
                terminal_cause_kind = 'model_call_failed'
          WHERE turn_id = $3",
    )
    .bind(starting_frontier)
    .bind(terminal_frontier)
    .bind(turn)
    .execute(&mut *gapped_terminal_frontier)
    .await?;
    let gapped = gapped_terminal_frontier
        .commit()
        .await
        .expect_err("a terminal frontier with a membership gap must not commit");
    assert_eq!(
        gapped.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );

    let mut cross_wired_terminal_frontier = pool.begin().await?;
    insert_origin_frontier(
        &mut cross_wired_terminal_frontier,
        session,
        Uuid::from_u128(0x921),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *cross_wired_terminal_frontier)
    .await?;
    insert_frontier(
        &mut cross_wired_terminal_frontier,
        session,
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session, failure_entry),
            (Decimal::from(2_u64), session, origin_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_frontier_id = $2,
                terminal_disposition_kind = 'failed',
                terminal_cause_kind = 'model_call_failed'
          WHERE turn_id = $3",
    )
    .bind(starting_frontier)
    .bind(terminal_frontier)
    .bind(turn)
    .execute(&mut *cross_wired_terminal_frontier)
    .await?;
    let cross_wired = cross_wired_terminal_frontier
        .commit()
        .await
        .expect_err("a reordered terminal frontier must not commit");
    assert_eq!(
        cross_wired
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let mut attempted_failure = pool.begin().await?;
    insert_origin_frontier(
        &mut attempted_failure,
        session,
        Uuid::from_u128(0x921),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *attempted_failure)
    .await?;
    insert_frontier(
        &mut attempted_failure,
        session,
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session, origin_entry),
            (Decimal::from(2_u64), session, failure_entry),
        ],
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb21))
    .bind(turn)
    .bind(session)
    .execute(&mut *attempted_failure)
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1",
    )
    .bind(Uuid::from_u128(0xb21))
    .execute(&mut *attempted_failure)
    .await?;
    let attempted_failure_error = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_frontier_id = $2,
                terminal_disposition_kind = 'failed',
                terminal_cause_kind = 'model_call_failed'
          WHERE turn_id = $3",
    )
    .bind(starting_frontier)
    .bind(terminal_frontier)
    .bind(turn)
    .execute(&mut *attempted_failure)
    .await
    .expect_err("a direct queued failure cannot carry an ended attempt");
    assert_eq!(
        attempted_failure_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_queued_failure_without_attempt")
    );
    attempted_failure.rollback().await?;

    let mut failure = pool.begin().await?;
    insert_origin_frontier(
        &mut failure,
        session,
        Uuid::from_u128(0x921),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *failure)
    .await?;
    insert_frontier(
        &mut failure,
        session,
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session, origin_entry),
            (Decimal::from(2_u64), session, failure_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_frontier_id = $2,
                terminal_disposition_kind = 'failed',
                terminal_cause_kind = 'model_call_failed'
          WHERE turn_id = $3",
    )
    .bind(starting_frontier)
    .bind(terminal_frontier)
    .bind(turn)
    .execute(&mut *failure)
    .await?;
    failure.commit().await?;

    let semantic_shape: (String, i64, i64, i64, i64, i64, Option<Uuid>, Option<Uuid>) =
        sqlx::query_as(
            "SELECT
            turn.state_kind,
            (SELECT count(*)
               FROM semantic_transcript_entry
              WHERE source_session_id = $1),
            (SELECT count(*)
               FROM turn_attempt
              WHERE turn_id = $3),
            starting.member_count::bigint,
            terminal.member_count::bigint,
            (SELECT count(*)
               FROM context_frontier_member AS member
               JOIN semantic_transcript_entry AS entry
                 ON entry.source_session_id = member.source_session_id
                AND entry.semantic_entry_id = member.semantic_entry_id
              WHERE member.owning_session_id = $1
                AND member.context_frontier_id = $2
                AND entry.payload_kind = 'turn_failed'),
            turn.terminal_attempt_id,
            turn.terminal_model_call_id
         FROM turn_lifecycle AS turn
         JOIN context_frontier AS starting
           ON starting.owning_session_id = turn.session_id
          AND starting.context_frontier_id = turn.starting_frontier_id
         JOIN context_frontier AS terminal
           ON terminal.owning_session_id = turn.session_id
          AND terminal.context_frontier_id = turn.terminal_frontier_id
         WHERE turn.turn_id = $3",
        )
        .bind(session)
        .bind(starting_frontier)
        .bind(turn)
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        semantic_shape,
        ("terminal".to_owned(), 2, 0, 1, 2, 0, None, None)
    );

    let late_attempt = sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb22))
    .bind(turn)
    .bind(session)
    .execute(&pool)
    .await
    .expect_err("an attempt cannot be inserted after direct terminalization");
    assert_eq!(
        late_attempt
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let overrun = sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, 3, $1, $3)",
    )
    .bind(session)
    .bind(terminal_frontier)
    .bind(failure_entry)
    .execute(&pool)
    .await
    .expect_err("a committed frontier cannot grow beyond its declared count");
    assert_eq!(
        overrun
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("context_frontier_member_within_declared_count")
    );

    let trigger_inventory: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            count(*) FILTER (
                WHERE relation.relname = 'context_frontier'
                  AND candidate.tgname = 'context_frontier_requires_complete_membership'
                  AND candidate.tgdeferrable
            ),
            count(*) FILTER (
                WHERE relation.relname = 'context_frontier_delta'
                  AND candidate.tgname = 'context_frontier_member_requires_complete_membership'
            ),
            count(*) FILTER (
                WHERE relation.relname = 'context_frontier_delta'
                  AND candidate.tgname = 'context_frontier_member_stays_within_declared_count'
                  AND NOT candidate.tgdeferrable
            ),
            count(*) FILTER (
                WHERE relation.relname = 'context_frontier_delta'
                  AND candidate.tgname = 'context_frontier_member_rechecks_declared_count'
                  AND candidate.tgdeferrable
            )
         FROM pg_trigger AS candidate
         JOIN pg_class AS relation
           ON relation.oid = candidate.tgrelid
         WHERE NOT candidate.tgisinternal",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(trigger_inventory, (1, 0, 1, 1));

    let index_inventory: (i64, i64) = sqlx::query_as(
        "SELECT
            count(*) FILTER (
                WHERE indexname = 'turn_attempt_by_turn_session'
                  AND indexdef LIKE '%(turn_id, session_id)%'
            ),
            count(*) FILTER (
                WHERE indexname = 'turn_lifecycle_by_session_position'
                  AND indexdef LIKE '%(session_id, acceptance_position)%'
            )
         FROM pg_indexes
         WHERE schemaname = current_schema()",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(index_inventory, (1, 1));

    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x424, 0x822, direct(0xc24)))
        .await?;
    submit
        .handle(
            start_input(
                0x425,
                0x822,
                "cross-session identity probe",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x924)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa24))),
        )
        .await?;
    let semantic_id_reuse = sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'origin_accepted_input', $3, NULL)",
    )
    .bind(Uuid::from_u128(0x822))
    .bind(origin_entry)
    .bind(Uuid::from_u128(0x924))
    .execute(&pool)
    .await
    .expect_err("a semantic entry identifier cannot be reused by another session");
    assert_eq!(
        semantic_id_reuse
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("semantic_transcript_entry_id_global")
    );

    let frontier_id_reuse = sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(Uuid::from_u128(0x822))
    .bind(starting_frontier)
    .execute(&pool)
    .await
    .expect_err("a context frontier identifier cannot be reused by another session");
    assert_eq!(
        frontier_id_reuse
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("context_frontier_id_global")
    );

    submit
        .handle(
            start_input(
                0x423,
                0x821,
                "still queued",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x922)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa22))),
        )
        .await?;

    let second_turn = Uuid::from_u128(0xa22);
    let second_origin = Uuid::from_u128(0xd23);
    let second_starting_frontier = Uuid::from_u128(0xe23);
    let second_attempt = Uuid::from_u128(0xb23);
    let mut omitted_predecessor_frontier = pool.begin().await?;
    insert_origin_frontier(
        &mut omitted_predecessor_frontier,
        session,
        Uuid::from_u128(0x922),
        second_origin,
        second_starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(second_attempt)
    .bind(second_turn)
    .bind(session)
    .execute(&mut *omitted_predecessor_frontier)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'after',
                immediate_predecessor_turn_id = $1,
                starting_frontier_id = $2,
                active_phase_kind = 'running',
                current_attempt_id = $3
          WHERE turn_id = $4",
    )
    .bind(turn)
    .bind(second_starting_frontier)
    .bind(second_attempt)
    .bind(second_turn)
    .execute(&mut *omitted_predecessor_frontier)
    .await?;
    let omitted = omitted_predecessor_frontier
        .commit()
        .await
        .expect_err("a successor start cannot omit its predecessor terminal frontier");
    assert_eq!(
        omitted.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );

    let mut reordered_predecessor_frontier = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'origin_accepted_input', $3, NULL)",
    )
    .bind(session)
    .bind(second_origin)
    .bind(Uuid::from_u128(0x922))
    .execute(&mut *reordered_predecessor_frontier)
    .await?;
    insert_frontier(
        &mut reordered_predecessor_frontier,
        session,
        second_starting_frontier,
        Decimal::from(3_u64),
        &[
            (Decimal::ONE, session, failure_entry),
            (Decimal::from(2_u64), session, origin_entry),
            (Decimal::from(3_u64), session, second_origin),
        ],
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(second_attempt)
    .bind(second_turn)
    .bind(session)
    .execute(&mut *reordered_predecessor_frontier)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'after',
                immediate_predecessor_turn_id = $1,
                starting_frontier_id = $2,
                active_phase_kind = 'running',
                current_attempt_id = $3
          WHERE turn_id = $4",
    )
    .bind(turn)
    .bind(second_starting_frontier)
    .bind(second_attempt)
    .bind(second_turn)
    .execute(&mut *reordered_predecessor_frontier)
    .await?;
    let reordered = reordered_predecessor_frontier
        .commit()
        .await
        .expect_err("a successor start cannot reorder predecessor membership");
    assert_eq!(
        reordered.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );

    let mut invalid_failure = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(Uuid::from_u128(0xd23))
    .bind(second_turn)
    .execute(&mut *invalid_failure)
    .await?;
    let queued_failure = invalid_failure
        .commit()
        .await
        .expect_err("a queued turn cannot acquire a failure entry");
    assert_eq!(
        queued_failure
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// direct queued failure and immutable frontier membership
/// remain closed under transactions that begin from stale concurrent snapshots.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn concurrent_attempt_and_frontier_inserts_fail_closed() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x451, 0x851, direct(0xc51)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x452,
                0x851,
                "concurrent static failure",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x951)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa51))),
        )
        .await?;

    let session = Uuid::from_u128(0x851);
    let turn = Uuid::from_u128(0xa51);
    let origin_entry = Uuid::from_u128(0xd51);
    let failure_entry = Uuid::from_u128(0xd52);
    let starting_frontier = Uuid::from_u128(0xe51);
    let terminal_frontier = Uuid::from_u128(0xe52);

    let mut terminalize = pool.begin().await?;
    insert_origin_frontier(
        &mut terminalize,
        session,
        Uuid::from_u128(0x951),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *terminalize)
    .await?;
    insert_frontier(
        &mut terminalize,
        session,
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session, origin_entry),
            (Decimal::from(2_u64), session, failure_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_frontier_id = $2,
                terminal_disposition_kind = 'failed',
                terminal_cause_kind = 'model_call_failed'
          WHERE turn_id = $3",
    )
    .bind(starting_frontier)
    .bind(terminal_frontier)
    .bind(turn)
    .execute(&mut *terminalize)
    .await?;

    let concurrent_attempt = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO turn_attempt
                    (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
                     state_kind, end_variant, end_disposition)
                 VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
            )
            .bind(Uuid::from_u128(0xb51))
            .bind(turn)
            .bind(session)
            .execute(&pool)
            .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !concurrent_attempt.is_finished(),
        "attempt insertion must serialize on the lifecycle row"
    );
    terminalize.commit().await?;
    let attempt_error = concurrent_attempt
        .await?
        .expect_err("an attempt racing direct terminalization must fail");
    assert_eq!(
        attempt_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    let attempt_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM turn_attempt WHERE turn_id = $1")
            .bind(turn)
            .fetch_one(&pool)
            .await?;
    assert_eq!(attempt_count, 0);

    let racing_frontier = Uuid::from_u128(0xe53);
    let mut header = pool.begin().await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(session)
    .bind(racing_frontier)
    .execute(&mut *header)
    .await?;

    let mut member = pool.begin().await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, 1, $1, $3)",
    )
    .bind(session)
    .bind(racing_frontier)
    .bind(failure_entry)
    .execute(&mut *member)
    .await?;
    let concurrent_member = tokio::spawn(async move { member.commit().await });
    header.commit().await?;
    let member_error = concurrent_member
        .await?
        .expect_err("a member racing an uncommitted header must fail closed");
    assert!(matches!(
        member_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503" | "23514")
    ));
    let member_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM context_frontier_member
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(session)
    .bind(racing_frontier)
    .fetch_one(&pool)
    .await?;
    assert_eq!(member_count, 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: all baseline authoritative rejections are typed
/// terminal records. Active-work delivery modes reject `NoActiveTurn`, stale
/// defaults and unresolved aliases retain their exact evidence, and missing
/// sessions create no aggregate or queued-work effects.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_submit_records_authoritative_rejections() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let direct_session_fixture = prepared(0x311, 0x711, direct(0x811));
    let direct_session = direct_session_fixture.session().id();
    create.handle(direct_session_fixture).await?;
    let default_alias = ModelAlias::from_uuid(Uuid::from_u128(0x812));
    let alias_session_fixture = prepared(0x312, 0x712, ModelSelectionRequest::Alias(default_alias));
    let alias_session = alias_session_fixture.session().id();
    create.handle(alias_session_fixture).await?;
    let repository = SubmitInputRepository::new(pool.clone());

    let missing = start_input(
        0x313,
        0x7ff,
        "missing",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let missing_recorded = repository
        .handle(
            missing.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x913)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa13))),
        )
        .await?;
    assert!(matches!(
        missing_recorded,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionNotFound { .. }
        ))
    ));
    create.handle(prepared(0x31a, 0x7ff, direct(0x81a))).await?;
    assert_eq!(
        repository
            .handle(
                missing,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x91a)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa1a))),
            )
            .await?,
        missing_recorded
    );

    let expected_turn = TurnId::from_uuid(Uuid::from_u128(0xb11));
    let interrupt = input_with_delivery(
        0x314,
        0x711,
        "active interrupt",
        DeliveryRequest::Interrupt {
            expected_active_turn: expected_turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    assert_eq!(
        repository
            .handle(
                interrupt,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x914)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa14))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::NoActiveTurn {
                session: direct_session,
                expected_active_turn: expected_turn,
            },
        ))
    );

    let next_safe_point = input_with_delivery(
        0x315,
        0x711,
        "active next safe point",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: expected_turn,
        },
    );
    assert_eq!(
        repository
            .handle(
                next_safe_point,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x915)),
                None,
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::NoActiveTurn {
                session: direct_session,
                expected_active_turn: expected_turn,
            },
        ))
    );

    let after_current = input_with_delivery(
        0x316,
        0x711,
        "active after current",
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: expected_turn,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    assert_eq!(
        repository
            .handle(
                after_current,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x916)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa16))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::NoActiveTurn {
                session: direct_session,
                expected_active_turn: expected_turn,
            },
        ))
    );

    let stale = start_input(
        0x318,
        0x711,
        "stale",
        2,
        ModelSelectionOverride::UseSessionDefault,
    );
    let stale_recorded = repository
        .handle(
            stale.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x918)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa18))),
        )
        .await?;
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
            session,
            expected,
            current,
        },
    )) = &stale_recorded
    else {
        panic!("stale defaults must record the exact version mismatch")
    };
    assert_eq!(*session, direct_session);
    assert_eq!(expected.as_u64(), 2);
    assert_eq!(current.as_u64(), 1);
    ReplaceSessionDefaultsRepository::new(pool.clone())
        .handle(replacement(0x31b, 0x711, 1, direct(0x81b)))
        .await?;
    assert_eq!(
        repository
            .handle(
                stale,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x91b)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa1b))),
            )
            .await?,
        stale_recorded
    );

    let unknown = start_input(
        0x319,
        0x712,
        "alias",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    assert_eq!(
        repository
            .handle(
                unknown,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x919)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa19))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::UnknownModelAlias {
                session: alias_session,
                alias: default_alias,
            },
        ))
    );

    let explicit_alias = ModelAlias::from_uuid(Uuid::from_u128(0x81c));
    let explicit_unknown = start_input(
        0x31c,
        0x711,
        "explicit alias",
        2,
        ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(explicit_alias)),
    );
    assert_eq!(
        repository
            .handle(
                explicit_unknown,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x91c)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa1c))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::UnknownModelAlias {
                session: direct_session,
                alias: explicit_alias,
            },
        ))
    );

    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM submit_input_command),
            (SELECT count(*) FROM accepted_input),
            (SELECT count(*) FROM queued_input_origin)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (7, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// the locked session row serializes concurrent
/// assignments into one gap-free position order, and a post-claim database
/// failure explicitly rolls back the claim and does not consume a position.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn submit_serializes_positions_and_rolls_back_failures() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x321, 0x721, direct(0x821)))
        .await?;
    let repository = SubmitInputRepository::new(pool.clone());
    let mut tasks = Vec::new();
    for offset in 0..6_u128 {
        let repository = repository.clone();
        tasks.push(tokio::spawn(async move {
            repository
                .handle(
                    start_input(
                        0x322 + offset,
                        0x721,
                        &format!("concurrent {offset}"),
                        1,
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                    AcceptedInputId::from_uuid(Uuid::from_u128(0x922 + offset)),
                    Some(TurnId::from_uuid(Uuid::from_u128(0xa22 + offset))),
                )
                .await
        }));
    }
    let mut positions = Vec::new();
    for task in tasks {
        let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(applied)) =
            task.await??
        else {
            panic!("each distinct concurrent command must apply");
        };
        positions.push(applied.acceptance_position().as_u64());
    }
    positions.sort_unstable();
    assert_eq!(positions, vec![1, 2, 3, 4, 5, 6]);

    let colliding = start_input(
        0x328,
        0x721,
        "collision",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let error = repository
        .handle(
            colliding.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x922)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa28))),
        )
        .await
        .expect_err("an accepted-input identity collision must abort the transaction");
    assert!(matches!(error, SubmitInputRepositoryError::Database(_)));
    assert!(repository.load(colliding.command_id()).await?.is_none());

    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(retried)) = repository
        .handle(
            colliding,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x928)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa28))),
        )
        .await?
    else {
        panic!("retry after rollback must apply");
    };
    assert_eq!(retried.acceptance_position().as_u64(), 7);

    let equal = start_input(
        0x329,
        0x721,
        "equal concurrent replay",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (left, right) = tokio::join!(
        {
            let repository = repository.clone();
            let command = equal.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                repository
                    .handle(
                        command,
                        AcceptedInputId::from_uuid(Uuid::from_u128(0x929)),
                        Some(TurnId::from_uuid(Uuid::from_u128(0xa29))),
                    )
                    .await
            }
        },
        {
            let repository = repository.clone();
            let command = equal.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                repository
                    .handle(
                        command,
                        AcceptedInputId::from_uuid(Uuid::from_u128(0x92a)),
                        Some(TurnId::from_uuid(Uuid::from_u128(0xa2a))),
                    )
                    .await
            }
        }
    );
    let left = left?;
    let right = right?;
    assert_eq!(left, right);
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(equal_applied)) = left
    else {
        panic!("equal concurrent first handling must converge on an application");
    };
    assert_eq!(equal_applied.acceptance_position().as_u64(), 8);
    let equal_counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM submit_input_command WHERE command_id = $1),
            (SELECT count(*) FROM accepted_input WHERE accepting_command_id = $1),
            (SELECT count(*)
               FROM queued_input_origin AS queued
               JOIN accepted_input AS accepted
                 ON accepted.accepted_input_id = queued.accepted_input_id
              WHERE accepted.accepting_command_id = $1),
            (SELECT count(*)
               FROM turn_lifecycle AS turn
               JOIN accepted_input AS accepted
                 ON accepted.origin_turn_id = turn.turn_id
              WHERE accepted.accepting_command_id = $1)",
    )
    .bind(Uuid::from_u128(0x329))
    .fetch_one(&pool)
    .await?;
    assert_eq!(equal_counts, (1, 1, 1, 1));

    pool.close().await;
    drop(container);
    Ok(())
}

/// a defaults replacement holds the pointer-row
/// lock when its version-row insert requests `FOR KEY SHARE` on the session
/// row through the non-deferrable session foreign key, while submit orders
/// the session row before the pointer row. The forced interleaving completes
/// with typed outcomes because submit's session-row lock is
/// `FOR NO KEY UPDATE`; `FOR UPDATE` deadlocks here (Postgres 40P01).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn submit_and_defaults_replacement_interleave_without_deadlock() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x341, 0x751, direct(0x851)))
        .await?;

    // Replacement side, first half: hold the pointer-row lock exactly as the
    // defaults-replacement compare-and-set does before its version insert.
    // The pointer's version foreign key is deferred, so the successor row may
    // follow the pointer change inside the same transaction.
    let mut replacement_side = pool.begin().await?;
    let cas = sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = $1
           AND current_version = 1",
    )
    .bind(Uuid::from_u128(0x751))
    .execute(&mut *replacement_side)
    .await?;
    assert_eq!(cas.rows_affected(), 1);

    // Submit side: locks the session row, then blocks on the held pointer.
    let submit = tokio::spawn({
        let repository = SubmitInputRepository::new(pool.clone());
        async move {
            repository
                .handle(
                    start_input(
                        0x342,
                        0x751,
                        "interleaved",
                        1,
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                    AcceptedInputId::from_uuid(Uuid::from_u128(0x942)),
                    Some(TurnId::from_uuid(Uuid::from_u128(0xa42))),
                )
                .await
        }
    });

    // Force the interleaving: proceed only once the submit transaction holds
    // its session-row lock and waits on the pointer row.
    let mut submit_blocked_on_pointer = false;
    for _ in 0..400 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*)
             FROM pg_stat_activity
             WHERE wait_event_type = 'Lock'
               AND query LIKE '%FROM session_current_defaults%'",
        )
        .fetch_one(&pool)
        .await?;
        if waiting > 0 {
            submit_blocked_on_pointer = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        submit_blocked_on_pointer,
        "the submit transaction must block on the held pointer row"
    );

    // Replacement side, second half: the insert's session foreign key takes
    // `FOR KEY SHARE` on the session row the submit transaction has locked.
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 2, 'direct', $2, NULL)",
    )
    .bind(Uuid::from_u128(0x751))
    .bind(Uuid::from_u128(0x852))
    .execute(&mut *replacement_side)
    .await?;
    replacement_side.commit().await?;

    // The unblocked submit records the advanced pointer as a typed stale
    // rejection rather than failing on infrastructure.
    assert!(matches!(
        submit.await??,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                expected,
                current,
                ..
            }
        )) if expected.as_u64() == 1 && current.as_u64() == 2
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// checked loads reject cross-wired immutable
/// effects even when database protections are deliberately disabled, and the
/// maximum stored position produces a durable exhaustion rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn submit_corruption_and_position_exhaustion_fail_closed() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x331, 0x731, direct(0x831)))
        .await?;
    let repository = SubmitInputRepository::new(pool.clone());
    let first = start_input(
        0x332,
        0x731,
        "uncorrupted",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    repository
        .handle(
            first.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x932)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa32))),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE submit_input_command
            DISABLE TRIGGER submit_input_command_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE submit_input_command
            SET actor_kind = 'recovery'
          WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x332))
    .execute(&pool)
    .await?;
    let non_user = repository
        .load(first.command_id())
        .await
        .expect_err("domain reconstitution rejects a stored non-user actor");
    assert!(matches!(
        non_user,
        SubmitInputRepositoryError::Corruption(SubmitInputCorruption::Domain(
            SubmitInputReconstitutionFailure::StoredActorMismatch
        ))
    ));
    sqlx::query(
        "UPDATE submit_input_command
            SET actor_kind = 'user'
          WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x332))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE accepted_input
            DISABLE TRIGGER accepted_input_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE queued_input_origin
            DISABLE TRIGGER queued_input_origin_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE queued_input_origin
            DROP CONSTRAINT queued_input_origin_accepted_input_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE accepted_input
            DROP CONSTRAINT accepted_input_queued_origin_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE turn_lifecycle
            DROP CONSTRAINT turn_lifecycle_queued_origin_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE queued_input_origin
            DROP CONSTRAINT queued_input_origin_turn_lifecycle_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE input_accepted_outbox_event
            DROP CONSTRAINT input_accepted_outbox_origin_fk",
    )
    .execute(&pool)
    .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE accepted_input
            SET acceptance_position = 18446744073709551615
          WHERE accepting_command_id = $1",
    )
    .bind(Uuid::from_u128(0x332))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE queued_input_origin
            SET acceptance_position = 18446744073709551615
          WHERE accepted_input_id = $1",
    )
    .bind(Uuid::from_u128(0x932))
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let exhausted = start_input(
        0x333,
        0x731,
        "exhausted",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::AcceptancePositionExhausted { last, .. },
    )) = repository
        .handle(
            exhausted,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x933)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa33))),
        )
        .await?
    else {
        panic!("the maximum stored position rejects the next input");
    };
    assert_eq!(
        last.as_u64(),
        u64::MAX,
        "the exhaustion receipt retains the maximum position"
    );

    sqlx::query("ALTER TABLE accepted_input_content_part DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE accepted_input_content_part
            SET text_value = 'cross-wired'
          WHERE accepted_input_id = $1",
    )
    .bind(Uuid::from_u128(0x932))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE accepted_input_content_part ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let corrupt = repository
        .load(first.command_id())
        .await
        .expect_err("domain correlation rejects altered accepted content");
    assert!(matches!(
        corrupt,
        SubmitInputRepositoryError::Corruption(SubmitInputCorruption::Domain(
            SubmitInputReconstitutionFailure::AcceptedContentMismatch
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}
