#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

use std::error::Error;

use signalbox_persistence::{
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options,
};
use signalboxd::{
    FencedHubDatabase, FencedPoolFloorReconciliation, SingleHubGuard, SingleHubGuardError,
    reconcile_fenced_pool_floor,
};
use sqlx::{Connection, PgPool, postgres::PgPoolOptions};
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
    let database = FencedHubDatabase::connect_with(options, None).await?;
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

/// A retired physical session is restored to the deployment floor without
/// replacing the fenced pool or its generation.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn fenced_pool_floor_reconciliation_reopens_retired_sessions() -> Result<(), Box<dyn Error>> {
    const FLOOR: u32 = 4;
    let (container, control_pool, database_url) = postgres().await?;
    let options = local_test_connection_options(&database_url)?;
    let database = FencedHubDatabase::connect_with(options, None).await?;
    let pool = database.pool().clone();
    let mut warm_a = pool.acquire().await?;
    let mut warm_b = pool.acquire().await?;
    let mut warm_c = pool.acquire().await?;
    let retired = pool.acquire().await?.detach();
    retired.close().await?;
    warm_a.return_to_pool().await;
    warm_b.return_to_pool().await;
    warm_c.return_to_pool().await;

    assert_eq!(pool.size(), FLOOR - 1);

    assert_eq!(
        reconcile_fenced_pool_floor(&pool, FLOOR).await?,
        FencedPoolFloorReconciliation::DeferredForIdleCapacity
    );
    assert_eq!(pool.size(), FLOOR - 1);

    let mut checkout_a = pool.acquire().await?;
    let mut checkout_b = pool.acquire().await?;
    let mut checkout_c = pool.acquire().await?;

    assert_eq!(pool.num_idle(), 0);
    assert_eq!(
        reconcile_fenced_pool_floor(&pool, FLOOR).await?,
        FencedPoolFloorReconciliation::Replenished
    );

    assert_eq!(pool.size(), FLOOR);
    checkout_a.return_to_pool().await;
    checkout_b.return_to_pool().await;
    checkout_c.return_to_pool().await;
    database.close().await?;
    control_pool.close().await;
    drop(container);
    Ok(())
}

/// Explicit fenced-database shutdown cannot release the singleton guard while
/// a connection detached from SQLx pool accounting retains generation
/// authority.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn fenced_database_close_waits_for_a_detached_pool_session() -> Result<(), Box<dyn Error>> {
    let (container, control_pool, database_url) = postgres().await?;
    let options = local_test_connection_options(&database_url)?;
    let database = FencedHubDatabase::connect_with(options, None).await?;
    let detached = database.pool().acquire().await?.detach();
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

    detached.close().await?;
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
    let database = FencedHubDatabase::connect_with(options, None).await?;
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
    let first = FencedHubDatabase::connect_with(options.clone(), None).await?;
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

    let successor = FencedHubDatabase::connect_with(options, None);
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

/// Guard loss releases the drained incarnation and reacquisition advances the fence.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn guard_recovery_rebuilds_a_fenced_pool_after_loss() -> Result<(), Box<dyn Error>> {
    let (container, control_pool, database_url) = postgres().await?;
    let options = local_test_connection_options(&database_url)?;
    let mut first = FencedHubDatabase::connect_with(options.clone(), None).await?;
    let previous_generation = first.generation();
    let old_pool = first.pool().clone();
    let guard_backend: i32 = sqlx::query_scalar(
        "SELECT DISTINCT pid FROM pg_locks
         WHERE locktype = 'advisory' AND mode = 'ExclusiveLock' AND granted
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
    )
    .fetch_one(&control_pool)
    .await?;
    sqlx::query("SELECT pg_terminate_backend($1)")
        .bind(guard_backend)
        .execute(&control_pool)
        .await?;

    assert!(matches!(
        first.check_guard().await,
        Err(SingleHubGuardError::GuardLost(_))
    ));
    assert!(first.close().await.is_err());
    assert!(old_pool.is_closed());
    let mut recovered = FencedHubDatabase::connect_with(options, None).await?;
    assert!(recovered.generation().get() > previous_generation.get());
    recovered.check_guard().await?;
    assert!(matches!(
        SingleHubGuard::acquire(&control_pool).await,
        Err(SingleHubGuardError::AlreadyRunning)
    ));
    recovered.close().await?;
    control_pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn guard_loss_during_pool_drain_starts_the_recovery_elapsed_bound()
-> Result<(), Box<dyn Error>> {
    use signalboxd::guard_recovery::{
        GuardRecoveryPolicy, GuardRecoveryStop, GuardedIncarnationOutcome, run_guarded_incarnations,
    };
    use std::time::Duration;
    let (container, control_pool, database_url) = postgres().await?;
    let database =
        FencedHubDatabase::connect_with(local_test_connection_options(&database_url)?, None)
            .await?;
    let checkout = database.pool().acquire().await?;
    let guard_backend: i32 = sqlx::query_scalar(
        "SELECT DISTINCT pid FROM pg_locks
         WHERE locktype = 'advisory' AND mode = 'ExclusiveLock' AND granted
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
    )
    .fetch_one(&control_pool)
    .await?;
    let mut database = Some(database);
    let recovery = run_guarded_incarnations(
        GuardRecoveryPolicy::new(
            Duration::from_millis(10),
            Duration::from_millis(20),
            Some(Duration::from_millis(100)),
        )
        .unwrap(),
        |observer| {
            let database = database.take().unwrap().with_recovery_observer(observer);
            async move { GuardedIncarnationOutcome::Finished(database.close().await) }
        },
        std::future::pending(),
    );
    tokio::pin!(recovery);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut recovery)
            .await
            .is_err(),
        "a healthy guard does not impose a recovery bound on pool drain"
    );
    sqlx::query("SELECT pg_terminate_backend($1)")
        .bind(guard_backend)
        .execute(&control_pool)
        .await?;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(10), &mut recovery).await?,
        Err(GuardRecoveryStop::ElapsedBoundExhausted)
    ));
    checkout.close().await?;
    control_pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn shutdown_interrupts_a_lost_guard_pool_drain_without_an_elapsed_bound()
-> Result<(), Box<dyn Error>> {
    use signalboxd::guard_recovery::{
        GuardRecoveryPolicy, GuardRecoveryStop, GuardedIncarnationOutcome, run_guarded_incarnations,
    };
    use std::time::Duration;
    let (container, control_pool, database_url) = postgres().await?;
    let database =
        FencedHubDatabase::connect_with(local_test_connection_options(&database_url)?, None)
            .await?;
    let checkout = database.pool().acquire().await?;
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_locks
         WHERE locktype = 'advisory' AND mode = 'ExclusiveLock' AND granted
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
    )
    .execute(&control_pool)
    .await?;
    let mut database = Some(database);
    let recovery = run_guarded_incarnations(
        GuardRecoveryPolicy::new(Duration::from_millis(10), Duration::from_millis(20), None)
            .unwrap(),
        |observer| {
            let database = database.take().unwrap().with_recovery_observer(observer);
            async move { GuardedIncarnationOutcome::Finished(database.close().await) }
        },
        tokio::time::sleep(Duration::from_millis(100)),
    );
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(10), recovery).await?,
        Err(GuardRecoveryStop::ShutdownRequested)
    ));
    checkout.close().await?;
    control_pool.close().await;
    drop(container);
    Ok(())
}

async fn observe_construction_guard_loss(
    control: &PgPool,
    database_url: &str,
    blocker_pid: i32,
) -> Result<(), Box<dyn Error>> {
    use signalboxd::guard_recovery::{
        GuardRecoveryPolicy, GuardedIncarnationOutcome, run_guarded_incarnations,
    };
    use std::time::Duration;

    let options =
        local_test_connection_options(database_url)?.application_name("construction-under-test");
    let construction = run_guarded_incarnations(
        GuardRecoveryPolicy::new(Duration::from_millis(10), Duration::from_millis(20), None)
            .expect("positive ascending recovery delays are valid"),
        |observer| {
            let options = options.clone();
            async move {
                assert!(!observer.is_recovering());
                let result = FencedHubDatabase::connect_with_observer(
                    options,
                    Some(1),
                    Some(observer.clone()),
                )
                .await;
                assert!(
                    matches!(
                        result,
                        Err(signalboxd::FencedHubDatabaseError::GuardLost(_))
                    ),
                    "{result:?}"
                );
                assert!(
                    observer.is_recovering(),
                    "construction must start recovery on the first incarnation"
                );
                GuardedIncarnationOutcome::Finished(())
            }
        },
        std::future::pending(),
    );
    let terminate = async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                 WHERE application_name = 'construction-under-test'
                   AND $1 = ANY(pg_blocking_pids(pid)))",
            )
            .bind(blocker_pid)
            .fetch_one(control)
            .await?;
            if blocked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let guard_pid: i32 = sqlx::query_scalar(
            "SELECT DISTINCT l.pid FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
             WHERE a.application_name = 'construction-under-test'
               AND l.locktype = 'advisory' AND l.objsubid = 2 AND l.granted",
        )
        .fetch_one(control)
        .await?;
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(guard_pid)
            .execute(control)
            .await?;
        Ok::<_, sqlx::Error>(())
    };
    let (constructed, terminated) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(construction, terminate)
    })
    .await?;
    assert_eq!(constructed, Ok(()));
    terminated?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn guard_loss_during_fence_initialization_starts_recovery() -> Result<(), Box<dyn Error>> {
    use sqlx::migrate::Migrate;
    let (_container, control, url) = postgres().await?;
    let mut blocker = control.acquire().await?;
    blocker.lock().await?;
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await?;

    observe_construction_guard_loss(&control, &url, blocker_pid).await?;

    blocker.unlock().await?;
    drop(blocker);
    let recovered =
        FencedHubDatabase::connect_with(local_test_connection_options(&url)?, None).await?;
    recovered.close().await?;
    control.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn guard_loss_during_fence_advance_starts_recovery() -> Result<(), Box<dyn Error>> {
    let (_container, control, url) = postgres().await?;
    signalbox_persistence::hub_fence::initialize_hub_fence(&control).await?;
    let mut blocker = control.begin().await?;
    sqlx::query("SELECT generation FROM hub_fence_state FOR UPDATE")
        .execute(&mut *blocker)
        .await?;
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await?;

    observe_construction_guard_loss(&control, &url, blocker_pid).await?;

    blocker.rollback().await?;
    let recovered =
        FencedHubDatabase::connect_with(local_test_connection_options(&url)?, None).await?;
    recovered.close().await?;
    control.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn guard_loss_during_fenced_pool_construction_starts_recovery() -> Result<(), Box<dyn Error>>
{
    let (_container, control, url) = postgres().await?;
    signalbox_persistence::hub_fence::initialize_hub_fence(&control).await?;
    // hub_fence::advisory_key uses this namespace on both halves of the generation key.
    let namespace: i64 = 1_396_852_273;
    let mut blocker = control.acquire().await?;
    sqlx::query(
        "SELECT pg_advisory_lock((generation::bigint + 1) # (($1 << 32) | $1)) FROM hub_fence_state",
    ).bind(namespace).execute(&mut *blocker).await?;
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await?;

    observe_construction_guard_loss(&control, &url, blocker_pid).await?;

    sqlx::query("SELECT pg_advisory_unlock_all()")
        .execute(&mut *blocker)
        .await?;
    drop(blocker);
    let recovered =
        FencedHubDatabase::connect_with(local_test_connection_options(&url)?, None).await?;
    recovered.close().await?;
    control.close().await;
    Ok(())
}
