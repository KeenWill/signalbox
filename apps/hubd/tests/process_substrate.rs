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

/// A successor cannot advance its durable generation until every prior
/// application-pool session releases its shared generation lock.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn successor_waits_for_every_prior_fenced_pool_session() -> Result<(), Box<dyn Error>> {
    let (container, bootstrap, database_url) = postgres().await?;
    bootstrap.close().await;
    let options = local_test_connection_options(&database_url)?;
    let first = FencedHubDatabase::connect_with(options.clone()).await?;
    assert_eq!(first.generation().get(), 2);
    let (guard, prior_pool, _generation) = first.into_parts();
    let prior_checkout = prior_pool.acquire().await?;

    guard.close().await?;
    let successor = FencedHubDatabase::connect_with(options);
    tokio::pin!(successor);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut successor)
            .await
            .is_err()
    );

    drop(prior_checkout);
    prior_pool.close().await;
    let successor = tokio::time::timeout(std::time::Duration::from_secs(10), successor).await??;
    assert_eq!(successor.generation().get(), 3);
    let (guard, pool, _generation) = successor.into_parts();
    pool.close().await;
    guard.close().await?;
    drop(container);
    Ok(())
}
