//! Scale and overflow proof for the bounded fleet attention projection.

use crate::*;
use signalbox_application::{
    AttentionChanges, AttentionContinuation, AttentionCursor, AttentionQuery, AttentionSort,
    max_attention_change_items, max_attention_snapshot_items,
};
use signalbox_persistence::attention::AttentionRepository;

const FLEET_SIZE: u128 = 130;
const FLEET_SEED: u128 = 0xa770_0000;

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

fn identity_query(after: Option<SessionId>) -> AttentionQuery {
    AttentionQuery::try_new(
        None,
        Vec::new(),
        false,
        AttentionSort::SessionIdentityAscending,
        after.map(AttentionContinuation::SessionIdentity),
    )
    .expect("the fixture catalog query is bounded")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn bounded_pages_cover_large_fleet() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    create_mixed_scale_fleet(&pool).await?;
    let repository = AttentionRepository::new(pool.clone());
    let first = repository.snapshot(identity_query(None)).await?;
    let second = repository
        .snapshot(identity_query(
            first.summaries.last().map(|row| row.session),
        ))
        .await?;
    let third = repository
        .snapshot(identity_query(
            second.summaries.last().map(|row| row.session),
        ))
        .await?;
    let fourth = repository
        .snapshot(identity_query(
            third.summaries.last().map(|row| row.session),
        ))
        .await?;
    let fifth = repository
        .snapshot(identity_query(
            fourth.summaries.last().map(|row| row.session),
        ))
        .await?;
    let sixth = repository
        .snapshot(identity_query(
            fifth.summaries.last().map(|row| row.session),
        ))
        .await?;
    let seventh = repository
        .snapshot(identity_query(
            sixth.summaries.last().map(|row| row.session),
        ))
        .await?;
    let eighth = repository
        .snapshot(identity_query(
            seventh.summaries.last().map(|row| row.session),
        ))
        .await?;
    let ninth = repository
        .snapshot(identity_query(
            eighth.summaries.last().map(|row| row.session),
        ))
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
    let repository = AttentionRepository::new(pool.clone());
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
