#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

//! Unified conversation-listing and display-title integration behavior over
//! real PostgreSQL: both origin classes in one transactionally fresh keyset
//! page, honest pagination, exact filters, import-time title derivation, and
//! the immutable resolved title facts stored at import time.

use std::error::Error;

use signalbox_application::{
    ConversationListCursor, ConversationListItem, ConversationListQuery, ConversationLister as _,
    ConversationOriginFilter, ConversationPageReader, ImportConversationOutcome,
    ImportConversationService, ImportedConversationIdGenerator,
};
use signalbox_conversation_import_claude_code::ClaudeCodeJsonlConverter;
use signalbox_conversation_import_codex::CodexRolloutJsonlConverter;
use signalbox_domain::{
    CreateSession, DirectModelSelection, DurableCommandId, ImportedConversationFormat,
    ImportedConversationId, ImportedTranscriptEntryId, ModelSelectionRequest,
    PreparedCreateSession, ReplaceSessionMetadata, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionId, SessionMetadataContent, TranscriptAncestry,
};
use signalbox_persistence::{
    conversation_import::{
        ImportedConversationCorruption, ImportedConversationRepository,
        ImportedConversationRepositoryError,
    },
    conversation_listing::ConversationListingRepository,
    create_session::CreateSessionRepository,
    disposable_postgres_server_args, disposable_postgres_state_tmpfs,
    disposable_test_container_labels, local_test_connection_options, migrate,
    session_metadata::SessionMetadataRepository,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_conversation_listing";
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

/// One synthetic Claude Code export whose summary record supplies the derived
/// display title and whose one user-role message supplies the fallback candidate.
const CLAUDE_SUMMARY_SOURCE: &str = concat!(
    "{\"type\":\"summary\",\"summary\":\"Imported planning summary\"}\n",
    "{\"type\":\"user\",\"message\":{\"role\":\"user\",",
    "\"content\":\"synthetic imported question\"}}"
);
/// The exact title [`CLAUDE_SUMMARY_SOURCE`]'s summary record derives.
const CLAUDE_SUMMARY_TITLE: &str = "Imported planning summary";
/// The normalized entry count of [`CLAUDE_SUMMARY_SOURCE`].
const CLAUDE_SUMMARY_ENTRY_COUNT: u64 = 2;

/// One synthetic Codex rollout whose only user-role message supplies the derived
/// display title.
const CODEX_USER_SOURCE: &str = concat!(
    "{\"timestamp\":\"2026-07-25T00:00:00Z\",\"type\":\"response_item\",",
    "\"payload\":{\"type\":\"message\",\"role\":\"user\",",
    "\"content\":[{\"type\":\"input_text\",\"text\":\"synthetic codex question\"}]}}"
);
/// The exact title [`CODEX_USER_SOURCE`]'s user-role message derives.
const CODEX_USER_TITLE: &str = "synthetic codex question";
/// The normalized entry count of [`CODEX_USER_SOURCE`].
const CODEX_USER_ENTRY_COUNT: u64 = 1;

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_cmd(disposable_postgres_server_args())
        .with_mount(disposable_postgres_state_tmpfs())
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

fn imported(value: u128) -> ImportedConversationId {
    ImportedConversationId::from_uuid(Uuid::from_u128(value))
}

fn command(value: u128) -> DurableCommandId {
    DurableCommandId::from_uuid(Uuid::from_u128(value))
}

fn creation(command_value: u128, session_value: u128) -> PreparedCreateSession {
    CreateSession::new(
        command(command_value),
        SessionCreationProvenance::new(
            SessionCreationCause::UserInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(session_value + 0x1000)),
        )),
    )
    .prepare(session(session_value))
    .expect("user-initiated creation without ancestry is preparable")
}

async fn create_fixture_session(pool: &PgPool, seed: u128) -> Result<SessionId, Box<dyn Error>> {
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation(seed + 0x8000, seed))
        .await?;
    Ok(session(seed))
}

async fn write_fixture_metadata(
    pool: &PgPool,
    command_seed: u128,
    target: SessionId,
    title: Option<&str>,
    archived: bool,
) -> Result<(), Box<dyn Error>> {
    let content =
        SessionMetadataContent::try_new(title.map(str::to_owned), Vec::new(), Vec::new(), archived)
            .expect("fixture metadata is valid");
    SessionMetadataRepository::new(pool.clone())
        .handle(ReplaceSessionMetadata::new(
            command(command_seed),
            target,
            content,
        ))
        .await?;
    Ok(())
}

/// Supplies one named conversation identity and sequential entry identities.
struct ListingIds {
    conversation: Option<ImportedConversationId>,
    next_entry: u128,
}

impl ListingIds {
    fn new(conversation: ImportedConversationId, next_entry: u128) -> Self {
        Self {
            conversation: Some(conversation),
            next_entry,
        }
    }
}

impl ImportedConversationIdGenerator for ListingIds {
    fn next_conversation_id(&mut self) -> ImportedConversationId {
        self.conversation
            .take()
            .expect("fixture supplies exactly one conversation identity")
    }

    fn next_entry_id(&mut self) -> ImportedTranscriptEntryId {
        let identity = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(self.next_entry));
        self.next_entry = self
            .next_entry
            .checked_add(1)
            .expect("fixture entry identity range is not exhausted");
        identity
    }
}

async fn import_claude_fixture(
    pool: &PgPool,
    conversation: ImportedConversationId,
    entry_seed: u128,
    source: &str,
) -> Result<ImportedConversationId, Box<dyn Error>> {
    let mut service = ImportConversationService::new(
        ListingIds::new(conversation, entry_seed),
        ClaudeCodeJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );
    match service.execute(source.as_bytes()).await? {
        ImportConversationOutcome::Inserted { conversation } => Ok(conversation),
        ImportConversationOutcome::AlreadyImported { .. } => {
            panic!("fixture source must be a fresh snapshot")
        }
    }
}

async fn import_codex_fixture(
    pool: &PgPool,
    conversation: ImportedConversationId,
    entry_seed: u128,
    source: &str,
) -> Result<ImportedConversationId, Box<dyn Error>> {
    let mut service = ImportConversationService::new(
        ListingIds::new(conversation, entry_seed),
        CodexRolloutJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );
    match service.execute(source.as_bytes()).await? {
        ImportConversationOutcome::Inserted { conversation } => Ok(conversation),
        ImportConversationOutcome::AlreadyImported { .. } => {
            panic!("fixture source must be a fresh snapshot")
        }
    }
}

async fn collect_page(
    pool: &PgPool,
    query: ConversationListQuery,
) -> Result<(Vec<ConversationListItem>, Option<ConversationListCursor>), Box<dyn Error>> {
    let mut page = ConversationListingRepository::new(pool.clone())
        .open_conversation_page(query)
        .await?;
    let mut items = Vec::new();
    while let Some(item) = ConversationPageReader::next_item(&mut page).await? {
        items.push(item);
    }
    Ok((items, page.next_after()))
}

fn query(
    title_contains: Option<&str>,
    origin: ConversationOriginFilter,
    include_archived: bool,
    page_size: u64,
    after: Option<ConversationListCursor>,
) -> ConversationListQuery {
    ConversationListQuery::try_new(
        title_contains.map(str::to_owned),
        origin,
        include_archived,
        page_size,
        after,
    )
    .expect("fixture query is valid")
}

/// One default page lists native sessions and imported conversations together
/// in strict identity order, each row carrying its per-origin facts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unified_page_lists_both_origin_classes_in_identity_order() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let first_session = create_fixture_session(&pool, 0x10).await?;
    let imported_claude =
        import_claude_fixture(&pool, imported(0x20), 0x300, CLAUDE_SUMMARY_SOURCE).await?;
    let second_session = create_fixture_session(&pool, 0x30).await?;
    write_fixture_metadata(&pool, 0x9001, second_session, Some("Native plan"), false).await?;

    let (items, next_after) = collect_page(
        &pool,
        query(None, ConversationOriginFilter::All, false, 50, None),
    )
    .await?;

    assert_eq!(next_after, None);
    assert_eq!(
        items,
        vec![
            ConversationListItem::NativeSession {
                session: first_session,
                title: None,
                archived: false,
                defaults_version: SessionConfigurationDefaultsVersion::first(),
            },
            ConversationListItem::ImportedConversation {
                conversation: imported_claude,
                title: Some(String::from(CLAUDE_SUMMARY_TITLE)),
                entry_count: CLAUDE_SUMMARY_ENTRY_COUNT,
                format: ImportedConversationFormat::ClaudeCodeSessionJsonlV2,
            },
            ConversationListItem::NativeSession {
                session: second_session,
                title: Some(String::from("Native plan")),
                archived: false,
                defaults_version: SessionConfigurationDefaultsVersion::first(),
            },
        ]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The origin filter selects exactly its named classes, and archived native
/// sessions appear only when requested while imported rows are unaffected.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unified_origin_and_archive_filters_select_exact_classes() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let archived_session = create_fixture_session(&pool, 0x10).await?;
    write_fixture_metadata(&pool, 0x9001, archived_session, Some("Archived plan"), true).await?;
    let live_session = create_fixture_session(&pool, 0x20).await?;
    let imported_codex =
        import_codex_fixture(&pool, imported(0x30), 0x300, CODEX_USER_SOURCE).await?;

    let (default_view, _) = collect_page(
        &pool,
        query(None, ConversationOriginFilter::All, false, 50, None),
    )
    .await?;
    assert_eq!(
        default_view.iter().map(cursor_of).collect::<Vec<_>>(),
        vec![
            ConversationListCursor::NativeSession(live_session),
            ConversationListCursor::ImportedConversation(imported_codex),
        ]
    );

    let (archived_view, _) = collect_page(
        &pool,
        query(None, ConversationOriginFilter::All, true, 50, None),
    )
    .await?;
    assert_eq!(
        archived_view.iter().map(cursor_of).collect::<Vec<_>>(),
        vec![
            ConversationListCursor::NativeSession(archived_session),
            ConversationListCursor::NativeSession(live_session),
            ConversationListCursor::ImportedConversation(imported_codex),
        ]
    );

    let (native_view, _) = collect_page(
        &pool,
        query(None, ConversationOriginFilter::Native, false, 50, None),
    )
    .await?;
    assert_eq!(
        native_view.iter().map(cursor_of).collect::<Vec<_>>(),
        vec![ConversationListCursor::NativeSession(live_session)]
    );

    let (imported_view, _) = collect_page(
        &pool,
        query(None, ConversationOriginFilter::Imported, false, 50, None),
    )
    .await?;
    assert_eq!(
        imported_view.iter().map(cursor_of).collect::<Vec<_>>(),
        vec![ConversationListCursor::ImportedConversation(imported_codex)]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The exact case-sensitive title substring matches present native metadata
/// titles and imported display titles alike, and matches nothing absent.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unified_title_filter_matches_native_and_imported_titles() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let titled_session = create_fixture_session(&pool, 0x10).await?;
    write_fixture_metadata(&pool, 0x9001, titled_session, Some("planning ahead"), false).await?;
    let untitled_session = create_fixture_session(&pool, 0x20).await?;
    let imported_claude =
        import_claude_fixture(&pool, imported(0x30), 0x300, CLAUDE_SUMMARY_SOURCE).await?;

    let (matched, _) = collect_page(
        &pool,
        query(
            Some("planning"),
            ConversationOriginFilter::All,
            false,
            50,
            None,
        ),
    )
    .await?;
    assert_eq!(
        matched.iter().map(cursor_of).collect::<Vec<_>>(),
        vec![
            ConversationListCursor::NativeSession(titled_session),
            ConversationListCursor::ImportedConversation(imported_claude),
        ]
    );

    let (imported_case, _) = collect_page(
        &pool,
        query(
            Some("Imported"),
            ConversationOriginFilter::All,
            false,
            50,
            None,
        ),
    )
    .await?;
    assert_eq!(
        imported_case.iter().map(cursor_of).collect::<Vec<_>>(),
        vec![ConversationListCursor::ImportedConversation(
            imported_claude
        )]
    );

    let (case_folded, _) = collect_page(
        &pool,
        query(
            Some("imported"),
            ConversationOriginFilter::All,
            false,
            50,
            None,
        ),
    )
    .await?;
    assert!(
        case_folded.is_empty(),
        "the substring filter is case-sensitive, so a lowercased spelling matches nothing"
    );

    let (unmatched, _) = collect_page(
        &pool,
        query(
            Some("absent"),
            ConversationOriginFilter::All,
            false,
            50,
            None,
        ),
    )
    .await?;
    assert!(unmatched.is_empty());
    let _ = untitled_session;

    pool.close().await;
    drop(container);
    Ok(())
}

/// Full pages report the exact continuation cursor, later pages resume after
/// it across origin classes, and an exhausted page reports no cursor.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unified_pagination_reports_its_cursor_without_silent_truncation()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let first_session = create_fixture_session(&pool, 0x10).await?;
    let imported_claude =
        import_claude_fixture(&pool, imported(0x20), 0x300, CLAUDE_SUMMARY_SOURCE).await?;
    let second_session = create_fixture_session(&pool, 0x30).await?;

    let (first_page, first_cursor) = collect_page(
        &pool,
        query(None, ConversationOriginFilter::All, false, 1, None),
    )
    .await?;
    assert_eq!(
        first_page.iter().map(cursor_of).collect::<Vec<_>>(),
        vec![ConversationListCursor::NativeSession(first_session)]
    );
    assert_eq!(
        first_cursor,
        Some(ConversationListCursor::NativeSession(first_session))
    );

    let (second_page, second_cursor) = collect_page(
        &pool,
        query(None, ConversationOriginFilter::All, false, 1, first_cursor),
    )
    .await?;
    assert_eq!(
        second_page.iter().map(cursor_of).collect::<Vec<_>>(),
        vec![ConversationListCursor::ImportedConversation(
            imported_claude
        )]
    );
    assert_eq!(
        second_cursor,
        Some(ConversationListCursor::ImportedConversation(
            imported_claude
        ))
    );

    let (final_page, final_cursor) = collect_page(
        &pool,
        query(None, ConversationOriginFilter::All, false, 1, second_cursor),
    )
    .await?;
    assert_eq!(
        final_page.iter().map(cursor_of).collect::<Vec<_>>(),
        vec![ConversationListCursor::NativeSession(second_session)]
    );
    assert_eq!(final_cursor, None);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: import derives and stores the display title once, from the summary
/// record for Claude Code and from the first attested user text for Codex.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_import_derives_and_stores_the_display_title() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let imported_claude =
        import_claude_fixture(&pool, imported(0x10), 0x300, CLAUDE_SUMMARY_SOURCE).await?;
    let imported_codex =
        import_codex_fixture(&pool, imported(0x20), 0x400, CODEX_USER_SOURCE).await?;

    let stored: Vec<(Uuid, Option<String>, String, i64)> = sqlx::query_as(
        "SELECT imported_conversation_id, display_title, display_title_state,
                declared_entry_count::bigint
           FROM imported_conversation
          ORDER BY imported_conversation_id",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        stored,
        vec![
            (
                imported_claude.into_uuid(),
                Some(String::from(CLAUDE_SUMMARY_TITLE)),
                String::from("derived"),
                i64::try_from(CLAUDE_SUMMARY_ENTRY_COUNT)?,
            ),
            (
                imported_codex.into_uuid(),
                Some(String::from(CODEX_USER_TITLE)),
                String::from("derived"),
                i64::try_from(CODEX_USER_ENTRY_COUNT)?,
            ),
        ]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A stored display title that disagrees with pure re-derivation is typed
/// corruption on the checked complete load.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn corrupt_display_title_fails_closed_on_load() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let imported_claude =
        import_claude_fixture(&pool, imported(0x10), 0x300, CLAUDE_SUMMARY_SOURCE).await?;
    sqlx::query("ALTER TABLE imported_conversation DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE imported_conversation
            SET display_title = 'Another title'
          WHERE imported_conversation_id = $1",
    )
    .bind(imported_claude.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE imported_conversation ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    let error = ImportedConversationRepository::new(pool.clone())
        .load(imported_claude)
        .await
        .expect_err("a drifted stored title must fail the checked load");
    assert!(matches!(
        error,
        ImportedConversationRepositoryError::Corruption(
            ImportedConversationCorruption::DisplayTitleMismatch
        )
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// The final imported-conversation header is append-only.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn final_imported_conversation_header_is_append_only() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let imported_claude =
        import_claude_fixture(&pool, imported(0x10), 0x300, CLAUDE_SUMMARY_SOURCE).await?;

    assert!(
        sqlx::query(
            "UPDATE imported_conversation
                SET declared_entry_count = declared_entry_count",
        )
        .execute(&pool)
        .await
        .is_err(),
        "non-title header updates must remain rejected"
    );
    assert!(
        sqlx::query(
            "UPDATE imported_conversation
                SET display_title = 'Replaced title'
              WHERE imported_conversation_id = $1",
        )
        .bind(imported_claude.into_uuid())
        .execute(&pool)
        .await
        .is_err(),
        "a resolved display title must remain immutable"
    );
    assert!(
        sqlx::query("DELETE FROM imported_conversation")
            .execute(&pool)
            .await
            .is_err(),
        "header deletion must remain rejected"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

fn cursor_of(item: &ConversationListItem) -> ConversationListCursor {
    item.cursor()
}
