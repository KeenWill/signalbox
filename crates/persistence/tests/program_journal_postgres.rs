//! PostgreSQL integration coverage for durable program journals.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::error::Error;

use signalbox_domain::{
    DeliveryKind, EffectRequest, InlineFramePayload, JournalFrame, ProgramCapability, ProgramFault,
    ProgramRunId, ReplayCursor, ReplayInstruction, ReplayedRequest, RequestFrame, RequestKind,
    ScopeOperation, ScopeOrdinal, ScopeRequest,
};
use signalbox_persistence::{
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
    program_journal::{ProgramJournalCorruption, ProgramJournalRepository},
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use uuid::Uuid;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_program_journal";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const RUN_ID: u128 = 0x5100_0100;

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_cmd(disposable_postgres_server_args())
        .with_mount(disposable_postgres_state_tmpfs_from_example()?)
        .with_tag(POSTGRES_IMAGE_TAG)
        .with_labels(disposable_test_container_labels())
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
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
