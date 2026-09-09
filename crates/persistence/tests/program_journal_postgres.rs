//! PostgreSQL integration coverage for durable program journals.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use signalbox_persistence::test_support::postgres::TestDatabase;
use std::error::Error;

use signalbox_domain::{
    DeliveryKind, EffectRequest, InlineFramePayload, JournalFrame, ProgramCapability, ProgramFault,
    ProgramRunId, ReplayCursor, ReplayInstruction, ReplayedRequest, RequestFrame, RequestKind,
    ScopeOperation, ScopeOrdinal, ScopeRequest,
};
use signalbox_persistence::program_journal::{ProgramJournalCorruption, ProgramJournalRepository};
use sqlx::PgPool;
use uuid::Uuid;

const RUN_ID: u128 = 0x5100_0100;

async fn migrated_postgres() -> Result<(TestDatabase, PgPool), Box<dyn Error>> {
    let (database, pool, _) =
        signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
    Ok((database, pool))
}

fn run_id() -> ProgramRunId {
    ProgramRunId::from_uuid(Uuid::from_u128(RUN_ID))
}

fn payload(value: &'static [u8]) -> InlineFramePayload {
    InlineFramePayload::new(value)
}

fn assert_constraint_error(error: sqlx::Error, expected_constraint: &str) {
    let sqlx::Error::Database(database) = error else {
        panic!("expected a PostgreSQL constraint error, got {error:?}");
    };

    assert_eq!(database.constraint(), Some(expected_constraint));
}

fn assert_trigger_error(error: sqlx::Error, expected_message: &str) {
    let sqlx::Error::Database(database) = error else {
        panic!("expected a PostgreSQL trigger error, got {error:?}");
    };

    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(database.message(), expected_message);
}

/// durable request and delivery projections retain one exact interleaving.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn journal_round_trip_preserves_concurrent_delivery_order() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let first_request = repository
        .append_request(run, None, RequestKind::Now(payload(b"first")))
        .await?;
    let second_request = repository
        .append_request(run, None, RequestKind::Random(payload(b"second")))
        .await?;
    let second_answer = repository
        .append_delivery(
            run,
            DeliveryKind::Answer {
                resolves: second_request.ordinal(),
                payload: payload(b"second-answer"),
            },
        )
        .await?;
    let first_answer = repository
        .append_delivery(
            run,
            DeliveryKind::Answer {
                resolves: first_request.ordinal(),
                payload: payload(b"first-answer"),
            },
        )
        .await?;

    let loaded = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    let mut replay = ReplayCursor::new(loaded);

    assert_eq!(replay.next_instruction(), ReplayInstruction::AwaitRequest);
    assert_eq!(
        replay.submit_request(first_request),
        Ok(ReplayedRequest::Matched)
    );
    assert_eq!(replay.next_instruction(), ReplayInstruction::AwaitRequest);
    assert_eq!(
        replay.submit_request(second_request),
        Ok(ReplayedRequest::Matched)
    );
    assert_eq!(
        replay.next_instruction(),
        ReplayInstruction::Deliver(second_answer)
    );
    assert_eq!(
        replay.next_instruction(),
        ReplayInstruction::Deliver(first_answer)
    );
    assert_eq!(replay.next_instruction(), ReplayInstruction::Live);

    pool.close().await;
    drop(container);
    Ok(())
}

/// persisted nondeterminism evidence retains both complete request frames.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn nondeterminism_fault_round_trips_both_frames() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let recorded_scope = ScopeOrdinal::try_from_u64(7).expect("fixture ordinal is positive");
    let recorded = repository
        .append_request(
            run,
            Some(recorded_scope),
            RequestKind::Effect(EffectRequest::new(
                ProgramCapability::Judge,
                "score".to_owned(),
                payload(b"recorded"),
            )),
        )
        .await?;
    let loaded = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    let mut replay = ReplayCursor::new(loaded);
    let observed_scope = ScopeOrdinal::try_from_u64(11).expect("fixture ordinal is positive");
    let declared_scope = ScopeOrdinal::try_from_u64(13).expect("fixture ordinal is positive");
    let parent_scope = ScopeOrdinal::try_from_u64(17).expect("fixture ordinal is positive");
    let observed = RequestFrame::new(
        recorded.ordinal(),
        Some(observed_scope),
        RequestKind::Scope(ScopeRequest::new(
            ScopeOperation::Close,
            declared_scope,
            Some(parent_scope),
        )),
    );
    let divergence = replay
        .submit_request(observed.clone())
        .expect_err("different canonical request bytes must diverge");
    let fault = repository.append_nondeterminism_fault(divergence).await?;

    let reloaded = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    let last = reloaded
        .entries()
        .last()
        .expect("the persisted fault is present");

    assert_eq!(last.frame(), &JournalFrame::Delivery(fault.clone()));

    let mut restarted_replay = ReplayCursor::new(reloaded);
    assert_eq!(
        restarted_replay.submit_request(observed),
        Ok(ReplayedRequest::Matched)
    );
    assert_eq!(
        restarted_replay.next_instruction(),
        ReplayInstruction::Deliver(fault)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn outstanding_requests_reject_requires_terminal_request() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let request = repository
        .append_request(run, None, RequestKind::Now(payload(b"nonterminal")))
        .await?;

    let insert = sqlx::query(
        "INSERT INTO program_run_journal_entry (
             run_id, journal_position, frame_direction, frame_kind, delivery_ordinal,
             resolves_request_ordinal, reject_reason, payload_inline
         ) VALUES ($1, 2, 'delivery', 'reject', 1, $2, 'outstanding_requests', '')",
    )
    .bind(run.into_uuid())
    .bind(rust_decimal::Decimal::from(request.ordinal().as_u64()))
    .execute(&pool)
    .await;

    assert_trigger_error(
        insert.expect_err("outstanding-requests rejection requires a terminal request"),
        "delivery must resolve one earlier compatible request",
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// committed journal frames cannot be updated or deleted.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn journal_frames_are_append_only() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let request = repository
        .append_request(run, None, RequestKind::Now(payload(b"immutable")))
        .await?;

    let update = sqlx::query(
        "UPDATE program_run_journal_entry
            SET payload_inline = 'changed'
          WHERE run_id = $1 AND request_ordinal = $2",
    )
    .bind(run.into_uuid())
    .bind(rust_decimal::Decimal::from(request.ordinal().as_u64()))
    .execute(&pool)
    .await;

    assert_trigger_error(
        update.expect_err("updating a journal entry is rejected"),
        "program_run_journal_entry is append-only",
    );

    let delete = sqlx::query(
        "DELETE FROM program_run_journal_entry
          WHERE run_id = $1 AND request_ordinal = $2",
    )
    .bind(run.into_uuid())
    .bind(rust_decimal::Decimal::from(request.ordinal().as_u64()))
    .execute(&pool)
    .await;

    assert_trigger_error(
        delete.expect_err("deleting a journal entry is rejected"),
        "program_run_journal_entry is append-only",
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn sequence_state_row_cannot_be_deleted() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;

    let delete = sqlx::query(
        "DELETE FROM program_run_journal_sequence_state
          WHERE run_id = $1",
    )
    .bind(run.into_uuid())
    .execute(&pool)
    .await;

    assert_trigger_error(
        delete.expect_err("deleting sequence state is rejected"),
        "program_run_journal_sequence_state is append-only",
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn sequence_state_run_identity_cannot_change() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    let other_run = ProgramRunId::from_uuid(Uuid::from_u128(RUN_ID + 1));
    repository.create_stream(run).await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO program_run_journal_stream (run_id, frame_contract_version)
         VALUES ($1, 1)",
    )
    .bind(other_run.into_uuid())
    .execute(&mut *transaction)
    .await?;

    let update = sqlx::query(
        "UPDATE program_run_journal_sequence_state
            SET run_id = $2
          WHERE run_id = $1",
    )
    .bind(run.into_uuid())
    .bind(other_run.into_uuid())
    .execute(&mut *transaction)
    .await;

    assert_trigger_error(
        update.expect_err("changing sequence-state identity is rejected"),
        "program_run_journal_sequence_state is append-only",
    );
    transaction.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn payloadless_scope_request_rejects_inline_bytes() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;

    let insert = sqlx::query(
        "INSERT INTO program_run_journal_entry (
             run_id, journal_position, frame_direction, frame_kind,
             request_ordinal, scope_operation, declared_scope_ordinal, payload_inline
         ) VALUES ($1, 1, 'request', 'scope', 1, 'open', 1, 'unexpected')",
    )
    .bind(run.into_uuid())
    .execute(&pool)
    .await;

    assert_constraint_error(
        insert.expect_err("scope requests cannot carry inline bytes"),
        "program_run_journal_entry_payload_shape",
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn nondeterminism_scope_evidence_requires_nonnull_operation() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let fault = repository
        .append_delivery(
            run,
            DeliveryKind::Fault(ProgramFault::Timeout(payload(b"ordinary-fault"))),
        )
        .await?;

    let insert = sqlx::query(
        "INSERT INTO program_run_journal_nondeterminism (
             run_id, journal_position,
             expected_request_ordinal, expected_kind,
             expected_declared_scope_ordinal, expected_payload_inline,
             observed_request_ordinal, observed_kind, observed_payload_inline
         )
         SELECT run_id, journal_position,
                1, 'scope', 1, '',
                1, 'now', ''
           FROM program_run_journal_entry
          WHERE run_id = $1 AND delivery_ordinal = $2",
    )
    .bind(run.into_uuid())
    .bind(rust_decimal::Decimal::from(fault.ordinal().as_u64()))
    .execute(&pool)
    .await;

    assert_constraint_error(
        insert.expect_err("scope evidence requires an operation"),
        "program_run_journal_nondeterminism_expected_scope_shape",
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn append_to_missing_stream_reports_missing_stream() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());

    let error = repository
        .append_request(run_id(), None, RequestKind::Now(payload(b"missing")))
        .await
        .expect_err("an append requires a created stream");

    assert_eq!(
        error.corruption(),
        Some(&ProgramJournalCorruption::MissingStream)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn nondeterminism_evidence_cannot_attach_to_request() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let request = repository
        .append_request(run, None, RequestKind::Now(payload(b"ordinary-request")))
        .await?;

    let insert = sqlx::query(
        "INSERT INTO program_run_journal_nondeterminism (
             run_id, journal_position,
             expected_request_ordinal, expected_kind, expected_payload_inline,
             observed_request_ordinal, observed_kind, observed_payload_inline
         )
         SELECT run_id, journal_position,
                request_ordinal, frame_kind, payload_inline,
                request_ordinal, frame_kind, payload_inline
           FROM program_run_journal_entry
          WHERE run_id = $1 AND request_ordinal = $2",
    )
    .bind(run.into_uuid())
    .bind(rust_decimal::Decimal::from(request.ordinal().as_u64()))
    .execute(&pool)
    .await;

    assert_trigger_error(
        insert.expect_err("evidence cannot attach to a request"),
        "nondeterminism fault and its complete twin frames must commit together",
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn generic_delivery_append_rejects_nondeterminism_fault() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let expected = repository
        .append_request(run, None, RequestKind::Now(payload(b"expected")))
        .await?;
    let observed = RequestFrame::new(
        expected.ordinal(),
        expected.scope(),
        RequestKind::Now(payload(b"observed")),
    );

    let error = repository
        .append_delivery(
            run,
            DeliveryKind::Fault(ProgramFault::Nondeterminism { expected, observed }),
        )
        .await
        .expect_err("generic delivery append cannot persist replay divergence");

    assert_eq!(
        error.corruption(),
        Some(&ProgramJournalCorruption::Inconsistent(
            "nondeterminism fault without replay failure"
        ))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn nondeterminism_scope_evidence_bounds_declared_ordinal() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let fault = repository
        .append_delivery(
            run,
            DeliveryKind::Fault(ProgramFault::Timeout(payload(b"ordinary-fault"))),
        )
        .await?;

    let insert = sqlx::query(
        "INSERT INTO program_run_journal_nondeterminism (
             run_id, journal_position,
             expected_request_ordinal, expected_kind, expected_scope_operation,
             expected_declared_scope_ordinal, expected_payload_inline,
             observed_request_ordinal, observed_kind, observed_payload_inline
         )
         SELECT run_id, journal_position,
                1, 'scope', 'open', 0, '',
                1, 'now', ''
           FROM program_run_journal_entry
          WHERE run_id = $1 AND delivery_ordinal = $2",
    )
    .bind(run.into_uuid())
    .bind(rust_decimal::Decimal::from(fault.ordinal().as_u64()))
    .execute(&pool)
    .await;

    assert_constraint_error(
        insert.expect_err("scope evidence requires a positive declared ordinal"),
        "program_run_journal_nondeterminism_ordinals_positive",
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn initial_sequence_state_must_match_empty_journal() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let run = run_id();
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO program_run_journal_stream (run_id, frame_contract_version)
         VALUES ($1, 1)",
    )
    .bind(run.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO program_run_journal_sequence_state (
             run_id, last_position, last_request_ordinal, last_delivery_ordinal
         ) VALUES ($1, 1, 1, 0)",
    )
    .bind(run.into_uuid())
    .execute(&mut *transaction)
    .await?;

    let commit = transaction.commit().await;

    assert_trigger_error(
        commit.expect_err("initial sequence counters must match the empty journal"),
        "program journal sequence state disagrees with committed frames",
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn sequence_state_cannot_be_primed_past_a_missing_frame() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE program_run_journal_sequence_state
            SET last_position = 1, last_request_ordinal = 1
          WHERE run_id = $1",
    )
    .bind(run.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO program_run_journal_entry (
             run_id, journal_position, frame_direction, frame_kind,
             request_ordinal, payload_inline
         ) VALUES ($1, 2, 'request', 'now', 2, 'gap')",
    )
    .bind(run.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE program_run_journal_sequence_state
            SET last_position = 2, last_request_ordinal = 2
          WHERE run_id = $1",
    )
    .bind(run.into_uuid())
    .execute(&mut *transaction)
    .await?;

    let commit = transaction.commit().await;

    assert_trigger_error(
        commit.expect_err("sequence counters cannot hide a missing frame"),
        "program journal sequence state disagrees with committed frames",
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cancellation_replays_one_terminal_delivery_with_outstanding_requests()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::DurableCommandId;
    use signalbox_persistence::program_cancellation::{
        self as cancellation, CancelProgramRun, ProgramCancellationOutcome as Outcome,
        ProgramCancellationResult as Result, ProgramTerminalState,
    };
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    repository
        .append_request(run, None, RequestKind::Now(payload(b"first")))
        .await?;
    repository
        .append_request(run, None, RequestKind::Random(payload(b"second")))
        .await?;
    let command = CancelProgramRun {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        run_id: run,
    };
    assert_eq!(
        cancellation::cancel(&pool, command.clone()).await?,
        Result::Recorded(Outcome::Applied)
    );
    assert_eq!(
        cancellation::cancel(&pool, command.clone()).await?,
        Result::Recorded(Outcome::Applied)
    );
    let journal = repository.load(run).await?.expect("retained run");
    assert_eq!(journal.entries().len(), 3);
    assert_eq!(
        journal.terminal_delivery().expect("cancelled run").kind(),
        &DeliveryKind::RunCancel(InlineFramePayload::new(
            command.command_id.into_uuid().to_string().into_bytes()
        ))
    );
    assert_eq!(
        cancellation::cancel(
            &pool,
            CancelProgramRun {
                command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                run_id: run
            }
        )
        .await?,
        Result::Recorded(Outcome::AlreadyTerminal(ProgramTerminalState::Cancelled))
    );
    assert_eq!(repository.load(run).await?, Some(journal));
    assert_eq!(
        cancellation::cancel(
            &pool,
            CancelProgramRun {
                command_id: command.command_id,
                run_id: ProgramRunId::from_uuid(Uuid::now_v7())
            }
        )
        .await?,
        Result::ConflictingReuse
    );
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cancellation_not_found_replays_after_the_run_is_created() -> Result<(), Box<dyn Error>> {
    use signalbox_domain::DurableCommandId;
    use signalbox_persistence::program_cancellation::{
        self as cancellation, CancelProgramRun, ProgramCancellationOutcome as Outcome,
        ProgramCancellationResult as Result,
    };
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    let command = CancelProgramRun {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        run_id: run,
    };
    assert_eq!(
        cancellation::cancel(&pool, command.clone()).await?,
        Result::Recorded(Outcome::NotFound)
    );
    repository.create_stream(run).await?;
    assert_eq!(
        cancellation::cancel(&pool, command).await?,
        Result::Recorded(Outcome::NotFound)
    );
    assert!(
        repository
            .load(run)
            .await?
            .expect("created run")
            .entries()
            .is_empty()
    );
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cancellation_preserves_the_first_terminal_fault() -> Result<(), Box<dyn Error>> {
    use signalbox_domain::DurableCommandId;
    use signalbox_persistence::program_cancellation::{
        self as cancellation, CancelProgramRun, ProgramCancellationOutcome as Outcome,
        ProgramCancellationResult as Result, ProgramTerminalState,
    };
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    repository
        .append_delivery(
            run,
            DeliveryKind::Fault(ProgramFault::Timeout(payload(b"deadline"))),
        )
        .await?;
    let journal = repository.load(run).await?.expect("faulted run");
    let command = CancelProgramRun {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        run_id: run,
    };
    assert_eq!(
        cancellation::cancel(&pool, command.clone()).await?,
        Result::Recorded(Outcome::AlreadyTerminal(ProgramTerminalState::Faulted))
    );
    assert_eq!(
        cancellation::cancel(&pool, command).await?,
        Result::Recorded(Outcome::AlreadyTerminal(ProgramTerminalState::Faulted))
    );
    assert_eq!(repository.load(run).await?, Some(journal));
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn concurrent_cancellation_commands_append_only_one_terminal_delivery()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::DurableCommandId;
    use signalbox_persistence::program_cancellation::{
        self as cancellation, CancelProgramRun, ProgramCancellationOutcome as Outcome,
        ProgramCancellationResult as Result, ProgramTerminalState,
    };
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let first = CancelProgramRun {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        run_id: run,
    };
    let second = CancelProgramRun {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        run_id: run,
    };
    let (first, second) = tokio::join!(
        cancellation::cancel(&pool, first),
        cancellation::cancel(&pool, second)
    );
    assert!(matches!(
        (first?, second?),
        (
            Result::Recorded(Outcome::Applied),
            Result::Recorded(Outcome::AlreadyTerminal(ProgramTerminalState::Cancelled))
        ) | (
            Result::Recorded(Outcome::AlreadyTerminal(ProgramTerminalState::Cancelled)),
            Result::Recorded(Outcome::Applied)
        )
    ));
    assert_eq!(
        repository
            .load(run)
            .await?
            .expect("cancelled run")
            .entries()
            .len(),
        1
    );
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cancellation_receipt_requires_its_own_command_in_the_terminal_delivery()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    repository
        .append_delivery(run, DeliveryKind::RunCancel(payload(b"another command")))
        .await?;
    let command = Uuid::now_v7();
    let mut transaction = pool.begin().await?;
    sqlx::query("INSERT INTO durable_command(command_id, command_kind, storage_version, claimed_at, issuer_kind) VALUES ($1, 'cancel_program_run', 1, transaction_timestamp(), 'operator')")
        .bind(command).execute(&mut *transaction).await?;
    sqlx::query("INSERT INTO cancel_program_run_command(command_id, run_id, outcome, terminal_state, cancellation_position) VALUES ($1, $2, 'applied', 'cancelled', 1)")
        .bind(command).bind(run.into_uuid()).execute(&mut *transaction).await?;
    assert_trigger_error(
        transaction
            .commit()
            .await
            .expect_err("a foreign cancellation is not this command's effect"),
        "applied program cancellation requires its terminal delivery",
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// Arbitrary name/revision and exact bytes shared by registration tests.
fn registration_request(
    name: &str,
) -> signalbox_domain::program_registration::ProgramRegistrationRequest {
    use signalbox_domain::program_registration::{ProgramGrants, ProgramRegistrationRequest};
    ProgramRegistrationRequest {
        name: name.into(),
        revision: "fixture-revision".into(),
        source: b"// source\nexport {};".to_vec(),
        artifact: "export {};".into(),
        grants: ProgramGrants::new([ProgramCapability::Session]),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn registrations_distinguish_names_and_grants_and_pin_run_authority()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::program_registration::ProgramGrants;
    use signalbox_persistence::program_registration::ProgramRegistrationRepository;
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramRegistrationRepository::new(pool.clone());
    let first = repository
        .register_user(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            registration_request("first"),
        )
        .await?;
    let renamed = repository
        .register_user(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            registration_request("second"),
        )
        .await?;
    let mut changed_grants = registration_request(&first.content.name);
    changed_grants.revision = "distinct-revision".into();
    changed_grants.grants = ProgramGrants::new([ProgramCapability::Register]);
    let different_grants = repository
        .register_user(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            changed_grants,
        )
        .await?;
    assert_ne!(first.id, renamed.id);
    assert_ne!(first.id, different_grants.id);
    let signalbox_domain::program_registration::ProgramExecutable::JavaScript {
        source_digest,
        artifact,
    } = &first.content.executable
    else {
        panic!("JavaScript registration retains its artifact");
    };
    assert_ne!(
        *source_digest,
        signalbox_domain::program_registration::ProgramContentDigest::of(artifact.as_bytes())
    );
    assert_eq!(first.content.executable, renamed.content.executable);
    let run = repository
        .start_run(
            signalbox_domain::ProgramRunId::from_uuid(Uuid::now_v7()),
            first.id,
            &[],
        )
        .await?;
    assert_eq!(repository.for_run(run).await?, Some(first.clone()));
    assert!(
        sqlx::query(
            "UPDATE program_registration SET grants = ARRAY['register'] WHERE registration_id = $1"
        )
        .bind(first.id.into_uuid())
        .execute(&pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query("UPDATE program_run_registration SET registration_id = $1 WHERE run_id = $2")
            .bind(different_grants.id.into_uuid())
            .bind(run.into_uuid())
            .execute(&pool)
            .await
            .is_err()
    );
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn child_registration_refuses_widening_without_creating_a_registration()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::program_registration::ProgramGrants;
    use signalbox_persistence::program_registration::{
        ProgramRegistrationError, ProgramRegistrationRepository,
    };
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramRegistrationRepository::new(pool.clone());
    let mut parent_request = registration_request("parent");
    parent_request.grants =
        ProgramGrants::new([ProgramCapability::Register, ProgramCapability::Session]);
    let parent = repository
        .register_user(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            parent_request,
        )
        .await?;
    let run = repository
        .start_run(
            signalbox_domain::ProgramRunId::from_uuid(Uuid::now_v7()),
            parent.id,
            &[],
        )
        .await?;
    let mut child_request = registration_request("child");
    child_request.grants = ProgramGrants::new([ProgramCapability::Judge]);
    assert!(matches!(
        repository
            .register_child(
                run,
                signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                child_request.clone()
            )
            .await,
        Err(ProgramRegistrationError::GrantsDenied)
    ));
    child_request.grants = ProgramGrants::new([ProgramCapability::Session]);
    let child = repository
        .register_child(
            run,
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            child_request,
        )
        .await?;
    let child_run = repository
        .start_run(
            signalbox_domain::ProgramRunId::from_uuid(Uuid::now_v7()),
            child.id,
            &[],
        )
        .await?;
    let grandchild = registration_request("grandchild");
    assert!(matches!(
        repository
            .register_child(
                child_run,
                signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                grandchild
            )
            .await,
        Err(ProgramRegistrationError::GrantsDenied)
    ));
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn run_creation_retries_preserve_the_binding_and_journal() -> Result<(), Box<dyn Error>> {
    use signalbox_persistence::program_registration::{
        ProgramRegistrationError, ProgramRegistrationRepository,
    };
    let (_container, pool) = migrated_postgres().await?;
    let registrations = ProgramRegistrationRepository::new(pool.clone());
    let journal = ProgramJournalRepository::new(pool.clone());
    let first = registrations
        .register_user(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            registration_request("first"),
        )
        .await?;
    let other = registrations
        .register_user(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            registration_request("other"),
        )
        .await?;
    let run = ProgramRunId::from_uuid(Uuid::now_v7());
    assert_eq!(registrations.start_run(run, first.id, b"input").await?, run);
    journal
        .append_request(run, None, RequestKind::Now(payload(b"retained request")))
        .await?;
    assert_eq!(registrations.start_run(run, first.id, b"input").await?, run);
    assert!(
        matches!(registrations.start_run(run, other.id, b"input").await, Err(ProgramRegistrationError::RunConflict { run: conflict }) if conflict == run)
    );
    assert!(matches!(
        registrations.start_run(run, first.id, b"changed").await,
        Err(ProgramRegistrationError::RunConflict { .. })
    ));
    assert_eq!(
        registrations.input_for_run(run).await?,
        Some(payload(b"input"))
    );
    assert!(
        sqlx::query("UPDATE program_run_registration SET input = $2 WHERE run_id = $1")
            .bind(run.into_uuid())
            .bind(b"changed".as_slice())
            .execute(&pool)
            .await
            .is_err()
    );
    assert_eq!(registrations.for_run(run).await?, Some(first.clone()));
    assert_eq!(
        journal
            .load(run)
            .await?
            .expect("retained run")
            .entries()
            .len(),
        1
    );
    let bare = ProgramRunId::from_uuid(Uuid::now_v7());
    journal.create_stream(bare).await?;
    assert!(
        matches!(registrations.start_run(bare, first.id, b"input").await, Err(ProgramRegistrationError::RunConflict { run: conflict }) if conflict == bare)
    );
    assert!(registrations.for_run(bare).await?.is_none());
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn registration_lookup_distinguishes_absence_adoption_and_immutable_conflicts()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::ProgramRegistrationId;
    use signalbox_persistence::program_registration::{
        ProgramRegistrationError, ProgramRegistrationRepository,
    };
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramRegistrationRepository::new(pool.clone());
    let id = ProgramRegistrationId::from_uuid(Uuid::now_v7());
    let request = registration_request(&Uuid::now_v7().to_string());
    let content = request.clone().into_content();
    assert_eq!(repository.find(id, &content).await?, None);
    let stored = repository.register_user(id, request).await?;
    assert_eq!(repository.find(id, &content).await?, Some(stored.clone()));
    let other_id = ProgramRegistrationId::from_uuid(Uuid::now_v7());
    assert!(matches!(repository.find(other_id, &content).await,
        Err(ProgramRegistrationError::RegistrationConflict { registration }) if registration == other_id));
    let other_content = registration_request(&Uuid::now_v7().to_string()).into_content();
    assert!(matches!(repository.find(id, &other_content).await,
        Err(ProgramRegistrationError::RegistrationConflict { registration }) if registration == id));
    let mut changed = content.clone();
    changed.grants = signalbox_domain::program_registration::ProgramGrants::new([]);
    assert_ne!(changed, content);
    assert!(matches!(repository.find(id, &changed).await,
        Err(ProgramRegistrationError::RegistrationConflict { registration }) if registration == id));
    assert_eq!(repository.find(other_id, &other_content).await?, None);
    assert_eq!(repository.find(id, &content).await?, Some(stored));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM program_registration")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 1, "lookup never creates a missing registration");
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn registration_creation_reconciles_equal_retries_and_refuses_changed_content()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::ProgramRegistrationId;
    use signalbox_persistence::program_registration::{
        ProgramRegistrationError, ProgramRegistrationRepository,
    };
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramRegistrationRepository::new(pool.clone());
    let id = ProgramRegistrationId::from_uuid(Uuid::now_v7());
    let request = registration_request("retry-registration");
    let first = repository.register_user(id, request.clone()).await?;
    assert_eq!(repository.register_user(id, request.clone()).await?, first);
    let mut changed = request.clone();
    changed.source.push(b' ');
    assert!(
        matches!(repository.register_user(id, changed).await, Err(ProgramRegistrationError::RegistrationConflict { registration }) if registration == id)
    );
    let other = ProgramRegistrationId::from_uuid(Uuid::now_v7());
    assert!(
        matches!(repository.register_user(other, request.clone()).await, Err(ProgramRegistrationError::RegistrationConflict { registration }) if registration == other)
    );
    assert_eq!(repository.register_user(id, request).await?, first);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM program_registration")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 1);
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_refusal_preserves_outstanding_work_then_accepts_a_new_result()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let work = repository
        .append_request(run, None, RequestKind::Now(payload(b"clock")))
        .await?;
    let refused = repository
        .complete_if_tail(run, 1, payload(b"premature"))
        .await?
        .expect("terminal refusal");
    assert!(matches!(
        refused.kind(),
        DeliveryKind::Reject {
            reason: signalbox_domain::RejectReason::OutstandingRequests,
            ..
        }
    ));
    assert!(
        repository
            .load(run)
            .await?
            .expect("journal")
            .result()
            .is_none()
    );
    repository
        .append_delivery(
            run,
            DeliveryKind::Answer {
                resolves: work.ordinal(),
                payload: payload(b"time"),
            },
        )
        .await?;
    let result = payload(b"retained result");
    repository
        .complete_if_tail(run, 4, result.clone())
        .await?
        .expect("completion");
    let loaded = repository.load(run).await?.expect("journal");
    assert_eq!(loaded.result(), Some(&result));
    assert!(!loaded.has_outstanding_requests());
    assert!(
        repository
            .complete_if_tail(run, 6, payload(b"replacement"))
            .await?
            .is_none()
    );
    assert!(
        repository
            .append_request(run, None, RequestKind::Now(payload(b"late")))
            .await
            .is_err()
    );
    assert!(
        repository
            .append_delivery(run, DeliveryKind::RunCancel(payload(b"late")))
            .await
            .is_err()
    );
    assert_eq!(
        repository.load(run).await?.expect("retained journal"),
        loaded
    );
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_answer_cannot_adopt_a_request_emitted_with_outstanding_work()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let work = repository
        .append_request(run, None, RequestKind::Now(payload(b"clock")))
        .await?;
    let terminal = repository
        .append_request(run, None, RequestKind::Terminal(payload(b"premature")))
        .await?;
    let acceptance = DeliveryKind::Answer {
        resolves: terminal.ordinal(),
        payload: InlineFramePayload::default(),
    };
    assert!(
        repository
            .append_delivery(run, acceptance.clone())
            .await
            .is_err()
    );
    repository
        .append_delivery(
            run,
            DeliveryKind::Answer {
                resolves: work.ordinal(),
                payload: payload(b"time"),
            },
        )
        .await?;
    assert!(repository.append_delivery(run, acceptance).await.is_err());
    repository
        .append_delivery(
            run,
            DeliveryKind::Reject {
                resolves: terminal.ordinal(),
                reason: signalbox_domain::RejectReason::OutstandingRequests,
            },
        )
        .await?;
    assert!(
        repository
            .load(run)
            .await?
            .expect("journal")
            .result()
            .is_none()
    );
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_answer_rejects_work_appended_and_resolved_after_its_request()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let terminal = repository
        .append_request(run, None, RequestKind::Terminal(payload(b"result")))
        .await?;
    let work = repository
        .append_request(run, None, RequestKind::Now(payload(b"clock")))
        .await?;
    repository
        .append_delivery(
            run,
            DeliveryKind::Answer {
                resolves: work.ordinal(),
                payload: payload(b"time"),
            },
        )
        .await?;
    let before = repository.load(run).await?.expect("pending terminal");
    let error = repository
        .append_delivery(
            run,
            DeliveryKind::Answer {
                resolves: terminal.ordinal(),
                payload: InlineFramePayload::default(),
            },
        )
        .await
        .expect_err("intervening work prevents successful completion");
    let signalbox_persistence::program_journal::ProgramJournalRepositoryError::Database {
        source,
        ..
    } = error
    else {
        panic!("expected database rejection, got {error:?}");
    };
    assert_trigger_error(
        source,
        "terminal resolution requires an immediate answer without outstanding work or an outstanding-work rejection",
    );
    assert_eq!(
        repository.load(run).await?.expect("unchanged journal"),
        before
    );
    assert!(before.result().is_none());
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_request_rejects_non_contract_resolutions() -> Result<(), Box<dyn Error>> {
    use signalbox_domain::RejectReason;
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let terminal = repository
        .append_request(run, None, RequestKind::Terminal(payload(b"result")))
        .await?;
    let ordinal = terminal.ordinal();
    let before = repository.load(run).await?.expect("unresolved terminal");
    for kind in [
        DeliveryKind::Wake {
            resolves: ordinal,
            payload: InlineFramePayload::default(),
        },
        DeliveryKind::Cancel {
            resolves: ordinal,
            payload: InlineFramePayload::default(),
        },
        DeliveryKind::Reject {
            resolves: ordinal,
            reason: RejectReason::OutstandingRequests,
        },
        DeliveryKind::Reject {
            resolves: ordinal,
            reason: RejectReason::CapabilityDenied,
        },
        DeliveryKind::Reject {
            resolves: ordinal,
            reason: RejectReason::UnsupportedOperation,
        },
    ] {
        let error = repository
            .append_delivery(run, kind.clone())
            .await
            .expect_err("invalid terminal resolution");
        let signalbox_persistence::program_journal::ProgramJournalRepositoryError::Database {
            source,
            ..
        } = error
        else {
            panic!("expected database rejection for {kind:?}, got {error:?}");
        };
        assert_trigger_error(
            source,
            "terminal resolution requires an immediate answer without outstanding work or an outstanding-work rejection",
        );
        assert_eq!(
            repository.load(run).await?.expect("unchanged journal"),
            before,
            "{kind:?}"
        );
    }
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_rejection_cannot_count_work_appended_after_emission() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let terminal = repository
        .append_request(run, None, RequestKind::Terminal(payload(b"result")))
        .await?;
    repository
        .append_request(run, None, RequestKind::Now(payload(b"late work")))
        .await?;
    let before = repository.load(run).await?.expect("unresolved terminal");
    let error = repository
        .append_delivery(
            run,
            DeliveryKind::Reject {
                resolves: terminal.ordinal(),
                reason: signalbox_domain::RejectReason::OutstandingRequests,
            },
        )
        .await
        .expect_err("late work does not justify terminal rejection");
    let signalbox_persistence::program_journal::ProgramJournalRepositoryError::Database {
        source,
        ..
    } = error
    else {
        panic!("expected database rejection, got {error:?}");
    };
    assert_trigger_error(
        source,
        "terminal resolution requires an immediate answer without outstanding work or an outstanding-work rejection",
    );
    assert_eq!(
        repository.load(run).await?.expect("unchanged journal"),
        before
    );
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_answer_rejects_an_intervening_scope_frame() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let terminal = repository
        .append_request(run, None, RequestKind::Terminal(payload(b"result")))
        .await?;
    repository
        .append_request(
            run,
            None,
            RequestKind::Scope(ScopeRequest::new(
                ScopeOperation::Open,
                ScopeOrdinal::try_from_u64(1).expect("scope"),
                None,
            )),
        )
        .await?;
    let before = repository.load(run).await?.expect("pending terminal");
    let error = repository
        .append_delivery(
            run,
            DeliveryKind::Answer {
                resolves: terminal.ordinal(),
                payload: InlineFramePayload::default(),
            },
        )
        .await
        .expect_err("intervening scope prevents successful completion");
    let signalbox_persistence::program_journal::ProgramJournalRepositoryError::Database {
        source,
        ..
    } = error
    else {
        panic!("expected database rejection, got {error:?}");
    };
    assert_trigger_error(
        source,
        "terminal resolution requires an immediate answer without outstanding work or an outstanding-work rejection",
    );
    assert_eq!(
        repository.load(run).await?.expect("unchanged journal"),
        before
    );
    assert!(before.result().is_none());
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cancellation_after_success_replays_the_retained_result() -> Result<(), Box<dyn Error>> {
    use signalbox_persistence::program_cancellation::{
        self, CancelProgramRun, ProgramCancellationOutcome as Outcome,
        ProgramCancellationResult as Result, ProgramTerminalState as State,
    };
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let result = payload(b"durable result");
    repository
        .complete_if_tail(run, 0, result.clone())
        .await?
        .expect("success");
    let before = repository.load(run).await?.expect("journal");
    let command = CancelProgramRun {
        command_id: signalbox_domain::DurableCommandId::from_uuid(Uuid::now_v7()),
        run_id: run,
    };
    let receipt = program_cancellation::cancel(&pool, command.clone()).await?;
    assert_eq!(
        receipt,
        Result::Recorded(Outcome::AlreadyTerminal(State::Succeeded(result)))
    );
    assert_eq!(program_cancellation::cancel(&pool, command).await?, receipt);
    assert_eq!(repository.load(run).await?.expect("journal"), before);
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cancel_and_success_race_settles_one_durable_outcome() -> Result<(), Box<dyn Error>> {
    use signalbox_persistence::program_cancellation::{
        self, CancelProgramRun, ProgramCancellationOutcome as Outcome,
        ProgramCancellationResult as Result, ProgramTerminalState as State,
    };
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let result = payload(b"race result");
    let command = CancelProgramRun {
        command_id: signalbox_domain::DurableCommandId::from_uuid(Uuid::now_v7()),
        run_id: run,
    };
    let (completed, cancelled) = tokio::join!(
        repository.complete_if_tail(run, 0, result.clone()),
        program_cancellation::cancel(&pool, command.clone())
    );
    let completed = completed?;
    let cancelled = cancelled?;
    let journal = repository.load(run).await?.expect("journal");
    if completed.is_some() {
        assert_eq!(
            cancelled,
            Result::Recorded(Outcome::AlreadyTerminal(State::Succeeded(result.clone())))
        );
        assert_eq!(journal.result(), Some(&result));
        assert_eq!(journal.entries().len(), 2);
    } else {
        assert_eq!(cancelled, Result::Recorded(Outcome::Applied));
        assert!(matches!(
            journal.terminal_delivery().expect("terminal").kind(),
            DeliveryKind::RunCancel(_)
        ));
        assert!(journal.result().is_none());
        assert_eq!(journal.entries().len(), 1);
    }
    assert_eq!(
        program_cancellation::cancel(&pool, command).await?,
        cancelled
    );
    assert_eq!(repository.load(run).await?.expect("journal"), journal);
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cancellation_prevents_accepting_a_pending_terminal_request() -> Result<(), Box<dyn Error>>
{
    use signalbox_persistence::program_cancellation::{
        self, CancelProgramRun, ProgramCancellationOutcome, ProgramCancellationResult,
    };
    let (_container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let terminal = repository
        .append_request(
            run,
            None,
            RequestKind::Terminal(payload(b"unaccepted result")),
        )
        .await?;
    let command = CancelProgramRun {
        command_id: signalbox_domain::DurableCommandId::from_uuid(Uuid::now_v7()),
        run_id: run,
    };
    assert_eq!(
        program_cancellation::cancel(&pool, command).await?,
        ProgramCancellationResult::Recorded(ProgramCancellationOutcome::Applied)
    );
    assert!(
        repository
            .append_delivery(
                run,
                DeliveryKind::Answer {
                    resolves: terminal.ordinal(),
                    payload: InlineFramePayload::default()
                }
            )
            .await
            .is_err()
    );
    assert!(
        repository
            .complete_if_tail(run, 2, payload(b"late success"))
            .await?
            .is_none()
    );
    let journal = repository.load(run).await?.expect("cancelled journal");
    assert!(matches!(
        journal.terminal_delivery().expect("terminal").kind(),
        DeliveryKind::RunCancel(_)
    ));
    assert!(journal.result().is_none());
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unfinished_registered_runs_exclude_terminal_outcomes_and_bare_streams()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::ProgramRegistrationId;
    use signalbox_persistence::program_registration::ProgramRegistrationRepository;

    let (_database, pool) = migrated_postgres().await?;
    let registrations = ProgramRegistrationRepository::new(pool.clone());
    let journal = ProgramJournalRepository::new(pool.clone());
    let registration = registrations
        .register_user(
            ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            registration_request("recovery"),
        )
        .await?;
    let empty = ProgramRunId::from_uuid(Uuid::now_v7());
    let waiting = ProgramRunId::from_uuid(Uuid::now_v7());
    let succeeded = ProgramRunId::from_uuid(Uuid::now_v7());
    let cancelled = ProgramRunId::from_uuid(Uuid::now_v7());
    let faulted = ProgramRunId::from_uuid(Uuid::now_v7());
    for run in [empty, waiting, succeeded, cancelled, faulted] {
        registrations
            .start_run(run, registration.id, b"input")
            .await?;
    }
    journal
        .create_stream(ProgramRunId::from_uuid(Uuid::now_v7()))
        .await?;
    journal
        .append_request(waiting, None, RequestKind::Now(payload(b"clock")))
        .await?;
    journal
        .complete_if_tail(succeeded, 0, payload(b"result"))
        .await?;
    journal
        .append_delivery(cancelled, DeliveryKind::RunCancel(payload(b"cancel")))
        .await?;
    journal
        .append_delivery(
            faulted,
            DeliveryKind::Fault(ProgramFault::ProgramError(payload(b"fault"))),
        )
        .await?;
    assert_eq!(registrations.unfinished_runs().await?, vec![empty, waiting]);
    pool.close().await;
    Ok(())
}
