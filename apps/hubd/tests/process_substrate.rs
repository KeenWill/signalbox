#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

use std::error::Error;

use signalbox_hubd::{SingleHubGuard, SingleHubGuardError};
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

async fn postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
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
    Ok((container, pool))
}

/// S24: the fixed session advisory guard admits one hub, refuses an overlap,
/// and releases only when its dedicated connection closes.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_single_hub_guard_is_exclusive_for_the_database() -> Result<(), Box<dyn Error>> {
    let (container, pool) = postgres().await?;
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

/// S24: losing the exact PostgreSQL session is fatal evidence; the guard does
/// not reconnect or reacquire behind the runtime's back.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s24_single_hub_guard_loss_is_observable() -> Result<(), Box<dyn Error>> {
    let (container, pool) = postgres().await?;
    let mut guard = SingleHubGuard::acquire(&pool).await?;

    container.stop().await?;

    assert!(matches!(
        guard.check().await,
        Err(SingleHubGuardError::GuardLost(_))
    ));
    Ok(())
}
