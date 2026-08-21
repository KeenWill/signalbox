//! Scale and overflow proof for the bounded fleet attention projection.

use crate::*;
use signalbox_application::{
    AttentionChanges, AttentionCursor, max_attention_change_items, max_attention_snapshot_items,
};
use signalbox_persistence::attention::AttentionRepository;

const FLEET_SIZE: u128 = 514;
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn bounded_pages_cover_large_fleet() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    create_mixed_scale_fleet(&pool).await?;
    let repository = AttentionRepository::new(pool.clone());
    let first = repository.snapshot(None).await?;
    let second = repository.snapshot(first.continuation_after).await?;
    let third = repository.snapshot(second.continuation_after).await?;
    let fourth = repository.snapshot(third.continuation_after).await?;
    let fifth = repository.snapshot(fourth.continuation_after).await?;
    let sixth = repository.snapshot(fifth.continuation_after).await?;
    let seventh = repository.snapshot(sixth.continuation_after).await?;
    let eighth = repository.snapshot(seventh.continuation_after).await?;
    let ninth = repository.snapshot(eighth.continuation_after).await?;

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
        first.continuation_after,
        Some(first.summaries.last().unwrap().session)
    );
    assert_eq!(
        second.continuation_after,
        Some(second.summaries.last().unwrap().session)
    );
    assert_eq!(
        third.continuation_after,
        Some(third.summaries.last().unwrap().session)
    );
    assert_eq!(
        fourth.continuation_after,
        Some(fourth.summaries.last().unwrap().session)
    );
    assert_eq!(
        fifth.continuation_after,
        Some(fifth.summaries.last().unwrap().session)
    );
    assert_eq!(
        sixth.continuation_after,
        Some(sixth.summaries.last().unwrap().session)
    );
    assert_eq!(
        seventh.continuation_after,
        Some(seventh.summaries.last().unwrap().session)
    );
    assert_eq!(
        eighth.continuation_after,
        Some(eighth.summaries.last().unwrap().session)
    );
    assert_eq!(ninth.continuation_after, None);
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
    let first = repository.snapshot(None).await?;
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
