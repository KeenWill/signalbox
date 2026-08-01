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
    SessionCreationProvenance, SessionId, SessionPlacement, SessionPlacementEventKind,
    SessionPlacementPath, SessionPlacementVersion, TranscriptAncestry, UpdateSessionPlacement,
    UpdateSessionPlacementResult,
};
use signalbox_persistence::{
    create_session::{
        CreateSessionCorruption, CreateSessionRepository, CreateSessionRepositoryError,
    },
    local_test_connection_options, migrate,
    session::{SessionCorruption, SessionRepository, SessionRepositoryError},
    session_placement::{
        SessionPlacementRepository, SessionPlacementRepositoryError,
        SessionPlacementRepositoryOutcome,
    },
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
const RESULT_SHAPE_CONSTRAINT: &str = "update_session_placement_command_result_shape";

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

fn scoped(path: &str) -> SessionPlacement {
    SessionPlacement::scoped(SessionPlacementPath::try_new(path.to_owned()).unwrap()).unwrap()
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
async fn s36_placement_update_appends_history_and_equal_replay_preserves_it()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let creation_command = command(0x103);
    let session_id = session(0x203);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            creation_command,
            session_id,
            SessionPlacement::pathless(),
        ))
        .await?;
    let update_command = command(0x104);
    let update = UpdateSessionPlacement::new(
        update_command,
        session_id,
        SessionPlacementVersion::INITIAL,
        scoped("projects.foo.session"),
    );
    let repository = SessionPlacementRepository::new(pool.clone());

    let first = repository.handle(update.clone()).await?;
    let SessionPlacementRepositoryOutcome::Recorded(UpdateSessionPlacementResult::Applied(event)) =
        &first
    else {
        panic!("fixture update must apply")
    };
    assert_eq!(event.kind(), SessionPlacementEventKind::Updated);
    assert_eq!(
        event.prior_version(),
        Some(SessionPlacementVersion::INITIAL)
    );
    assert_eq!(
        event.placement().version(),
        SessionPlacementVersion::try_from_u64(2).expect("fixture successor version is positive")
    );
    assert_eq!(repository.handle(update).await?, first);
    let history: Vec<(i64, String)> = sqlx::query_as(
        "SELECT version::bigint, event_kind
           FROM session_placement_event
          WHERE session_id = $1 ORDER BY version",
    )
    .bind(*session_id.as_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        history,
        vec![(1, "created".to_owned()), (2, "updated".to_owned())]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_missing_placement_head_fails_closed_for_reads_and_updates()
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
    let repository = SessionPlacementRepository::new(pool.clone());
    let read_error = repository
        .load_current(session_id)
        .await
        .expect_err("a public read must reject a present session without a placement head");
    let SessionPlacementRepositoryError::Corruption(read_reason) = read_error else {
        panic!("missing placement history fails a public read with typed corruption")
    };
    assert_eq!(read_reason, "session placement head missing");

    let update = UpdateSessionPlacement::new(
        command(0x106),
        session_id,
        SessionPlacementVersion::INITIAL,
        SessionPlacement::pathless(),
    );

    let error = repository
        .handle(update)
        .await
        .expect_err("a present session without placement history is corruption");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("missing placement history fails with typed corruption")
    };
    assert_eq!(reason, "session placement head missing");
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
async fn s36_impossible_mismatch_receipt_is_rejected_by_schema_and_replay()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session_id = session(0x206);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(0x110),
            session_id,
            SessionPlacement::pathless(),
        ))
        .await?;
    let update_command = command(0x111);
    let expected =
        SessionPlacementVersion::try_from_u64(2).expect("fixture mismatch version is positive");
    let update = UpdateSessionPlacement::new(
        update_command,
        session_id,
        expected,
        SessionPlacement::pathless(),
    );
    let repository = SessionPlacementRepository::new(pool.clone());
    repository.handle(update.clone()).await?;
    sqlx::query("ALTER TABLE update_session_placement_command DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    let constraint_error = sqlx::query(
        "UPDATE update_session_placement_command
            SET result_current_version = expected_version
          WHERE command_id = $1",
    )
    .bind(*update_command.as_uuid())
    .execute(&pool)
    .await
    .expect_err("mismatch evidence cannot claim the expected version is current");
    assert_eq!(
        constraint_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some(RESULT_SHAPE_CONSTRAINT)
    );
    sqlx::query(
        "ALTER TABLE update_session_placement_command
         DROP CONSTRAINT update_session_placement_command_result_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE update_session_placement_command
            SET result_current_version = expected_version
          WHERE command_id = $1",
    )
    .bind(*update_command.as_uuid())
    .execute(&pool)
    .await?;

    let error = repository
        .handle(update)
        .await
        .expect_err("replay independently rejects impossible mismatch evidence");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("impossible mismatch evidence fails with typed corruption")
    };
    assert_eq!(reason, "mismatch rejection version");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_impossible_exhaustion_receipt_is_rejected_by_schema_and_replay()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session_id = session(0x207);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(0x112),
            session_id,
            SessionPlacement::pathless(),
        ))
        .await?;
    let update_command = command(0x113);
    let expected = SessionPlacementVersion::try_from_u64(2)
        .expect("fixture non-exhausted version is positive");
    let update = UpdateSessionPlacement::new(
        update_command,
        session_id,
        expected,
        SessionPlacement::pathless(),
    );
    let repository = SessionPlacementRepository::new(pool.clone());
    repository.handle(update.clone()).await?;
    sqlx::query("ALTER TABLE update_session_placement_command DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    let constraint_error = sqlx::query(
        "UPDATE update_session_placement_command
            SET rejection_kind = 'version_exhausted',
                result_current_version = expected_version
          WHERE command_id = $1",
    )
    .bind(*update_command.as_uuid())
    .execute(&pool)
    .await
    .expect_err("exhaustion evidence requires the maximum version");
    assert_eq!(
        constraint_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some(RESULT_SHAPE_CONSTRAINT)
    );
    sqlx::query(
        "ALTER TABLE update_session_placement_command
         DROP CONSTRAINT update_session_placement_command_result_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE update_session_placement_command
            SET rejection_kind = 'version_exhausted',
                result_current_version = expected_version
          WHERE command_id = $1",
    )
    .bind(*update_command.as_uuid())
    .execute(&pool)
    .await?;

    let error = repository
        .handle(update)
        .await
        .expect_err("replay independently rejects non-maximum exhaustion evidence");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("impossible exhaustion evidence fails with typed corruption")
    };
    assert_eq!(reason, "version exhaustion ordinal");

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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_cross_wired_applied_receipt_fails_closed() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session_id = session(0x205);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(0x107),
            session_id,
            SessionPlacement::pathless(),
        ))
        .await?;
    let first_command = command(0x108);
    let first = UpdateSessionPlacement::new(
        first_command,
        session_id,
        SessionPlacementVersion::INITIAL,
        scoped("projects.foo.first"),
    );
    let second = UpdateSessionPlacement::new(
        command(0x109),
        session_id,
        SessionPlacementVersion::try_from_u64(2).expect("fixture version is positive"),
        scoped("projects.foo.second"),
    );
    let repository = SessionPlacementRepository::new(pool.clone());
    repository.handle(first.clone()).await?;
    repository.handle(second).await?;
    sqlx::query("ALTER TABLE update_session_placement_command DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE update_session_placement_command SET result_version = 3 WHERE command_id = $1",
    )
    .bind(*first_command.as_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE update_session_placement_command ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    let error = repository
        .handle(first)
        .await
        .expect_err("an applied receipt cannot name another command's event");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("cross-wired receipt fails with typed corruption")
    };
    assert_eq!(reason, "applied event provenance");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_current_read_and_update_authenticate_the_placement_update_receipt()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session_id = session(0x20a);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(0x116),
            session_id,
            SessionPlacement::pathless(),
        ))
        .await?;
    SessionPlacementRepository::new(pool.clone())
        .handle(UpdateSessionPlacement::new(
            command(0x117),
            session_id,
            SessionPlacementVersion::INITIAL,
            scoped("projects.foo.recorded"),
        ))
        .await?;
    sqlx::query("ALTER TABLE session_placement_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE session_placement_event
            SET placement_path = 'projects.foo.forged'
          WHERE session_id = $1 AND version = 2",
    )
    .bind(*session_id.as_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_placement_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    let error = SessionRepository::new(pool.clone())
        .load_session(session_id)
        .await
        .expect_err("current placement must agree with its provenance receipt");
    let SessionRepositoryError::Corruption(SessionCorruption::Inconsistent(reason)) = error else {
        panic!("cross-wired current placement fails with typed session corruption")
    };
    assert_eq!(reason, "current placement provenance receipt");
    let update_error = SessionPlacementRepository::new(pool.clone())
        .handle(UpdateSessionPlacement::new(
            command(0x118),
            session_id,
            SessionPlacementVersion::try_from_u64(2).expect("fixture current version is positive"),
            scoped("projects.foo.replacement"),
        ))
        .await
        .expect_err("an update must authenticate the current event before advancing it");
    let SessionPlacementRepositoryError::Corruption(update_reason) = update_error else {
        panic!("cross-wired current placement fails update with typed corruption")
    };
    assert_eq!(update_reason, "current placement provenance receipt");

    pool.close().await;
    drop(container);
    Ok(())
}
