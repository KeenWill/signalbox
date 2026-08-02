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
    SessionPlacementVersion, TranscriptAncestry, UpdateSessionPlacement,
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
const UPDATE_FIXTURE_SESSION_ID_SEED: u128 = 0x20c;
const UPDATE_FIXTURE_CREATION_COMMAND_ID_SEED: u128 = 0x10c;
const UPDATE_FIXTURE_COMMAND_ID_SEED: u128 = 0x10d;
const UPDATE_FIXTURE_RESULT_VERSION: u64 = 2;
const UPDATE_FIXTURE_REPLACEMENT_PATH: &str = "projects.foo.session";
const UPDATE_FIXTURE_CONFLICTING_REPLACEMENT_PATH: &str = "projects.foo.conflict";
const PAGED_HISTORY_SESSION_ID_SEED: u128 = 0x20f;
const PAGED_HISTORY_CREATION_COMMAND_ID_SEED: u128 = 0x10f;
const PAGED_HISTORY_UPDATE_COMMAND_ID_SEED: u128 = 0x300;
const PAGED_HISTORY_UPDATE_COUNT: u64 = 65;
const PAGED_HISTORY_EXPECTED_VERSION: u64 = 66;
const PAGED_HISTORY_PATH_PREFIX: &str = "projects.history.revision";

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

fn scoped(path: &str) -> SessionPlacement {
    SessionPlacement::scoped(SessionPlacementPath::try_new(path.to_owned()).unwrap()).unwrap()
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

#[track_caller]
fn recorded_applied_update(
    outcome: &SessionPlacementRepositoryOutcome,
) -> &signalbox_domain::UpdateSessionPlacementApplied {
    let SessionPlacementRepositoryOutcome::Recorded(UpdateSessionPlacementResult::Applied(applied)) =
        outcome
    else {
        panic!("the placement update fixture must record an applied result")
    };
    applied
}

#[track_caller]
fn assert_placement_provenance_corruption(
    result: Result<Option<signalbox_domain::Session>, SessionRepositoryError>,
) {
    let Err(SessionRepositoryError::Corruption(SessionCorruption::Inconsistent(reason))) = result
    else {
        panic!("corrupt placement provenance must fail the ordinary session load")
    };
    assert_eq!(reason, "session placement provenance receipt");
}

#[track_caller]
fn assert_placement_repository_corruption<T>(result: Result<T, SessionPlacementRepositoryError>) {
    let Err(SessionPlacementRepositoryError::Corruption(reason)) = result else {
        panic!("corrupt placement history must fail with typed corruption")
    };
    assert_eq!(reason, "session placement provenance receipt");
}

async fn cross_wire_initial_placement_provenance(
    pool: &PgPool,
    session_id: SessionId,
    update_command_id: DurableCommandId,
) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE session_placement_event DISABLE TRIGGER USER")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE session_placement_event
            SET provenance_command_id = $2
          WHERE session_id = $1 AND version = 1",
    )
    .bind(*session_id.as_uuid())
    .bind(*update_command_id.as_uuid())
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE session_placement_event ENABLE TRIGGER USER")
        .execute(pool)
        .await?;
    Ok(())
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
async fn s36_missing_placement_head_fails_closed_for_creation_reads() -> Result<(), Box<dyn Error>>
{
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_post_migration_legacy_creation_materializes_pathless_placement()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let command_id = command(0x118);
    let session_id = session(0x20c);
    let legacy_placement = SessionPlacement::pathless();
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(command_id, session_id, legacy_placement.clone()))
        .await?;

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "CREATE TEMP TABLE legacy_creation ON COMMIT DROP AS
         SELECT * FROM create_session_command WHERE command_id = $1",
    )
    .bind(*command_id.as_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM session_current_placement WHERE session_id = $1")
        .bind(*session_id.as_uuid())
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM session_placement_event WHERE session_id = $1")
        .bind(*session_id.as_uuid())
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM create_session_command WHERE command_id = $1")
        .bind(*command_id.as_uuid())
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE durable_command SET storage_version = 4 WHERE command_id = $1")
        .bind(*command_id.as_uuid())
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "UPDATE legacy_creation
            SET storage_version = 4, placement_path = NULL,
                root_global_read_intent = FALSE",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query("SET LOCAL session_replication_role = origin")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO create_session_command
            (command_id, command_kind, storage_version, creation_cause,
             ancestry_kind, initial_defaults_version, model_selection_kind,
             direct_model_selection_id, model_alias_id, dangerous_tool_auto_approval,
             system_prompt, template_name, template_content_digest, placement_path,
             root_global_read_intent, result_kind, created_session_id)
         SELECT command_id, command_kind, storage_version, creation_cause,
                ancestry_kind, initial_defaults_version, model_selection_kind,
                direct_model_selection_id, model_alias_id, dangerous_tool_auto_approval,
                system_prompt, template_name, template_content_digest, placement_path,
                root_global_read_intent, result_kind, created_session_id
           FROM legacy_creation",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let loaded = SessionRepository::new(pool.clone())
        .load_session(session_id)
        .await?
        .expect("the legacy session remains readable after rolling forward");
    assert_eq!(
        loaded.current_placement(),
        &signalbox_domain::VersionedSessionPlacement::initial(legacy_placement)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_applied_update_receipt_requires_the_expected_predecessor() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    let command_id = command(0x116);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, $2, $3, transaction_timestamp())",
    )
    .bind(*command_id.as_uuid())
    .bind("update_session_placement")
    .bind(1_i16)
    .execute(&mut *transaction)
    .await?;
    let malformed = sqlx::query(
        "INSERT INTO update_session_placement_command
            (command_id, command_kind, storage_version, session_id,
             expected_version, replacement_path, root_global_read_intent,
             result_kind, rejection_kind, result_version, result_current_version)
         VALUES ($1, 'update_session_placement', 1, $2,
                 7, NULL, FALSE, 'applied', NULL, 2, NULL)",
    )
    .bind(*command_id.as_uuid())
    .bind(*session(0x20a).as_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("an applied update receipt must advance its expected predecessor");
    let database_error = malformed
        .as_database_error()
        .expect("PostgreSQL reports the applied-result shape constraint");

    assert_eq!(
        database_error.constraint(),
        Some("update_session_placement_command_result_shape")
    );

    transaction.rollback().await?;

    let command_id = command(0x117);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, $2, $3, transaction_timestamp())",
    )
    .bind(*command_id.as_uuid())
    .bind("update_session_placement")
    .bind(1_i16)
    .execute(&mut *transaction)
    .await?;
    let malformed = sqlx::query(
        "INSERT INTO update_session_placement_command
            (command_id, command_kind, storage_version, session_id,
             expected_version, replacement_path, root_global_read_intent,
             result_kind, rejection_kind, result_version, result_current_version)
         VALUES ($1, 'update_session_placement', 1, $2,
                 1, NULL, FALSE, 'applied', NULL, NULL, NULL)",
    )
    .bind(*command_id.as_uuid())
    .bind(*session(0x20b).as_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("an applied update receipt must name its resulting version");
    let database_error = malformed
        .as_database_error()
        .expect("PostgreSQL reports the applied-result shape constraint");

    assert_eq!(
        database_error.constraint(),
        Some("update_session_placement_command_result_shape")
    );

    transaction.rollback().await?;
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_inv012_update_handle_applies_first_command() -> Result<(), Box<dyn Error>> {
    let (container, pool, repository, update) = placement_update_fixture().await?;
    let first = repository.handle(update).await?;
    let applied = recorded_applied_update(&first);
    let expected_version = SessionPlacementVersion::try_from_u64(UPDATE_FIXTURE_RESULT_VERSION)
        .expect("the fixture result version is positive");

    assert_eq!(
        applied.event().prior_version(),
        Some(SessionPlacementVersion::INITIAL)
    );
    assert_eq!(applied.event().placement().version(), expected_version);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_inv012_update_handle_replays_equal_command() -> Result<(), Box<dyn Error>> {
    let (container, pool, repository, update) = placement_update_fixture().await?;
    let first = repository.handle(update.clone()).await?;

    assert_eq!(repository.handle(update).await?, first);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_inv012_update_replay_authenticates_the_applied_predecessor_chain()
-> Result<(), Box<dyn Error>> {
    let (container, pool, repository, update) = placement_update_fixture().await?;
    repository.handle(update.clone()).await?;
    cross_wire_initial_placement_provenance(&pool, update.session(), update.command_id()).await?;

    assert_placement_repository_corruption(repository.handle(update).await);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_inv012_current_placement_rejects_an_incomplete_applied_receipt()
-> Result<(), Box<dyn Error>>
{
    let (container, pool, repository, update) = placement_update_fixture().await?;
    repository.handle(update.clone()).await?;
    sqlx::query(
        "ALTER TABLE update_session_placement_command
            DROP CONSTRAINT update_session_placement_command_result_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE update_session_placement_command DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE update_session_placement_command
            SET rejection_kind = 'session_not_found'
          WHERE command_id = $1",
    )
    .bind(*update.command_id().as_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE update_session_placement_command ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    assert_placement_repository_corruption(repository.load_current(update.session()).await);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_inv012_update_handle_rejects_conflicting_reuse() -> Result<(), Box<dyn Error>> {
    let (container, pool, repository, update) = placement_update_fixture().await?;
    repository.handle(update).await?;
    let command_id = command(UPDATE_FIXTURE_COMMAND_ID_SEED);
    let session_id = session(UPDATE_FIXTURE_SESSION_ID_SEED);

    assert_eq!(
        repository
            .handle(UpdateSessionPlacement::new(
                command_id,
                session_id,
                SessionPlacementVersion::INITIAL,
                scoped(UPDATE_FIXTURE_CONFLICTING_REPLACEMENT_PATH),
            ))
            .await?,
        SessionPlacementRepositoryOutcome::ConflictingReuse { command_id }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

async fn placement_update_fixture() -> Result<
    (
        ContainerAsync<Postgres>,
        PgPool,
        SessionPlacementRepository,
        UpdateSessionPlacement,
    ),
    Box<dyn Error>,
> {
    let (container, pool) = migrated_postgres().await?;
    let session_id = session(UPDATE_FIXTURE_SESSION_ID_SEED);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(UPDATE_FIXTURE_CREATION_COMMAND_ID_SEED),
            session_id,
            SessionPlacement::pathless(),
        ))
        .await?;
    let command_id = command(UPDATE_FIXTURE_COMMAND_ID_SEED);
    let update = UpdateSessionPlacement::new(
        command_id,
        session_id,
        SessionPlacementVersion::INITIAL,
        scoped(UPDATE_FIXTURE_REPLACEMENT_PATH),
    );
    let repository = SessionPlacementRepository::new(pool.clone());
    Ok((container, pool, repository, update))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_complete_history_authentication_crosses_bounded_pages() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session_id = session(PAGED_HISTORY_SESSION_ID_SEED);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(PAGED_HISTORY_CREATION_COMMAND_ID_SEED),
            session_id,
            SessionPlacement::pathless(),
        ))
        .await?;
    let repository = SessionPlacementRepository::new(pool.clone());
    let expected = append_paged_history_fixture(&repository, session_id).await?;
    let expected_version = SessionPlacementVersion::try_from_u64(PAGED_HISTORY_EXPECTED_VERSION)
        .expect("the fixture's pinned final version is positive");

    assert_eq!(expected.version(), expected_version);
    assert_eq!(
        repository.load_current(session_id).await?.unwrap(),
        expected
    );

    pool.close().await;
    drop(container);
    Ok(())
}

async fn append_paged_history_fixture(
    repository: &SessionPlacementRepository,
    session_id: SessionId,
) -> Result<signalbox_domain::VersionedSessionPlacement, Box<dyn Error>> {
    let mut prior_version = SessionPlacementVersion::INITIAL;
    let mut current = None;
    for update_index in 1..=PAGED_HISTORY_UPDATE_COUNT {
        let replacement = scoped(format!("{PAGED_HISTORY_PATH_PREFIX}{update_index}").as_str());
        let outcome = repository
            .handle(UpdateSessionPlacement::new(
                command(PAGED_HISTORY_UPDATE_COMMAND_ID_SEED + u128::from(update_index)),
                session_id,
                prior_version,
                replacement,
            ))
            .await?;
        let applied = recorded_applied_update(&outcome);
        prior_version = applied.event().placement().version();
        current = Some(applied.event().placement().clone());
    }
    Ok(current.expect("the fixture appends at least one placement update"))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_inv002_inv012_ordinary_session_load_authenticates_complete_placement_history()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session_id = session(UPDATE_FIXTURE_SESSION_ID_SEED);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(UPDATE_FIXTURE_CREATION_COMMAND_ID_SEED),
            session_id,
            SessionPlacement::pathless(),
        ))
        .await?;
    let update_command_id = command(UPDATE_FIXTURE_COMMAND_ID_SEED);
    SessionPlacementRepository::new(pool.clone())
        .handle(UpdateSessionPlacement::new(
            update_command_id,
            session_id,
            SessionPlacementVersion::INITIAL,
            scoped(UPDATE_FIXTURE_REPLACEMENT_PATH),
        ))
        .await?;
    cross_wire_initial_placement_provenance(&pool, session_id, update_command_id).await?;

    assert_placement_provenance_corruption(
        SessionRepository::new(pool.clone())
            .load_session(session_id)
            .await,
    );

    pool.close().await;
    drop(container);
    Ok(())
}
