//! PostgreSQL integration proof for bounded stable session timeline reads.

#![allow(
    clippy::expect_used,
    reason = "this standalone integration-test crate uses explicit fixture expectations"
)]

use std::error::Error;

use signalbox_application::{TimelineWindowAnchor, TimelineWindowLimits};
use signalbox_domain::{
    CreateSession, DirectModelSelection, DurableCommandId, ModelSelectionRequest,
    SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance, SessionId,
    TranscriptAncestry,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository, session_timeline::SessionTimelineRepository,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::{migrated_postgres, test_session_credential_pin};

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

    assert_eq!(
        descriptor.sizes.item_count,
        u64::try_from(first.items.len()).expect("fixture window length fits u64")
    );
    assert_eq!(descriptor.bounds.latest, Some(creation_address));
    assert_eq!(first.items[0].address, creation_address);
    assert_eq!(latest.items[0].address, creation_address);
    assert_eq!(around.items[0].address, creation_address);
    assert!(!first.has_more_before);
    assert!(!latest.has_more_after);

    pool.close().await;
    drop(container);
    Ok(())
}
