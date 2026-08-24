//! PostgreSQL integration proof for bounded stable session timeline reads.

#![allow(
    clippy::expect_used,
    reason = "this standalone integration-test crate uses explicit fixture expectations"
)]

use std::error::Error;

use signalbox_application::{
    TimelineAddress, TimelineContinuation, TimelineWindowAnchor, TimelineWindowLimits,
};
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
        DurableCommandId::from_uuid(identity.into_uuid()),
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
async fn impossible_projection_structured_bytes_are_corruption() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x99c);
    create_session(&pool, identity).await?;
    sqlx::query("UPDATE session_timeline_fact SET event_kind_bytes = 0 WHERE session_id = $1")
        .bind(identity.into_uuid())
        .execute(&pool)
        .await?;
    let error = SessionTimelineRepository::new(pool.clone())
        .read_descriptor(identity)
        .await
        .expect_err("an impossible structured-byte total is durable corruption");

    assert!(matches!(
        error,
        SessionTimelineRepositoryError::Corruption(SessionTimelineCorruption::InvalidOrdinal(
            "projected structured bytes"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn window_outside_projection_facts_is_corruption() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x997);
    create_session(&pool, identity).await?;
    commission_fixture_session_goal(&pool, identity, 0x0009_9700).await?;
    sqlx::query(
        "UPDATE session_timeline_fact \
         SET item_count = 1, first_sequence = latest_sequence, \
             event_kind_bytes = octet_length('session_created') \
         WHERE session_id = $1",
    )
    .bind(identity.into_uuid())
    .execute(&pool)
    .await?;
    let limits = TimelineWindowLimits::new(256, 64 * 1024).expect("fixture limits are bounded");
    let error = SessionTimelineRepository::new(pool.clone())
        .read_window(identity, TimelineWindowAnchor::Latest, limits)
        .await
        .expect_err("a window outside stored projection bounds is durable corruption");

    assert!(matches!(
        error,
        SessionTimelineRepositoryError::Corruption(SessionTimelineCorruption::InvalidOrdinal(
            "window bounds"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn window_exceeding_projection_totals_is_corruption() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x999);
    create_session(&pool, identity).await?;
    commission_fixture_session_goal(&pool, identity, 0x0009_9900).await?;
    sqlx::query(
        "UPDATE session_timeline_fact \
         SET item_count = 2, event_kind_bytes = 2 * octet_length('session_created') \
         WHERE session_id = $1",
    )
    .bind(identity.into_uuid())
    .execute(&pool)
    .await?;
    let limits = TimelineWindowLimits::new(256, 64 * 1024).expect("fixture limits are bounded");
    let error = SessionTimelineRepository::new(pool.clone())
        .read_window(identity, TimelineWindowAnchor::First, limits)
        .await
        .expect_err("a window exceeding descriptor totals is durable corruption");

    assert!(matches!(
        error,
        SessionTimelineRepositoryError::Corruption(SessionTimelineCorruption::InvalidOrdinal(
            "window totals"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn first_window_must_reach_stored_first_address() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    create_session(&pool, session(0x99a)).await?;
    let identity = session(0x99b);
    create_session(&pool, identity).await?;
    sqlx::query(
        "UPDATE session_timeline_fact \
         SET item_count = 2, first_sequence = first_sequence - 1, \
             event_kind_bytes = 2 * octet_length('session_created') \
         WHERE session_id = $1",
    )
    .bind(identity.into_uuid())
    .execute(&pool)
    .await?;
    let limits = TimelineWindowLimits::new(1, 256).expect("fixture limits are bounded");
    let error = SessionTimelineRepository::new(pool.clone())
        .read_window(identity, TimelineWindowAnchor::First, limits)
        .await
        .expect_err("a first window missing the stored bound is durable corruption");

    assert!(matches!(
        error,
        SessionTimelineRepositoryError::Corruption(SessionTimelineCorruption::InvalidOrdinal(
            "window bounds"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn empty_endpoint_and_around_windows_are_corruption() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x998);
    create_session(&pool, identity).await?;
    // This disposable isolated container may bypass the immutable parent-row guard
    // and typed-record foreign key to simulate missing durable event headers.
    sqlx::query("DROP TRIGGER outbox_event_is_append_only ON outbox_event")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE session_created_outbox_event DROP CONSTRAINT session_created_outbox_event_header_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM outbox_event WHERE session_id = $1")
        .bind(identity.into_uuid())
        .execute(&pool)
        .await?;
    let repository = SessionTimelineRepository::new(pool.clone());
    let limits = TimelineWindowLimits::new(1, 256).expect("fixture limits are bounded");
    let first = repository
        .read_window(identity, TimelineWindowAnchor::First, limits)
        .await
        .expect_err("an empty first window is durable corruption");
    let latest = repository
        .read_window(identity, TimelineWindowAnchor::Latest, limits)
        .await
        .expect_err("an empty latest window is durable corruption");
    let address =
        TimelineAddress::new(std::num::NonZeroU64::new(1).expect("fixture address is nonzero"));
    let around = repository
        .read_window(identity, TimelineWindowAnchor::Around(address), limits)
        .await
        .expect_err("an empty around window is durable corruption");

    assert!(matches!(
        first,
        SessionTimelineRepositoryError::Corruption(SessionTimelineCorruption::InvalidOrdinal(
            "window items"
        ))
    ));
    assert!(matches!(
        latest,
        SessionTimelineRepositoryError::Corruption(SessionTimelineCorruption::InvalidOrdinal(
            "window items"
        ))
    ));
    assert!(matches!(
        around,
        SessionTimelineRepositoryError::Corruption(SessionTimelineCorruption::InvalidOrdinal(
            "window items"
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

/// Recomputes `queued_turn_count` with the predicate the migration backfill
/// uses, so a test can assert the incrementally maintained fact and a
/// freshly backfilled database would record the same total.
async fn backfilled_queued_turn_count(
    pool: &PgPool,
    identity: SessionId,
) -> Result<u64, Box<dyn Error>> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM turn_lifecycle AS turn
          WHERE turn.session_id = $1
            AND turn.state_kind = 'queued'
            AND NOT turn.delegation_runtime_terminal
            AND goal_turn_is_runtime_relevant(turn.session_id, turn.turn_id)",
    )
    .bind(identity.into_uuid())
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count)?)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn goal_work_facts_agree_with_a_fresh_backfill_at_every_transition()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x99d);
    create_session(&pool, identity).await?;
    let repository = SessionTimelineRepository::new(pool.clone());

    // The migration maintains `queued_turn_count` incrementally but seeds it
    // from a one-time backfill, so the two definitions have to agree at every
    // point a database might be migrated. Recomputing the backfill predicate
    // after each transition is what makes a divergence fail here rather than
    // surface as a counter that drifts only on already-migrated deployments.
    let created = repository
        .read_descriptor(identity)
        .await?
        .expect("created session has a descriptor");
    assert_eq!(created.work.queued_turn_count, 0);
    assert_eq!(backfilled_queued_turn_count(&pool, identity).await?, 0);

    commission_fixture_session_goal(&pool, identity, 0x0009_9900).await?;
    let pursuing = repository
        .read_descriptor(identity)
        .await?
        .expect("commissioned session has a descriptor");
    assert_eq!(pursuing.work.queued_turn_count, 1);
    assert_eq!(backfilled_queued_turn_count(&pool, identity).await?, 1);

    stop_fixture_session_goal(&pool, identity, 0x0009_9A00).await?;
    let stopped = repository
        .read_descriptor(identity)
        .await?
        .expect("stopped session has a descriptor");
    assert_eq!(stopped.work.queued_turn_count, 0);
    assert_eq!(backfilled_queued_turn_count(&pool, identity).await?, 0);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blocked_goal_event_cannot_name_a_queued_goal_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x99e);
    create_session(&pool, identity).await?;
    let seed = 0x0009_9B00_u128;
    commission_fixture_session_goal(&pool, identity, seed).await?;

    // `blocked` and `achieved` retire a generation just as `user_stopped` and
    // `superseded` do, yet no fact trigger compensates for them by name. That
    // is safe only because a queued goal turn cannot coexist with either as
    // the latest event, and this is the constraint that holds that line: the
    // event must name the *current* goal turn in an unsuccessful *terminal*
    // state, which a queued turn is not. If this ever starts succeeding, the
    // timeline work facts need a compensating path for blocked generations.
    let error = sqlx::query(
        "INSERT INTO goal_event (
             session_id, event_ordinal, generation, event_kind,
             blocked_reason, need, scheduler_turn_id
         ) VALUES ($1, 2, 1, 'blocked', 'execution_failure', $2, $3)",
    )
    .bind(identity.into_uuid())
    .bind("fixture goal blocked on an execution failure")
    .bind(Uuid::from_u128(seed + 2))
    .execute(&pool)
    .await
    .expect_err("a queued goal turn is not an unsuccessful terminal turn");

    let database_error = error
        .as_database_error()
        .expect("the rejection is a database constraint failure");
    assert_eq!(
        database_error.constraint(),
        Some("goal_event_scheduler_failure_turn")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Reads a whole multi-event fixture history as the oracle later window
/// assertions compare against.
///
/// Both continuations must be exhausted, so the returned addresses really are
/// the session's entire logical order rather than a page of it.
async fn whole_history(
    repository: &SessionTimelineRepository,
    identity: SessionId,
) -> Result<Vec<TimelineAddress>, Box<dyn Error>> {
    let unbounded = TimelineWindowLimits::new(256, 64 * 1024).expect("fixture limits are bounded");
    let window = repository
        .read_window(identity, TimelineWindowAnchor::First, unbounded)
        .await?
        .expect("the fixture session has a first window");
    assert_eq!(
        window.continuation_before,
        TimelineContinuation::Exhausted,
        "the oracle window must reach the start of history"
    );
    assert_eq!(
        window.continuation_after,
        TimelineContinuation::Exhausted,
        "the oracle window must reach the end of history"
    );
    assert!(
        window.items.len() >= 3,
        "the fixture session must have several durable events to page across, got {}",
        window.items.len()
    );
    Ok(window.items.iter().map(|item| item.address).collect())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn backward_paging_covers_every_event_exactly_once() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x99f);
    create_session(&pool, identity).await?;
    commission_fixture_session_goal(&pool, identity, 0x0009_9500).await?;
    stop_fixture_session_goal(&pool, identity, 0x0009_9600).await?;
    let repository = SessionTimelineRepository::new(pool.clone());
    let expected = whole_history(&repository, identity).await?;

    // One item per page forces a continuation cursor at every boundary, so the
    // walk observes `MoreAt` repeatedly rather than only `Exhausted`. A
    // backward window that admitted its own anchor — `<= $2` instead of
    // `< $2` — would repeat each boundary address and lengthen this walk.
    let single = TimelineWindowLimits::new(1, 256).expect("fixture limits are bounded");
    let mut walked = Vec::new();
    let mut anchor = TimelineWindowAnchor::Latest;
    for _ in 0..=expected.len() {
        let window = repository
            .read_window(identity, anchor, single)
            .await?
            .expect("every page of a live session resolves");
        assert_eq!(
            window.items.len(),
            1,
            "each page honours the one-item ceiling"
        );
        walked.push(window.items[0].address);
        match window.continuation_before {
            TimelineContinuation::Exhausted => break,
            TimelineContinuation::MoreAt(boundary) => {
                assert_eq!(
                    boundary, window.items[0].address,
                    "the backward cursor names the boundary item the caller already holds"
                );
                anchor = TimelineWindowAnchor::Before(boundary);
            }
        }
    }
    walked.reverse();

    assert_eq!(
        walked, expected,
        "paging backward one item at a time visits every address exactly once, in logical order"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn around_windows_reach_both_sides_of_an_interior_anchor() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x9a0);
    create_session(&pool, identity).await?;
    commission_fixture_session_goal(&pool, identity, 0x0009_9700).await?;
    stop_fixture_session_goal(&pool, identity, 0x0009_9800).await?;
    let repository = SessionTimelineRepository::new(pool.clone());
    let expected = whole_history(&repository, identity).await?;
    let anchor_index = expected.len() / 2;
    let anchor_address = expected[anchor_index];

    // A three-item ceiling around an interior address admits the anchor and one
    // neighbour on each side. A one-sided read, or a distance ordering that
    // stopped comparing `abs()`, would return three consecutive items on a
    // single side and fail the containment assertions below.
    let three = TimelineWindowLimits::new(3, 64 * 1024).expect("fixture limits are bounded");
    let around = repository
        .read_window(
            identity,
            TimelineWindowAnchor::Around(anchor_address),
            three,
        )
        .await?
        .expect("an addressed window resolves for a live session");
    let addresses: Vec<TimelineAddress> = around.items.iter().map(|item| item.address).collect();

    assert_eq!(
        addresses.len(),
        3,
        "the around window fills its item ceiling"
    );
    let start = expected
        .iter()
        .position(|address| *address == addresses[0])
        .expect("the around window starts inside the session history");
    assert_eq!(
        &expected[start..start + addresses.len()],
        addresses.as_slice(),
        "the around window is a contiguous slice of history in logical order"
    );
    assert!(
        start < anchor_index,
        "the around window reaches below its anchor"
    );
    assert!(
        start + addresses.len() > anchor_index + 1,
        "the around window reaches above its anchor"
    );

    pool.close().await;
    drop(container);
    Ok(())
}
