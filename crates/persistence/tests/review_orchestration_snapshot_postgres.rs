#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

//! The time bounds on the coherent review snapshot's shared table locks.
//!
//! The snapshot locks tables ordinary turn processing writes, so every phase
//! that can hold or queue those locks carries its own bound. These tests hold a
//! conflicting writer open and observe what the snapshot does with the wait.

mod support;

use std::{error::Error, time::Duration};

use signalbox_application::ReviewOrchestrationAttemptId;
use signalbox_persistence::{
    local_test_connection_options, migrate,
    review_orchestration::{PostgresReviewOrchestrationStore, ReviewOrchestrationStoreError},
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use support::blocked_backends_reached;
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_review_snapshot";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const ABSENT_ATTEMPT: u128 = 0x5_0a17;
/// The first table in the snapshot's lock inventory, so a writer holding it
/// blocks the snapshot before it has acquired any of the others.
const FIRST_LOCKED_TABLE: &str = "LOCK TABLE accepted_input IN ROW EXCLUSIVE MODE";
/// Longer than one expired snapshot lock wait and shorter than the admission
/// bound, so this wait resolves only if the snapshot stops queueing in between.
const WRITER_LOCK_WAIT: &str = "3s";
/// Comfortably past the five-second admission bound, so an expiry that the
/// snapshot reports itself is never mistaken for the harness giving up.
const ADMISSION_OBSERVATION_CEILING: Duration = Duration::from_secs(30);

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_fsync_enabled()
        .with_tag(POSTGRES_IMAGE_TAG)
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

fn absent_attempt() -> ReviewOrchestrationAttemptId {
    ReviewOrchestrationAttemptId::from_uuid(Uuid::from_u128(ABSENT_ATTEMPT))
}

/// A snapshot that never wins its locks reports the expiry and leaves the pool
/// usable, rather than waiting on the writer for as long as the writer lives.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_snapshot_admission_expires_and_the_pool_recovers()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresReviewOrchestrationStore::new(pool.clone());
    let mut writer = pool.begin().await?;
    sqlx::query(FIRST_LOCKED_TABLE)
        .execute(&mut *writer)
        .await?;

    let expiry = tokio::time::timeout(
        ADMISSION_OBSERVATION_CEILING,
        store.load_snapshot(absent_attempt()),
    )
    .await
    .expect("the snapshot reports its own admission expiry")
    .expect_err("a held conflicting lock denies snapshot admission");

    assert!(matches!(
        expiry,
        ReviewOrchestrationStoreError::SnapshotAdmissionTimedOut
    ));
    writer.rollback().await?;
    assert!(
        tokio::time::timeout(
            ADMISSION_OBSERVATION_CEILING,
            store.load_snapshot(absent_attempt())
        )
        .await
        .expect("a released writer readmits the snapshot")?
        .is_none()
    );
    Ok(())
}

/// A writer that arrives while a snapshot is queued for the same table is not
/// held behind that queued request for the whole admission window: the
/// snapshot's own lock wait expires and releases the queue between attempts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_writer_arriving_behind_a_queued_snapshot_is_not_held_for_the_admission_window()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresReviewOrchestrationStore::new(pool.clone());
    let mut holder = pool.begin().await?;
    sqlx::query(FIRST_LOCKED_TABLE)
        .execute(&mut *holder)
        .await?;
    let snapshot = tokio::spawn(async move { store.load_snapshot(absent_attempt()).await });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the snapshot queues behind the writer holding the first locked table"
    );

    let mut arriving = pool.begin().await?;
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(WRITER_LOCK_WAIT)
        .execute(&mut *arriving)
        .await?;
    let admitted = sqlx::query(FIRST_LOCKED_TABLE)
        .execute(&mut *arriving)
        .await;

    assert!(
        admitted.is_ok(),
        "a writer must not wait out the snapshot's admission window: {admitted:?}"
    );
    arriving.rollback().await?;
    holder.rollback().await?;
    let expiry = tokio::time::timeout(ADMISSION_OBSERVATION_CEILING, snapshot)
        .await
        .expect("the snapshot task finishes within the observation ceiling")?;
    assert!(matches!(
        expiry,
        Err(ReviewOrchestrationStoreError::SnapshotAdmissionTimedOut)
    ));
    Ok(())
}
