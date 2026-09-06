#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics, explicit fixture expectations, and impossible fixture branches; the workspace gate remains active for production targets"
)]

mod support;

use std::{error::Error, sync::Arc};

use signalbox_domain::{
    Actor, CreateSession, DirectModelSelection, DurableCommandId, ModelSelectionRequest,
    PreparedCreateSession, ReplaceSessionMetadata, ReplaceSessionMetadataReconstitutionFailure,
    ReplaceSessionMetadataRejectedResult, ReplaceSessionMetadataResult,
    SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance, SessionId,
    SessionMetadataContent, ToolRequestId, TranscriptAncestry,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
    session_metadata::{
        ReplaceSessionMetadataHandlingOutcome, SessionMetadataCorruption,
        SessionMetadataRepository, SessionMetadataRepositoryError,
    },
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

use support::blocked_backends_reached;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_metadata";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

fn test_session_credential_pin() -> signalbox_persistence::SessionCredentialPin {
    signalbox_persistence::SessionCredentialPin::try_new(vec![
        signalbox_persistence::SessionModelCredential::new(
            "test-model-family",
            "test-model-primary",
        ),
    ])
    .expect("test credential pin is valid")
}
const UNSUPPORTED_COMMAND_ACTOR: &str = "recovery";

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_cmd(disposable_postgres_server_args())
        .with_mount(disposable_postgres_state_tmpfs_from_example()?)
        .with_tag(POSTGRES_IMAGE_TAG)
        .with_labels(disposable_test_container_labels())
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
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(session_value + 0x1000)),
        )),
    )
    .prepare(session(session_value))
    .expect("user-initiated creation without ancestry is preparable")
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
    ReplaceSessionMetadata::new(command(command_value), session(session_value), content)
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

#[track_caller]
fn assert_check_violation(error: &sqlx::Error) {
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
}

/// a created session with no metadata write has the canonical
/// unwritten snapshot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_initial_metadata_read_returns_unwritten_snapshot() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    create_repository.handle(creation(0x801, 0x701)).await?;
    let repository = SessionMetadataRepository::new(pool.clone());

    let initial = repository
        .load_session_metadata(session(0x701))
        .await?
        .expect("created session exists");
    assert_eq!(initial.content(), &SessionMetadataContent::empty());
    assert_eq!(initial.last_writer(), None);

    pool.close().await;
    drop(container);
    Ok(())
}

/// a missing-session rejection is durable and equal replay returns it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_missing_session_rejection_replays_exactly() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    let absent = replacement(0x901, 0x799, metadata(Some("absent"), &[], &[], false));
    let absent_outcome = repository.handle(absent.clone()).await?;
    assert!(matches!(
        absent_outcome,
        ReplaceSessionMetadataHandlingOutcome::Recorded(ReplaceSessionMetadataResult::Rejected(
            ReplaceSessionMetadataRejectedResult::SessionNotFound(_)
        ))
    ));
    assert_eq!(repository.handle(absent).await?, absent_outcome);

    pool.close().await;
    drop(container);
    Ok(())
}

/// an applied receipt with an unsupported stored command actor fails
/// closed during repository reconstitution.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn applied_metadata_receipt_rejects_unsupported_command_actor() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    repository
        .handle(replacement(
            0x902,
            0x701,
            metadata(Some("applied"), &[], &[], false),
        ))
        .await?;

    sqlx::query(
        "ALTER TABLE replace_session_metadata_command
         DISABLE TRIGGER replace_session_metadata_command_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE replace_session_metadata_command
            SET actor_kind = $1,
                result_actor_kind = $1
          WHERE command_id = $2",
    )
    .bind(UNSUPPORTED_COMMAND_ACTOR)
    .bind(Uuid::from_u128(0x902))
    .execute(&pool)
    .await?;

    let Err(SessionMetadataRepositoryError::Corruption(SessionMetadataCorruption::Unsupported {
        field,
        value,
    })) = repository.load_command(command(0x902)).await
    else {
        panic!("recovery agency must remain unsupported for metadata commands")
    };
    assert_eq!(field, "command actor");
    assert_eq!(value, UNSUPPORTED_COMMAND_ACTOR);

    pool.close().await;
    drop(container);
    Ok(())
}

/// a rejected receipt with an unsupported stored command actor fails
/// closed during repository reconstitution.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn rejected_metadata_receipt_rejects_unsupported_command_actor() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    repository
        .handle(replacement(
            0x901,
            0x799,
            metadata(Some("rejected"), &[], &[], false),
        ))
        .await?;

    sqlx::query(
        "ALTER TABLE replace_session_metadata_command
         DISABLE TRIGGER replace_session_metadata_command_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE replace_session_metadata_command
            SET actor_kind = $1
          WHERE command_id = $2",
    )
    .bind(UNSUPPORTED_COMMAND_ACTOR)
    .bind(Uuid::from_u128(0x901))
    .execute(&pool)
    .await?;

    let Err(SessionMetadataRepositoryError::Corruption(SessionMetadataCorruption::Unsupported {
        field,
        value,
    })) = repository.load_command(command(0x901)).await
    else {
        panic!("recovery agency must remain unsupported for metadata commands")
    };
    assert_eq!(field, "command actor");
    assert_eq!(value, UNSUPPORTED_COMMAND_ACTOR);

    pool.close().await;
    drop(container);
    Ok(())
}

/// changing both stored actor projections to another supported actor
/// cannot change the constructor-selected command issuer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_receipt_rejects_supported_actor_reattribution() -> Result<(), Box<dyn Error>> {
    const TARGET_SESSION: u128 = 0x701;
    const REPLACEMENT_COMMAND: u128 = 0x902;
    const CORRUPT_TOOL_REQUEST: u128 = 0xA01;

    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, TARGET_SESSION))
        .await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    repository
        .handle(replacement(
            REPLACEMENT_COMMAND,
            TARGET_SESSION,
            metadata(Some("applied"), &[], &[], false),
        ))
        .await?;

    sqlx::query(
        "ALTER TABLE replace_session_metadata_command
         DISABLE TRIGGER replace_session_metadata_command_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE replace_session_metadata_command
            SET actor_kind = 'tool',
                actor_tool_request_id = $1,
                result_actor_kind = 'tool',
                result_actor_tool_request_id = $1
          WHERE command_id = $2",
    )
    .bind(Uuid::from_u128(CORRUPT_TOOL_REQUEST))
    .bind(Uuid::from_u128(REPLACEMENT_COMMAND))
    .execute(&pool)
    .await?;

    let Err(SessionMetadataRepositoryError::Corruption(SessionMetadataCorruption::Domain(
        ReplaceSessionMetadataReconstitutionFailure::CommandActorMismatch,
    ))) = repository.load_command(command(REPLACEMENT_COMMAND)).await
    else {
        panic!("supported actor reattribution must fail closed")
    };

    pool.close().await;
    drop(container);
    Ok(())
}

/// one applied command retains its exact receipt, equal
/// replay, and conflicting-reuse classification without changing current state.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn applied_metadata_replay_and_conflict_are_exact() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    let first_content = metadata(
        Some("Planning"),
        &["daily", "work"],
        &[("run", "17"), ("trigger", "")],
        false,
    );
    let first = replacement(0x902, 0x701, first_content.clone());
    let first_outcome = repository.handle(first.clone()).await?;
    let ReplaceSessionMetadataHandlingOutcome::Recorded(first_result) = &first_outcome else {
        panic!("first metadata write must record");
    };
    let ReplaceSessionMetadataResult::Applied(first_applied) = first_result else {
        panic!("first metadata write must apply");
    };
    assert_eq!(first_applied.snapshot().content(), &first_content);
    assert_eq!(
        first_applied
            .snapshot()
            .last_writer()
            .expect("applied write has a writer")
            .actor(),
        Actor::User
    );
    assert_eq!(repository.handle(first.clone()).await?, first_outcome);
    assert_eq!(
        repository
            .load_command(first.command_id())
            .await?
            .expect("durable receipt exists")
            .result(),
        first_result
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

    pool.close().await;
    drop(container);
    Ok(())
}

/// an admitted tool-attributed replacement round-trips the exact
/// request agency through the immutable receipt and current writer stamp.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tool_metadata_actor_round_trips_exactly() -> Result<(), Box<dyn Error>> {
    const CREATION_COMMAND: u128 = 0x801;
    const TARGET_SESSION: u128 = 0x701;
    const REPLACEMENT_COMMAND: u128 = 0x902;
    const TOOL_REQUEST: u128 = 0xA01;

    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(CREATION_COMMAND, TARGET_SESSION))
        .await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    let tool_request = ToolRequestId::from_uuid(Uuid::from_u128(TOOL_REQUEST));
    let replacement = ReplaceSessionMetadata::new_for_tool(
        command(REPLACEMENT_COMMAND),
        session(TARGET_SESSION),
        tool_request,
        SessionMetadataContent::empty(),
    );

    let outcome = repository.handle(replacement.clone()).await?;
    let ReplaceSessionMetadataHandlingOutcome::Recorded(ReplaceSessionMetadataResult::Applied(
        applied,
    )) = &outcome
    else {
        panic!("the first replacement for an existing session must apply")
    };
    let recorded = repository
        .load_command(replacement.command_id())
        .await?
        .expect("the tool-attributed replacement receipt exists");

    assert_eq!(
        replacement.actor(),
        Actor::Tool {
            request: tool_request
        }
    );
    assert_eq!(recorded.command(), &replacement);
    assert_eq!(
        recorded.result(),
        &ReplaceSessionMetadataResult::Applied(applied.clone())
    );
    assert_eq!(
        applied
            .snapshot()
            .last_writer()
            .expect("an applied replacement has a writer")
            .actor(),
        replacement.actor()
    );
    assert_eq!(repository.handle(replacement).await?, outcome);

    pool.close().await;
    drop(container);
    Ok(())
}

/// a multi-member tag and attribute set is retained exactly in both
/// the current snapshot and its immutable receipt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_satellites_install_as_one_receipt() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    let tags = (0..3)
        .map(|index| format!("tag-{index:03}"))
        .collect::<Vec<_>>();
    let attributes = (0..3)
        .map(|index| (format!("key-{index:03}"), format!("value-{index:03}")))
        .collect::<Vec<_>>();
    let content =
        SessionMetadataContent::try_new(Some(String::from("multiple")), tags, attributes, false)
            .expect("the exact metadata members are valid");
    let command = replacement(0x902, 0x701, content.clone());

    assert!(matches!(
        repository.handle(command.clone()).await?,
        ReplaceSessionMetadataHandlingOutcome::Recorded(ReplaceSessionMetadataResult::Applied(_))
    ));
    assert_eq!(
        repository
            .load_session_metadata(session(0x701))
            .await?
            .expect("the written session exists")
            .content(),
        &content
    );
    assert_eq!(
        repository
            .load_command(command.command_id())
            .await?
            .expect("the replacement receipt exists")
            .command()
            .replacement(),
        &content
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// a blocked writer samples one post-lock statement timestamp and
/// records that exact value in both current state and its durable receipt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_writer_stamp_is_sampled_after_lock_and_shared() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;

    let mut held_lock = pool.begin().await?;
    let _: Uuid = sqlx::query_scalar(
        "SELECT session_id
           FROM session
          WHERE session_id = $1
            FOR NO KEY UPDATE",
    )
    .bind(Uuid::from_u128(0x701))
    .fetch_one(&mut *held_lock)
    .await?;

    let pending = tokio::spawn({
        let repository = SessionMetadataRepository::new(pool.clone());
        async move {
            repository
                .handle(replacement(
                    0x902,
                    0x701,
                    metadata(Some("post-lock"), &[], &[], false),
                ))
                .await
        }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the metadata writer must wait for the held session lock"
    );
    let post_wait_lower_bound: i64 = sqlx::query_scalar(
        "SELECT floor(
            extract(epoch FROM statement_timestamp()) * 1000000
         )::bigint",
    )
    .fetch_one(&pool)
    .await?;

    held_lock.commit().await?;
    let outcome = pending.await??;
    let ReplaceSessionMetadataHandlingOutcome::Recorded(ReplaceSessionMetadataResult::Applied(
        applied,
    )) = outcome
    else {
        panic!("the released metadata writer must apply");
    };
    let updated_at = applied
        .snapshot()
        .last_writer()
        .expect("an applied write carries its writer")
        .updated_at()
        .as_unix_micros();
    assert!(
        updated_at >= u64::try_from(post_wait_lower_bound)?,
        "the writer timestamp must be sampled after the lock wait"
    );

    let stored_times_match: bool = sqlx::query_scalar(
        "SELECT current.updated_at = receipt.result_updated_at
           FROM session_metadata AS current
           JOIN replace_session_metadata_command AS receipt
             ON receipt.result_applied_session_id = current.session_id
          WHERE current.session_id = $1
            AND receipt.command_id = $2",
    )
    .bind(Uuid::from_u128(0x701))
    .bind(Uuid::from_u128(0x902))
    .fetch_one(&pool)
    .await?;
    assert!(
        stored_times_match,
        "current state and the receipt must retain the exact same timestamp"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_list_applies_exact_tag_and_title_filters() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    create_repository.handle(creation(0x801, 0x701)).await?;
    create_repository.handle(creation(0x802, 0x702)).await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    repository
        .handle(replacement(
            0x903,
            0x701,
            metadata(Some("Alpha planning"), &["blue", "daily"], &[], false),
        ))
        .await?;
    repository
        .handle(replacement(
            0x904,
            0x702,
            metadata(Some("Alpha"), &["daily"], &[], false),
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
        [session(0x701)]
    );
    assert_eq!(items[0].tags().collect::<Vec<_>>(), ["blue", "daily"]);
    assert_eq!(continuation, None);

    pool.close().await;
    drop(container);
    Ok(())
}

/// the default metadata list hides archived sessions without changing
/// the inclusive organizational view.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_list_hides_archived_sessions_by_default() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    create_repository.handle(creation(0x801, 0x701)).await?;
    create_repository.handle(creation(0x802, 0x702)).await?;
    create_repository.handle(creation(0x803, 0x703)).await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    repository
        .handle(replacement(
            0x901,
            0x702,
            metadata(Some("archived"), &[], &[], true),
        ))
        .await?;

    let archived_hidden =
        signalbox_application::SessionMetadataListQuery::try_new(Vec::new(), None, false, 10, None)
            .expect("fixture page is valid");
    let (items, continuation) = collect_page(&repository, archived_hidden).await?;
    assert_eq!(
        items.iter().map(|item| item.session()).collect::<Vec<_>>(),
        [session(0x701), session(0x703)]
    );
    assert_eq!(continuation, None);

    let archived_included =
        signalbox_application::SessionMetadataListQuery::try_new(Vec::new(), None, true, 10, None)
            .expect("fixture page is valid");
    let (items, continuation) = collect_page(&repository, archived_included).await?;
    assert_eq!(
        items.iter().map(|item| item.session()).collect::<Vec<_>>(),
        [session(0x701), session(0x702), session(0x703)]
    );
    assert_eq!(continuation, None);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_list_uses_bounded_keyset_pages() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    create_repository.handle(creation(0x801, 0x701)).await?;
    create_repository.handle(creation(0x802, 0x702)).await?;
    create_repository.handle(creation(0x803, 0x703)).await?;
    create_repository.handle(creation(0x804, 0x704)).await?;
    let repository = SessionMetadataRepository::new(pool.clone());
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

    pool.close().await;
    drop(container);
    Ok(())
}

/// the atomic race contract requires both distinct writes to record,
/// one exact serialized current winner, and replay of both receipts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn concurrent_replacements_serialize_and_replay() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    let repository = SessionMetadataRepository::new(pool.clone());
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

    pool.close().await;
    drop(container);
    Ok(())
}

/// append-only installation evidence prevents an earlier applied
/// receipt from becoming current for the same session a second time.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn prior_metadata_receipt_cannot_be_reinstalled() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    repository
        .handle(replacement(
            0x905,
            0x701,
            metadata(Some("first"), &[], &[], false),
        ))
        .await?;
    let current = metadata(Some("second"), &[], &[], false);
    repository
        .handle(replacement(0x906, 0x701, current.clone()))
        .await?;

    let reinstallation = sqlx::query(
        "UPDATE session_metadata AS current
            SET source_command_id = receipt.command_id,
                title = receipt.replacement_title,
                archived = receipt.replacement_archived,
                updated_at = receipt.result_updated_at,
                actor_kind = receipt.result_actor_kind,
                actor_turn_id = receipt.result_actor_turn_id,
                actor_tool_request_id = receipt.result_actor_tool_request_id
           FROM replace_session_metadata_command AS receipt
          WHERE current.session_id = $1
            AND receipt.command_id = $2",
    )
    .bind(Uuid::from_u128(0x701))
    .bind(Uuid::from_u128(0x905))
    .execute(&pool)
    .await
    .expect_err("an earlier applied receipt cannot become current again");
    assert_check_violation(&reinstallation);
    assert_eq!(
        repository
            .load_session_metadata(session(0x701))
            .await?
            .expect("the session remains readable")
            .content(),
        &current
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// each installation authenticates the complete current snapshot
/// before another write in the same transaction can supersede it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_installation_authenticates_snapshot_before_supersession()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'replace_session_metadata', 1, statement_timestamp(), 'operator')",
    )
    .bind(Uuid::from_u128(0x905))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_metadata
            (session_id, source_command_id, title, archived, updated_at,
             actor_kind)
         VALUES ($1, $2, 'current title', false, to_timestamp(1), 'user')",
    )
    .bind(Uuid::from_u128(0x701))
    .bind(Uuid::from_u128(0x905))
    .execute(&mut *transaction)
    .await?;

    let unauthenticated = sqlx::query(
        "INSERT INTO replace_session_metadata_command
            (command_id, command_kind, storage_version, session_id,
             issuer_kind, actor_kind, replacement_title, replacement_archived,
             result_kind, result_session_id, result_applied_session_id,
             result_updated_at, result_actor_kind)
         VALUES
            ($1, 'replace_session_metadata', 1, $2,
             'user', 'user', 'receipt title', false,
             'applied', $2, $2, to_timestamp(1), 'user')",
    )
    .bind(Uuid::from_u128(0x905))
    .bind(Uuid::from_u128(0x701))
    .execute(&mut *transaction)
    .await
    .expect_err("installation evidence must authenticate its snapshot immediately");
    assert_check_violation(&unauthenticated);
    transaction.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(())
}

/// deleting installation evidence cannot reopen an applied receipt for
/// a second physical installation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_installation_evidence_rejects_delete() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    SessionMetadataRepository::new(pool.clone())
        .handle(replacement(
            0x905,
            0x701,
            metadata(Some("retained"), &[], &[], false),
        ))
        .await?;

    let deletion = sqlx::query(
        "DELETE FROM session_metadata_installation
          WHERE session_id = $1
            AND source_command_id = $2",
    )
    .bind(Uuid::from_u128(0x701))
    .bind(Uuid::from_u128(0x905))
    .execute(&pool)
    .await
    .expect_err("metadata installation evidence is append-only");
    assert_check_violation(&deletion);

    pool.close().await;
    drop(container);
    Ok(())
}

/// a sealed metadata receipt parent cannot be rewritten.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_receipt_parent_rejects_update() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    SessionMetadataRepository::new(pool.clone())
        .handle(replacement(
            0x902,
            0x701,
            metadata(Some("immutable"), &[], &[], false),
        ))
        .await?;
    let immutable = sqlx::query(
        "UPDATE replace_session_metadata_command
         SET result_kind = result_kind
         WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x902))
    .execute(&pool)
    .await
    .expect_err("durable metadata receipts are append-only");
    assert_check_violation(&immutable);

    pool.close().await;
    drop(container);
    Ok(())
}

/// a written metadata root cannot return to the initial absent state.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn written_metadata_root_rejects_delete() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    let written = metadata(Some("guarded"), &["retained"], &[], false);
    repository
        .handle(replacement(0x902, 0x701, written.clone()))
        .await?;
    let deleted_root = sqlx::query("DELETE FROM session_metadata WHERE session_id = $1")
        .bind(Uuid::from_u128(0x701))
        .execute(&pool)
        .await
        .expect_err("written current metadata cannot return to absent state");
    assert_check_violation(&deleted_root);
    assert_eq!(
        repository
            .load_session_metadata(session(0x701))
            .await?
            .expect("guarded metadata remains readable")
            .content()
            .clone(),
        written
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// a mutable root cannot change its owning session even when it has no
/// tag or attribute children.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_root_rejects_identity_change_without_satellites() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    create_repository.handle(creation(0x801, 0x701)).await?;
    create_repository.handle(creation(0x802, 0x702)).await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    repository
        .handle(replacement(
            0x901,
            0x701,
            metadata(Some("root-only"), &[], &[], false),
        ))
        .await?;

    let moved_root = sqlx::query(
        "UPDATE session_metadata
            SET session_id = $2
          WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x701))
    .bind(Uuid::from_u128(0x702))
    .execute(&pool)
    .await
    .expect_err("a metadata root cannot move to another session");
    assert_check_violation(&moved_root);

    assert_eq!(
        repository
            .load_session_metadata(session(0x701))
            .await?
            .expect("source session remains readable")
            .content()
            .title(),
        Some("root-only")
    );
    assert_eq!(
        repository
            .load_session_metadata(session(0x702))
            .await?
            .expect("target session remains readable")
            .last_writer(),
        None
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// deleting one current tag cannot silently change a snapshot without
/// a matching immutable replacement receipt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn current_metadata_tag_rejects_partial_delete() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    let written = metadata(
        Some("guarded"),
        &["retained"],
        &[("source", "fixture")],
        false,
    );
    repository
        .handle(replacement(0x901, 0x701, written.clone()))
        .await?;

    let partial_delete = sqlx::query(
        "DELETE FROM session_metadata_tag
          WHERE session_id = $1
            AND tag = 'retained'",
    )
    .bind(Uuid::from_u128(0x701))
    .execute(&pool)
    .await
    .expect_err("a current tag cannot be deleted outside complete replacement");
    assert_check_violation(&partial_delete);
    assert_eq!(
        repository
            .load_session_metadata(session(0x701))
            .await?
            .expect("guarded metadata remains readable")
            .content(),
        &written
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// updating one current attribute cannot silently change a snapshot
/// without a matching immutable replacement receipt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn current_metadata_attribute_rejects_out_of_band_update() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    let written = metadata(Some("guarded"), &[], &[("source", "fixture")], false);
    repository
        .handle(replacement(0x901, 0x701, written.clone()))
        .await?;

    let partial_update = sqlx::query(
        "UPDATE session_metadata_attribute
            SET attribute_value = 'changed'
          WHERE session_id = $1
            AND attribute_key = 'source'",
    )
    .bind(Uuid::from_u128(0x701))
    .execute(&pool)
    .await
    .expect_err("a current attribute cannot be updated outside complete replacement");
    assert_check_violation(&partial_update);
    assert_eq!(
        repository
            .load_session_metadata(session(0x701))
            .await?
            .expect("guarded metadata remains readable")
            .content(),
        &written
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// sealed receipt satellites cannot gain either kind of late member.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_receipt_satellites_reject_late_inserts() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    SessionMetadataRepository::new(pool.clone())
        .handle(replacement(
            0x902,
            0x701,
            metadata(Some("sealed"), &[], &[], false),
        ))
        .await?;
    let late_tag = sqlx::query(
        "INSERT INTO replace_session_metadata_command_tag (command_id, tag)
         VALUES ($1, 'late')",
    )
    .bind(Uuid::from_u128(0x902))
    .execute(&pool)
    .await
    .expect_err("a committed receipt cannot gain a later tag");
    assert_check_violation(&late_tag);

    let late_attribute = sqlx::query(
        "INSERT INTO replace_session_metadata_command_attribute
            (command_id, attribute_key, attribute_value)
         VALUES ($1, 'late', 'value')",
    )
    .bind(Uuid::from_u128(0x902))
    .execute(&pool)
    .await
    .expect_err("a committed receipt cannot gain a later attribute");
    assert_check_violation(&late_attribute);

    pool.close().await;
    drop(container);
    Ok(())
}

/// the applied receipt shape always carries its actor evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn applied_metadata_receipt_requires_result_actor() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let missing_applied_actor = sqlx::query(
        "INSERT INTO replace_session_metadata_command
            (command_id, command_kind, storage_version, session_id,
             issuer_kind, actor_kind, replacement_archived, result_kind,
             result_session_id, result_applied_session_id,
             result_updated_at, result_actor_kind)
         VALUES
            ($1, 'replace_session_metadata', 1, $2,
             'user', 'user', false, 'applied', $2, $2,
             statement_timestamp(), NULL)",
    )
    .bind(Uuid::from_u128(0x907))
    .bind(Uuid::from_u128(0x701))
    .execute(&pool)
    .await
    .expect_err("an applied receipt must name its result actor");
    assert_check_violation(&missing_applied_actor);

    pool.close().await;
    drop(container);
    Ok(())
}

/// the current metadata timestamp cannot use PostgreSQL's positive
/// infinity sentinel.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn current_metadata_timestamp_must_be_finite() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    SessionMetadataRepository::new(pool.clone())
        .handle(replacement(
            0x901,
            0x701,
            metadata(Some("finite"), &[], &[], false),
        ))
        .await?;

    let infinite = sqlx::query(
        "UPDATE session_metadata
            SET updated_at = 'infinity'::timestamptz
          WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x701))
    .execute(&pool)
    .await
    .expect_err("current metadata timestamps must be finite");
    assert_check_violation(&infinite);

    pool.close().await;
    drop(container);
    Ok(())
}

/// an applied receipt timestamp cannot use PostgreSQL's negative
/// infinity sentinel.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_receipt_timestamp_must_be_finite() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let infinite = sqlx::query(
        "INSERT INTO replace_session_metadata_command
            (command_id, command_kind, storage_version, session_id,
             issuer_kind, actor_kind, replacement_archived, result_kind,
             result_session_id, result_applied_session_id,
             result_updated_at, result_actor_kind)
         VALUES
            ($1, 'replace_session_metadata', 1, $2,
             'user', 'user', false, 'applied', $2, $2,
             '-infinity'::timestamptz, 'user')",
    )
    .bind(Uuid::from_u128(0x908))
    .bind(Uuid::from_u128(0x701))
    .execute(&pool)
    .await
    .expect_err("metadata receipt timestamps must be finite");
    assert_check_violation(&infinite);

    pool.close().await;
    drop(container);
    Ok(())
}

/// an applied receipt must name a retained current metadata root for
/// its exact target session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn applied_metadata_receipt_requires_current_root() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'replace_session_metadata', 1, statement_timestamp(), 'operator')",
    )
    .bind(Uuid::from_u128(0x908))
    .execute(&mut *transaction)
    .await?;
    let missing_root = sqlx::query(
        "INSERT INTO replace_session_metadata_command
            (command_id, command_kind, storage_version, session_id,
             issuer_kind, actor_kind, replacement_archived, result_kind,
             result_session_id, result_applied_session_id,
             result_updated_at, result_actor_kind)
         VALUES
            ($1, 'replace_session_metadata', 1, $2,
             'user', 'user', false, 'applied', $2, $2,
             statement_timestamp(), 'user')",
    )
    .bind(Uuid::from_u128(0x908))
    .bind(Uuid::from_u128(0x701))
    .execute(&mut *transaction)
    .await
    .expect_err("an applied receipt must name its current metadata root immediately");
    assert_check_violation(&missing_root);
    transaction.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(())
}

/// a user-global identifier claimed by another command kind is a
/// conflict, never an absent metadata command.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cross_kind_reuse_is_conflict() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    let repository = SessionMetadataRepository::new(pool.clone());
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

/// a list item validates the complete stored metadata even though its
/// public projection intentionally omits attributes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_list_validates_omitted_attributes() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(0x801, 0x701))
        .await?;
    let repository = SessionMetadataRepository::new(pool.clone());
    repository
        .handle(replacement(
            0x901,
            0x701,
            metadata(Some("checked"), &[], &[("payload", "")], false),
        ))
        .await?;
    let oversized = "x".repeat(SessionMetadataContent::MAX_TOTAL_UTF8_BYTES);
    sqlx::query(
        "ALTER TABLE session_metadata_attribute
         DISABLE TRIGGER session_metadata_attribute_update_is_rejected",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_metadata_attribute
         DISABLE TRIGGER session_metadata_attribute_matches_receipt",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_metadata_attribute
         SET attribute_value = $2
         WHERE session_id = $1 AND attribute_key = 'payload'",
    )
    .bind(Uuid::from_u128(0x701))
    .bind(oversized)
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_metadata_attribute
         ENABLE TRIGGER session_metadata_attribute_matches_receipt",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_metadata_attribute
         ENABLE TRIGGER session_metadata_attribute_update_is_rejected",
    )
    .execute(&pool)
    .await?;

    let query = signalbox_application::SessionMetadataListQuery::default_page(5);
    let mut page = repository.open_page(query).await?;
    assert!(matches!(
        page.next_item().await,
        Err(SessionMetadataRepositoryError::Corruption(
            SessionMetadataCorruption::InvalidContent(_)
        ))
    ));
    drop(page);

    pool.close().await;
    drop(container);
    Ok(())
}

/// bulk truncation cannot bypass metadata current-state or sealed
/// receipt guards.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_tables_reject_truncate() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;

    let current_root = sqlx::query("TRUNCATE session_metadata CASCADE")
        .execute(&pool)
        .await
        .expect_err("current metadata roots are not truncatable");
    assert_check_violation(&current_root);

    let current_tags = sqlx::query("TRUNCATE session_metadata_tag")
        .execute(&pool)
        .await
        .expect_err("current metadata tags are not truncatable");
    assert_check_violation(&current_tags);

    let current_attributes = sqlx::query("TRUNCATE session_metadata_attribute")
        .execute(&pool)
        .await
        .expect_err("current metadata attributes are not truncatable");
    assert_check_violation(&current_attributes);

    let installations = sqlx::query("TRUNCATE session_metadata_installation CASCADE")
        .execute(&pool)
        .await
        .expect_err("metadata installation evidence is not truncatable");
    assert_check_violation(&installations);

    let receipt_parent = sqlx::query("TRUNCATE replace_session_metadata_command CASCADE")
        .execute(&pool)
        .await
        .expect_err("metadata receipt parents are not truncatable");
    assert_check_violation(&receipt_parent);

    let receipt_tags = sqlx::query("TRUNCATE replace_session_metadata_command_tag")
        .execute(&pool)
        .await
        .expect_err("metadata receipt tags are not truncatable");
    assert_check_violation(&receipt_tags);

    let receipt_attributes = sqlx::query("TRUNCATE replace_session_metadata_command_attribute")
        .execute(&pool)
        .await
        .expect_err("metadata receipt attributes are not truncatable");
    assert_check_violation(&receipt_attributes);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn metadata_schema_bounds_every_indexed_string() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
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
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'replace_session_metadata', 1, transaction_timestamp(), 'operator')",
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
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'replace_session_metadata', 1, transaction_timestamp(), 'operator')",
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
