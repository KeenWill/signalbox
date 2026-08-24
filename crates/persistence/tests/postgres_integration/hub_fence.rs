//! Database-backed coverage for production hub-fence pool construction.

use crate::*;
use signalbox_persistence::hub_fence::{
    FENCED_POOL_MAX_CONNECTIONS, FENCED_POOL_MIN_CONNECTIONS, advance_hub_fence,
    initialize_hub_fence,
};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn connect_pool_uses_production_fenced_pool_capacity() -> Result<(), Box<dyn Error>> {
    let (container, bootstrap_pool, database_url) = unmigrated_postgres().await?;
    initialize_hub_fence(&bootstrap_pool).await?;
    let mut guard_connection = bootstrap_pool.acquire().await?;
    let mut advanced_fence = advance_hub_fence(&mut guard_connection).await?;
    let fenced_pool = advanced_fence
        .connect_pool(local_test_connection_options(&database_url)?)
        .await?;

    assert_eq!(
        fenced_pool.options().get_max_connections(),
        FENCED_POOL_MAX_CONNECTIONS
    );
    assert_eq!(
        fenced_pool.options().get_min_connections(),
        FENCED_POOL_MIN_CONNECTIONS
    );
    assert_eq!(fenced_pool.size(), FENCED_POOL_MIN_CONNECTIONS);

    fenced_pool.close().await;
    drop(advanced_fence);
    drop(guard_connection);
    bootstrap_pool.close().await;
    drop(container);
    Ok(())
}
