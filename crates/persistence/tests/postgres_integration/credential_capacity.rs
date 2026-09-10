//! Durable capacity observations follow the credential that served the call.

use std::time::{Duration, SystemTime};

use crate::*;
use signalbox_domain::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};
use signalbox_persistence::credential_capacity::load_credential_rate_limits;

async fn commit_capacity_call(
    pool: &PgPool,
    seed: u128,
    snapshot: Option<ProviderRateLimitSnapshot>,
) -> Result<RestartModelCallFixture, Box<dyn Error>> {
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(pool, seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed)
        .with_rate_limits(snapshot);
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x40)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x41)),
            )),
            |_| panic!("capacity fixture has no pending steering"),
        )
        .await?;
    Ok(fixture)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_capacity_survives_connection_restart_with_exact_call_provenance()
-> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let observed_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let snapshot = ProviderRateLimitSnapshot::new(
        observed_at,
        vec![
            ProviderRateLimitWindow::new(
                73,
                Some(Duration::from_secs(18_000)),
                Some(observed_at + Duration::from_secs(700)),
            ),
            ProviderRateLimitWindow::new(0, None, None),
        ],
    );
    let fixture = commit_capacity_call(&pool, 0xca00, Some(snapshot.clone())).await?;
    pool.close().await;

    let reopened = PgPoolOptions::new()
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let retained = load_credential_rate_limits(
        &mut *reopened.acquire().await?,
        model_credential_reference().as_str(),
    )
    .await?;
    assert_eq!(retained, Some(snapshot));
    let source: Uuid =
        sqlx::query_scalar("SELECT observation_model_call_id FROM credential_rate_limit_snapshot")
            .fetch_one(&reopened)
            .await?;
    assert_eq!(source, fixture.call.into_uuid());
    reopened.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_capacity_older_call_completion_cannot_replace_newer_evidence()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let observed_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let recent = ProviderRateLimitSnapshot::new(
        observed_at,
        vec![ProviderRateLimitWindow::new(17, None, None)],
    );
    let older = ProviderRateLimitSnapshot::new(
        observed_at - Duration::from_secs(1),
        vec![ProviderRateLimitWindow::new(83, None, None)],
    );
    commit_capacity_call(&pool, 0xcb00, Some(recent.clone())).await?;
    commit_capacity_call(&pool, 0xcc00, Some(older)).await?;
    let retained = load_credential_rate_limits(
        &mut *pool.acquire().await?,
        model_credential_reference().as_str(),
    )
    .await?;
    assert_eq!(retained, Some(recent));
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_capacity_newer_snapshot_replaces_all_prior_windows()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let observed_at =
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000) + Duration::from_nanos(111);
    let older = ProviderRateLimitSnapshot::new(
        observed_at,
        vec![
            ProviderRateLimitWindow::new(23, Some(Duration::from_secs(300)), Some(observed_at)),
            ProviderRateLimitWindow::new(71, None, None),
        ],
    );
    let recent = ProviderRateLimitSnapshot::new(
        observed_at + Duration::from_nanos(1),
        vec![ProviderRateLimitWindow::new(0, None, None)],
    );
    commit_capacity_call(&pool, 0xcf00, Some(older)).await?;
    commit_capacity_call(&pool, 0xd000, Some(recent.clone())).await?;
    let retained = load_credential_rate_limits(
        &mut *pool.acquire().await?,
        model_credential_reference().as_str(),
    )
    .await?;
    assert_eq!(retained, Some(recent));
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_capacity_absent_report_preserves_prior_evidence() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let snapshot = ProviderRateLimitSnapshot::new(
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        vec![ProviderRateLimitWindow::new(42, None, None)],
    );
    commit_capacity_call(&pool, 0xcd00, Some(snapshot.clone())).await?;
    commit_capacity_call(&pool, 0xce00, None).await?;
    let retained = load_credential_rate_limits(
        &mut *pool.acquire().await?,
        model_credential_reference().as_str(),
    )
    .await?;
    assert_eq!(retained, Some(snapshot));
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_capacity_probe_preserves_newer_call_evidence_and_records_its_own_provenance()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::credential_capacity::retain_credential_capacity_probe;
    let (container, pool, _) = migrated_postgres().await?;
    let observed_at = SystemTime::now();
    let from_call = ProviderRateLimitSnapshot::new(
        observed_at,
        vec![ProviderRateLimitWindow::new(23, None, None)],
    );
    const CALL: u128 = 0xce00; // arbitrary independent call fixture
    let fixture = commit_capacity_call(&pool, CALL, Some(from_call.clone())).await?;
    let profile = model_credential_reference();
    let stale_probe = ProviderRateLimitSnapshot::new(
        observed_at - Duration::from_secs(1),
        vec![ProviderRateLimitWindow::new(0, None, None)],
    );
    assert!(!retain_credential_capacity_probe(&pool, profile.as_str(), &stale_probe).await?);
    assert_eq!(
        load_credential_rate_limits(&mut *pool.acquire().await?, profile.as_str()).await?,
        Some(from_call)
    );
    let source: Option<Uuid> = sqlx::query_scalar("SELECT observation_model_call_id FROM credential_rate_limit_snapshot WHERE credential_reference = $1").bind(profile.as_str()).fetch_one(&pool).await?;
    assert_eq!(source, Some(fixture.call.into_uuid()));
    let fresh_probe = ProviderRateLimitSnapshot::new(
        observed_at + Duration::from_secs(1),
        vec![ProviderRateLimitWindow::new(100, None, None)],
    );
    assert!(retain_credential_capacity_probe(&pool, profile.as_str(), &fresh_probe).await?);
    assert_eq!(
        load_credential_rate_limits(&mut *pool.acquire().await?, profile.as_str()).await?,
        Some(fresh_probe)
    );
    let source: Option<Uuid> = sqlx::query_scalar("SELECT observation_model_call_id FROM credential_rate_limit_snapshot WHERE credential_reference = $1").bind(profile.as_str()).fetch_one(&pool).await?;
    assert_eq!(
        source, None,
        "an out-of-call probe must not claim a model call as provenance"
    );
    pool.close().await;
    drop(container);
    Ok(())
}
