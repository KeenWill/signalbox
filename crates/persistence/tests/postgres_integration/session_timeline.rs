//! PostgreSQL integration proof for bounded stable session timeline reads.

#![allow(
    clippy::expect_used,
    reason = "this standalone integration-test crate uses explicit fixture expectations"
)]

use std::{error::Error, num::NonZeroU64};

use signalbox_application::{
    SessionTimelineDetailBody, TimelineAddress, TimelineContinuation, TimelineDetailLimits,
    TimelineWindowAnchor, TimelineWindowLimits,
};
use signalbox_domain::{
    CreateSession, DirectModelSelection, DurableCommandId, ModelSelectionRequest,
    SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance, SessionId,
    ToolApprovalDecision, ToolAttemptId, ToolEffectClass, TranscriptAncestry, TurnAttemptId,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    session_timeline::{
        SessionTimelineCorruption, SessionTimelineRepository, SessionTimelineRepositoryError,
    },
    tool_loop::PostgresToolLoopRepository,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    checkpoint_confirmed_tool_round, commission_fixture_session_goal, decide_tool_request,
    migrated_postgres, stop_fixture_session_goal, test_session_credential_pin,
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
async fn item_and_region_details_share_the_stable_creation_address() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x994);
    create_session(&pool, identity).await?;
    let repository = SessionTimelineRepository::new(pool.clone());
    let descriptor = repository
        .read_descriptor(identity)
        .await?
        .expect("created session has a descriptor");
    let address = descriptor
        .bounds
        .first
        .expect("created session has a first address");
    let limits = TimelineDetailLimits::new(1, 256).expect("fixture limits are bounded");
    let item = repository
        .read_item_details(identity, address, None, limits)
        .await?
        .expect("the creation detail exists");
    let region = repository
        .read_region_details(identity, address, address, None, limits)
        .await?
        .expect("the creation region exists");

    assert_eq!(item.items.len(), usize::from(limits.max_items()));
    assert_eq!(item.items[0].address, address);
    assert!(matches!(
        item.items[0].body,
        SessionTimelineDetailBody::SessionCreated {
            imported_evidence: None
        }
    ));
    assert!(item.projected_body_bytes > 0);
    assert!(item.projected_body_bytes <= limits.max_projected_bytes());
    assert_eq!(item.continuation, None);
    assert_eq!(region, item);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tool_detail_selects_one_lease_generation_for_sandbox_posture() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x99a0;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let tool_loop = PostgresToolLoopRepository::new(pool.clone());
    tool_loop
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe0)),
        )
        .await?;
    let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1));
    tool_loop
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            attempt,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the approved request prepares its physical attempt");
    tool_loop
        .authorize_attempt(fixture.session, fixture.turn, attempt)
        .await?;

    let lease = Uuid::from_u128(seed + 0xe2);
    sqlx::query("ALTER TABLE runner_lease_generation DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             predecessor_generation)
         VALUES
            ($1, 1, $2, $3, $4, 'current_time', 'pure', 1, $5, 1, NULL),
            ($1, 2, $2, $3, $4, 'current_time', 'pure', 1, $5, 1, 1)",
    )
    .bind(lease)
    .bind(attempt.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 0xe3))
    .bind(Uuid::from_u128(seed + 0xe4))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_lease_generation ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_physical_attempt_lease_binding DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_physical_attempt_lease_binding (attempt_id, lease_id)
         VALUES ($1, $2)",
    )
    .bind(attempt.into_uuid())
    .bind(lease)
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_physical_attempt_lease_binding ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    let sequence: i64 = sqlx::query_scalar(
        "SELECT event_sequence::bigint
           FROM tool_batch_transition_outbox_event
          WHERE producing_model_call_id = $1
            AND transition_kind = 'proposed'",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    let address = TimelineAddress::new(
        NonZeroU64::new(u64::try_from(sequence)?).expect("the durable event sequence is positive"),
    );
    let page = SessionTimelineRepository::new(pool.clone())
        .read_item_details(
            fixture.session,
            address,
            None,
            TimelineDetailLimits::new(1, 512).expect("fixture limits are bounded"),
        )
        .await?
        .expect("the tool-batch detail exists");
    let SessionTimelineDetailBody::ToolBatch { tools, .. } = &page.items[0].body else {
        panic!("the selected event projects a tool-batch body");
    };
    let expected_attempt = attempt.into_uuid().to_string();

    assert_eq!(tools.len(), 1);
    assert_eq!(
        tools[0].attempt_id.as_deref(),
        Some(expected_attempt.as_str())
    );
    assert!(tools[0].sandbox_posture.is_some());

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
