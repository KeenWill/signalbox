//! PostgreSQL integration proof for bounded stable session timeline reads.

#![allow(
    clippy::expect_used,
    reason = "this standalone integration-test crate uses explicit fixture expectations"
)]

use std::{error::Error, num::NonZeroU64};

use signalbox_application::{
    SessionTimelineDetailBody, SessionTimelineEventKind, TimelineAddress, TimelineContinuation,
    TimelineDetailLimits, TimelineWindowAnchor, TimelineWindowLimits,
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
    Decimal, commission_fixture_session_goal, insert_frontier, migrated_postgres,
    prepared_complete_delegation_outbox, stop_fixture_session_goal, test_session_credential_pin,
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
        SessionTimelineDetailBody::EventFact { .. }
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
async fn item_detail_returns_absent_for_an_unallocated_future_address() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x996);
    create_session(&pool, identity).await?;
    let allocated: i64 = sqlx::query_scalar(
        "SELECT last_sequence::bigint FROM outbox_sequence_state WHERE singleton",
    )
    .fetch_one(&pool)
    .await?;
    let future_sequence = u64::try_from(allocated)?
        .checked_add(1)
        .expect("fixture sequence has room");
    let address = TimelineAddress::new(
        NonZeroU64::new(future_sequence).expect("future sequence is positive"),
    );
    let limits = TimelineDetailLimits::new(1, 256).expect("fixture limits are bounded");
    let detail = SessionTimelineRepository::new(pool.clone())
        .read_item_details(identity, address, None, limits)
        .await?;

    assert_eq!(detail, None);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn input_detail_rejects_a_header_beyond_the_allocator() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x996);
    create_session(&pool, identity).await?;
    commission_fixture_session_goal(&pool, identity, 0x0009_9600).await?;
    let sequence: i64 = sqlx::query_scalar(
        "SELECT event_sequence::bigint
           FROM input_accepted_outbox_event
          WHERE session_id = $1",
    )
    .bind(identity.into_uuid())
    .fetch_one(&pool)
    .await?;
    sqlx::query("ALTER TABLE outbox_sequence_state DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE outbox_sequence_state
            SET last_sequence = $1 - 1
          WHERE singleton",
    )
    .bind(sequence)
    .execute(&pool)
    .await?;

    let address = TimelineAddress::new(
        NonZeroU64::new(u64::try_from(sequence)?).expect("outbox sequence is positive"),
    );
    let limits = TimelineDetailLimits::new(1, 256).expect("fixture limits are bounded");
    let error = SessionTimelineRepository::new(pool.clone())
        .read_item_details(identity, address, None, limits)
        .await
        .expect_err("an unallocated input header must fail before its text is loaded");
    assert!(matches!(
        error,
        SessionTimelineRepositoryError::Corruption(SessionTimelineCorruption::MissingDetailRecord)
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_detail_validates_body_shape_without_projecting_body_text()
-> Result<(), Box<dyn Error>> {
    let (container, pool, fixture) = prepared_complete_delegation_outbox(0x9970).await?;
    let sequence: i64 = sqlx::query_scalar(
        "SELECT event_sequence::bigint
           FROM delegation_update_outbox_event
          WHERE session_id = $1 AND update_kind = 'session_message'",
    )
    .bind(fixture.child.into_uuid())
    .fetch_one(&pool)
    .await?;
    let address = TimelineAddress::new(
        NonZeroU64::new(u64::try_from(sequence)?).expect("outbox sequence is positive"),
    );
    let limits = TimelineDetailLimits::new(1, 256).expect("fixture limits are bounded");
    let repository = SessionTimelineRepository::new(pool.clone());
    let detail = repository
        .read_item_details(fixture.child, address, None, limits)
        .await?
        .expect("the delegation detail exists");
    assert!(matches!(
        detail.items[0].body,
        SessionTimelineDetailBody::EventFact {
            kind: SessionTimelineEventKind::DelegationUpdate
        }
    ));

    sqlx::query(
        "ALTER TABLE delegation_update_outbox_event
         DROP CONSTRAINT delegation_update_subject_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE delegation_update_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE delegation_update_outbox_event
            SET content_text = NULL
          WHERE event_sequence = $1",
    )
    .bind(sequence)
    .execute(&pool)
    .await?;
    let error = repository
        .read_item_details(fixture.child, address, None, limits)
        .await
        .expect_err("a corrupt delegation body shape must fail closed");
    assert!(matches!(error, SessionTimelineRepositoryError::Outbox(_)));

    pool.close().await;
    drop(container);
    Ok(())
}

async fn rejected_response_text_position_constraint(
    fixture_seed: u128,
    entry_seed: u128,
    second_start: i64,
) -> Result<Option<String>, Box<dyn Error>> {
    let (container, pool, fixture) = prepared_complete_delegation_outbox(fixture_seed).await?;
    let call: Uuid =
        sqlx::query_scalar("SELECT model_call_id FROM model_call WHERE session_id = $1 LIMIT 1")
            .bind(fixture.parent.into_uuid())
            .fetch_one(&pool)
            .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             assistant_text_value, producing_model_call_id,
             assistant_response_part_ordinal,
             assistant_response_text_start_bytes)
         VALUES ($1, $2, 'assistant_text', 'abc', $4, 100, 0),
                ($1, $3, 'assistant_text', 'def', $4, 101, $5)",
    )
    .bind(fixture.parent.into_uuid())
    .bind(Uuid::from_u128(entry_seed))
    .bind(Uuid::from_u128(entry_seed + 1))
    .bind(call)
    .bind(second_start)
    .execute(&mut *transaction)
    .await?;
    let error = sqlx::query(
        "SET CONSTRAINTS semantic_transcript_response_text_positions_contiguous IMMEDIATE",
    )
    .execute(&mut *transaction)
    .await
    .expect_err("non-contiguous response text positions must be rejected");
    let constraint = error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::constraint)
        .map(str::to_owned);
    transaction.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(constraint)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn assistant_response_text_positions_reject_gaps() -> Result<(), Box<dyn Error>> {
    let constraint = rejected_response_text_position_constraint(0x9980, 0x9981, 4).await?;

    assert_eq!(
        constraint.as_deref(),
        Some("semantic_transcript_response_text_positions_contiguous")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn assistant_response_text_positions_reject_overlaps() -> Result<(), Box<dyn Error>> {
    let constraint = rejected_response_text_position_constraint(0x9990, 0x9991, 2).await?;

    assert_eq!(
        constraint.as_deref(),
        Some("semantic_transcript_response_text_positions_contiguous")
    );
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
    assert_eq!(
        generation_work_fact_disagreements(&pool, identity).await?,
        0
    );

    commission_fixture_session_goal(&pool, identity, 0x0009_9900).await?;
    let pursuing = repository
        .read_descriptor(identity)
        .await?
        .expect("commissioned session has a descriptor");
    assert_eq!(pursuing.work.queued_turn_count, 1);
    assert_eq!(backfilled_queued_turn_count(&pool, identity).await?, 1);
    assert_eq!(
        generation_work_fact_disagreements(&pool, identity).await?,
        0
    );

    stop_fixture_session_goal(&pool, identity, 0x0009_9A00).await?;
    let stopped = repository
        .read_descriptor(identity)
        .await?
        .expect("stopped session has a descriptor");
    assert_eq!(stopped.work.queued_turn_count, 0);
    assert_eq!(backfilled_queued_turn_count(&pool, identity).await?, 0);
    assert_eq!(
        generation_work_fact_disagreements(&pool, identity).await?,
        0
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Counts the generations whose incrementally maintained queued count differs
/// from a fresh recomputation of the intersection it stands for.
///
/// That fact is what makes the goal-event reconciliation two keyed reads rather
/// than a scan of the generation's history or of the session's queue, so it is
/// only as good as its agreement with the join it replaced. Recomputing after
/// each transition is what makes a drifting delta fail here instead of
/// surfacing as a wrong `queued_turn_count` much later. A generation whose
/// turns have all left the queue keeps a zero row the recomputation has no
/// group for, so the stored side drops zeroes before the comparison.
async fn generation_work_fact_disagreements(
    pool: &PgPool,
    identity: SessionId,
) -> Result<i64, Box<dyn Error>> {
    let disagreements: i64 = sqlx::query_scalar(
        "WITH stored AS (
             SELECT goal_generation, queued_turn_count
               FROM session_goal_generation_work_fact
              WHERE session_id = $1 AND queued_turn_count <> 0
         ), recomputed AS (
             SELECT goal.goal_generation, count(*)::numeric AS queued_turn_count
               FROM goal_turn AS goal
               JOIN turn_lifecycle AS turn
                 ON turn.session_id = goal.session_id
                AND turn.turn_id = goal.turn_id
              WHERE goal.session_id = $1
                AND turn.state_kind = 'queued'
                AND NOT turn.delegation_runtime_terminal
              GROUP BY goal.goal_generation
         )
         SELECT count(*)::bigint FROM (
             (TABLE stored EXCEPT ALL TABLE recomputed)
             UNION ALL
             (TABLE recomputed EXCEPT ALL TABLE stored)
         ) AS disagreements",
    )
    .bind(identity.into_uuid())
    .fetch_one(pool)
    .await?;
    Ok(disagreements)
}

/// Recomputes the goal half of the credit predicate the way the lifecycle
/// trigger now evaluates it, and compares it against
/// `goal_turn_is_queue_order_relevant` for every turn in the session.
///
/// The trigger cannot call that function for the state a row is *leaving*,
/// because it reads the row's stored `state_kind`. Asserting the two agree on
/// live rows is what keeps the extracted predicate honest.
async fn relevance_predicates_disagree(
    pool: &PgPool,
    identity: SessionId,
) -> Result<i64, Box<dyn Error>> {
    let disagreements: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM turn_lifecycle AS turn
          WHERE turn.session_id = $1
            AND goal_turn_is_queue_order_relevant(turn.session_id, turn.turn_id)
                IS DISTINCT FROM (
                    turn.state_kind <> 'queued'
                    OR goal_turn_generation_is_pursued(
                        turn.session_id, turn.turn_id
                    )
                )",
    )
    .bind(identity.into_uuid())
    .fetch_one(pool)
    .await?;
    Ok(disagreements)
}

/// A queued goal turn whose generation a goal event retired is no longer
/// credited to `queued_turn_count`, so the lifecycle trigger must not subtract
/// it again when that turn later leaves the queue.
///
/// No repository path reaches this state today, which is exactly why it is
/// asserted here rather than through one. Activation refuses a retired turn
/// (`start_eligible_turn` filters on `goal_turn_is_runtime_relevant`) and every
/// terminalization requires `state_kind = 'active'`, so a retired queued turn
/// merely lingers. The delegation cascade cannot reach it either: a goal turn is
/// `origin_kind = 'accepted_input'` by `goal_turn_lifecycle_fk`, and
/// `turn_lifecycle_delegation_runtime_terminal_shape` admits that flag only on
/// `origin_kind = 'delegation'`. The fact triggers still have to agree with the
/// backfill on their own terms instead of borrowing those gates, because an
/// unguarded subtraction removes credit that was never granted and drives the
/// count to -1, aborting the writing transaction on the nonnegative check.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_retired_queued_goal_turn_is_never_subtracted_twice() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x9a1);
    create_session(&pool, identity).await?;
    let seed = 0x0009_A100_u128;
    commission_fixture_session_goal(&pool, identity, seed).await?;
    let repository = SessionTimelineRepository::new(pool.clone());
    let pursuing = repository
        .read_descriptor(identity)
        .await?
        .expect("commissioned session has a descriptor");
    assert_eq!(pursuing.work.queued_turn_count, 1);

    stop_fixture_session_goal(&pool, identity, seed + 0x100).await?;
    let stopped = repository
        .read_descriptor(identity)
        .await?
        .expect("stopped session has a descriptor");
    assert_eq!(stopped.work.queued_turn_count, 0);
    assert_eq!(backfilled_queued_turn_count(&pool, identity).await?, 0);
    assert_eq!(relevance_predicates_disagree(&pool, identity).await?, 0);
    assert_eq!(
        generation_work_fact_disagreements(&pool, identity).await?,
        0
    );

    // The retired turn leaves the queue. Only the final-state assertion is
    // suspended -- it demands the attempts and boundary entries a turn that
    // never ran does not have. The transition guard and the fact trigger both
    // stay enabled, so this exercises exactly the subtraction under test. The
    // suspension spans its own statements rather than the cancelling
    // transaction because a deferred trigger the update leaves pending blocks
    // any `ALTER TABLE` that would re-enable it in that same transaction.
    let goal_turn = Uuid::from_u128(seed + 2);
    let frontier = Uuid::from_u128(seed + 0x200);
    let mut connection = pool.acquire().await?;
    insert_frontier(
        &mut connection,
        identity.into_uuid(),
        frontier,
        Decimal::ZERO,
        &[],
    )
    .await?;
    drop(connection);
    sqlx::raw_sql(
        "ALTER TABLE turn_lifecycle
             DISABLE TRIGGER turn_lifecycle_requires_complete_final_state",
    )
    .execute(&pool)
    .await?;
    let cancelling = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_frontier_id = $1,
                terminal_disposition_kind = 'cancelled'
          WHERE session_id = $2 AND turn_id = $3",
    )
    .bind(frontier)
    .bind(identity.into_uuid())
    .bind(goal_turn)
    .execute(&pool)
    .await;
    sqlx::raw_sql(
        "ALTER TABLE turn_lifecycle
             ENABLE TRIGGER turn_lifecycle_requires_complete_final_state",
    )
    .execute(&pool)
    .await?;
    assert_eq!(
        cancelling?.rows_affected(),
        1,
        "the retired queued goal turn leaves the queue"
    );

    let cancelled = repository
        .read_descriptor(identity)
        .await?
        .expect("the session still has a descriptor");
    assert_eq!(
        cancelled.work.queued_turn_count, 0,
        "a turn the goal event already retired must not be subtracted again"
    );
    assert_eq!(backfilled_queued_turn_count(&pool, identity).await?, 0);
    assert_eq!(relevance_predicates_disagree(&pool, identity).await?, 0);
    assert_eq!(
        generation_work_fact_disagreements(&pool, identity).await?,
        0,
        "the generation's count follows the turn out of the queue"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The goal-event reconciliation maintains `queued_turn_count` with a delta
/// keyed on the generation each event leaves pursued, rather than by rescanning
/// the session. Carrying a second generation through the same fixture is what
/// exercises that incremental path: the first event of a session returns before
/// the allocator, the retiring event moves the turns of the generation it
/// retires, and the next commission credits the generation it opens.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn goal_work_facts_track_a_second_generation_incrementally() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let identity = session(0x9a2);
    create_session(&pool, identity).await?;
    let repository = SessionTimelineRepository::new(pool.clone());

    commission_fixture_session_goal(&pool, identity, 0x0009_A200).await?;
    let first = repository
        .read_descriptor(identity)
        .await?
        .expect("commissioned session has a descriptor");
    assert_eq!(first.work.queued_turn_count, 1);
    assert_eq!(backfilled_queued_turn_count(&pool, identity).await?, 1);
    assert_eq!(
        generation_work_fact_disagreements(&pool, identity).await?,
        0
    );

    stop_fixture_session_goal(&pool, identity, 0x0009_A300).await?;
    let retired = repository
        .read_descriptor(identity)
        .await?
        .expect("stopped session has a descriptor");
    assert_eq!(retired.work.queued_turn_count, 0);
    assert_eq!(backfilled_queued_turn_count(&pool, identity).await?, 0);
    assert_eq!(
        generation_work_fact_disagreements(&pool, identity).await?,
        0
    );

    // The second generation credits only its own turn. The first generation's
    // turn is still queued and must stay uncredited, which a delta keyed on the
    // pursued generation gets right and an event-kind delta would not.
    commission_fixture_session_goal(&pool, identity, 0x0009_A400).await?;
    let second = repository
        .read_descriptor(identity)
        .await?
        .expect("recommissioned session has a descriptor");
    assert_eq!(second.work.queued_turn_count, 1);
    assert_eq!(backfilled_queued_turn_count(&pool, identity).await?, 1);
    assert_eq!(relevance_predicates_disagree(&pool, identity).await?, 0);
    assert_eq!(
        generation_work_fact_disagreements(&pool, identity).await?,
        0,
        "the retired generation keeps its own queued turn, uncredited"
    );

    stop_fixture_session_goal(&pool, identity, 0x0009_A500).await?;
    let settled = repository
        .read_descriptor(identity)
        .await?
        .expect("stopped session has a descriptor");
    assert_eq!(settled.work.queued_turn_count, 0);
    assert_eq!(backfilled_queued_turn_count(&pool, identity).await?, 0);
    assert_eq!(relevance_predicates_disagree(&pool, identity).await?, 0);
    assert_eq!(
        generation_work_fact_disagreements(&pool, identity).await?,
        0
    );

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
