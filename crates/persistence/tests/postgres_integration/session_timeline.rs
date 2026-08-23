//! PostgreSQL integration proof for bounded stable session timeline reads.

#![allow(
    clippy::expect_used,
    reason = "this standalone integration-test crate uses explicit fixture expectations"
)]

use std::{error::Error, num::NonZeroU64};

use signalbox_application::{
    SessionTimelineDetailBody, TimelineAddress, TimelineContinuation, TimelineDelegationDetail,
    TimelineDetailLimits, TimelineWindowAnchor, TimelineWindowLimits,
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
    migrated_postgres, prepared_complete_delegation_outbox, stop_fixture_session_goal,
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
async fn proposed_tool_detail_freezes_members_before_later_attempts() -> Result<(), Box<dyn Error>>
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
    let runner = Uuid::from_u128(seed + 0xe3);
    sqlx::query("ALTER TABLE runner_session_placement_record DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, directory_selection_kind,
             workspace_requirement_kind, requested_sandbox_profile,
             permission_override_count, state_kind, pinned_tool_count)
         VALUES ($1, 1, 1, 'created', 'identity', $2, 'runner_default',
                 'none', 'workspace_restricted', 0, 'unpinned', 0)",
    )
    .bind(fixture.session.into_uuid())
    .bind(runner)
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_session_placement_record ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
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
    .bind(runner)
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
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].request_id, request);
    assert_eq!(tools[0].attempt_id, None);
    assert_eq!(tools[0].sandbox_posture, None);

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
    let SessionTimelineDetailBody::Delegation(delegation) = &detail.items[0].body else {
        panic!("expected delegation detail");
    };
    assert!(matches!(
        delegation,
        TimelineDelegationDetail::SessionMessage { .. }
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
) -> Result<(Option<String>, String), Box<dyn Error>> {
    let (container, pool, fixture) = prepared_complete_delegation_outbox(fixture_seed).await?;
    let call: Uuid =
        sqlx::query_scalar("SELECT model_call_id FROM model_call WHERE session_id = $1 LIMIT 1")
            .bind(fixture.parent.into_uuid())
            .fetch_one(&pool)
            .await?;
    // These synthetic entries intentionally contradict only the response-text
    // position invariant. Keep the completed tool round's unrelated exact
    // inventory check from masking the constraint under test.
    sqlx::query(
        "ALTER TABLE semantic_transcript_entry
         DISABLE TRIGGER semantic_entry_requires_matching_turn_state",
    )
    .execute(&pool)
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
    let error = transaction
        .commit()
        .await
        .expect_err("non-contiguous response text positions must not commit");
    let database_error = error
        .as_database_error()
        .expect("the deferred trigger reports a database error");
    let code = database_error.code().map(|code| code.into_owned());
    let message = database_error.message().to_owned();

    pool.close().await;
    drop(container);
    Ok((code, message))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn assistant_response_text_positions_reject_gaps() -> Result<(), Box<dyn Error>> {
    let (code, message) = rejected_response_text_position_constraint(0x9980, 0x9981, 4).await?;

    assert_eq!(code.as_deref(), Some("23514"));
    assert!(
        message.starts_with("assistant response text positions are not contiguous for model call "),
        "unexpected database error: {message}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn assistant_response_text_positions_reject_overlaps() -> Result<(), Box<dyn Error>> {
    let (code, message) = rejected_response_text_position_constraint(0x9990, 0x9991, 2).await?;

    assert_eq!(code.as_deref(), Some("23514"));
    assert!(
        message.starts_with("assistant response text positions are not contiguous for model call "),
        "unexpected database error: {message}"
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
