//! PostgreSQL integration proof for bounded stable session timeline reads.

#![allow(
    clippy::expect_used,
    reason = "this standalone integration-test crate uses explicit fixture expectations"
)]

use std::error::Error;

use signalbox_application::{TimelineContinuation, TimelineWindowAnchor, TimelineWindowLimits};
use signalbox_domain::{
    CreateSession, DirectModelSelection, DurableCommandId, ModelSelectionRequest,
    SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance, SessionId,
    TranscriptAncestry,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    session_timeline::{
        SessionTimelineCorruption, SessionTimelineRepository, SessionTimelineRepositoryError,
    },
};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    commission_fixture_session_goal, migrated_postgres, stop_fixture_session_goal,
    test_session_credential_pin,
};

fn credential_pin() -> signalbox_persistence::SessionCredentialPin {
    test_session_credential_pin()
}

fn session(value: u128) -> SessionId {
    SessionId::from_uuid(Uuid::from_u128(value))
}

async fn create_session(pool: &PgPool, identity: SessionId) -> Result<(), Box<dyn Error>> {
    let prepared = CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0x0009_9101)),
        SessionCreationProvenance::new(
            SessionCreationCause::UserInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(0x0009_9102)),
        )),
    )
    .prepare(identity)
    .expect("fixture session creation is valid");
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(prepared)
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn descriptor_and_windows_share_one_stable_creation_address() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x991);
    create_session(&pool, identity).await?;
    let repository = SessionTimelineRepository::new(pool.clone());
    let descriptor = repository
        .read_descriptor(identity)
        .await?
        .expect("created session has a descriptor");
    let limits = TimelineWindowLimits::new(1, 256).expect("fixture limits are bounded");
    let first = repository
        .read_window(identity, TimelineWindowAnchor::First, limits)
        .await?
        .expect("created session has a first window");
    let latest = repository
        .read_window(identity, TimelineWindowAnchor::Latest, limits)
        .await?
        .expect("created session has a latest window");
    let creation_address = descriptor
        .bounds
        .first
        .expect("created session has a first address");
    let around = repository
        .read_window(
            identity,
            TimelineWindowAnchor::Around(creation_address),
            limits,
        )
        .await?
        .expect("created session has an addressed window");
    let after_latest = repository
        .read_window(
            identity,
            TimelineWindowAnchor::After(creation_address),
            limits,
        )
        .await?
        .expect("created session has an empty window after its latest event");

    assert_eq!(
        descriptor.sizes.item_count,
        u64::try_from(first.items.len()).expect("fixture window length fits u64")
    );
    assert_eq!(descriptor.bounds.latest, Some(creation_address));
    assert_eq!(first.items[0].address, creation_address);
    assert_eq!(latest.items[0].address, creation_address);
    assert_eq!(around.items[0].address, creation_address);
    assert_eq!(
        descriptor.sizes.projected_structured_bytes,
        u64::from(first.items[0].projected_structured_bytes)
    );
    assert_eq!(first.continuation_before, TimelineContinuation::Exhausted);
    assert_eq!(latest.continuation_after, TimelineContinuation::Exhausted);
    assert!(after_latest.items.is_empty());
    assert_eq!(
        after_latest.continuation_before,
        TimelineContinuation::Exhausted
    );
    assert_eq!(
        after_latest.continuation_after,
        TimelineContinuation::Exhausted
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn missing_allocator_is_observation_cursor_corruption() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x993);
    create_session(&pool, identity).await?;
    // This disposable isolated container may bypass the deletion guard to simulate
    // a missing allocator observation cursor without affecting another test.
    sqlx::query("DROP TRIGGER outbox_sequence_state_cannot_be_deleted ON outbox_sequence_state")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM outbox_sequence_state WHERE singleton")
        .execute(&pool)
        .await?;
    let error = SessionTimelineRepository::new(pool.clone())
        .read_descriptor(identity)
        .await
        .expect_err("a missing allocator singleton is durable corruption");

    assert!(matches!(
        error,
        SessionTimelineRepositoryError::Corruption(SessionTimelineCorruption::Missing(
            "observation cursor"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn missing_projection_facts_are_corruption_not_session_absence() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x992);
    create_session(&pool, identity).await?;
    sqlx::query("DELETE FROM session_timeline_fact WHERE session_id = $1")
        .bind(identity.into_uuid())
        .execute(&pool)
        .await?;
    let error = SessionTimelineRepository::new(pool.clone())
        .read_descriptor(identity)
        .await
        .expect_err("missing projection facts are durable corruption");

    assert!(matches!(
        error,
        SessionTimelineRepositoryError::Corruption(SessionTimelineCorruption::Missing(
            "projection facts"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn empty_projection_facts_are_corruption() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x995);
    create_session(&pool, identity).await?;
    sqlx::query(
        "UPDATE session_timeline_fact SET item_count = 0, first_sequence = NULL, latest_sequence = NULL WHERE session_id = $1",
    )
    .bind(identity.into_uuid())
    .execute(&pool)
    .await?;
    let error = SessionTimelineRepository::new(pool.clone())
        .read_descriptor(identity)
        .await
        .expect_err("empty projection facts are durable corruption");

    assert!(matches!(
        error,
        SessionTimelineRepositoryError::Corruption(SessionTimelineCorruption::InvalidOrdinal(
            "item count"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn projection_count_larger_than_address_span_is_corruption() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x996);
    create_session(&pool, identity).await?;
    sqlx::query("UPDATE session_timeline_fact SET item_count = 2 WHERE session_id = $1")
        .bind(identity.into_uuid())
        .execute(&pool)
        .await?;
    let error = SessionTimelineRepository::new(pool.clone())
        .read_descriptor(identity)
        .await
        .expect_err("a count larger than the address span is durable corruption");

    assert!(matches!(
        error,
        SessionTimelineRepositoryError::Corruption(SessionTimelineCorruption::InvalidOrdinal(
            "timeline bounds"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retired_queued_goal_turn_is_removed_from_work_facts() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x994);
    create_session(&pool, identity).await?;
    commission_fixture_session_goal(&pool, identity, 0x0009_9400).await?;
    let repository = SessionTimelineRepository::new(pool.clone());
    let pursuing = repository
        .read_descriptor(identity)
        .await?
        .expect("commissioned session has a descriptor");
    stop_fixture_session_goal(&pool, identity, 0x0009_9500).await?;
    let stopped = repository
        .read_descriptor(identity)
        .await?
        .expect("stopped session has a descriptor");

    assert_eq!(pursuing.work.queued_turn_count, 1);
    assert_eq!(stopped.work.queued_turn_count, 0);

    pool.close().await;
    drop(container);
    Ok(())
}
