#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "standalone PostgreSQL integration fixtures use assertion panics"
)]

use std::error::Error;

use signalbox_domain::{
    CreateSession, DirectModelSelection, DurableCommandId, ModelSelectionRequest,
    RootPlacementGlobalReadIntent, SessionConfigurationDefaults, SessionCreationCause,
    SessionCreationProvenance, SessionId, SessionPlacement, SessionPlacementPath,
    SessionPlacementVersion, TranscriptAncestry,
};
use signalbox_persistence::{
    create_session::{
        CreateSessionCorruption, CreateSessionRepository, CreateSessionRepositoryError,
    },
    local_test_connection_options, migrate,
    session::SessionRepository,
    session_placement::{SessionPlacementRepository, SessionPlacementRepositoryError},
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_placement";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_fsync_enabled()
        .with_tag(POSTGRES_IMAGE_TAG)
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

fn credential_pin() -> signalbox_persistence::SessionCredentialPin {
    signalbox_persistence::SessionCredentialPin::try_new(vec![
        signalbox_persistence::SessionModelCredential::new("fixture", "fixture-primary"),
    ])
    .unwrap()
}

fn session(value: u128) -> SessionId {
    SessionId::from_uuid(Uuid::from_u128(value))
}

fn command(value: u128) -> DurableCommandId {
    DurableCommandId::from_uuid(Uuid::from_u128(value))
}

fn root(path: &str) -> SessionPlacement {
    SessionPlacement::root_global_read(
        SessionPlacementPath::try_new(path.to_owned()).unwrap(),
        RootPlacementGlobalReadIntent::Acknowledged,
    )
    .unwrap()
}

fn creation(
    command_id: DurableCommandId,
    session_id: SessionId,
    placement: SessionPlacement,
) -> signalbox_domain::PreparedCreateSession {
    CreateSession::new_with_placement(
        command_id,
        SessionCreationProvenance::new(
            SessionCreationCause::UserInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(0x1000)),
        )),
        placement,
    )
    .prepare(session_id)
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_root_creation_record_states_global_read_intent_explicitly()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let command_id = command(0x101);
    let session_id = session(0x201);
    let root = root("operator");
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(command_id, session_id, root.clone()))
        .await?;

    let record: (Option<String>, bool) = sqlx::query_as(
        "SELECT placement_path, root_global_read_intent
           FROM create_session_command WHERE command_id = $1",
    )
    .bind(*command_id.as_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        record,
        (
            root.path().map(|path| path.as_str().to_owned()),
            root.records_root_global_read_intent(),
        )
    );
    let loaded = SessionRepository::new(pool.clone())
        .load_session(session_id)
        .await?
        .unwrap();
    assert_eq!(loaded.current_placement().placement(), &root);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_pathless_creation_keeps_the_legacy_unscoped_value() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let command_id = command(0x102);
    let session_id = session(0x202);
    let placement = SessionPlacement::pathless();
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(command_id, session_id, placement.clone()))
        .await?;

    let loaded = SessionRepository::new(pool.clone())
        .load_session(session_id)
        .await?
        .unwrap();
    assert_eq!(
        loaded.current_placement().version(),
        SessionPlacementVersion::INITIAL
    );
    assert_eq!(loaded.current_placement().placement(), &placement);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_missing_placement_head_fails_closed_for_current_and_creation_reads()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session_id = session(0x204);
    let creation = creation(command(0x105), session_id, SessionPlacement::pathless());
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation.clone())
        .await?;
    sqlx::raw_sql(
        "ALTER TABLE session_current_placement DISABLE TRIGGER USER;
         DELETE FROM session_current_placement;
         ALTER TABLE session_current_placement ENABLE TRIGGER USER;",
    )
    .execute(&pool)
    .await?;

    let read_error = SessionPlacementRepository::new(pool.clone())
        .load_current(session_id)
        .await
        .expect_err("a public read must reject a present session without a placement head");
    let SessionPlacementRepositoryError::Corruption(read_reason) = read_error else {
        panic!("missing placement history fails a public read with typed corruption")
    };
    assert_eq!(read_reason, "session placement head missing");

    let creation_error = CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation)
        .await
        .expect_err("creation replay requires its current placement head");
    let CreateSessionRepositoryError::Corruption(CreateSessionCorruption::Missing(field)) =
        creation_error
    else {
        panic!("missing placement head fails creation replay with typed corruption")
    };
    assert_eq!(field, "current_placement_head_version");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_creation_replay_rejects_cross_wired_placement_provenance() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    let first_command = command(0x114);
    let first_session = session(0x208);
    let first = creation(first_command, first_session, SessionPlacement::pathless());
    let repository = CreateSessionRepository::new(pool.clone(), credential_pin());
    repository.handle(first.clone()).await?;
    let second_command = command(0x115);
    repository
        .handle(creation(
            second_command,
            session(0x209),
            SessionPlacement::pathless(),
        ))
        .await?;
    sqlx::query("ALTER TABLE session_placement_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE session_placement_event
            SET provenance_command_id = $2
          WHERE session_id = $1",
    )
    .bind(*first_session.as_uuid())
    .bind(*second_command.as_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_placement_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    let error = repository
        .handle(first)
        .await
        .expect_err("creation replay cannot use another command's placement event");
    let CreateSessionRepositoryError::Corruption(CreateSessionCorruption::Missing(field)) = error
    else {
        panic!("cross-wired creation placement fails with typed corruption")
    };
    assert_eq!(field, "stored_placement_version");

    pool.close().await;
    drop(container);
    Ok(())
}
