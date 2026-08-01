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
    create_session::CreateSessionRepository,
    local_test_connection_options, migrate,
    process_read::{ProcessReadRepository, ProcessScopedTranscriptRead},
    session::SessionRepository,
    session_placement::{SessionPlacementRepository, SessionPlacementRepositoryOutcome},
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s36_transcript_open_enforces_the_single_prefix_rule_and_legacy_exceptions()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let requester = session(0x301);
    let sibling = session(0x302);
    let descendant = session(0x303);
    let ancestor = session(0x304);
    let disjoint = session(0x305);
    let pathless = session(0x306);
    let root_session = session(0x307);
    let creation_repository = CreateSessionRepository::new(pool.clone(), credential_pin());
    creation_repository
        .handle(creation(
            command(0x401),
            requester,
            scoped("projects.foo.reviews.pr123"),
        ))
        .await?;
    creation_repository
        .handle(creation(
            command(0x402),
            sibling,
            scoped("projects.foo.reviews.pr456"),
        ))
        .await?;
    creation_repository
        .handle(creation(
            command(0x403),
            descendant,
            scoped("projects.foo.reviews.pr456.followup"),
        ))
        .await?;
    creation_repository
        .handle(creation(
            command(0x404),
            ancestor,
            scoped("projects.foo.session"),
        ))
        .await?;
    creation_repository
        .handle(creation(
            command(0x405),
            disjoint,
            scoped("projects.bar.session"),
        ))
        .await?;
    creation_repository
        .handle(creation(
            command(0x406),
            pathless,
            SessionPlacement::Pathless,
        ))
        .await?;
    creation_repository
        .handle(creation(command(0x407), root_session, root("operator")))
        .await?;
    let reads = ProcessReadRepository::new(pool.clone());

    let ProcessScopedTranscriptRead::Opened(sibling_reader) =
        reads.open_scoped_transcript(requester, sibling).await?
    else {
        panic!("sibling is readable")
    };
    drop(sibling_reader);
    let ProcessScopedTranscriptRead::Opened(descendant_reader) =
        reads.open_scoped_transcript(requester, descendant).await?
    else {
        panic!("descendant is readable")
    };
    drop(descendant_reader);
    let ProcessScopedTranscriptRead::Refused(ancestor_refusal) =
        reads.open_scoped_transcript(requester, ancestor).await?
    else {
        panic!("ancestor is refused")
    };
    let ProcessScopedTranscriptRead::Refused(disjoint_refusal) =
        reads.open_scoped_transcript(requester, disjoint).await?
    else {
        panic!("disjoint subtree is refused")
    };
    assert_eq!(
        ancestor_refusal.requesting_directory(),
        disjoint_refusal.requesting_directory()
    );
    let ProcessScopedTranscriptRead::Opened(pathless_reader) =
        reads.open_scoped_transcript(pathless, disjoint).await?
    else {
        panic!("pathless requester keeps legacy reads")
    };
    drop(pathless_reader);
    let ProcessScopedTranscriptRead::Opened(root_reader) =
        reads.open_scoped_transcript(root_session, pathless).await?
    else {
        panic!("root placement reads pathless targets globally")
    };
    drop(root_reader);

    pool.close().await;
    drop(container);
    Ok(())
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
    let placement = SessionPlacement::Pathless;
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
            SessionPlacement::Pathless,
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
        SessionPlacementVersion::INITIAL.next().unwrap()
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
