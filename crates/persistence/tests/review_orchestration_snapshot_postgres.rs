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
/// The last table in the inventory, so a writer holding it blocks the snapshot
/// only after it has already acquired every other lock in the same statement.
const LAST_LOCKED_TABLE: &str = "LOCK TABLE review_target IN ROW EXCLUSIVE MODE";
/// Longer than the five-second admission bound, so this writer resolves when
/// the denied acquisition releases its locks rather than by giving up first.
const WRITER_LOCK_WAIT: &str = "15s";
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

/// Runs a snapshot against a held `contended` lock and requires a writer that
/// arrives for the inventory's first table to be admitted while the snapshot is
/// still denied.
///
/// The holder keeps its lock until the snapshot has reported its expiry, so the
/// writer's admission never depends on which side finished first.
async fn writer_is_admitted_beside_a_denied_snapshot(
    contended: &'static str,
) -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresReviewOrchestrationStore::new(pool.clone());
    let mut holder = pool.begin().await?;
    sqlx::query(contended).execute(&mut *holder).await?;
    let snapshot = tokio::spawn(async move { store.load_snapshot(absent_attempt()).await });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the snapshot blocks on the lock the holder took"
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
    let expiry = tokio::time::timeout(ADMISSION_OBSERVATION_CEILING, snapshot)
        .await
        .expect("the snapshot task finishes within the observation ceiling")?;
    assert!(matches!(
        expiry,
        Err(ReviewOrchestrationStoreError::SnapshotAdmissionTimedOut)
    ));
    holder.rollback().await?;
    Ok(())
}

/// A writer queued behind a snapshot's own queued request is released when that
/// acquisition runs out the admission bound, rather than when the daemon
/// happens to abandon the request.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_writer_queued_behind_a_denied_snapshot_is_released_on_the_admission_bound()
-> Result<(), Box<dyn Error>> {
    writer_is_admitted_beside_a_denied_snapshot(FIRST_LOCKED_TABLE).await
}

/// The same holds for a table the blocked snapshot has *already* locked.
///
/// Blocking the snapshot on the inventory's last table leaves it holding shared
/// locks on all the earlier ones while it waits, so a writer contending an
/// earlier table is released only because the bound covers the whole
/// multi-table statement rather than each acquisition within it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_writer_contending_a_lock_the_blocked_snapshot_already_holds_is_still_released()
-> Result<(), Box<dyn Error>> {
    writer_is_admitted_beside_a_denied_snapshot(LAST_LOCKED_TABLE).await
}
