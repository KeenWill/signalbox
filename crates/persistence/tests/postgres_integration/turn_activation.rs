//! Concurrent turn activation, lock-blocked interleaving, and occupied scheduler slot handling.

use crate::*;

/// S01: scheduler-row locking serializes concurrent passes for one
/// session so exactly one service activates and the other observes the winner.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_concurrent_start_eligible_turn_passes_activate_once() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x391, 0x791, direct(0x891)))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x791));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x392,
                0x791,
                "concurrent activation",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x991)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa91))),
        )
        .await?;

    let mut first = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd91))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xe91))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xb91))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let mut second = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd92))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xe92))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xb92))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let (first_outcome, second_outcome) =
        tokio::join!(first.execute(session), second.execute(session));
    let first_outcome = first_outcome?;
    let second_outcome = second_outcome?;
    assert!(
        matches!(
            (&first_outcome, &second_outcome),
            (
                StartEligibleTurnOutcome::Activated(_),
                StartEligibleTurnOutcome::NoEligibleTurn
            ) | (
                StartEligibleTurnOutcome::NoEligibleTurn,
                StartEligibleTurnOutcome::Activated(_)
            )
        ),
        "unexpected concurrent outcomes: {first_outcome:?}, {second_outcome:?}"
    );

    let counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'),
            (SELECT count(*)
               FROM semantic_transcript_entry
              WHERE source_session_id = $1),
            (SELECT count(*)
               FROM context_frontier
              WHERE owning_session_id = $1),
            (SELECT count(*)
               FROM turn_attempt
              WHERE session_id = $1)",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (1, 1, 1, 1));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blocked_backends_poll_reports_zero_for_an_idle_database() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    assert!(
        blocked_backends_reached(&pool, 0).await?,
        "an idle database has no lock-blocked backend"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blocked_backends_poll_detects_one_scheduler_row_waiter() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x4e1, 0x8e1, direct(0xce1)))
        .await?;
    let mut holder = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(0x8e1))
        .execute(&mut *holder)
        .await?;
    let waiter = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
                .bind(Uuid::from_u128(0x8e1))
                .execute(&pool)
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "one queued scheduler-row waiter must be detected"
    );

    holder.rollback().await?;
    waiter.await??;
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blocked_backends_poll_reports_when_expected_count_never_forms()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x4e2, 0x8e2, direct(0xce2)))
        .await?;
    let mut holder = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(0x8e2))
        .execute(&mut *holder)
        .await?;
    let waiter = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
                .bind(Uuid::from_u128(0x8e2))
                .execute(&pool)
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the fixture must establish its sole blocked waiter"
    );
    assert!(
        !blocked_backends_reached(&pool, 2).await?,
        "a second waiter never forms, so the poll must exhaust its budget and report false"
    );

    holder.rollback().await?;
    waiter.await??;
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blocked_backends_poll_returns_to_zero_after_release() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x4e3, 0x8e3, direct(0xce3)))
        .await?;
    let mut holder = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(0x8e3))
        .execute(&mut *holder)
        .await?;
    let waiter = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
                .bind(Uuid::from_u128(0x8e3))
                .execute(&pool)
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the fixture must establish a blocked waiter before releasing it"
    );
    holder.rollback().await?;
    waiter.await??;
    assert!(
        blocked_backends_reached(&pool, 0).await?,
        "the released waiter leaves no blocked backend"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// submit orders the session row
/// (`FOR NO KEY UPDATE`) before the scheduler row (`FOR UPDATE`), while
/// activation orders the scheduler row first and then requests `FOR KEY
/// SHARE` on the session row through its inserts' session foreign keys. The
/// forced overlap — the activation queued on the scheduler row first, the
/// submission verifiably holding its session-row lock while queued behind it
/// — completes with typed outcomes on both sides because referential
/// `KEY SHARE` does not conflict with submit's held session lock; a
/// session-row `FOR UPDATE` on the submit side would close this reverse
/// order into a deadlock (Postgres 40P01) surfacing as a `Database` error.
/// Postgres grants a contended row to its first queued waiter, so the
/// activation commits first and the unblocked submission records the typed
/// `ActiveTurnPresent` rejection naming the activated turn while its
/// candidate identities persist nothing. The sibling test queues the
/// submission ahead and pins the applied arm.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn submit_and_activation_interleave_without_deadlock() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x4b1, 0x8b1, direct(0xcb1)))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8b1));
    let queued_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9b1));
    let queued_turn = TurnId::from_uuid(Uuid::from_u128(0xab1));
    let racing_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9b2));
    let racing_turn = TurnId::from_uuid(Uuid::from_u128(0xab2));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x4b2,
                0x8b1,
                "eligible queued origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            queued_input,
            Some(queued_turn),
        )
        .await?;

    // Hold the scheduler row so both racers verifiably queue on it before
    // either proceeds: the activation pass blocks on it first, then the
    // submission takes its session-row lock and queues behind the activation.
    let mut scheduler_blocker = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(0x8b1))
        .execute(&mut *scheduler_blocker)
        .await?;

    let activation = tokio::spawn({
        let mut service = StartEligibleTurnService::new(
            FixedStartEligibleTurnIds::new(
                [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdb1))],
                [ContextFrontierId::from_uuid(Uuid::from_u128(0xeb1))],
                [TurnAttemptId::from_uuid(Uuid::from_u128(0xbb1))],
            ),
            StartEligibleTurnRepository::new(pool.clone()),
        );
        async move { service.execute(session).await }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the eligibility pass must block on the held scheduler row"
    );

    let submission = tokio::spawn({
        let repository = SubmitInputRepository::new(pool.clone());
        async move {
            repository
                .handle(
                    start_input(
                        0x4b3,
                        0x8b1,
                        "racing start",
                        1,
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                    racing_input,
                    Some(racing_turn),
                )
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 2).await?,
        "the submission must hold its session row and queue behind the eligibility pass"
    );

    scheduler_blocker.rollback().await?;
    let activation_outcome = activation.await?.expect(
        "the activation side must serialize without deadlocking; a 40P01 surfaces here as a \
         Database error",
    );
    let submission_outcome = submission.await?.expect(
        "the submission side must serialize without deadlocking; a 40P01 surfaces here as a \
         Database error",
    );

    // The first-queued eligibility pass commits the sole queued origin.
    let StartEligibleTurnOutcome::Activated(activated) = activation_outcome else {
        panic!("the raced eligibility pass must activate the queued origin");
    };
    assert_eq!(activated.turn(), queued_turn);
    assert_eq!(
        activated.accepted_input().expect("accepted origin").id(),
        queued_input
    );

    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::ActiveTurnPresent {
            session: rejected_session,
            active_turn,
        },
    )) = &submission_outcome
    else {
        panic!("the submission behind the activation must record the slot: {submission_outcome:?}");
    };
    assert_eq!(*rejected_session, session);
    assert_eq!(*active_turn, queued_turn);

    let rejection_effects: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM accepted_input WHERE accepted_input_id = $1),
            (SELECT count(*) FROM turn_lifecycle WHERE turn_id = $2),
            (SELECT count(*)
               FROM submit_input_command
              WHERE command_id = $3
                AND rejection_kind = 'active_turn_present'
                AND result_actual_active_turn_id = $4)",
    )
    .bind(racing_input.into_uuid())
    .bind(racing_turn.into_uuid())
    .bind(Uuid::from_u128(0x4b3))
    .bind(queued_turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        rejection_effects,
        (0, 0, 1),
        "a rejected raced submission must persist its evidence and nothing else"
    );

    let invariant_shape: (i64, Uuid, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'),
            (SELECT turn_id
               FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'),
            (SELECT count(*) FROM accepted_input WHERE session_id = $1),
            (SELECT max(acceptance_position)::bigint
               FROM accepted_input
              WHERE session_id = $1)",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(invariant_shape, (1, queued_turn.into_uuid(), 1, 1));

    pool.close().await;
    drop(container);
    Ok(())
}

/// the opposite scheduler queue order
/// to the sibling interleave test — the submission holds its session row and
/// the first place in the scheduler queue while the activation waits behind
/// it. Postgres grants a contended row to its first queued waiter, so the
/// serialized submission commits its applied origin at the next gap-free
/// position together with its queued-work effects, and the eligibility pass
/// then activates the earliest queued origin over that grown acceptance tail
/// with exactly one active turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn submit_queued_ahead_of_activation_interleaves() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x4d1, 0x8d1, direct(0xcd1)))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8d1));
    let queued_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9d1));
    let queued_turn = TurnId::from_uuid(Uuid::from_u128(0xad1));
    let racing_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9d2));
    let racing_turn = TurnId::from_uuid(Uuid::from_u128(0xad2));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x4d2,
                0x8d1,
                "eligible queued origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            queued_input,
            Some(queued_turn),
        )
        .await?;

    // Hold the scheduler row so both racers verifiably queue on it before
    // either proceeds: the submission takes its session-row lock and blocks
    // first, then the activation pass queues behind the submission.
    let mut scheduler_blocker = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(0x8d1))
        .execute(&mut *scheduler_blocker)
        .await?;

    let submission = tokio::spawn({
        let repository = SubmitInputRepository::new(pool.clone());
        async move {
            repository
                .handle(
                    start_input(
                        0x4d3,
                        0x8d1,
                        "racing start",
                        1,
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                    racing_input,
                    Some(racing_turn),
                )
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the submission must hold its session row and block on the held scheduler row"
    );

    let activation = tokio::spawn({
        let mut service = StartEligibleTurnService::new(
            FixedStartEligibleTurnIds::new(
                [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdd1))],
                [ContextFrontierId::from_uuid(Uuid::from_u128(0xed1))],
                [TurnAttemptId::from_uuid(Uuid::from_u128(0xbd1))],
            ),
            StartEligibleTurnRepository::new(pool.clone()),
        );
        async move { service.execute(session).await }
    });
    assert!(
        blocked_backends_reached(&pool, 2).await?,
        "the eligibility pass must queue behind the blocked submission"
    );

    scheduler_blocker.rollback().await?;
    let submission_outcome = submission.await?.expect(
        "the submission side must serialize without deadlocking; a 40P01 surfaces here as a \
         Database error",
    );
    let activation_outcome = activation.await?.expect(
        "the activation side must serialize without deadlocking; a 40P01 surfaces here as a \
         Database error",
    );

    // Behind the committed submission, the eligibility pass still activates
    // the earliest queued origin.
    let StartEligibleTurnOutcome::Activated(activated) = activation_outcome else {
        panic!("the raced eligibility pass must activate the queued origin");
    };
    assert_eq!(activated.turn(), queued_turn);
    assert_eq!(
        activated.accepted_input().expect("accepted origin").id(),
        queued_input
    );

    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(applied),
    )) = &submission_outcome
    else {
        panic!("the submission ahead of the activation must apply: {submission_outcome:?}");
    };
    assert_eq!(applied.accepted_input(), racing_input);
    assert_eq!(applied.turn(), racing_turn);
    assert_eq!(applied.acceptance_position().as_u64(), 2);

    let applied_effects: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM accepted_input
              WHERE accepted_input_id = $1
                AND acceptance_position = 2
                AND disposition_kind = 'origin_of'
                AND origin_turn_id = $2),
            (SELECT count(*) FROM queued_input_origin WHERE accepted_input_id = $1)",
    )
    .bind(racing_input.into_uuid())
    .bind(racing_turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        applied_effects,
        (1, 1),
        "an applied raced submission must persist its acceptance and queued work"
    );

    let invariant_shape: (i64, Uuid, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'),
            (SELECT turn_id
               FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'),
            (SELECT count(*) FROM accepted_input WHERE session_id = $1),
            (SELECT max(acceptance_position)::bigint
               FROM accepted_input
              WHERE session_id = $1)",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(invariant_shape, (1, queued_turn.into_uuid(), 2, 2));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03: nonexistent and empty sessions are false wake-ups that
/// return `NoEligibleTurn` and create no lifecycle effects.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_start_eligible_turn_false_wakeups_are_noops() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let missing = SessionId::from_uuid(Uuid::from_u128(0x7a0));
    let empty = SessionId::from_uuid(Uuid::from_u128(0x7a1));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x3a1, 0x7a1, direct(0x8a1)))
        .await?;

    let mut service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xda0)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xda1)),
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(0xea0)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xea1)),
            ],
            [
                TurnAttemptId::from_uuid(Uuid::from_u128(0xba0)),
                TurnAttemptId::from_uuid(Uuid::from_u128(0xba1)),
            ],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert_eq!(
        service.execute(missing).await?,
        StartEligibleTurnOutcome::NoEligibleTurn
    );
    assert_eq!(
        service.execute(empty).await?,
        StartEligibleTurnOutcome::NoEligibleTurn
    );
    let effects: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM turn_lifecycle),
            (SELECT count(*) FROM semantic_transcript_entry),
            (SELECT count(*) FROM context_frontier),
            (SELECT count(*) FROM turn_attempt)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(effects, (0, 0, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: once the scheduler lock admits and prepares one exact
/// queued candidate, a guarded activation that matches no row is durable
/// divergence, not a stale wake-up, and rolls back every preceding write.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_start_eligible_turn_zero_row_guard_is_inconsistent() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x3a2, 0x7a2, direct(0x8a2)))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x7a2));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xaa2));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x3a3,
                0x7a2,
                "guarded update divergence",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9a2)),
            Some(turn),
        )
        .await?;

    sqlx::query(
        "CREATE FUNCTION suppress_guarded_activation()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             RETURN NULL;
         END
         $$",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE TRIGGER suppress_guarded_activation
         BEFORE UPDATE OF state_kind ON turn_lifecycle
         FOR EACH ROW
         WHEN (OLD.state_kind = 'queued' AND NEW.state_kind = 'active')
         EXECUTE FUNCTION suppress_guarded_activation()",
    )
    .execute(&pool)
    .await?;

    let mut service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xda2))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xea2))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xba2))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let error = service
        .execute(session)
        .await
        .expect_err("zero-row guarded activation must surface durable divergence");
    assert!(matches!(
        error,
        StartEligibleTurnRepositoryError::Corruption(StartEligibleTurnCorruption::Inconsistent(
            "guarded activation matched no row"
        ))
    ));

    let unchanged: (String, i64, i64, i64) = sqlx::query_as(
        "SELECT
            state_kind,
            (SELECT count(*)
               FROM semantic_transcript_entry
              WHERE source_session_id = $2),
            (SELECT count(*)
               FROM context_frontier
              WHERE owning_session_id = $2),
            (SELECT count(*)
               FROM turn_attempt
              WHERE session_id = $2)
         FROM turn_lifecycle
        WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(unchanged, ("queued".into(), 0, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// each durable candidate-identity collision is
/// typed and rolls back all earlier activation writes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn start_eligible_turn_identity_collisions_roll_back() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x3b1, 0x7b1, direct(0x8b1)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x3b2,
                0x7b1,
                "identity source",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9b1)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xab1))),
        )
        .await?;
    let existing_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdb1));
    let existing_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xeb1));
    let existing_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbb1));
    let mut source_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new([existing_entry], [existing_frontier], [existing_attempt]),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        source_service
            .execute(SessionId::from_uuid(Uuid::from_u128(0x7b1)))
            .await?,
        StartEligibleTurnOutcome::Activated(_)
    ));

    for (offset, origin, frontier, attempt, expected) in [
        (
            2_u128,
            existing_entry,
            ContextFrontierId::from_uuid(Uuid::from_u128(0xeb2)),
            TurnAttemptId::from_uuid(Uuid::from_u128(0xbb2)),
            StartEligibleTurnIdentityCollision::OriginEntry,
        ),
        (
            3,
            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdb3)),
            existing_frontier,
            TurnAttemptId::from_uuid(Uuid::from_u128(0xbb3)),
            StartEligibleTurnIdentityCollision::StartingFrontier,
        ),
        (
            4,
            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdb4)),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xeb4)),
            existing_attempt,
            StartEligibleTurnIdentityCollision::InitialAttempt,
        ),
    ] {
        let session_uuid = Uuid::from_u128(0x7b0 + offset);
        let session = SessionId::from_uuid(session_uuid);
        let turn = TurnId::from_uuid(Uuid::from_u128(0xab0 + offset));
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
            .handle(prepared(
                0x3b0 + offset * 2,
                0x7b0 + offset,
                direct(0x8b0 + offset),
            ))
            .await?;
        SubmitInputRepository::new(pool.clone())
            .handle(
                start_input(
                    0x3b1 + offset * 2,
                    0x7b0 + offset,
                    "identity collision target",
                    1,
                    ModelSelectionOverride::UseSessionDefault,
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9b0 + offset)),
                Some(turn),
            )
            .await?;
        let mut service = StartEligibleTurnService::new(
            FixedStartEligibleTurnIds::new([origin], [frontier], [attempt]),
            StartEligibleTurnRepository::new(pool.clone()),
        );
        let error = service
            .execute(session)
            .await
            .expect_err("the reused durable candidate must fail");
        assert!(
            matches!(
                error,
                StartEligibleTurnRepositoryError::IdentityCollision(actual)
                    if actual == expected
            ),
            "unexpected collision result: {error:?}"
        );
        let unchanged: (String, i64, i64, i64) = sqlx::query_as(
            "SELECT
                state_kind,
                (SELECT count(*)
                   FROM semantic_transcript_entry
                  WHERE source_session_id = $2),
                (SELECT count(*)
                   FROM context_frontier
                  WHERE owning_session_id = $2),
                (SELECT count(*)
                   FROM turn_attempt
                  WHERE session_id = $2)
             FROM turn_lifecycle
            WHERE turn_id = $1",
        )
        .bind(turn.into_uuid())
        .bind(session_uuid)
        .fetch_one(&pool)
        .await?;
        assert_eq!(unchanged, ("queued".into(), 0, 0, 0));
    }

    pool.close().await;
    drop(container);
    Ok(())
}

/// an incomplete scheduling inventory fails closed before
/// any origin entry, frontier, attempt, or lifecycle transition is written.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn start_eligible_turn_corrupt_projection_fails_closed() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x3c1, 0x7c1, direct(0x8c1)))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x7c1));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xac1));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x3c2,
                0x7c1,
                "corrupt projection",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9c1)),
            Some(turn),
        )
        .await?;
    sqlx::query(
        "ALTER TABLE queued_input_origin
            DROP CONSTRAINT queued_input_origin_turn_lifecycle_fk",
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
        "ALTER TABLE turn_model_settings_resolved
            DROP CONSTRAINT turn_model_settings_resolved_turn_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM turn_lifecycle WHERE turn_id = $1")
        .bind(turn.into_uuid())
        .execute(&pool)
        .await?;

    let mut service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdc1))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xec1))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xbc1))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let error = service
        .execute(session)
        .await
        .expect_err("the incomplete inventory must not authorize activation");
    assert!(matches!(
        error,
        StartEligibleTurnRepositoryError::Corruption(StartEligibleTurnCorruption::Scheduling(
            SubmitInputCorruption::Inconsistent("complete scheduling turn inventory")
        ))
    ));
    let effects: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM semantic_transcript_entry),
            (SELECT count(*) FROM context_frontier),
            (SELECT count(*) FROM turn_attempt)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(effects, (0, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S09: after the first queued turn fails, the adapter
/// activates the next turn with exact predecessor lineage and a
/// prefix-preserving starting frontier.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s09_start_eligible_turn_preserves_failed_predecessor_prefix() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x3d1, 0x7d1, direct(0x8d1)))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x7d1));
    let accepted_first = AcceptedInputId::from_uuid(Uuid::from_u128(0x9d1));
    let accepted_second = AcceptedInputId::from_uuid(Uuid::from_u128(0x9d2));
    let first_turn = TurnId::from_uuid(Uuid::from_u128(0xad1));
    let second_turn = TurnId::from_uuid(Uuid::from_u128(0xad2));
    let submit = SubmitInputRepository::new(pool.clone());
    submit
        .handle(
            start_input(
                0x3d2,
                0x7d1,
                "first queued",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_first,
            Some(first_turn),
        )
        .await?;
    submit
        .handle(
            start_input(
                0x3d3,
                0x7d1,
                "second queued",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_second,
            Some(second_turn),
        )
        .await?;

    let first_origin = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdd1));
    let first_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xed1));
    let first_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbd1));
    let mut first_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new([first_origin], [first_frontier], [first_attempt]),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        first_service.execute(session).await?,
        StartEligibleTurnOutcome::Activated(_)
    ));

    let failure_entry = Uuid::from_u128(0xdd2);
    let terminal_frontier = Uuid::from_u128(0xed2);
    let mut terminalize = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session.into_uuid())
    .bind(failure_entry)
    .bind(first_turn.into_uuid())
    .execute(&mut *terminalize)
    .await?;
    insert_frontier(
        &mut terminalize,
        session.into_uuid(),
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session.into_uuid(), first_origin.into_uuid()),
            (Decimal::from(2_u64), session.into_uuid(), failure_entry),
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
    .bind(first_attempt.into_uuid())
    .execute(&mut *terminalize)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = $1,
                active_phase_kind = NULL,
                terminal_attempt_id = current_attempt_id,
                current_attempt_id = NULL,
                terminal_disposition_kind = 'failed',
                terminal_cause_kind = 'model_call_failed'
          WHERE turn_id = $2",
    )
    .bind(terminal_frontier)
    .bind(first_turn.into_uuid())
    .execute(&mut *terminalize)
    .await?;
    terminalize.commit().await?;

    let second_origin = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdd3));
    let second_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xed3));
    let second_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbd3));
    let mut second_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new([second_origin], [second_frontier], [second_attempt]),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) = second_service.execute(session).await?
    else {
        panic!("the successor must activate after its failed predecessor");
    };
    assert_eq!(activated.turn(), second_turn);
    assert_eq!(
        activated.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: first_turn,
        }
    );
    assert_eq!(activated.start().frontier().snapshot(), second_frontier);

    let members: Vec<(i64, Uuid)> = sqlx::query_as(
        "SELECT member_position::bigint, semantic_entry_id
           FROM context_frontier_member
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
          ORDER BY member_position",
    )
    .bind(session.into_uuid())
    .bind(second_frontier.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        members,
        vec![
            (1, first_origin.into_uuid()),
            (2, failure_entry),
            (3, second_origin.into_uuid()),
        ]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: one complete schema-level eligibility
/// transaction can bind the exact origin frontier and prepared attempt, while
/// the database independently rejects contradictory lifecycle histories.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_turn_storage_enforces_lifecycle_consistency() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x401, 0x801, direct(0xc01)))
        .await?;
    let submit = SubmitInputRepository::new(pool.clone());
    submit
        .handle(
            start_input(
                0x402,
                0x801,
                "first",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x901)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa01))),
        )
        .await?;
    submit
        .handle(
            start_input(
                0x403,
                0x801,
                "second",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x902)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa02))),
        )
        .await?;

    let session = Uuid::from_u128(0x801);
    let first_turn = Uuid::from_u128(0xa01);
    let first_attempt = Uuid::from_u128(0xb01);
    let first_entry = Uuid::from_u128(0xd01);
    let first_frontier = Uuid::from_u128(0xe01);
    let mut activation = pool.begin().await?;
    insert_origin_frontier(
        &mut activation,
        session,
        Uuid::from_u128(0x901),
        first_entry,
        first_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(first_attempt)
    .bind(first_turn)
    .bind(session)
    .execute(&mut *activation)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                active_phase_kind = 'running',
                current_attempt_id = $2
          WHERE turn_id = $3
            AND state_kind = 'queued'",
    )
    .bind(first_frontier)
    .bind(first_attempt)
    .bind(first_turn)
    .execute(&mut *activation)
    .await?;
    activation.commit().await?;

    let active_shape: (String, String, String, String, i64) = sqlx::query_as(
        "SELECT turn.state_kind, turn.start_lineage_kind,
                turn.active_phase_kind, attempt.state_kind,
                frontier.member_count::bigint
           FROM turn_lifecycle AS turn
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = turn.current_attempt_id
           JOIN context_frontier AS frontier
             ON frontier.owning_session_id = turn.session_id
            AND frontier.context_frontier_id = turn.starting_frontier_id
          WHERE turn.turn_id = $1",
    )
    .bind(first_turn)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        active_shape,
        (
            "active".into(),
            "first_in_session".into(),
            "running".into(),
            "prepared".into(),
            1
        )
    );

    let born_active = sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_accepted_input_id, acceptance_position,
             state_kind, start_lineage_kind, immediate_predecessor_turn_id,
             starting_frontier_id, terminal_frontier_id, active_phase_kind,
             current_attempt_id, terminal_disposition_kind)
         SELECT turn_id, session_id, origin_accepted_input_id, acceptance_position,
                state_kind, start_lineage_kind, immediate_predecessor_turn_id,
                starting_frontier_id, terminal_frontier_id, active_phase_kind,
                current_attempt_id, terminal_disposition_kind
           FROM turn_lifecycle
          WHERE turn_id = $1",
    )
    .bind(first_turn)
    .execute(&pool)
    .await
    .expect_err("even a complete active shape must first be inserted as queued");
    assert_eq!(
        born_active
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    assert_eq!(
        born_active
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_inserted_queued")
    );

    for (attempt_id, state_kind, end_variant, end_disposition) in [
        (Uuid::from_u128(0xb05), "running", None, None),
        (
            Uuid::from_u128(0xb06),
            "ended",
            Some("without_stop"),
            Some("known_failure"),
        ),
    ] {
        let born_nonprepared = sqlx::query(
            "INSERT INTO turn_attempt
                (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
                 state_kind, end_variant, end_disposition)
             VALUES ($1, $2, $3, NULL, $4, $5, $6)",
        )
        .bind(attempt_id)
        .bind(Uuid::from_u128(0xa02))
        .bind(session)
        .bind(state_kind)
        .bind(end_variant)
        .bind(end_disposition)
        .execute(&pool)
        .await
        .expect_err("every attempt must first be inserted as prepared");
        assert_eq!(
            born_nonprepared
                .as_database_error()
                .and_then(|error| error.code()),
            Some("23514".into())
        );
        assert_eq!(
            born_nonprepared
                .as_database_error()
                .and_then(|error| error.constraint()),
            Some("turn_attempt_inserted_prepared"),
            "unexpected insert guard for born-{state_kind} attempt"
        );
    }

    let mut second_activation = pool.begin().await?;
    insert_origin_frontier(
        &mut second_activation,
        session,
        Uuid::from_u128(0x902),
        Uuid::from_u128(0xd02),
        Uuid::from_u128(0xe02),
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb02))
    .bind(Uuid::from_u128(0xa02))
    .bind(session)
    .execute(&mut *second_activation)
    .await?;
    let second_active = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'after',
                immediate_predecessor_turn_id = $1,
                starting_frontier_id = $2,
                active_phase_kind = 'running',
                current_attempt_id = $3
          WHERE turn_id = $4",
    )
    .bind(first_turn)
    .bind(Uuid::from_u128(0xe02))
    .bind(Uuid::from_u128(0xb02))
    .bind(Uuid::from_u128(0xa02))
    .execute(&mut *second_activation)
    .await
    .expect_err("the partial unique index must reject a second active turn");
    assert_eq!(
        second_active
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_one_active_per_session")
    );
    second_activation.rollback().await?;

    let mut duplicate_live = pool.begin().await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb03))
    .bind(Uuid::from_u128(0xa02))
    .bind(session)
    .execute(&mut *duplicate_live)
    .await?;
    let second_live = sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, $4, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb04))
    .bind(Uuid::from_u128(0xa02))
    .bind(session)
    .bind(Uuid::from_u128(0xb03))
    .execute(&mut *duplicate_live)
    .await
    .expect_err("the partial unique index must reject a second live attempt");
    assert_eq!(
        second_live
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_attempt_one_live_per_turn")
    );
    duplicate_live.rollback().await?;

    let immutable_start = sqlx::query(
        "UPDATE turn_lifecycle
            SET starting_frontier_id = $1
          WHERE turn_id = $2",
    )
    .bind(Uuid::from_u128(0xeff))
    .bind(first_turn)
    .execute(&pool)
    .await
    .expect_err("a committed turn start must be write-once");
    assert_eq!(
        immutable_start
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let immutable_member = sqlx::query(
        "UPDATE context_frontier_delta
            SET member_position = 2
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(session)
    .bind(first_frontier)
    .execute(&pool)
    .await
    .expect_err("committed frontier membership must be immutable");
    assert_eq!(
        immutable_member
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let out_of_bounds_member = sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, 2, $1, $3)",
    )
    .bind(session)
    .bind(first_frontier)
    .bind(first_entry)
    .execute(&pool)
    .await
    .expect_err("committed frontier membership cannot exceed its declared count");
    assert_eq!(
        out_of_bounds_member
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("context_frontier_member_within_declared_count")
    );

    let duplicate_frontier = Uuid::from_u128(0xe04);
    let mut duplicate_membership = pool.begin().await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 2)",
    )
    .bind(session)
    .bind(duplicate_frontier)
    .execute(&mut *duplicate_membership)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, 1, $1, $3)",
    )
    .bind(session)
    .bind(duplicate_frontier)
    .bind(first_entry)
    .execute(&mut *duplicate_membership)
    .await?;
    let duplicate_member = sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, 2, $1, $3)",
    )
    .bind(session)
    .bind(duplicate_frontier)
    .bind(first_entry)
    .execute(&mut *duplicate_membership)
    .await
    .expect_err("one exact source-qualified entry cannot occur twice");
    assert_eq!(
        duplicate_member
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("context_frontier_member_entry_once")
    );
    duplicate_membership.rollback().await?;

    let mut unavailable_continuation = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1",
    )
    .bind(first_attempt)
    .execute(&mut *unavailable_continuation)
    .await?;
    let successor_attempt = Uuid::from_u128(0xb02);
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, $4, 'prepared', NULL, NULL)",
    )
    .bind(successor_attempt)
    .bind(first_turn)
    .bind(session)
    .bind(first_attempt)
    .execute(&mut *unavailable_continuation)
    .await?;
    let replacement_error = sqlx::query(
        "UPDATE turn_lifecycle
            SET current_attempt_id = $1
          WHERE turn_id = $2",
    )
    .bind(successor_attempt)
    .bind(first_turn)
    .execute(&mut *unavailable_continuation)
    .await
    .expect_err("a running turn cannot replace its sealed current attempt");
    assert_eq!(
        replacement_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    assert!(replacement_error.as_database_error().is_some_and(|error| {
        error
            .message()
            .contains("running turn cannot replace its current attempt")
    }));
    unavailable_continuation.rollback().await?;

    let failure_entry = Uuid::from_u128(0xd03);
    let terminal_frontier = Uuid::from_u128(0xe03);
    for contradictory_disposition in [
        "turn_completed",
        "turn_refused",
        "yielded_to_durable_wait",
        "ambiguous",
    ] {
        let mut contradictory_terminal = pool.begin().await?;
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 origin_accepted_input_id, failed_turn_id)
             VALUES ($1, $2, 'turn_failed', NULL, $3)",
        )
        .bind(session)
        .bind(failure_entry)
        .bind(first_turn)
        .execute(&mut *contradictory_terminal)
        .await?;
        insert_frontier(
            &mut contradictory_terminal,
            session,
            terminal_frontier,
            Decimal::from(2_u64),
            &[
                (Decimal::ONE, session, first_entry),
                (Decimal::from(2_u64), session, failure_entry),
            ],
        )
        .await?;
        sqlx::query(
            "UPDATE turn_attempt
                SET state_kind = 'ended',
                    end_variant = 'without_stop',
                    end_disposition = $1
              WHERE turn_attempt_id = $2",
        )
        .bind(contradictory_disposition)
        .bind(first_attempt)
        .execute(&mut *contradictory_terminal)
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
        .bind(terminal_frontier)
        .bind(first_turn)
        .execute(&mut *contradictory_terminal)
        .await?;

        let contradictory_terminal_error = contradictory_terminal
            .commit()
            .await
            .expect_err("a failed turn cannot retain a contradictory ended attempt");
        let database_error = contradictory_terminal_error
            .as_database_error()
            .expect("deferred lifecycle validation must return a database error");
        assert_eq!(database_error.code(), Some("23514".into()));
        assert!(
            database_error
                .message()
                .contains("permits only known_failure or lost ended attempts"),
            "unexpected terminal consistency error for {contradictory_disposition}: {}",
            database_error.message()
        );
    }

    let mut terminalize = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(first_turn)
    .execute(&mut *terminalize)
    .await?;
    insert_frontier(
        &mut terminalize,
        session,
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session, first_entry),
            (Decimal::from(2_u64), session, failure_entry),
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
    .bind(first_attempt)
    .execute(&mut *terminalize)
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
    .bind(terminal_frontier)
    .bind(first_turn)
    .execute(&mut *terminalize)
    .await?;
    terminalize.commit().await?;

    let immutable_attempt = sqlx::query(
        "UPDATE turn_attempt
            SET end_disposition = 'lost'
          WHERE turn_attempt_id = $1",
    )
    .bind(first_attempt)
    .execute(&pool)
    .await
    .expect_err("an ended attempt must be immutable");
    assert_eq!(
        immutable_attempt
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let born_terminal = sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_accepted_input_id, acceptance_position,
             state_kind, start_lineage_kind, immediate_predecessor_turn_id,
             starting_frontier_id, terminal_frontier_id, active_phase_kind,
             current_attempt_id, terminal_disposition_kind)
         SELECT turn_id, session_id, origin_accepted_input_id, acceptance_position,
                state_kind, start_lineage_kind, immediate_predecessor_turn_id,
                starting_frontier_id, terminal_frontier_id, active_phase_kind,
                current_attempt_id, terminal_disposition_kind
           FROM turn_lifecycle
          WHERE turn_id = $1",
    )
    .bind(first_turn)
    .execute(&pool)
    .await
    .expect_err("even a complete terminal shape must first be inserted as queued");
    assert_eq!(
        born_terminal
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    assert_eq!(
        born_terminal
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_inserted_queued")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S03 / S08 / S09:
/// occupied-slot After and NextSafePoint handling commits the exact distinct
/// effects, checked replay survives a pool/repository restart, and the
/// restarted adapter advances from the complete validated acceptance tail
/// without admitting an unrelated non-lifecycle frontier into the projection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_slot_after_and_safe_point_apply_replay_and_restart() -> Result<(), Box<dyn Error>>
{
    let (container, pool, database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x431, 0x831, direct(0xc31)))
        .await?;
    let active_origin_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x931));
    let active_origin_turn = TurnId::from_uuid(Uuid::from_u128(0xa31));
    let repository = SubmitInputRepository::new(pool.clone());
    repository
        .handle(
            start_input(
                0x432,
                0x831,
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
            session: Uuid::from_u128(0x831),
            origin_entry: Uuid::from_u128(0xd31),
            starting_frontier: Uuid::from_u128(0xe31),
            initial_attempt: Uuid::from_u128(0xb31),
        },
    )
    .await?;
    assert_eq!(activated.accepted_input().id(), active_origin_input);
    assert_eq!(activated.turn(), active_origin_turn);
    let mut unrelated_frontier = pool.begin().await?;
    insert_frontier(
        &mut unrelated_frontier,
        Uuid::from_u128(0x831),
        Uuid::from_u128(0xef31),
        Decimal::ONE,
        &[(Decimal::ONE, Uuid::from_u128(0x831), Uuid::from_u128(0xd31))],
    )
    .await?;
    unrelated_frontier.commit().await?;

    let after = input_with_delivery(
        0x433,
        0x831,
        "after active",
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa31)),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let after_outcome = repository
        .handle(
            after.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x932)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa32))),
        )
        .await?;
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(after_applied),
    )) = &after_outcome
    else {
        panic!("matching AfterCurrentTurn must create queued origin work");
    };
    assert_eq!(after_applied.acceptance_position().as_u64(), 2);

    let safe_point = input_with_delivery(
        0x434,
        0x831,
        "steer active",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa31)),
        },
    );
    let safe_point_outcome = repository
        .handle(
            safe_point.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x933)),
            None,
        )
        .await?;
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::PendingSteering(steering),
    )) = &safe_point_outcome
    else {
        panic!("matching NextSafePoint must create pending steering");
    };
    assert_eq!(steering.acceptance_position().as_u64(), 3);
    assert_eq!(
        steering.binding().source_turn(),
        TurnId::from_uuid(Uuid::from_u128(0xa31))
    );

    assert_eq!(
        repository
            .handle(
                after.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9ff)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xaff))),
            )
            .await?,
        after_outcome
    );
    assert_eq!(
        repository
            .handle(
                safe_point.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fe)),
                None,
            )
            .await?,
        safe_point_outcome
    );

    let mut application_service = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fb)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fa)),
            ],
            [TurnId::from_uuid(Uuid::from_u128(0xafb))],
        ),
        repository.clone(),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    let after_request = SubmitInputRequest::try_new(
        after.command_id(),
        after.session(),
        after.content().clone(),
        after.delivery(),
    )?;
    let safe_point_request = SubmitInputRequest::try_new(
        safe_point.command_id(),
        safe_point.session(),
        safe_point.content().clone(),
        safe_point.delivery(),
    )?;
    assert_eq!(
        application_service.execute(after_request).await?,
        SubmitInputOutcome::Recorded(match &after_outcome {
            SubmitInputHandlingOutcome::Recorded(result) => result.clone(),
            SubmitInputHandlingOutcome::ConflictingReuse { .. } => {
                unreachable!("the exact occupied-slot command was recorded")
            }
        })
    );
    assert_eq!(
        application_service.execute(safe_point_request).await?,
        SubmitInputOutcome::Recorded(match &safe_point_outcome {
            SubmitInputHandlingOutcome::Recorded(result) => result.clone(),
            SubmitInputHandlingOutcome::ConflictingReuse { .. } => {
                unreachable!("the exact occupied-slot command was recorded")
            }
        })
    );

    let effect_shape: (i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM accepted_input
              WHERE accepted_input_id = $1
                AND delivery_kind = 'after_current_turn'
                AND disposition_kind = 'origin_of'
                AND origin_turn_id = $2
                AND expected_defaults_version = 1),
            (SELECT count(*) FROM queued_input_origin WHERE accepted_input_id = $1),
            (SELECT count(*) FROM turn_lifecycle WHERE origin_accepted_input_id = $1),
            (SELECT count(*)
               FROM accepted_input
              WHERE accepted_input_id = $3
                AND delivery_kind = 'next_safe_point'
                AND disposition_kind = 'pending_steering'
                AND expected_active_turn_id = $4
                AND expected_defaults_version IS NULL
                AND model_override_kind IS NULL
                AND replacement_model_kind IS NULL
                AND replacement_direct_model_selection_id IS NULL
                AND replacement_model_alias_id IS NULL
                AND origin_turn_id IS NULL),
            (SELECT count(*) FROM queued_input_origin WHERE accepted_input_id = $3),
            (SELECT count(*) FROM turn_lifecycle WHERE origin_accepted_input_id = $3),
            (SELECT count(*)
               FROM information_schema.columns
              WHERE table_schema = current_schema()
                AND table_name = 'accepted_input'
                AND column_name = 'steering_source_turn_id'),
            (SELECT count(*)
               FROM submit_input_command
              WHERE command_id = $5
                AND result_actual_active_turn_id = $4)",
    )
    .bind(Uuid::from_u128(0x932))
    .bind(Uuid::from_u128(0xa32))
    .bind(Uuid::from_u128(0x933))
    .bind(Uuid::from_u128(0xa31))
    .bind(Uuid::from_u128(0x434))
    .fetch_one(&pool)
    .await?;
    assert_eq!(effect_shape, (1, 1, 1, 1, 0, 0, 0, 1));

    drop(repository);
    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let restarted = SubmitInputRepository::new(restarted_pool.clone());
    assert_eq!(
        restarted
            .handle(
                after,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fd)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xafd))),
            )
            .await?,
        after_outcome
    );
    assert_eq!(
        restarted
            .handle(
                safe_point,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9fc)),
                None,
            )
            .await?,
        safe_point_outcome
    );

    let after_restart = input_with_delivery(
        0x435,
        0x831,
        "after restart",
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa31)),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(after_restart),
    )) = restarted
        .handle(
            after_restart,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x934)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa33))),
        )
        .await?
    else {
        panic!("restart must preserve occupied-slot origin submission");
    };
    assert_eq!(after_restart.acceptance_position().as_u64(), 4);

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S03 / S08: the composed production
/// chain — CreateSession service, accepted start submission, and
/// StartEligibleTurn service activation — produces the occupied slot the
/// seeded occupied-slot tests assume: a matching After request queues at the
/// next gap-free position, a matching NextSafePoint binds pending steering to
/// the activated turn, and a start names the activated turn in its typed
/// rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_slot_handling_composes_with_service_activated_first_turn()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8a1));
    let mut create_service = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(created) = create_service
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4a1)),
            SessionConfigurationDefaults::new(direct(0xca1)),
        )?)
        .await?
    else {
        panic!("user-initiated composed creation must apply");
    };
    assert_eq!(created.session(), session);

    let origin_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9a1));
    let origin_turn = TurnId::from_uuid(Uuid::from_u128(0xaa1));
    let mut submit_service = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [
                origin_input,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9a2)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9a3)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9a4)),
            ],
            [
                origin_turn,
                TurnId::from_uuid(Uuid::from_u128(0xaa2)),
                TurnId::from_uuid(Uuid::from_u128(0xaa3)),
            ],
        ),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    let start = start_input(
        0x4a2,
        0x8a1,
        "composed start",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(origin),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            start.command_id(),
            start.session(),
            start.content().clone(),
            start.delivery(),
        )?)
        .await?
    else {
        panic!("the composed no-active-turn start must apply");
    };
    assert_eq!(origin.turn(), origin_turn);
    assert_eq!(origin.acceptance_position().as_u64(), 1);

    let starting_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xea1));
    let mut activation_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xda1))],
            [starting_frontier],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xba1))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) =
        activation_service.execute(session).await?
    else {
        panic!("the sole composed queued turn must activate");
    };
    assert_eq!(activated.session(), session);
    assert_eq!(activated.turn(), origin.turn());
    assert_eq!(
        activated.accepted_input().expect("accepted origin").id(),
        origin.accepted_input()
    );
    assert_eq!(
        activated.start().lineage(),
        AcceptedInputStartingLineage::FirstInSession
    );
    assert_eq!(activated.start().frontier().snapshot(), starting_frontier);

    let after = input_with_delivery(
        0x4a3,
        0x8a1,
        "after service-activated turn",
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: activated.turn(),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(after_applied),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            after.command_id(),
            after.session(),
            after.content().clone(),
            after.delivery(),
        )?)
        .await?
    else {
        panic!("matching AfterCurrentTurn must queue against the service-activated turn");
    };
    assert_eq!(after_applied.acceptance_position().as_u64(), 2);

    let safe_point = input_with_delivery(
        0x4a4,
        0x8a1,
        "steer service-activated turn",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: activated.turn(),
        },
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::PendingSteering(steering),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            safe_point.command_id(),
            safe_point.session(),
            safe_point.content().clone(),
            safe_point.delivery(),
        )?)
        .await?
    else {
        panic!("matching NextSafePoint must bind against the service-activated turn");
    };
    assert_eq!(steering.acceptance_position().as_u64(), 3);
    assert_eq!(steering.binding().source_turn(), activated.turn());

    let blocked_start = start_input(
        0x4a5,
        0x8a1,
        "blocked composed start",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let blocked = submit_service
        .execute(SubmitInputRequest::try_new(
            blocked_start.command_id(),
            blocked_start.session(),
            blocked_start.content().clone(),
            blocked_start.delivery(),
        )?)
        .await?;
    let SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::ActiveTurnPresent {
            session: rejected_session,
            active_turn,
        },
    )) = blocked
    else {
        panic!("a start against the service-activated slot must be rejected");
    };
    assert_eq!(
        rejected_session, session,
        "the occupied-slot rejection names the session"
    );
    assert_eq!(
        active_turn,
        activated.turn(),
        "the occupied-slot rejection names the active turn"
    );

    let effect_shape: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'),
            (SELECT count(*)
               FROM accepted_input
              WHERE accepted_input_id = $2
                AND delivery_kind = 'after_current_turn'
                AND disposition_kind = 'origin_of'
                AND origin_turn_id = $3),
            (SELECT count(*) FROM queued_input_origin WHERE accepted_input_id = $2),
            (SELECT count(*)
               FROM accepted_input
              WHERE accepted_input_id = $4
                AND delivery_kind = 'next_safe_point'
                AND disposition_kind = 'pending_steering'
                AND expected_active_turn_id = $5),
            (SELECT count(*) FROM queued_input_origin WHERE accepted_input_id = $4)",
    )
    .bind(session.into_uuid())
    .bind(after_applied.accepted_input().into_uuid())
    .bind(after_applied.turn().into_uuid())
    .bind(steering.accepted_input().into_uuid())
    .bind(activated.turn().into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(effect_shape, (1, 1, 1, 1, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S08 / S09: after the production chain
/// activates the first turn and terminal facts close it, the production
/// activation service commits the After-lineage successor, and occupied-slot
/// handling against that successor matches the first-in-session pass: After
/// queues at the next gap-free position, NextSafePoint binds to the
/// successor, and a start names it. The predecessor's terminalization uses
/// this suite's raw terminal seam (the same seam the S09 predecessor-prefix
/// test uses) because no production terminalization adapter exists yet; every
/// other step is the production chain.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_slot_handling_composes_with_service_activated_after_lineage_turn()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8c1));
    let mut create_service = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(created) = create_service
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4c1)),
            SessionConfigurationDefaults::new(direct(0xcc1)),
        )?)
        .await?
    else {
        panic!("user-initiated composed creation must apply");
    };
    assert_eq!(created.session(), session);

    let first_turn = TurnId::from_uuid(Uuid::from_u128(0xac1));
    let second_turn = TurnId::from_uuid(Uuid::from_u128(0xac2));
    let mut submit_service = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9c1)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9c2)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9c3)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9c4)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x9c5)),
            ],
            [
                first_turn,
                second_turn,
                TurnId::from_uuid(Uuid::from_u128(0xac3)),
                TurnId::from_uuid(Uuid::from_u128(0xac4)),
            ],
        ),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    let first_start = start_input(
        0x4c2,
        0x8c1,
        "first composed start",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(first_origin),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            first_start.command_id(),
            first_start.session(),
            first_start.content().clone(),
            first_start.delivery(),
        )?)
        .await?
    else {
        panic!("the first composed start must apply");
    };
    assert_eq!(first_origin.turn(), first_turn);
    let second_start = start_input(
        0x4c3,
        0x8c1,
        "second composed start",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(second_origin),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            second_start.command_id(),
            second_start.session(),
            second_start.content().clone(),
            second_start.delivery(),
        )?)
        .await?
    else {
        panic!("the second composed start must queue behind the first");
    };
    assert_eq!(second_origin.turn(), second_turn);
    assert_eq!(second_origin.acceptance_position().as_u64(), 2);

    let first_origin_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdc1));
    let first_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbc1));
    let mut first_activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [first_origin_entry],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xec1))],
            [first_attempt],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(first_activated) =
        first_activation.execute(session).await?
    else {
        panic!("the first composed queued turn must activate");
    };
    assert_eq!(first_activated.turn(), first_turn);

    // Raw terminal seam: no production terminalization adapter exists yet, so
    // the predecessor's failure facts commit exactly as in the S09
    // predecessor-prefix test.
    let failure_entry = Uuid::from_u128(0xdc2);
    let terminal_frontier = Uuid::from_u128(0xec2);
    let mut terminalize = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session.into_uuid())
    .bind(failure_entry)
    .bind(first_turn.into_uuid())
    .execute(&mut *terminalize)
    .await?;
    insert_frontier(
        &mut terminalize,
        session.into_uuid(),
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (
                Decimal::ONE,
                session.into_uuid(),
                first_origin_entry.into_uuid(),
            ),
            (Decimal::from(2_u64), session.into_uuid(), failure_entry),
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
    .bind(first_attempt.into_uuid())
    .execute(&mut *terminalize)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = $1,
                active_phase_kind = NULL,
                terminal_attempt_id = current_attempt_id,
                current_attempt_id = NULL,
                terminal_disposition_kind = 'failed',
                terminal_cause_kind = 'model_call_failed'
          WHERE turn_id = $2",
    )
    .bind(terminal_frontier)
    .bind(first_turn.into_uuid())
    .execute(&mut *terminalize)
    .await?;
    terminalize.commit().await?;

    let mut second_activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdc3))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xec3))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xbc3))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(second_activated) =
        second_activation.execute(session).await?
    else {
        panic!("the successor must activate after its failed predecessor");
    };
    assert_eq!(second_activated.turn(), second_turn);
    assert_eq!(
        second_activated.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: first_turn,
        }
    );

    let after = input_with_delivery(
        0x4c4,
        0x8c1,
        "after the After-lineage turn",
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: second_activated.turn(),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(after_applied),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            after.command_id(),
            after.session(),
            after.content().clone(),
            after.delivery(),
        )?)
        .await?
    else {
        panic!("matching AfterCurrentTurn must queue against the After-lineage turn");
    };
    assert_eq!(after_applied.acceptance_position().as_u64(), 3);

    let safe_point = input_with_delivery(
        0x4c5,
        0x8c1,
        "steer the After-lineage turn",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: second_activated.turn(),
        },
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::PendingSteering(steering),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            safe_point.command_id(),
            safe_point.session(),
            safe_point.content().clone(),
            safe_point.delivery(),
        )?)
        .await?
    else {
        panic!("matching NextSafePoint must bind against the After-lineage turn");
    };
    assert_eq!(steering.acceptance_position().as_u64(), 4);
    assert_eq!(steering.binding().source_turn(), second_activated.turn());

    let blocked_start = start_input(
        0x4c6,
        0x8c1,
        "blocked start behind the After-lineage turn",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let blocked = submit_service
        .execute(SubmitInputRequest::try_new(
            blocked_start.command_id(),
            blocked_start.session(),
            blocked_start.content().clone(),
            blocked_start.delivery(),
        )?)
        .await?;
    let SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::ActiveTurnPresent {
            session: rejected_session,
            active_turn,
        },
    )) = blocked
    else {
        panic!("a start against the After-lineage slot must be rejected");
    };
    assert_eq!(
        rejected_session, session,
        "the successor rejection names the session"
    );
    assert_eq!(
        active_turn,
        second_activated.turn(),
        "the successor rejection names the active turn"
    );

    let successor_shape: (i64, String, Uuid, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1 AND state_kind = 'active'),
            turn.start_lineage_kind,
            turn.immediate_predecessor_turn_id,
            frontier.member_count::bigint
         FROM turn_lifecycle AS turn
         JOIN context_frontier AS frontier
           ON frontier.owning_session_id = turn.session_id
          AND frontier.context_frontier_id = turn.starting_frontier_id
        WHERE turn.turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(second_activated.turn().into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        successor_shape,
        (1, "after".into(), first_turn.into_uuid(), 3)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// the session-before-scheduler lock order
/// serializes mixed occupied-slot acceptances into one gap-free order while
/// preserving each delivery's distinct atomic effect shape.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_slot_mixed_acceptances_serialize_positions_and_effects()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x451, 0x851, direct(0xc51)))
        .await?;
    let active_origin_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x951));
    let active_origin_turn = TurnId::from_uuid(Uuid::from_u128(0xa51));
    let repository = SubmitInputRepository::new(pool.clone());
    repository
        .handle(
            start_input(
                0x452,
                0x851,
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
            session: Uuid::from_u128(0x851),
            origin_entry: Uuid::from_u128(0xd51),
            starting_frontier: Uuid::from_u128(0xe51),
            initial_attempt: Uuid::from_u128(0xb51),
        },
    )
    .await?;
    assert_eq!(activated.accepted_input().id(), active_origin_input);
    assert_eq!(activated.turn(), active_origin_turn);

    let (positions, turn_origins, pending_steering) =
        run_mixed_occupied_acceptances(repository).await?;
    assert_eq!(positions, vec![2, 3, 4, 5, 6, 7]);
    assert_eq!((turn_origins, pending_steering), (3, 3));

    let effects: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            count(*) FILTER (WHERE delivery_kind = 'after_current_turn'),
            count(*) FILTER (WHERE delivery_kind = 'next_safe_point'),
            (SELECT count(*)
               FROM queued_input_origin
              WHERE session_id = $1
                AND acceptance_position > 1),
            (SELECT count(*)
               FROM accepted_input
              WHERE session_id = $1
                AND disposition_kind = 'pending_steering'
                AND origin_turn_id IS NULL
                AND expected_defaults_version IS NULL)
          FROM accepted_input
         WHERE session_id = $1
           AND acceptance_position > 1",
    )
    .bind(Uuid::from_u128(0x851))
    .fetch_one(&pool)
    .await?;
    assert_eq!(effects, (3, 3, 3, 3));

    pool.close().await;
    drop(container);
    Ok(())
}

/// Asserts that an occupied-slot rejection naming `source_turn` fails deferred
/// source-origin validation.
///
/// The two cases that exercise this differ only in their identifiers, so the
/// assertions live here and each case reads as one straight-line call
/// (`docs/agents/testing-style.md` rule 2).
async fn assert_rejected_source_origin(
    pool: &PgPool,
    command_id: Uuid,
    source_turn: Uuid,
    description: &str,
) {
    let error = insert_cross_wired_occupied_rejection(
        pool,
        command_id,
        Uuid::from_u128(0x46a),
        source_turn,
    )
    .await
    .expect_err(description);
    let database_error = error
        .as_database_error()
        .expect("deferred source-origin validation must return a database error");
    assert_eq!(database_error.code(), Some("23503".into()));
    assert_eq!(
        database_error.constraint(),
        Some("submit_input_command_rejected_source_origin")
    );
}

/// occupied-slot result
/// shapes and correlations are database-enforced, pending steering keeps its
/// source active and cannot become semantic origin, and its immutable receipt
/// survives a later current-disposition change.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_slot_schema_constraints_and_checked_decode_fail_closed()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let steering_frontier_assertion: String = sqlx::query_scalar(
        "SELECT pg_get_functiondef(oid)
           FROM pg_proc
          WHERE proname = 'assert_model_call_steering_final_state'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(steering_frontier_assertion.contains(
        "earlier.disposition_kind IN (
                    'pending_steering',
                    'reclassified_as_turn_origin'
               )"
    ));

    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x461, 0x861, direct(0xc61)))
        .await?;
    let active_origin_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x961));
    let active_origin_turn = TurnId::from_uuid(Uuid::from_u128(0xa61));
    let repository = SubmitInputRepository::new(pool.clone());
    repository
        .handle(
            start_input(
                0x462,
                0x861,
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
            session: Uuid::from_u128(0x861),
            origin_entry: Uuid::from_u128(0xd61),
            starting_frontier: Uuid::from_u128(0xe61),
            initial_attempt: Uuid::from_u128(0xb61),
        },
    )
    .await?;
    assert_eq!(activated.accepted_input().id(), active_origin_input);
    assert_eq!(activated.turn(), active_origin_turn);
    let safe_source = input_with_delivery(
        0x463,
        0x861,
        "safe-point representation",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa61)),
        },
    );
    let SubmitInputHandlingOutcome::Recorded(safe_result) = repository
        .handle(
            safe_source.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x962)),
            None,
        )
        .await?
    else {
        panic!("safe-point input must be recorded");
    };

    let semantic_pending_error = sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'origin_accepted_input', $3, NULL)",
    )
    .bind(Uuid::from_u128(0x861))
    .bind(Uuid::from_u128(0xd62))
    .bind(Uuid::from_u128(0x962))
    .execute(&pool)
    .await
    .expect_err("pending steering cannot establish a semantic turn origin");
    let semantic_pending_database_error = semantic_pending_error
        .as_database_error()
        .expect("deferred semantic-origin validation must return a database error");
    assert_eq!(semantic_pending_database_error.code(), Some("23514".into()));
    assert_eq!(
        semantic_pending_database_error.constraint(),
        Some("semantic_transcript_entry_origin_disposition")
    );

    let mut terminalize_source = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(Uuid::from_u128(0x861))
    .bind(Uuid::from_u128(0xd63))
    .bind(Uuid::from_u128(0xa61))
    .execute(&mut *terminalize_source)
    .await?;
    insert_frontier(
        &mut terminalize_source,
        Uuid::from_u128(0x861),
        Uuid::from_u128(0xe63),
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, Uuid::from_u128(0x861), Uuid::from_u128(0xd61)),
            (
                Decimal::from(2_u64),
                Uuid::from_u128(0x861),
                Uuid::from_u128(0xd63),
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
    .bind(Uuid::from_u128(0xb61))
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
    .bind(Uuid::from_u128(0xe63))
    .bind(Uuid::from_u128(0xa61))
    .execute(&mut *terminalize_source)
    .await?;
    let terminalize_source_error = terminalize_source
        .commit()
        .await
        .expect_err("pending steering must keep its source turn active");
    let terminalize_source_database_error = terminalize_source_error
        .as_database_error()
        .expect("deferred pending-source validation must return a database error");
    assert_eq!(
        terminalize_source_database_error.code(),
        Some("23514".into())
    );
    assert_eq!(
        terminalize_source_database_error.constraint(),
        Some("turn_lifecycle_pending_steering_closed")
    );

    repository
        .handle(
            input_with_delivery(
                0x464,
                0x861,
                "alternate lifecycle",
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa61)),
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x963)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa62))),
        )
        .await?;

    repository
        .handle(
            input_with_delivery(
                0x46a,
                0x861,
                "unknown alias rejection",
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa61)),
                    configuration: input_choices(
                        1,
                        ModelSelectionOverride::ReplaceWith(alias(0xc69)),
                    ),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x96a)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa6a))),
        )
        .await?;

    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x46b, 0x86b, direct(0xc6b)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x46c,
                0x86b,
                "other-session origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x96b)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa6b))),
        )
        .await?;

    assert_rejected_source_origin(
        &pool,
        Uuid::from_u128(0x46d),
        Uuid::from_u128(0xa6f),
        "missing source turn",
    )
    .await;
    assert_rejected_source_origin(
        &pool,
        Uuid::from_u128(0x46e),
        Uuid::from_u128(0xa6b),
        "cross-session source turn",
    )
    .await;

    let new_constraints: Vec<String> = sqlx::query_scalar(
        "SELECT conname
           FROM pg_constraint
          WHERE conname IN (
                'accepted_input_pending_result_key',
                'accepted_input_expected_active_turn_fk',
                'accepted_input_general_command_result_fk',
                'submit_input_command_actual_active_turn_fk',
                'submit_input_command_pending_effect_fk',
                'submit_input_command_general_applied_effect_fk'
          )
          ORDER BY conname",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(new_constraints.len(), 6);

    let scheduling_support_indexes: Vec<(String, bool)> = sqlx::query_as(
        "SELECT
            indexname,
            indexdef LIKE
                CASE indexname
                    WHEN 'accepted_input_pending_by_source_turn'
                        THEN '%(session_id, expected_active_turn_id) WHERE (disposition_kind = ''pending_steering''::text)'
                    WHEN 'queued_input_origin_by_session_position'
                        THEN '%(session_id, acceptance_position)'
                END
           FROM pg_indexes
          WHERE schemaname = current_schema()
            AND indexname IN (
                'accepted_input_pending_by_source_turn',
                'queued_input_origin_by_session_position'
            )
          ORDER BY indexname",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        scheduling_support_indexes,
        vec![
            ("accepted_input_pending_by_source_turn".to_owned(), true),
            ("queued_input_origin_by_session_position".to_owned(), true),
        ]
    );

    let forbidden_configuration = sqlx::query(
        "INSERT INTO accepted_input
            (accepted_input_id, accepting_command_id, session_id,
             delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             acceptance_position, disposition_kind, origin_turn_id)
         VALUES
            ($1, $2, $3,
             'next_safe_point', $4, 1, 'use_session_default',
             NULL, NULL, NULL, 4, 'pending_steering', NULL)",
    )
    .bind(Uuid::from_u128(0x969))
    .bind(Uuid::from_u128(0x469))
    .bind(Uuid::from_u128(0x861))
    .bind(Uuid::from_u128(0xa61))
    .execute(&pool)
    .await
    .expect_err("pending steering cannot persist origin configuration");
    assert_eq!(
        forbidden_configuration
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let extra_queue = sqlx::query(
        "INSERT INTO queued_input_origin
            (turn_id, accepted_input_id, session_id, acceptance_position,
             priority_kind, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             requested_model_alias_id, frozen_model_kind,
             frozen_direct_model_selection_id, frozen_model_alias_id,
             frozen_alias_selected_direct_id, model_parameters,
             known_provider_failure_retry, model_fallback)
         VALUES
            ($1, $2, $3, 2, 'ordinary', 1,
             'direct', $4, NULL, 'direct', $4, NULL, NULL,
             'provider_defaults', 'disabled', 'disabled')",
    )
    .bind(Uuid::from_u128(0xf61))
    .bind(Uuid::from_u128(0x962))
    .bind(Uuid::from_u128(0x861))
    .bind(Uuid::from_u128(0xc61))
    .execute(&pool)
    .await
    .expect_err("pending steering cannot acquire a queued turn");
    assert_eq!(
        extra_queue
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23503".into())
    );

    let mut cross_wired = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'submit_input', 3, transaction_timestamp(), 'operator')",
    )
    .bind(Uuid::from_u128(0x466))
    .execute(&mut *cross_wired)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_actual_active_turn_id, result_expected_active_turn_id,
             result_expected_defaults_version, result_current_defaults_version,
             result_unknown_alias_id, result_selected_defaults_version,
             result_last_position)
         VALUES
            ($1, 'submit_input', 3, $2,
             'user', NULL, NULL,
             'next_safe_point', $3, NULL, NULL, NULL, NULL, NULL,
             'applied', NULL, $2, $4, NULL, $3,
             NULL, NULL, NULL, NULL, NULL, NULL)",
    )
    .bind(Uuid::from_u128(0x466))
    .bind(Uuid::from_u128(0x861))
    .bind(Uuid::from_u128(0xa62))
    .bind(Uuid::from_u128(0x966))
    .execute(&mut *cross_wired)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command_content_part
            (command_id, position, part_kind, text_value)
         VALUES ($1, 0, 'text', 'cross-wired steering')",
    )
    .bind(Uuid::from_u128(0x466))
    .execute(&mut *cross_wired)
    .await?;
    sqlx::query(
        "INSERT INTO accepted_input
            (accepted_input_id, accepting_command_id, session_id,
             delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             acceptance_position, disposition_kind, origin_turn_id)
         VALUES
            ($1, $2, $3,
             'next_safe_point', $4, NULL, NULL, NULL, NULL, NULL,
             4, 'pending_steering', NULL)",
    )
    .bind(Uuid::from_u128(0x966))
    .bind(Uuid::from_u128(0x466))
    .bind(Uuid::from_u128(0x861))
    .bind(Uuid::from_u128(0xa61))
    .execute(&mut *cross_wired)
    .await?;
    sqlx::query(
        "INSERT INTO accepted_input_content_part
            (accepted_input_id, position, part_kind, text_value)
         VALUES ($1, 0, 'text', 'cross-wired steering')",
    )
    .bind(Uuid::from_u128(0x966))
    .execute(&mut *cross_wired)
    .await?;
    let cross_wired_error = cross_wired
        .commit()
        .await
        .expect_err("command and pending acceptance must bind the same source turn");
    assert_eq!(
        cross_wired_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23503".into())
    );

    sqlx::query(
        "ALTER TABLE accepted_input
            DISABLE TRIGGER accepted_input_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE accepted_input
            DROP CONSTRAINT accepted_input_delivery_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE accepted_input
            SET disposition_kind = 'origin_of'
          WHERE accepted_input_id = $1",
    )
    .bind(Uuid::from_u128(0x962))
    .execute(&pool)
    .await?;
    let replayed = repository
        .load(safe_source.command_id())
        .await?
        .expect("mutable disposition cannot erase the immutable receipt");
    assert_eq!(replayed.result(), &safe_result);

    pool.close().await;
    drop(container);
    Ok(())
}
