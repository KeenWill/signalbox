//! PostgreSQL integration coverage for durable program journals.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::error::Error;

use signalbox_domain::{
    DeliveryKind, InlineFramePayload, JournalFrame, ProgramRunId, ReplayCursor, ReplayInstruction,
    ReplayedRequest, RequestKind,
};
use signalbox_persistence::{
    disposable_test_container_labels, local_test_connection_options, migrate,
    program_journal::ProgramJournalRepository,
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
        .with_fsync_enabled()
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

/// INV-064: durable request and delivery projections retain one exact interleaving.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv064_journal_round_trip_preserves_concurrent_delivery_order()
-> Result<(), Box<dyn Error>> {
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

/// INV-065: persisted nondeterminism evidence retains both complete request frames.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv065_nondeterminism_fault_round_trips_both_frames() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = ProgramJournalRepository::new(pool.clone());
    let run = run_id();
    repository.create_stream(run).await?;
    let recorded = repository
        .append_request(run, None, RequestKind::Sleep(payload(b"recorded")))
        .await?;
    let loaded = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    let mut replay = ReplayCursor::new(loaded);
    let observed = signalbox_domain::RequestFrame::new(
        recorded.ordinal(),
        recorded.scope(),
        RequestKind::Sleep(payload(b"different")),
    );
    let divergence = replay
        .submit_request(observed.clone())
        .expect_err("different canonical request bytes must diverge");
    let fault = repository
        .append_nondeterminism_fault(run, divergence)
        .await?;

    let reloaded = repository
        .load(run)
        .await?
        .expect("the created journal stream exists");
    let last = reloaded
        .entries()
        .last()
        .expect("the persisted fault is present");

    assert_eq!(last.frame(), &JournalFrame::Delivery(fault),);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-066: committed journal frames cannot be updated or deleted.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv066_journal_frames_are_append_only() -> Result<(), Box<dyn Error>> {
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

    assert!(update.is_err());

    pool.close().await;
    drop(container);
    Ok(())
}
