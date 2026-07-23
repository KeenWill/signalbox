#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

use std::error::Error;

use signalbox_hubd::{FencedHubDatabase, SingleHubGuard, SingleHubGuardError};
use signalbox_persistence::local_test_connection_options;
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_hub_guard";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

async fn postgres() -> Result<(ContainerAsync<Postgres>, PgPool, String), Box<dyn Error>> {
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
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    Ok((container, pool, database_url))
}

/// The fixed session advisory guard admits one hub, refuses an overlap, and
/// releases only when its dedicated connection closes.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn single_hub_guard_is_exclusive_for_the_database() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = postgres().await?;
    let guard = SingleHubGuard::acquire(&pool).await?;

    assert!(matches!(
        SingleHubGuard::acquire(&pool).await,
        Err(SingleHubGuardError::AlreadyRunning)
    ));

    guard.close().await?;
    let replacement = SingleHubGuard::acquire(&pool).await?;
    replacement.close().await?;
    pool.close().await;
    drop(container);
    Ok(())
}

/// Losing the exact PostgreSQL session is observable; the guard does not
/// reconnect or reacquire behind the runtime's back.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn single_hub_guard_loss_is_observable() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = postgres().await?;
    let mut guard = SingleHubGuard::acquire(&pool).await?;

    container.stop().await?;

    assert!(matches!(
        guard.check().await,
        Err(SingleHubGuardError::GuardLost(_))
    ));
    Ok(())
}

/// Explicit fenced-database shutdown drains every physical pool checkout before
/// the singleton guard becomes acquirable.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn fenced_database_close_drains_pool_before_guard_release() -> Result<(), Box<dyn Error>> {
    let (container, control_pool, database_url) = postgres().await?;
    let options = local_test_connection_options(&database_url)?;
    let database = FencedHubDatabase::connect_with(options).await?;
    let pool = database.pool().clone();
    let checkout_a = pool.acquire().await?;
    let checkout_b = pool.acquire().await?;
    let close = database.close();
    tokio::pin!(close);

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut close)
            .await
            .is_err()
    );
    assert!(matches!(
        SingleHubGuard::acquire(&control_pool).await,
        Err(SingleHubGuardError::AlreadyRunning)
    ));

    drop(checkout_a);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut close)
            .await
            .is_err()
    );
    assert!(matches!(
        SingleHubGuard::acquire(&control_pool).await,
        Err(SingleHubGuardError::AlreadyRunning)
    ));

    drop(checkout_b);
    close.await?;
    let replacement = SingleHubGuard::acquire(&control_pool).await?;
    replacement.close().await?;
    control_pool.close().await;
    drop(container);
    Ok(())
}

/// Omitting explicit fenced-database shutdown fails closed: dropping the owner
/// never releases the singleton guard while escaped raw pool handles may exist.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn implicit_fenced_database_drop_retains_guard() -> Result<(), Box<dyn Error>> {
    let (container, control_pool, database_url) = postgres().await?;
    let options = local_test_connection_options(&database_url)?;
    let database = FencedHubDatabase::connect_with(options).await?;
    let escaped_pool = database.pool().clone();
    let checkout = escaped_pool.acquire().await?;

    drop(database);
    assert!(matches!(
        SingleHubGuard::acquire(&control_pool).await,
        Err(SingleHubGuardError::AlreadyRunning)
    ));

    drop(checkout);
    escaped_pool.close().await;
    assert!(matches!(
        SingleHubGuard::acquire(&control_pool).await,
        Err(SingleHubGuardError::AlreadyRunning)
    ));

    control_pool.close().await;
    container.stop().await?;
    Ok(())
}

/// A successor cannot advance its durable generation until every prior
/// application-pool session releases its shared generation lock.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn successor_waits_for_every_prior_fenced_pool_session() -> Result<(), Box<dyn Error>> {
    let (container, control_pool, database_url) = postgres().await?;
    let options = local_test_connection_options(&database_url)?;
    let first = FencedHubDatabase::connect_with(options.clone()).await?;
    assert_eq!(first.generation().get(), 2);
    let prior_pool = first.pool().clone();
    let mut prior_checkout_a = prior_pool.acquire().await?;
    let mut prior_checkout_b = prior_pool.acquire().await?;
    let prior_backend_a: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *prior_checkout_a)
        .await?;
    let prior_backend_b: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *prior_checkout_b)
        .await?;
    assert_ne!(prior_backend_a, prior_backend_b);

    let guard_backend: i32 = sqlx::query_scalar(
        "SELECT pid
           FROM pg_locks
          WHERE locktype = 'advisory'
            AND classid = 1396856881
            AND objid = 1213547057
            AND objsubid = 2
            AND granted",
    )
    .fetch_one(&control_pool)
    .await?;
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(guard_backend)
        .fetch_one(&control_pool)
        .await?;
    assert!(terminated);

    let successor = FencedHubDatabase::connect_with(options);
    tokio::pin!(successor);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut successor)
            .await
            .is_err()
    );

    let close_prior = first.close();
    tokio::pin!(close_prior);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut close_prior)
            .await
            .is_err()
    );

    drop(prior_checkout_a);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut close_prior)
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut successor)
            .await
            .is_err()
    );

    drop(prior_checkout_b);
    let _guard_close_result = close_prior.await;
    let successor = tokio::time::timeout(std::time::Duration::from_secs(10), successor).await??;
    assert_eq!(successor.generation().get(), 3);
    successor.close().await?;
    control_pool.close().await;
    drop(container);
    Ok(())
}
