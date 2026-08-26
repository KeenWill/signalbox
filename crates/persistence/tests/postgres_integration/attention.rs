//! Scale and overflow proof for the bounded fleet attention projection.

use crate::*;
use signalbox_application::{
    AttentionChanges, AttentionContinuation, AttentionCursor, AttentionQuery, AttentionSort,
    max_attention_change_items, max_attention_snapshot_items,
};
use signalbox_domain::{ReplaceSessionMetadata, SessionMetadataContent};
use signalbox_persistence::attention::{
    AttentionCorruption, AttentionRepository, AttentionRepositoryError,
};
use signalbox_persistence::session_metadata::SessionMetadataRepository;

const FLEET_SIZE: u128 = 258;
const FLEET_SEED: u128 = 0xa770_0000;
/// These reads turn on fleet size and journal length, never on how many
/// automatic resumptions a deployment still owes, so they state the unbounded
/// automatic-resume budget instead of a number their story never uses.
const UNBOUNDED_AUTOMATIC_RESUME_BUDGET: Option<u32> = None;

async fn create_mixed_scale_fleet(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    for offset in 0..FLEET_SIZE {
        create_attention_session(pool, offset).await?;
    }
    Ok(())
}

async fn create_attention_session(pool: &PgPool, offset: u128) -> Result<(), Box<dyn Error>> {
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            FLEET_SEED + offset,
            FLEET_SEED + FLEET_SIZE + offset,
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(Uuid::from_u128(
                FLEET_SEED + FLEET_SIZE * 2,
            ))),
        ))
        .await?;
    Ok(())
}

fn resync_cursor(changes: AttentionChanges) -> AttentionCursor {
    let AttentionChanges::ResyncRequired { cursor } = changes else {
        panic!("the oversized journal gap must require resynchronization");
    };
    cursor
}

fn identity_query(continuation: Option<AttentionContinuation>) -> AttentionQuery {
    AttentionQuery::try_new(
        None,
        Vec::new(),
        false,
        AttentionSort::SessionIdentityAscending,
        continuation,
    )
    .expect("the fixture catalog query is bounded")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn bounded_pages_cover_large_fleet() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    create_mixed_scale_fleet(&pool).await?;
    let repository = AttentionRepository::new(pool.clone(), UNBOUNDED_AUTOMATIC_RESUME_BUDGET);
    let first = repository.snapshot(identity_query(None)).await?;
    let second = repository
        .snapshot(identity_query(first.continuation.clone()))
        .await?;
    let third = repository
        .snapshot(identity_query(second.continuation.clone()))
        .await?;
    let fourth = repository
        .snapshot(identity_query(third.continuation.clone()))
        .await?;
    let fifth = repository
        .snapshot(identity_query(fourth.continuation.clone()))
        .await?;
    let sixth = repository
        .snapshot(identity_query(fifth.continuation.clone()))
        .await?;
    let seventh = repository
        .snapshot(identity_query(sixth.continuation.clone()))
        .await?;
    let eighth = repository
        .snapshot(identity_query(seventh.continuation.clone()))
        .await?;
    let ninth = repository
        .snapshot(identity_query(eighth.continuation.clone()))
        .await?;
    let searched_session = first.summaries[7].session;
    let searched = repository
        .snapshot(
            AttentionQuery::try_new(
                Some(searched_session.into_uuid().to_string()),
                Vec::new(),
                false,
                AttentionSort::LastActivityDescending,
                None,
            )
            .expect("the exact-identity search is bounded"),
        )
        .await?;

    assert_eq!(
        first.summaries.len(),
        usize::from(max_attention_snapshot_items())
    );
    assert_eq!(
        second.summaries.len(),
        usize::from(max_attention_snapshot_items())
    );
    assert_eq!(
        third.summaries.len(),
        usize::from(max_attention_snapshot_items())
    );
    assert_eq!(
        fourth.summaries.len(),
        usize::from(max_attention_snapshot_items())
    );
    assert_eq!(
        fifth.summaries.len(),
        usize::from(max_attention_snapshot_items())
    );
    assert_eq!(
        sixth.summaries.len(),
        usize::from(max_attention_snapshot_items())
    );
    assert_eq!(
        seventh.summaries.len(),
        usize::from(max_attention_snapshot_items())
    );
    assert_eq!(
        eighth.summaries.len(),
        usize::from(max_attention_snapshot_items())
    );
    assert_eq!(
        ninth.summaries.len(),
        usize::try_from(FLEET_SIZE % u128::from(max_attention_snapshot_items()))
            .expect("the final page size fits usize")
    );
    assert_eq!(
        first.total,
        u64::try_from(FLEET_SIZE).expect("the fleet size fits the exact total")
    );
    assert_eq!(searched.total, 1);
    assert_eq!(searched.summaries[0].session, searched_session);
    assert_eq!(
        first.summaries.len()
            + second.summaries.len()
            + third.summaries.len()
            + fourth.summaries.len()
            + fifth.summaries.len()
            + sixth.summaries.len()
            + seventh.summaries.len()
            + eighth.summaries.len()
            + ninth.summaries.len(),
        usize::try_from(FLEET_SIZE).expect("the fleet size fits usize")
    );
    assert_eq!(
        first.continuation,
        Some(AttentionContinuation::SessionIdentity(
            first.summaries.last().unwrap().session
        ))
    );
    assert_eq!(
        second.continuation,
        Some(AttentionContinuation::SessionIdentity(
            second.summaries.last().unwrap().session
        ))
    );
    assert_eq!(
        third.continuation,
        Some(AttentionContinuation::SessionIdentity(
            third.summaries.last().unwrap().session
        ))
    );
    assert_eq!(
        fourth.continuation,
        Some(AttentionContinuation::SessionIdentity(
            fourth.summaries.last().unwrap().session
        ))
    );
    assert_eq!(
        fifth.continuation,
        Some(AttentionContinuation::SessionIdentity(
            fifth.summaries.last().unwrap().session
        ))
    );
    assert_eq!(
        sixth.continuation,
        Some(AttentionContinuation::SessionIdentity(
            sixth.summaries.last().unwrap().session
        ))
    );
    assert_eq!(
        seventh.continuation,
        Some(AttentionContinuation::SessionIdentity(
            seventh.summaries.last().unwrap().session
        ))
    );
    assert_eq!(
        eighth.continuation,
        Some(AttentionContinuation::SessionIdentity(
            eighth.summaries.last().unwrap().session
        ))
    );
    assert_eq!(ninth.continuation, None);
    assert!(first.summaries.last().unwrap().session < second.summaries[0].session);
    assert!(second.summaries.last().unwrap().session < third.summaries[0].session);
    assert!(third.summaries.last().unwrap().session < fourth.summaries[0].session);
    assert!(fourth.summaries.last().unwrap().session < fifth.summaries[0].session);
    assert!(fifth.summaries.last().unwrap().session < sixth.summaries[0].session);
    assert!(sixth.summaries.last().unwrap().session < seventh.summaries[0].session);
    assert!(seventh.summaries.last().unwrap().session < eighth.summaries[0].session);
    assert!(eighth.summaries.last().unwrap().session < ninth.summaries[0].session);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oversized_change_burst_requires_resync() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    create_attention_session(&pool, 0).await?;
    let repository = AttentionRepository::new(pool.clone(), UNBOUNDED_AUTOMATIC_RESUME_BUDGET);
    let first = repository.snapshot(AttentionQuery::hot_page()).await?;
    let changed_session = first.summaries[0].session;
    sqlx::query(
        "INSERT INTO operator_attention_change (session_id, fact_kind)
         SELECT $1, 'turn' FROM generate_series(1, $2)",
    )
    .bind(changed_session.into_uuid())
    .bind(i32::from(max_attention_change_items()) + 1)
    .execute(&pool)
    .await?;
    let resync = resync_cursor(repository.changes_after(first.cursor).await?);

    assert!(resync > first.cursor);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn missing_activity_fact_fails_the_default_catalog_page_closed() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    create_attention_session(&pool, 0).await?;
    create_attention_session(&pool, 1).await?;
    let repository = AttentionRepository::new(pool.clone(), UNBOUNDED_AUTOMATIC_RESUME_BUDGET);
    let complete = repository.snapshot(AttentionQuery::hot_page()).await?;
    assert_eq!(complete.summaries.len(), 2);
    assert_eq!(complete.total, 2);
    sqlx::query("DELETE FROM session_timeline_fact WHERE session_id = $1")
        .bind(complete.summaries[0].session.into_uuid())
        .execute(&pool)
        .await?;
    let error = repository
        .snapshot(AttentionQuery::hot_page())
        .await
        .expect_err("a session missing its activity fact is projection corruption, not absence");

    assert!(matches!(
        error,
        AttentionRepositoryError::Corruption(AttentionCorruption::Missing(_))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_activity_drives_hot_sort_filters_counts_and_resync() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    create_attention_session(&pool, 0).await?;
    create_attention_session(&pool, 1).await?;
    let repository = AttentionRepository::new(pool.clone(), UNBOUNDED_AUTOMATIC_RESUME_BUDGET);
    let before = repository.snapshot(AttentionQuery::hot_page()).await?;
    let target = SessionId::from_uuid(Uuid::from_u128(FLEET_SEED + FLEET_SIZE));
    let prior_activity = before
        .summaries
        .iter()
        .find(|summary| summary.session == target)
        .expect("the target session is in the initial catalog")
        .last_activity
        .recorded_at;
    let visible_metadata = SessionMetadataContent::try_new(
        Some("needle catalog title".to_owned()),
        vec!["focus".to_owned()],
        Vec::new(),
        false,
    )
    .expect("the visible metadata fixture is valid");
    SessionMetadataRepository::new(pool.clone())
        .handle(ReplaceSessionMetadata::new(
            DurableCommandId::from_uuid(Uuid::from_u128(FLEET_SEED + FLEET_SIZE * 4)),
            target,
            visible_metadata,
        ))
        .await?;
    let visible = repository.snapshot(AttentionQuery::hot_page()).await?;
    let filtered = repository
        .snapshot(
            AttentionQuery::try_new(
                Some("needle".to_owned()),
                vec!["focus".to_owned()],
                false,
                AttentionSort::LastActivityDescending,
                None,
            )
            .expect("the metadata filter is bounded"),
        )
        .await?;
    let follow = repository.changes_after(before.cursor).await?;

    assert_eq!(visible.summaries[0].session, target);
    assert_eq!(
        visible.summaries[0].title_summary.as_deref(),
        Some("needle catalog title")
    );
    assert!(visible.summaries[0].last_activity.recorded_at > prior_activity);
    assert_eq!(filtered.total, 1);
    assert_eq!(filtered.summaries[0].session, target);
    assert!(matches!(follow, AttentionChanges::ResyncRequired { .. }));

    let archived_metadata = SessionMetadataContent::try_new(
        Some("needle catalog title".to_owned()),
        vec!["focus".to_owned()],
        Vec::new(),
        true,
    )
    .expect("the archived metadata fixture is valid");
    SessionMetadataRepository::new(pool.clone())
        .handle(ReplaceSessionMetadata::new(
            DurableCommandId::from_uuid(Uuid::from_u128(FLEET_SEED + FLEET_SIZE * 4 + 1)),
            target,
            archived_metadata,
        ))
        .await?;
    let default_after_archive = repository.snapshot(AttentionQuery::hot_page()).await?;
    let archived = repository
        .snapshot(
            AttentionQuery::try_new(
                None,
                vec!["focus".to_owned()],
                true,
                AttentionSort::LastActivityDescending,
                None,
            )
            .expect("the archive filter is bounded"),
        )
        .await?;

    assert_eq!(default_after_archive.total, 1);
    assert_eq!(archived.total, 1);
    assert!(archived.summaries[0].archived);

    pool.close().await;
    drop(container);
    Ok(())
}

/// Direct attention publishers and outbox publishers share the canonical
/// allocator row, so their cursor order follows their commit order.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn attention_and_outbox_publishers_share_commit_order() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    create_attention_session(&pool, 0).await?;
    let first_session = Uuid::from_u128(FLEET_SEED + FLEET_SIZE);
    let second_session = Uuid::from_u128(FLEET_SEED + FLEET_SIZE + 1);

    let mut first_transaction = pool.begin().await?;
    let first_sequence: i64 = sqlx::query_scalar(
        "INSERT INTO operator_attention_change (session_id, fact_kind)
         VALUES ($1, 'turn')
         RETURNING change_sequence",
    )
    .bind(first_session)
    .fetch_one(&mut *first_transaction)
    .await?;
    let second = tokio::spawn({
        let pool = pool.clone();
        async move {
            create_attention_session(&pool, 1)
                .await
                .expect("the concurrent session publisher succeeds");
        }
    });

    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the outbox publisher must wait for the earlier attention publisher"
    );
    first_transaction.commit().await?;
    second.await?;

    let second_sequence: i64 = sqlx::query_scalar(
        "SELECT min(change_sequence)
           FROM operator_attention_change
          WHERE session_id = $1",
    )
    .bind(second_session)
    .fetch_one(&pool)
    .await?;
    assert!(first_sequence < second_sequence);

    pool.close().await;
    drop(container);
    Ok(())
}

/// A goal command naming a session that was never created still owes its
/// operator a durable `session_not_found` receipt, which is why `goal_command`
/// carries no session reference. The attention journal's session reference is
/// immediate, so publishing that rejection would fail the receipt write.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unknown_session_goal_rejection_is_recorded_and_publishes_no_attention()
-> Result<(), Box<dyn Error>> {
    const UNKNOWN_SESSION_SEED: u128 = 0xa772_0000;

    let (container, pool, _database_url) = migrated_postgres().await?;
    let unknown_session = SessionId::from_uuid(Uuid::from_u128(UNKNOWN_SESSION_SEED));
    let outcome = GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(UNKNOWN_SESSION_SEED + 1)),
                unknown_session,
                GoalUserAction::Resume(None),
            ),
            None,
            |_| None,
        )
        .await?;
    let published: i64 =
        sqlx::query_scalar("SELECT count(*) FROM operator_attention_change WHERE session_id = $1")
            .bind(unknown_session.into_uuid())
            .fetch_one(&pool)
            .await?;
    let recorded: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM goal_command
          WHERE session_id = $1
            AND result_kind = 'rejected'
            AND rejection_kind = 'session_not_found'",
    )
    .bind(unknown_session.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert!(matches!(
        outcome,
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Rejected(
            GoalCommandRejection::SessionNotFound
        ))
    ));
    assert_eq!(recorded, 1);
    assert_eq!(published, 0);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegated_session_backfill_uses_creation_placement_time() -> Result<(), Box<dyn Error>> {
    const DELEGATED_BACKFILL_SEED: u128 = 0xa771_0000;

    let (container, pool, _database_url) = postgres_before_attention_migration().await?;
    let fixture = prepare_canonical_raw_delegation(&pool, DELEGATED_BACKFILL_SEED).await?;
    let mut setup = pool.begin().await?;
    insert_raw_delegation_with_update(&mut setup, fixture).await?;
    setup.commit().await?;
    let migration = MIGRATOR
        .iter()
        .find(|migration| migration.version == OPERATOR_ATTENTION_CHANGE_MIGRATION_VERSION)
        .expect("the attention migration is registered");
    let mut connection = pool.acquire().await?;
    connection.apply("_sqlx_migrations", migration).await?;
    drop(connection);
    let (fact_kind, uses_creation_time): (String, bool) = sqlx::query_as(
        "SELECT attention.fact_kind,
                attention.recorded_at = placement.recorded_at
           FROM operator_attention_change AS attention
           JOIN session_placement_event AS placement
             ON placement.session_id = attention.session_id
            AND placement.version = 1
          WHERE attention.session_id = $1",
    )
    .bind(fixture.child.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(fact_kind, "session");
    assert!(uses_creation_time);

    pool.close().await;
    drop(container);
    Ok(())
}
