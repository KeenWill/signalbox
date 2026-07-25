#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{error::Error, sync::Arc};

use signalbox_domain::{
    Actor, CreateSession, DirectModelSelection, DurableCommandId, ModelSelectionRequest,
    PreparedCreateSession, ReplaceSessionMetadata, ReplaceSessionMetadataRejectedResult,
    ReplaceSessionMetadataResult, SessionConfigurationDefaults, SessionCreationCause,
    SessionCreationProvenance, SessionId, SessionMetadataContent, TranscriptAncestry,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    local_test_connection_options, migrate,
    session_metadata::{
        ReplaceSessionMetadataHandlingOutcome, SessionMetadataRepository,
        SessionMetadataRepositoryError,
    },
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_metadata";
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

fn session(value: u128) -> SessionId {
    SessionId::from_uuid(Uuid::from_u128(value))
}

fn command(value: u128) -> DurableCommandId {
    DurableCommandId::from_uuid(Uuid::from_u128(value))
}

fn creation(command_value: u128, session_value: u128) -> PreparedCreateSession {
    CreateSession::new(
        command(command_value),
        SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(session_value + 0x1000)),
        )),
    )
    .prepare(session(session_value))
    .expect("owner-initiated creation without ancestry is preparable")
}

fn metadata(
    title: Option<&str>,
    tags: &[&str],
    attributes: &[(&str, &str)],
    archived: bool,
) -> SessionMetadataContent {
    SessionMetadataContent::try_new(
        title.map(str::to_owned),
        tags.iter().map(|value| (*value).to_owned()).collect(),
        attributes
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        archived,
    )
    .expect("fixture metadata is valid")
}

fn replacement(
    command_value: u128,
    session_value: u128,
    content: SessionMetadataContent,
) -> ReplaceSessionMetadata {
    ReplaceSessionMetadata::new(
        command(command_value),
        session(session_value),
        Actor::Owner,
        content,
    )
}

async fn collect_page(
    repository: &SessionMetadataRepository,
    query: signalbox_application::SessionMetadataListQuery,
) -> Result<
    (
        Vec<signalbox_application::SessionMetadataListItem>,
        Option<SessionId>,
    ),
    Box<dyn Error>,
> {
    let mut page = repository.open_page(query).await?;
    let mut items = Vec::new();
    while let Some(item) = page.next_item().await? {
        items.push(item);
    }
    Ok((items, page.next_after_session()))
}

/// INV-002 / INV-005 / INV-012 / INV-013: metadata remains a normalized
/// satellite, durable replay is exact and owner-global, archiving affects only
/// discoverability, concurrent replacements serialize, and list filters/page
/// cursors observe one bounded snapshot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv002_inv005_inv012_inv013_metadata_replay_listing_and_last_writer()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let create_repository = CreateSessionRepository::new(pool.clone());
    for value in 0x701..=0x704 {
        create_repository
            .handle(creation(value + 0x100, value))
            .await?;
    }
    let repository = SessionMetadataRepository::new(pool.clone());

    let initial = repository
        .load_session_metadata(session(0x701))
        .await?
        .expect("created session exists");
    assert_eq!(initial.content(), &SessionMetadataContent::empty());
    assert_eq!(initial.last_writer(), None);

    let absent = replacement(0x901, 0x799, metadata(Some("absent"), &[], &[], false));
    let absent_outcome = repository.handle(absent.clone()).await?;
    assert!(matches!(
        absent_outcome,
        ReplaceSessionMetadataHandlingOutcome::Recorded(ReplaceSessionMetadataResult::Rejected(
            ReplaceSessionMetadataRejectedResult::SessionNotFound(_)
        ))
    ));
    assert_eq!(repository.handle(absent).await?, absent_outcome);

    let first_content = metadata(
        Some("Planning"),
        &["daily", "work"],
        &[("run", "17"), ("trigger", "")],
        false,
    );
    let first = replacement(0x902, 0x701, first_content.clone());
    let first_outcome = repository.handle(first.clone()).await?;
    let ReplaceSessionMetadataHandlingOutcome::Recorded(ReplaceSessionMetadataResult::Applied(
        first_applied,
    )) = &first_outcome
    else {
        panic!("first metadata write must apply");
    };
    assert_eq!(first_applied.snapshot().content(), &first_content);
    assert_eq!(
        first_applied
            .snapshot()
            .last_writer()
            .expect("applied write has a writer")
            .actor(),
        Actor::Owner
    );
    assert_eq!(repository.handle(first.clone()).await?, first_outcome);
    assert_eq!(
        repository
            .load_command(first.command_id())
            .await?
            .expect("durable receipt exists")
            .result(),
        match &first_outcome {
            ReplaceSessionMetadataHandlingOutcome::Recorded(result) => result,
            ReplaceSessionMetadataHandlingOutcome::ConflictingReuse { .. } => {
                unreachable!("fixture first handling recorded")
            }
        }
    );

    let conflicting = replacement(0x902, 0x701, metadata(Some("different"), &[], &[], false));
    assert_eq!(
        repository.handle(conflicting).await?,
        ReplaceSessionMetadataHandlingOutcome::ConflictingReuse {
            command_id: command(0x902)
        }
    );
    assert_eq!(
        repository
            .load_session_metadata(session(0x701))
            .await?
            .expect("session remains present")
            .content(),
        &first_content
    );

    repository
        .handle(replacement(
            0x903,
            0x702,
            metadata(Some("Alpha planning"), &["blue", "daily"], &[], false),
        ))
        .await?;
    repository
        .handle(replacement(
            0x904,
            0x703,
            metadata(Some("Alpha"), &["daily"], &[], true),
        ))
        .await?;

    let filtered = signalbox_application::SessionMetadataListQuery::try_new(
        vec![String::from("daily"), String::from("blue")],
        Some(String::from("Alpha")),
        false,
        10,
        None,
    )
    .expect("fixture filter is valid");
    let (items, continuation) = collect_page(&repository, filtered).await?;
    assert_eq!(
        items.iter().map(|item| item.session()).collect::<Vec<_>>(),
        [session(0x702)]
    );
    assert_eq!(items[0].tags().collect::<Vec<_>>(), ["blue", "daily"]);
    assert_eq!(continuation, None);

    let archived_hidden =
        signalbox_application::SessionMetadataListQuery::try_new(Vec::new(), None, false, 10, None)
            .expect("fixture page is valid");
    let (items, continuation) = collect_page(&repository, archived_hidden).await?;
    assert_eq!(
        items.iter().map(|item| item.session()).collect::<Vec<_>>(),
        [session(0x701), session(0x702), session(0x704)]
    );
    assert_eq!(continuation, None);

    let first_page =
        signalbox_application::SessionMetadataListQuery::try_new(Vec::new(), None, true, 2, None)
            .expect("fixture page is valid");
    let (items, continuation) = collect_page(&repository, first_page).await?;
    assert_eq!(
        items.iter().map(|item| item.session()).collect::<Vec<_>>(),
        [session(0x701), session(0x702)]
    );
    assert_eq!(continuation, Some(session(0x702)));
    let second_page = signalbox_application::SessionMetadataListQuery::try_new(
        Vec::new(),
        None,
        true,
        2,
        continuation,
    )
    .expect("continuation page is valid");
    let (items, continuation) = collect_page(&repository, second_page).await?;
    assert_eq!(
        items.iter().map(|item| item.session()).collect::<Vec<_>>(),
        [session(0x703), session(0x704)]
    );
    assert_eq!(continuation, None);

    let left = replacement(0x905, 0x701, metadata(Some("left"), &["left"], &[], false));
    let right = replacement(
        0x906,
        0x701,
        metadata(Some("right"), &["right"], &[], false),
    );
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (left_outcome, right_outcome) = tokio::join!(
        async {
            barrier.wait().await;
            repository.handle(left.clone()).await
        },
        async {
            barrier.wait().await;
            repository.handle(right.clone()).await
        }
    );
    assert!(matches!(
        left_outcome?,
        ReplaceSessionMetadataHandlingOutcome::Recorded(ReplaceSessionMetadataResult::Applied(_))
    ));
    assert!(matches!(
        right_outcome?,
        ReplaceSessionMetadataHandlingOutcome::Recorded(ReplaceSessionMetadataResult::Applied(_))
    ));
    let final_title = repository
        .load_session_metadata(session(0x701))
        .await?
        .expect("session exists")
        .content()
        .title()
        .expect("both concurrent values have titles")
        .to_owned();
    assert!(matches!(final_title.as_str(), "left" | "right"));
    assert!(matches!(
        repository.handle(left).await?,
        ReplaceSessionMetadataHandlingOutcome::Recorded(_)
    ));
    assert!(matches!(
        repository.handle(right).await?,
        ReplaceSessionMetadataHandlingOutcome::Recorded(_)
    ));

    let immutable = sqlx::query(
        "UPDATE replace_session_metadata_command
         SET result_kind = result_kind
         WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x902))
    .execute(&pool)
    .await
    .expect_err("durable metadata receipts are append-only");
    assert_eq!(
        immutable
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let late_tag = sqlx::query(
        "INSERT INTO replace_session_metadata_command_tag (command_id, tag)
         VALUES ($1, 'late')",
    )
    .bind(Uuid::from_u128(0x902))
    .execute(&pool)
    .await
    .expect_err("a committed receipt cannot gain a later tag");
    assert_eq!(
        late_tag
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let late_attribute = sqlx::query(
        "INSERT INTO replace_session_metadata_command_attribute
            (command_id, attribute_key, attribute_value)
         VALUES ($1, 'late', 'value')",
    )
    .bind(Uuid::from_u128(0x902))
    .execute(&pool)
    .await
    .expect_err("a committed receipt cannot gain a later attribute");
    assert_eq!(
        late_attribute
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let missing_applied_actor = sqlx::query(
        "INSERT INTO replace_session_metadata_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, replacement_archived, result_kind,
             result_session_id, result_updated_at, result_actor_kind)
         VALUES
            ($1, 'replace_session_metadata', 1, $2,
             'owner', false, 'applied', $2, transaction_timestamp(), NULL)",
    )
    .bind(Uuid::from_u128(0x907))
    .bind(Uuid::from_u128(0x701))
    .execute(&pool)
    .await
    .expect_err("an applied receipt must name its result actor");
    assert_eq!(
        missing_applied_actor
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let cross_kind = replacement(0x801, 0x701, SessionMetadataContent::empty());
    assert_eq!(
        repository.handle(cross_kind).await?,
        ReplaceSessionMetadataHandlingOutcome::ConflictingReuse {
            command_id: command(0x801)
        }
    );
    assert!(matches!(
        repository.load_command(command(0x801)).await,
        Err(SessionMetadataRepositoryError::DifferentCommandKind { .. })
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_schema_bounds_every_indexed_string() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let create_repository = CreateSessionRepository::new(pool.clone());
    create_repository.handle(creation(0xa01, 0x801)).await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    repository
        .handle(replacement(
            0xa02,
            0x801,
            metadata(Some("bounded"), &["tag"], &[("key", "value")], false),
        ))
        .await?;
    let oversized = "x".repeat(SessionMetadataContent::MAX_INDEXED_UTF8_BYTES + 1);

    let current_tag = sqlx::query(
        "INSERT INTO session_metadata_tag (session_id, tag)
         VALUES ($1, $2)",
    )
    .bind(Uuid::from_u128(0x801))
    .bind(&oversized)
    .execute(&pool)
    .await
    .expect_err("the current tag index rejects an oversized key");
    assert_eq!(
        current_tag
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let current_attribute = sqlx::query(
        "INSERT INTO session_metadata_attribute
            (session_id, attribute_key, attribute_value)
         VALUES ($1, $2, '')",
    )
    .bind(Uuid::from_u128(0x801))
    .bind(&oversized)
    .execute(&pool)
    .await
    .expect_err("the current attribute index rejects an oversized key");
    assert_eq!(
        current_attribute
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let mut receipt_tag_transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'replace_session_metadata', 1, transaction_timestamp())",
    )
    .bind(Uuid::from_u128(0xa03))
    .execute(&mut *receipt_tag_transaction)
    .await?;
    let receipt_tag = sqlx::query(
        "INSERT INTO replace_session_metadata_command_tag (command_id, tag)
         VALUES ($1, $2)",
    )
    .bind(Uuid::from_u128(0xa03))
    .bind(&oversized)
    .execute(&mut *receipt_tag_transaction)
    .await
    .expect_err("the receipt tag index rejects an oversized key");
    assert_eq!(
        receipt_tag
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    receipt_tag_transaction.rollback().await?;

    let mut receipt_attribute_transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'replace_session_metadata', 1, transaction_timestamp())",
    )
    .bind(Uuid::from_u128(0xa04))
    .execute(&mut *receipt_attribute_transaction)
    .await?;
    let receipt_attribute = sqlx::query(
        "INSERT INTO replace_session_metadata_command_attribute
            (command_id, attribute_key, attribute_value)
         VALUES ($1, $2, '')",
    )
    .bind(Uuid::from_u128(0xa04))
    .bind(&oversized)
    .execute(&mut *receipt_attribute_transaction)
    .await
    .expect_err("the receipt attribute index rejects an oversized key");
    assert_eq!(
        receipt_attribute
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    receipt_attribute_transaction.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(())
}
