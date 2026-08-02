#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "standalone PostgreSQL integration fixtures use assertion panics"
)]

use std::error::Error;

use expect_test::expect;
use signalbox_domain::{
    CreateSession, DirectModelSelection, DurableCommandId, ModelSelectionRequest,
    RootPlacementGlobalReadIntent, SessionConfigurationDefaults, SessionCreationCause,
    SessionCreationProvenance, SessionId, SessionPlacement, SessionPlacementEventKind,
    SessionPlacementPath, SessionPlacementVersion, TranscriptAncestry, UpdateSessionPlacement,
    UpdateSessionPlacementResult,
};
use signalbox_expect_table::table;
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

#[derive(Debug, Eq, PartialEq, sqlx::FromRow)]
struct PlacementHistoryRow {
    version: i64,
    event_kind: String,
}

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
async fn s36_placement_update_rejects_reserved_command_identities_before_claim()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session_id = session(0x230);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(0x130),
            session_id,
            SessionPlacement::pathless(),
        ))
        .await?;
    let repository = SessionPlacementRepository::new(pool.clone());
    let nil_command = DurableCommandId::from_uuid(Uuid::nil());
    let nil_error = repository
        .handle(UpdateSessionPlacement::new(
            nil_command,
            session_id,
            SessionPlacementVersion::INITIAL,
            SessionPlacement::pathless(),
        ))
        .await
        .expect_err("nil command identity is rejected before a durable claim");
    let SessionPlacementRepositoryError::InvalidCommandId = nil_error else {
        panic!("nil command identity returns the typed rejection")
    };
    let max_command = DurableCommandId::from_uuid(Uuid::max());
    let max_error = repository
        .handle(UpdateSessionPlacement::new(
            max_command,
            session_id,
            SessionPlacementVersion::INITIAL,
            SessionPlacement::pathless(),
        ))
        .await
        .expect_err("max command identity is rejected before a durable claim");
    let SessionPlacementRepositoryError::InvalidCommandId = max_error else {
        panic!("max command identity returns the typed rejection")
    };
    let sentinel_claim_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM durable_command WHERE command_id = $1 OR command_id = $2",
    )
    .bind(*nil_command.as_uuid())
    .bind(*max_command.as_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(sentinel_claim_count, 0);

    pool.close().await;
    drop(container);
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

struct PlacementUpdateFixture {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    repository: SessionPlacementRepository,
    update: UpdateSessionPlacement,
    expected_result_version: SessionPlacementVersion,
    session: SessionId,
}

async fn placement_update_fixture() -> Result<PlacementUpdateFixture, Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session = session(0x203);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(0x103),
            session,
            SessionPlacement::pathless(),
        ))
        .await?;
    let update = UpdateSessionPlacement::new(
        command(0x104),
        session,
        SessionPlacementVersion::INITIAL,
        scoped("projects.foo.session"),
    );
    let repository = SessionPlacementRepository::new(pool.clone());
    Ok(PlacementUpdateFixture {
        container,
        pool,
        repository,
        update,
        expected_result_version: SessionPlacementVersion::try_from_u64(
            UPDATE_FIXTURE_RESULT_VERSION,
        )
        .expect("fixture result version is positive"),
        session,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_placement_update_returns_successor_event_shape() -> Result<(), Box<dyn Error>> {
    let fixture = placement_update_fixture().await?;
    let expected_prior = fixture.update.expected_version();
    let expected_version = fixture.expected_result_version;
    let result = fixture.repository.handle(fixture.update).await?;
    let SessionPlacementRepositoryOutcome::Recorded(UpdateSessionPlacementResult::Applied(applied)) =
        result
    else {
        panic!("fixture update must apply")
    };
    let event = applied.event();

    assert_eq!(event.kind(), SessionPlacementEventKind::Updated);
    assert_eq!(event.prior_version(), Some(expected_prior));
    assert_eq!(event.placement().version(), expected_version);

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_placement_update_appends_created_and_updated_history() -> Result<(), Box<dyn Error>> {
    let fixture = placement_update_fixture().await?;
    fixture.repository.handle(fixture.update).await?;
    let history: Vec<PlacementHistoryRow> = sqlx::query_as(
        "SELECT version::bigint AS version, event_kind AS event_kind
           FROM session_placement_event
          WHERE session_id = $1 ORDER BY version",
    )
    .bind(*fixture.session.as_uuid())
    .fetch_all(&fixture.pool)
    .await?;

    expect![[r#"
        ┌─────────┬────────────┐
        │ version │ event_kind │
        ├─────────┼────────────┤
        │       1 │ created    │
        │       2 │ updated    │
        └─────────┴────────────┘
    "#]]
    .assert_eq(&table(history));

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

struct MissingPlacementHeadFixture {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    session: SessionId,
    creation: signalbox_domain::PreparedCreateSession,
}

async fn missing_placement_head_fixture() -> Result<MissingPlacementHeadFixture, Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session = session(0x204);
    let creation = creation(command(0x105), session, SessionPlacement::pathless());
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
    Ok(MissingPlacementHeadFixture {
        container,
        pool,
        session,
        creation,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_public_placement_read_rejects_a_missing_current_head() -> Result<(), Box<dyn Error>> {
    let fixture = missing_placement_head_fixture().await?;
    let error = SessionPlacementRepository::new(fixture.pool.clone())
        .load_current(fixture.session)
        .await
        .expect_err("a public read must reject a present session without a placement head");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("a missing placement head fails a public read with typed corruption")
    };
    assert_eq!(reason, "session placement head missing");

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_placement_update_rejects_a_missing_current_head() -> Result<(), Box<dyn Error>> {
    let fixture = missing_placement_head_fixture().await?;
    let update = UpdateSessionPlacement::new(
        command(0x106),
        fixture.session,
        SessionPlacementVersion::INITIAL,
        SessionPlacement::pathless(),
    );
    let error = SessionPlacementRepository::new(fixture.pool.clone())
        .handle(update)
        .await
        .expect_err("a present session without placement history is corruption");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("a missing placement head fails an update with typed corruption")
    };
    assert_eq!(reason, "session placement head missing");

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_creation_replay_rejects_a_missing_current_placement_head() -> Result<(), Box<dyn Error>>
{
    let fixture = missing_placement_head_fixture().await?;
    let error = CreateSessionRepository::new(fixture.pool.clone(), credential_pin())
        .handle(fixture.creation)
        .await
        .expect_err("creation replay requires its current placement head");
    let CreateSessionRepositoryError::Corruption(CreateSessionCorruption::Missing(field)) = error
    else {
        panic!("a missing placement head fails creation replay with typed corruption")
    };
    assert_eq!(field, "current_placement_head_version");

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

struct StatefulRejectionReceiptFixture {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    repository: SessionPlacementRepository,
    update: UpdateSessionPlacement,
    command: DurableCommandId,
}

async fn stateful_rejection_receipt_fixture()
-> Result<StatefulRejectionReceiptFixture, Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session = session(0x206);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(0x110),
            session,
            SessionPlacement::pathless(),
        ))
        .await?;
    let command = command(0x111);
    let update = UpdateSessionPlacement::new(
        command,
        session,
        SessionPlacementVersion::try_from_u64(2).expect("fixture mismatch version is positive"),
        SessionPlacement::pathless(),
    );
    let repository = SessionPlacementRepository::new(pool.clone());
    repository.handle(update.clone()).await?;
    sqlx::query("ALTER TABLE update_session_placement_command DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    Ok(StatefulRejectionReceiptFixture {
        container,
        pool,
        repository,
        update,
        command,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_mismatch_receipt_schema_rejects_the_expected_version_as_current()
-> Result<(), Box<dyn Error>> {
    let fixture = stateful_rejection_receipt_fixture().await?;
    let error = sqlx::query(
        "UPDATE update_session_placement_command
            SET result_current_version = expected_version
          WHERE command_id = $1",
    )
    .bind(*fixture.command.as_uuid())
    .execute(&fixture.pool)
    .await
    .expect_err("mismatch evidence cannot claim the expected version is current");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some(RESULT_SHAPE_CONSTRAINT)
    );

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_mismatch_receipt_replay_rejects_the_expected_version_as_current()
-> Result<(), Box<dyn Error>> {
    let fixture = stateful_rejection_receipt_fixture().await?;
    sqlx::query(
        "ALTER TABLE update_session_placement_command
         DROP CONSTRAINT update_session_placement_command_result_shape",
    )
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "UPDATE update_session_placement_command
            SET result_current_version = expected_version
          WHERE command_id = $1",
    )
    .bind(*fixture.command.as_uuid())
    .execute(&fixture.pool)
    .await?;

    let error = fixture
        .repository
        .handle(fixture.update)
        .await
        .expect_err("replay independently rejects impossible mismatch evidence");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("impossible mismatch evidence fails with typed corruption")
    };
    assert_eq!(reason, "mismatch rejection version");

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_exhaustion_receipt_schema_requires_the_maximum_version() -> Result<(), Box<dyn Error>>
{
    let fixture = stateful_rejection_receipt_fixture().await?;
    let error = sqlx::query(
        "UPDATE update_session_placement_command
            SET rejection_kind = 'version_exhausted',
                result_current_version = expected_version
          WHERE command_id = $1",
    )
    .bind(*fixture.command.as_uuid())
    .execute(&fixture.pool)
    .await
    .expect_err("exhaustion evidence requires the maximum version");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some(RESULT_SHAPE_CONSTRAINT)
    );

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_exhaustion_receipt_replay_requires_the_maximum_version() -> Result<(), Box<dyn Error>>
{
    let fixture = stateful_rejection_receipt_fixture().await?;
    sqlx::query(
        "ALTER TABLE update_session_placement_command
         DROP CONSTRAINT update_session_placement_command_result_shape",
    )
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "UPDATE update_session_placement_command
            SET rejection_kind = 'version_exhausted',
                result_current_version = expected_version
          WHERE command_id = $1",
    )
    .bind(*fixture.command.as_uuid())
    .execute(&fixture.pool)
    .await?;

    let error = fixture
        .repository
        .handle(fixture.update)
        .await
        .expect_err("replay independently rejects non-maximum exhaustion evidence");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("impossible exhaustion evidence fails with typed corruption")
    };
    assert_eq!(reason, "version exhaustion ordinal");

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_public_placement_read_rejects_cross_wired_creation_provenance()
-> Result<(), Box<dyn Error>> {
    let fixture = corrupt_creation_placement_provenance_fixture().await?;
    let error = SessionPlacementRepository::new(fixture.pool.clone())
        .load_current(fixture.session)
        .await
        .expect_err("a public placement read must authenticate creation provenance");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("cross-wired creation provenance fails public read with typed corruption")
    };
    assert_eq!(reason, "session placement provenance receipt");

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_creation_replay_rejects_cross_wired_placement_provenance() -> Result<(), Box<dyn Error>>
{
    let fixture = corrupt_creation_placement_provenance_fixture().await?;
    let error = CreateSessionRepository::new(fixture.pool.clone(), credential_pin())
        .handle(fixture.creation)
        .await
        .expect_err("creation replay cannot use another command's placement event");
    let CreateSessionRepositoryError::Corruption(CreateSessionCorruption::Missing(field)) = error
    else {
        panic!("cross-wired creation placement fails with typed corruption")
    };
    assert_eq!(field, "stored_placement_version");

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

struct CorruptCreationPlacementProvenanceFixture {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    session: SessionId,
    creation: signalbox_domain::PreparedCreateSession,
}

async fn corrupt_creation_placement_provenance_fixture()
-> Result<CorruptCreationPlacementProvenanceFixture, Box<dyn Error>> {
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
    Ok(CorruptCreationPlacementProvenanceFixture {
        container,
        pool,
        session: first_session,
        creation: first,
    })
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
    let second_command = command(0x109);
    let second = UpdateSessionPlacement::new(
        second_command,
        session_id,
        SessionPlacementVersion::try_from_u64(2).expect("fixture version is positive"),
        scoped("projects.foo.second"),
    );
    let repository = SessionPlacementRepository::new(pool.clone());
    repository.handle(first.clone()).await?;
    repository.handle(second).await?;
    sqlx::query("ALTER TABLE session_placement_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE session_placement_event
            SET provenance_command_id = $2
          WHERE session_id = $1 AND version = 2",
    )
    .bind(*session_id.as_uuid())
    .bind(*second_command.as_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_placement_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    let error = repository
        .handle(first)
        .await
        .expect_err("an applied receipt cannot bypass placement history authentication");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("cross-wired receipt fails with typed corruption")
    };
    assert_eq!(reason, "session placement provenance receipt");

    pool.close().await;
    drop(container);
    Ok(())
}

struct CorruptPlacementPredecessorFixture {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    session: SessionId,
}

async fn corrupt_placement_predecessor_fixture()
-> Result<CorruptPlacementPredecessorFixture, Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session = session(0x231);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(0x131),
            session,
            SessionPlacement::pathless(),
        ))
        .await?;
    let repository = SessionPlacementRepository::new(pool.clone());
    repository
        .handle(UpdateSessionPlacement::new(
            command(0x132),
            session,
            SessionPlacementVersion::INITIAL,
            scoped("projects.foo.first"),
        ))
        .await?;
    let successor_command = command(0x133);
    repository
        .handle(UpdateSessionPlacement::new(
            successor_command,
            session,
            SessionPlacementVersion::try_from_u64(2)
                .expect("fixture predecessor version is positive"),
            scoped("projects.foo.second"),
        ))
        .await?;
    sqlx::query("ALTER TABLE session_placement_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE session_placement_event
            SET provenance_command_id = $2
          WHERE session_id = $1 AND version = 2",
    )
    .bind(*session.as_uuid())
    .bind(*successor_command.as_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_placement_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    Ok(CorruptPlacementPredecessorFixture {
        container,
        pool,
        session,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_current_read_authenticates_every_placement_predecessor() -> Result<(), Box<dyn Error>>
{
    let fixture = corrupt_placement_predecessor_fixture().await?;
    let error = SessionPlacementRepository::new(fixture.pool.clone())
        .load_current(fixture.session)
        .await
        .expect_err("current placement requires an authenticated predecessor chain");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("a cross-wired predecessor fails current read with typed corruption")
    };
    assert_eq!(reason, "session placement provenance receipt");

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_placement_update_authenticates_every_placement_predecessor()
-> Result<(), Box<dyn Error>> {
    let fixture = corrupt_placement_predecessor_fixture().await?;
    let error = SessionPlacementRepository::new(fixture.pool.clone())
        .handle(UpdateSessionPlacement::new(
            command(0x134),
            fixture.session,
            SessionPlacementVersion::try_from_u64(3).expect("fixture current version is positive"),
            scoped("projects.foo.third"),
        ))
        .await
        .expect_err("placement update requires an authenticated predecessor chain");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("a cross-wired predecessor fails update with typed corruption")
    };
    assert_eq!(reason, "session placement provenance receipt");

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

struct CorruptPlacementUpdateReceiptFixture {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    session: SessionId,
}

async fn corrupt_placement_update_receipt_fixture()
-> Result<CorruptPlacementUpdateReceiptFixture, Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session = session(0x20a);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(0x116),
            session,
            SessionPlacement::pathless(),
        ))
        .await?;
    SessionPlacementRepository::new(pool.clone())
        .handle(UpdateSessionPlacement::new(
            command(0x117),
            session,
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
    .bind(*session.as_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_placement_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    Ok(CorruptPlacementUpdateReceiptFixture {
        container,
        pool,
        session,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_current_read_authenticates_the_placement_update_receipt() -> Result<(), Box<dyn Error>>
{
    let fixture = corrupt_placement_update_receipt_fixture().await?;
    let error = SessionRepository::new(fixture.pool.clone())
        .load_session(fixture.session)
        .await
        .expect_err("current placement must agree with its provenance receipt");
    let SessionRepositoryError::Corruption(SessionCorruption::Inconsistent(reason)) = error else {
        panic!("cross-wired current placement fails with typed session corruption")
    };
    assert_eq!(reason, "current placement provenance receipt");

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_placement_update_authenticates_the_current_placement_receipt()
-> Result<(), Box<dyn Error>> {
    let fixture = corrupt_placement_update_receipt_fixture().await?;
    let error = SessionPlacementRepository::new(fixture.pool.clone())
        .handle(UpdateSessionPlacement::new(
            command(0x118),
            fixture.session,
            SessionPlacementVersion::try_from_u64(2).expect("fixture current version is positive"),
            scoped("projects.foo.replacement"),
        ))
        .await
        .expect_err("an update must authenticate the current event before advancing it");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("cross-wired current placement fails update with typed corruption")
    };
    assert_eq!(reason, "session placement provenance receipt");

    fixture.pool.close().await;
    drop(fixture.container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_rejected_update_replay_authenticates_the_reported_current_version()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session = session(0x224);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(0x127),
            session,
            SessionPlacement::pathless(),
        ))
        .await?;
    let command_id = command(0x128);
    let update = UpdateSessionPlacement::new(
        command_id,
        session,
        SessionPlacementVersion::try_from_u64(2).expect("fixture mismatch version is positive"),
        scoped("projects.foo.rejected"),
    );
    let repository = SessionPlacementRepository::new(pool.clone());
    repository.handle(update.clone()).await?;
    sqlx::query("ALTER TABLE update_session_placement_command DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE update_session_placement_command
            SET result_current_version = 3
          WHERE command_id = $1",
    )
    .bind(*command_id.as_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE update_session_placement_command ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    let error = repository
        .handle(update)
        .await
        .expect_err("rejected replay must authenticate its reported current placement event");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("a nonexistent rejection version fails replay with typed corruption")
    };
    assert_eq!(reason, "rejection current placement event");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_applied_update_replay_requires_the_event_to_reach_the_current_head()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session_id = session(0x223);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(0x125),
            session_id,
            SessionPlacement::pathless(),
        ))
        .await?;
    let update = UpdateSessionPlacement::new(
        command(0x126),
        session_id,
        SessionPlacementVersion::INITIAL,
        scoped("projects.foo.head"),
    );
    let repository = SessionPlacementRepository::new(pool.clone());
    repository.handle(update.clone()).await?;
    sqlx::query("ALTER TABLE session_current_placement DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE session_current_placement SET current_version = 1
          WHERE session_id = $1",
    )
    .bind(*session_id.as_uuid())
    .execute(&pool)
    .await?;

    let error = repository
        .handle(update)
        .await
        .expect_err("applied replay requires its event to have reached the current head");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("a lagging placement head fails applied replay with typed corruption")
    };
    assert_eq!(reason, "applied event not reached by placement head");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_inv012_rejected_update_replay_requires_the_reported_version_to_reach_the_current_head()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session_id = session(0x226);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(0x12a),
            session_id,
            SessionPlacement::pathless(),
        ))
        .await?;
    let repository = SessionPlacementRepository::new(pool.clone());
    repository
        .handle(UpdateSessionPlacement::new(
            command(0x12b),
            session_id,
            SessionPlacementVersion::INITIAL,
            scoped("projects.foo.reached"),
        ))
        .await?;
    let rejected = UpdateSessionPlacement::new(
        command(0x12c),
        session_id,
        SessionPlacementVersion::try_from_u64(3).expect("fixture mismatch version is positive"),
        scoped("projects.foo.rejected"),
    );
    repository.handle(rejected.clone()).await?;
    sqlx::query("ALTER TABLE session_current_placement DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE session_current_placement SET current_version = 1
          WHERE session_id = $1",
    )
    .bind(*session_id.as_uuid())
    .execute(&pool)
    .await?;

    let error = repository
        .handle(rejected)
        .await
        .expect_err("rejected replay requires the reported version to have reached the head");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("a lagging placement head fails rejected replay with typed corruption")
    };
    assert_eq!(reason, "rejection event not reached by placement head");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_current_read_rejects_a_corrupt_placement_update_typed_header()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let session_id = session(0x220);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(0x120),
            session_id,
            SessionPlacement::pathless(),
        ))
        .await?;
    SessionPlacementRepository::new(pool.clone())
        .handle(UpdateSessionPlacement::new(
            command(0x121),
            session_id,
            SessionPlacementVersion::INITIAL,
            scoped("projects.foo.header"),
        ))
        .await?;
    sqlx::query("ALTER TABLE update_session_placement_command DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE update_session_placement_command DROP CONSTRAINT
             update_session_placement_command_command_kind_check",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE update_session_placement_command
            SET command_kind = 'goal'
          WHERE session_id = $1",
    )
    .bind(*session_id.as_uuid())
    .execute(&pool)
    .await?;

    let error = SessionPlacementRepository::new(pool.clone())
        .load_current(session_id)
        .await
        .expect_err("a public read authenticates the placement update typed header");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("a corrupt placement update header fails with typed corruption")
    };
    assert_eq!(reason, "session placement provenance receipt");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_creation_receipt_must_match_the_session_ancestry_family() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    let session_id = session(0x221);
    let imported_conversation_id = Uuid::from_u128(0x320);
    let imported_frontier_entry_id = Uuid::from_u128(0x321);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command(0x122),
            session_id,
            SessionPlacement::pathless(),
        ))
        .await?;
    sqlx::query("ALTER TABLE session DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE session
            SET ancestry_kind = 'imported_conversation',
                imported_conversation_id = $2,
                imported_frontier_entry_id = $3,
                imported_frontier_position = 1,
                imported_relationship_kind = 'resume'
          WHERE session_id = $1",
    )
    .bind(*session_id.as_uuid())
    .bind(imported_conversation_id)
    .bind(imported_frontier_entry_id)
    .execute(&pool)
    .await?;

    let error = SessionPlacementRepository::new(pool.clone())
        .load_current(session_id)
        .await
        .expect_err("a native receipt cannot authenticate imported ancestry");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("a cross-family creation receipt fails with typed corruption")
    };
    assert_eq!(reason, "session placement provenance receipt");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_current_placement_read_rejects_a_pre_v6_scoped_creation_receipt()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let command = command(0x129);
    let session = session(0x225);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(
            command,
            session,
            scoped("projects.foo.impossible_legacy"),
        ))
        .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE create_session_command
         DROP CONSTRAINT create_session_command_placement_versioned",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE durable_command SET storage_version = 4 WHERE command_id = $1")
        .bind(*command.as_uuid())
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE create_session_command SET storage_version = 4 WHERE command_id = $1")
        .bind(*command.as_uuid())
        .execute(&mut *transaction)
        .await?;
    sqlx::query("SET LOCAL session_replication_role = origin")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;

    let error = SessionPlacementRepository::new(pool.clone())
        .load_current(session)
        .await
        .expect_err("pre-v6 creation receipts can authenticate only pathless placement");
    let SessionPlacementRepositoryError::Corruption(reason) = error else {
        panic!("a pre-v6 scoped creation receipt fails with typed corruption")
    };
    assert_eq!(reason, "session placement provenance receipt");

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
    let command_id = command(0x119);
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
    .bind(*session(0x20b).as_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("an applied update receipt must advance its expected predecessor");
    let database_error = malformed
        .as_database_error()
        .expect("PostgreSQL reports the applied-result shape constraint");

    assert_eq!(database_error.constraint(), Some(RESULT_SHAPE_CONSTRAINT));

    transaction.rollback().await?;
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_applied_update_receipt_requires_a_result_version() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
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

    assert_eq!(database_error.constraint(), Some(RESULT_SHAPE_CONSTRAINT));

    transaction.rollback().await?;
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_inv012_update_handle_applies_first_command() -> Result<(), Box<dyn Error>> {
    let (container, pool, repository, update) = placement_authentication_fixture().await?;
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
    let (container, pool, repository, update) = placement_authentication_fixture().await?;
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
    let (container, pool, repository, update) = placement_authentication_fixture().await?;
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
-> Result<(), Box<dyn Error>> {
    let (container, pool, repository, update) = placement_authentication_fixture().await?;
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
    let (container, pool, repository, update) = placement_authentication_fixture().await?;
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

async fn placement_authentication_fixture() -> Result<
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
