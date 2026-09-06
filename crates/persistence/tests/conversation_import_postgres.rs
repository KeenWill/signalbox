#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{
    collections::VecDeque,
    env,
    error::Error,
    fs,
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
};

use rust_decimal::Decimal;
use signalbox_application::{
    ImportConversationError, ImportConversationOutcome, ImportConversationService,
    ImportedConversationConverter, ImportedConversationIdGenerator,
};
use signalbox_conversation_import_claude_code::ClaudeCodeJsonlConverter;
use signalbox_conversation_import_codex::CodexRolloutJsonlConverter;
use signalbox_domain::{
    BlobDigest, ImportedConversation, ImportedConversationFormat, ImportedConversationId,
    ImportedConversationReconstitutionFailure, ImportedRawRecordHash, ImportedRawRecordPosition,
    ImportedRawSourceRecord, ImportedRecordEntryPosition, ImportedSourceAttestation,
    ImportedSourceMetadata, ImportedSpeaker, ImportedStructuredObjectMember,
    ImportedStructuredValue, ImportedText, ImportedToolResultBlock, ImportedToolResultValue,
    ImportedTranscriptContent, ImportedTranscriptEntryId, ImportedTranscriptEntryInput,
    ImportedTranscriptPosition,
};
use signalbox_persistence::{
    conversation_import::{
        ImportedConversationCorruption, ImportedConversationIdentityCollision,
        ImportedConversationRepository, ImportedConversationRepositoryError,
        corrupt_integration_imported_blob,
    },
    conversation_import_discovery::{
        ImportedConversationDiscoveryRepository, ImportedConversationPageRequest,
        ImportedEntryWindowAnchor,
    },
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
};
use sqlx::{PgPool, Transaction, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_import_integration";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const ARBITRARY_LINEAGE_ENTRY_ID_START: u128 = 1;

enum EntryIdentitySupply {
    Fixed(VecDeque<ImportedTranscriptEntryId>),
    Generated { next: u128 },
}

struct FixedIds {
    conversations: VecDeque<ImportedConversationId>,
    entries: EntryIdentitySupply,
}

impl FixedIds {
    /// Supplies conversation identities the test has already named, so an
    /// expectation states the fixture's own identity instead of restating the
    /// seed behind it. Entry identities are arbitrary-by-construction because
    /// lineage tests do not observe them.
    fn for_conversations(conversations: impl IntoIterator<Item = ImportedConversationId>) -> Self {
        Self {
            conversations: conversations.into_iter().collect(),
            entries: EntryIdentitySupply::Generated {
                next: ARBITRARY_LINEAGE_ENTRY_ID_START,
            },
        }
    }

    /// Mints conversation identities from seeds for tests that never name them.
    fn new(conversations: &[u128], entries: impl IntoIterator<Item = u128>) -> Self {
        Self {
            conversations: conversations
                .iter()
                .copied()
                .map(|value| ImportedConversationId::from_uuid(Uuid::from_u128(value)))
                .collect(),
            entries: EntryIdentitySupply::Fixed(
                entries
                    .into_iter()
                    .map(|value| ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(value)))
                    .collect(),
            ),
        }
    }
}

impl ImportedConversationIdGenerator for FixedIds {
    fn next_conversation_id(&mut self) -> ImportedConversationId {
        self.conversations
            .pop_front()
            .expect("fixture supplies every conversation identity")
    }

    fn next_entry_id(&mut self) -> ImportedTranscriptEntryId {
        match &mut self.entries {
            EntryIdentitySupply::Fixed(entries) => entries
                .pop_front()
                .expect("fixture supplies every imported-entry identity"),
            EntryIdentitySupply::Generated { next } => {
                let identity = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(*next));
                *next = next
                    .checked_add(1)
                    .expect("generated imported-entry identity range is not exhausted");
                identity
            }
        }
    }
}

struct SequentialIds {
    conversations: VecDeque<ImportedConversationId>,
    next_entry: u128,
}

impl SequentialIds {
    fn new(conversations: [u128; 2], next_entry: u128) -> Self {
        Self {
            conversations: conversations
                .into_iter()
                .map(|value| ImportedConversationId::from_uuid(Uuid::from_u128(value)))
                .collect(),
            next_entry,
        }
    }
}

impl ImportedConversationIdGenerator for SequentialIds {
    fn next_conversation_id(&mut self) -> ImportedConversationId {
        self.conversations
            .pop_front()
            .expect("real transcript validation supplies two candidate identities")
    }

    fn next_entry_id(&mut self) -> ImportedTranscriptEntryId {
        let identity = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(self.next_entry));
        self.next_entry = self
            .next_entry
            .checked_add(1)
            .expect("real transcript entry identity range is not exhausted");
        identity
    }
}

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool, String), Box<dyn Error>> {
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
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool, database_url))
}

#[derive(Clone, Copy)]
/// Named behavior facts returned by the plumbing-only resume fixture.
///
/// Both arrays preserve the selected imported frontier's prefix order.
struct ImportedSeedFacts {
    conversation: Uuid,
    imported_prefix: [Uuid; 2],
    post_frontier_entry: Uuid,
    session: Uuid,
    semantic_prefix: [Uuid; 2],
    seed_frontier: Uuid,
}

fn imported_seed_facts() -> ImportedSeedFacts {
    ImportedSeedFacts {
        conversation: Uuid::from_u128(0x1000_0000_0000_4000_8000_0000_0000_0039),
        imported_prefix: [
            Uuid::from_u128(0x2000_0000_0000_4000_8000_0000_0000_0039),
            Uuid::from_u128(0x2000_0000_0000_4000_8000_0000_0000_0040),
        ],
        post_frontier_entry: Uuid::from_u128(0x2000_0000_0000_4000_8000_0000_0000_0041),
        session: Uuid::from_u128(0x4000_0000_0000_4000_8000_0000_0000_0039),
        semantic_prefix: [
            Uuid::from_u128(0x6000_0000_0000_4000_8000_0000_0000_0039),
            Uuid::from_u128(0x6000_0000_0000_4000_8000_0000_0000_0040),
        ],
        seed_frontier: Uuid::from_u128(0x7000_0000_0000_4000_8000_0000_0000_0039),
    }
}

#[track_caller]
fn assert_attested_source_result_kind(
    content: &ImportedTranscriptContent,
    expected_source_type: &str,
) {
    let ImportedTranscriptContent::ToolResult {
        content: ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(blocks)),
        ..
    } = content
    else {
        panic!("fixture expected an attested block-valued tool result");
    };
    let [ImportedToolResultBlock::SourceResultBlock { source_type }] = blocks.as_ref() else {
        panic!("fixture expected exactly one source-result block");
    };
    let ImportedSourceAttestation::Attested(source_type) = source_type else {
        panic!("fixture expected an attested source-result type");
    };
    assert_eq!(source_type.as_str(), expected_source_type);
}

async fn insert_imported_source_scaffolding(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
) -> Result<ImportedSeedFacts, sqlx::Error> {
    let facts = imported_seed_facts();
    insert_catalogued_raw_source(transaction, vec![0x11; 32], 1).await?;
    sqlx::query(
        "INSERT INTO imported_conversation
            (imported_conversation_id, storage_version, source_format,
             converter_version, source_digest, declared_raw_record_count,
             declared_entry_count, display_title, display_title_state)
         VALUES ($1, 1, 'claude_code_session_jsonl', 1, $2, 1, 3,
                 NULL, 'underivable')",
    )
    .bind(facts.conversation)
    .bind(vec![0x22_u8; 32])
    .execute(&mut **transaction)
    .await?;
    insert_imported_source_members(transaction).await?;
    Ok(facts)
}

async fn insert_imported_source_members(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "INSERT INTO imported_conversation_raw_record
            (imported_conversation_id, raw_record_position, content_hash,
             conversion_digest, normalized_value_encoding,
             declared_entry_count)
         VALUES
            ('10000000-0000-4000-8000-000000000039', 1,
             decode(repeat('11', 32), 'hex'),
             decode(repeat('33', 32), 'hex'), decode('01', 'hex'), 3);
         INSERT INTO imported_transcript_entry
            (imported_conversation_id, imported_entry_position,
             imported_transcript_entry_id, raw_record_position,
             record_entry_position, source_speaker_kind, content_encoding,
             source_metadata_encoding)
         VALUES
            ('10000000-0000-4000-8000-000000000039', 1,
             '20000000-0000-4000-8000-000000000039', 1, 1,
             'attested_user', decode('02010100', 'hex'), decode('01', 'hex')),
            ('10000000-0000-4000-8000-000000000039', 2,
             '20000000-0000-4000-8000-000000000040', 1, 2,
             'attested_assistant', decode('02010100', 'hex'), decode('02', 'hex')),
            ('10000000-0000-4000-8000-000000000039', 3,
             '20000000-0000-4000-8000-000000000041', 1, 3,
             'attested_user', decode('02010100', 'hex'), decode('03', 'hex'));",
    )
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn insert_catalogued_raw_source(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    content_hash: Vec<u8>,
    byte_length: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO blob_store_binding (store_name, namespace_id)
         VALUES ('integration', '0000696d-706f-7274-6564-5f736f757263')
         ON CONFLICT DO NOTHING",
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO blob (digest, byte_length)
         VALUES ($1, $2)",
    )
    .bind(&content_hash)
    .bind(byte_length)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO blob_replica (digest, store_name, object_key)
         VALUES ($1, 'integration', 'sha256/fixture/' || encode($1, 'hex'))",
    )
    .bind(&content_hash)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO imported_raw_source_record (content_hash)
         VALUES ($1)",
    )
    .bind(content_hash)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// The `creation_cause` a fixture writes.
///
/// `202608110001_user_role_storage_vocabulary` renamed the stored value, so the
/// spelling a fixture must use depends on where its database stands. A fixture
/// seeding a database held at an earlier migration has to write the retired
/// spelling: the `CHECK` in force at that point admits nothing else, and the
/// insert would fail before the migration under test could run. A fixture
/// seeding a fully migrated database writes the current spelling for the
/// mirror-image reason.
const CURRENT_CREATION_CAUSE: &str = "interactive";
async fn insert_imported_session_scaffolding(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
) -> Result<(), sqlx::Error> {
    insert_imported_session_scaffolding_with_creation_cause(transaction, CURRENT_CREATION_CAUSE)
        .await
}

async fn insert_imported_session_scaffolding_with_creation_cause(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    creation_cause: &str,
) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES
            ('30000000-0000-4000-8000-000000000039',
             'create_session_from_imported_frontier', 1,
             transaction_timestamp(), 'operator');",
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session
            (session_id, creation_cause, ancestry_kind,
             imported_conversation_id, imported_frontier_entry_id,
             imported_frontier_position, imported_relationship_kind)
         VALUES
            ('40000000-0000-4000-8000-000000000039',
             $1, 'imported_conversation',
             '10000000-0000-4000-8000-000000000039',
             '20000000-0000-4000-8000-000000000040', 2, 'resume')",
    )
    .bind(creation_cause)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_lifecycle
            (session_id, state_kind, owned, start_gate_held, actor_kind)
         VALUES ('40000000-0000-4000-8000-000000000039', 'created', false, false, 'operator')",
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_ownership_event
            (session_id, event_ordinal, transition_kind, owned_after, actor_kind)
         VALUES ('40000000-0000-4000-8000-000000000039', 1,
                 'created_unmonitored', false, 'operator')",
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::raw_sql(
        "INSERT INTO session_scheduler (session_id)
         VALUES ('40000000-0000-4000-8000-000000000039');
         INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('40000000-0000-4000-8000-000000000039', 1, 'direct',
             '50000000-0000-4000-8000-000000000039', NULL);
         INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ('40000000-0000-4000-8000-000000000039', 1);",
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO create_session_from_imported_frontier_command
            (command_id, command_kind, storage_version,
             imported_conversation_id, imported_frontier_entry_id,
             imported_frontier_position, imported_relationship_kind,
             creation_cause, ancestry_kind, initial_defaults_version,
             model_selection_kind, direct_model_selection_id, model_alias_id,
             result_kind, created_session_id)
         VALUES
            ('30000000-0000-4000-8000-000000000039',
             'create_session_from_imported_frontier', 1,
             '10000000-0000-4000-8000-000000000039',
             '20000000-0000-4000-8000-000000000040', 2, 'resume',
             $1, 'imported_conversation', 1,
             'direct', '50000000-0000-4000-8000-000000000039', NULL,
             'applied', '40000000-0000-4000-8000-000000000039')",
    )
    .bind(creation_cause)
    .execute(&mut **transaction)
    .await?;
    sqlx::raw_sql(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES
            ('40000000-0000-4000-8000-000000000039',
             '70000000-0000-4000-8000-000000000039', 2);",
    )
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn insert_imported_resume_seed_scaffolding(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
) -> Result<ImportedSeedFacts, sqlx::Error> {
    insert_imported_resume_seed_scaffolding_with_creation_cause(transaction, CURRENT_CREATION_CAUSE)
        .await
}

async fn insert_imported_resume_seed_scaffolding_with_creation_cause(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    creation_cause: &str,
) -> Result<ImportedSeedFacts, sqlx::Error> {
    let facts = insert_imported_source_scaffolding(transaction).await?;
    insert_imported_session_scaffolding_with_creation_cause(transaction, creation_cause).await?;
    Ok(facts)
}

async fn insert_imported_semantic_prefix(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    facts: ImportedSeedFacts,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             imported_conversation_id, imported_transcript_entry_id)
         VALUES
            ($1, $2, 'imported_entry', $3, $4),
            ($1, $5, 'imported_entry', $3, $6)",
    )
    .bind(facts.session)
    .bind(facts.semantic_prefix[0])
    .bind(facts.conversation)
    .bind(facts.imported_prefix[0])
    .bind(facts.semantic_prefix[1])
    .bind(facts.imported_prefix[1])
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn insert_exact_seed_members(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    facts: ImportedSeedFacts,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES
            ($1, $2, 1, $1, $3),
            ($1, $2, 2, $1, $4)",
    )
    .bind(facts.session)
    .bind(facts.seed_frontier)
    .bind(facts.semantic_prefix[0])
    .bind(facts.semantic_prefix[1])
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// S28: one applied imported-frontier command can commit only with its
/// exact ancestry, imported semantic prefix, and one-to-one seed frontier.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_exact_imported_session_seed_commits() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    let seed = insert_imported_resume_seed_scaffolding(&mut transaction).await?;
    insert_imported_semantic_prefix(&mut transaction, seed).await?;
    insert_exact_seed_members(&mut transaction, seed).await?;
    sqlx::query(
        "INSERT INTO imported_session_seed
            (session_id, seed_context_frontier_id)
         VALUES
            ($1, $2)",
    )
    .bind(seed.session)
    .bind(seed.seed_frontier)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let stored: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM imported_session_seed),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE payload_kind = 'imported_entry'),
            (SELECT count(*) FROM context_frontier_member)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, (1, 2, 2));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: the complete seed can be assembled in any in-transaction order;
/// inserting its one-to-one link before the semantic prefix remains valid.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_seed_link_can_precede_semantic_prefix() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    let seed = insert_imported_resume_seed_scaffolding(&mut transaction).await?;
    sqlx::query(
        "INSERT INTO imported_session_seed
            (session_id, seed_context_frontier_id)
         VALUES ($1, $2)",
    )
    .bind(seed.session)
    .bind(seed.seed_frontier)
    .execute(&mut *transaction)
    .await?;
    insert_imported_semantic_prefix(&mut transaction, seed).await?;
    insert_exact_seed_members(&mut transaction, seed).await?;
    transaction.commit().await?;

    let stored: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM imported_session_seed),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE payload_kind = 'imported_entry'),
            (SELECT count(*) FROM context_frontier_member)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, (1, 2, 2));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: a seed link inserted by a nested transaction still belongs to
/// its top-level transaction, so the remaining prefix may be assembled after the
/// savepoint is released.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_savepoint_seed_link_can_precede_semantic_prefix() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    let seed = insert_imported_resume_seed_scaffolding(&mut transaction).await?;

    sqlx::query("SAVEPOINT insert_seed_link")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO imported_session_seed
            (session_id, seed_context_frontier_id)
         VALUES ($1, $2)",
    )
    .bind(seed.session)
    .bind(seed.seed_frontier)
    .execute(&mut *transaction)
    .await?;
    sqlx::query("RELEASE SAVEPOINT insert_seed_link")
        .execute(&mut *transaction)
        .await?;

    insert_imported_semantic_prefix(&mut transaction, seed).await?;
    insert_exact_seed_members(&mut transaction, seed).await?;
    transaction.commit().await?;

    let stored: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM imported_session_seed),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE payload_kind = 'imported_entry'),
            (SELECT count(*) FROM context_frontier_member)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, (1, 2, 2));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: the one-to-one seed link can precede its imported session;
/// the deferred ancestry check validates the final cross-table facts.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_seed_link_can_precede_imported_session() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    let seed = insert_imported_source_scaffolding(&mut transaction).await?;
    sqlx::query(
        "INSERT INTO imported_session_seed
            (session_id, seed_context_frontier_id)
         VALUES ($1, $2)",
    )
    .bind(seed.session)
    .bind(seed.seed_frontier)
    .execute(&mut *transaction)
    .await?;
    insert_imported_session_scaffolding(&mut transaction).await?;
    insert_imported_semantic_prefix(&mut transaction, seed).await?;
    insert_exact_seed_members(&mut transaction, seed).await?;
    transaction.commit().await?;

    let stored: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM imported_session_seed),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE payload_kind = 'imported_entry'),
            (SELECT count(*) FROM context_frontier_member)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, (1, 2, 2));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: once the complete same-transaction seed check is discharged,
/// another imported semantic row cannot extend the selected prefix.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_immediate_seed_check_seals_same_transaction_prefix() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    let seed = insert_imported_resume_seed_scaffolding(&mut transaction).await?;
    insert_imported_semantic_prefix(&mut transaction, seed).await?;
    insert_exact_seed_members(&mut transaction, seed).await?;
    sqlx::query(
        "INSERT INTO imported_session_seed
            (session_id, seed_context_frontier_id)
         VALUES ($1, $2)",
    )
    .bind(seed.session)
    .bind(seed.seed_frontier)
    .execute(&mut *transaction)
    .await?;
    sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *transaction)
        .await?;

    let error = sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             imported_conversation_id, imported_transcript_entry_id)
         VALUES ($1, $2, 'imported_entry', $3, $4)",
    )
    .bind(seed.session)
    .bind(Uuid::from_u128(0x6000_0000_0000_4000_8000_0000_0000_0041))
    .bind(seed.conversation)
    .bind(seed.post_frontier_entry)
    .execute(&mut *transaction)
    .await
    .expect_err("a discharged seed check must seal the selected prefix");
    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("imported_semantic_entry_requires_selected_prefix")
    );
    transaction.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: imported ancestry cannot commit without the separate one-to-one
/// seed record, even when the materialized frontier content is exact.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_imported_ancestry_without_seed_is_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    let seed = insert_imported_resume_seed_scaffolding(&mut transaction).await?;
    insert_imported_semantic_prefix(&mut transaction, seed).await?;
    insert_exact_seed_members(&mut transaction, seed).await?;
    let error = transaction
        .commit()
        .await
        .expect_err("imported ancestry without its seed record must fail");
    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("imported_session_requires_seed")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: equal imported members in the wrong order are not the selected
/// imported prefix.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_reordered_imported_seed_members_are_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    let seed = insert_imported_resume_seed_scaffolding(&mut transaction).await?;
    insert_imported_semantic_prefix(&mut transaction, seed).await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES
            ($1, $2, 1, $1, $3),
            ($1, $2, 2, $1, $4)",
    )
    .bind(seed.session)
    .bind(seed.seed_frontier)
    .bind(seed.semantic_prefix[1])
    .bind(seed.semantic_prefix[0])
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO imported_session_seed
            (session_id, seed_context_frontier_id)
         VALUES ($1, $2)",
    )
    .bind(seed.session)
    .bind(seed.seed_frontier)
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("reordered imported members must fail");
    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("imported_session_seed_exact_prefix")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: an imported semantic payload cannot fabricate any native
/// accepted-input, turn, call, or tool evidence.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_imported_semantic_entry_rejects_native_payload_columns() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let error = sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             imported_conversation_id, imported_transcript_entry_id,
             assistant_text_value)
         VALUES
            ('40000000-0000-4000-8000-000000000039',
             '60000000-0000-4000-8000-000000000039', 'imported_entry',
             '10000000-0000-4000-8000-000000000039',
             '20000000-0000-4000-8000-000000000039', 'fabricated')",
    )
    .execute(&pool)
    .await
    .expect_err("an imported payload with native evidence columns must fail");
    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("semantic_transcript_entry_imported_shape")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: the new durable command discriminator still requires its complete
/// typed record at the transaction boundary.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_imported_creation_registry_claim_requires_typed_record() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES
            ('30000000-0000-4000-8000-000000000039',
             'create_session_from_imported_frontier', 1,
             transaction_timestamp(), 'operator')",
    )
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("an imported creation claim without its typed row must fail");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some(std::borrow::Cow::Borrowed("23503"))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: the reciprocal template-provenance creation FK does not make
/// the preexisting native command table truncatable.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_native_creation_command_truncate_remains_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let error = sqlx::query("TRUNCATE TABLE create_session_command")
        .execute(&pool)
        .await
        .expect_err("native creation commands must remain protected from truncate");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some(std::borrow::Cow::Borrowed("0A000"))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: row-level immutability cannot be bypassed by truncating the table
/// that carries exact seed-frontier membership.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_seed_frontier_member_truncate_is_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let error = sqlx::query("TRUNCATE TABLE context_frontier_delta")
        .execute(&pool)
        .await
        .expect_err("seed-bearing frontier membership must reject truncate");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some(std::borrow::Cow::Borrowed("23514"))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: seed construction is ordered once per session; after the seed link
/// exists, its imported semantic prefix cannot grow.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_committed_seed_rejects_late_prefix_inserts() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    let seed = insert_imported_resume_seed_scaffolding(&mut transaction).await?;
    insert_imported_semantic_prefix(&mut transaction, seed).await?;
    insert_exact_seed_members(&mut transaction, seed).await?;
    sqlx::query(
        "INSERT INTO imported_session_seed
            (session_id, seed_context_frontier_id)
         VALUES
            ($1, $2)",
    )
    .bind(seed.session)
    .bind(seed.seed_frontier)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let semantic_error = sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             imported_conversation_id, imported_transcript_entry_id)
         VALUES
            ($1, $2, 'imported_entry', $3, $4)",
    )
    .bind(seed.session)
    .bind(Uuid::from_u128(0x6000_0000_0000_4000_8000_0000_0000_0041))
    .bind(seed.conversation)
    .bind(seed.imported_prefix[0])
    .execute(&pool)
    .await
    .expect_err("a committed imported semantic prefix is sealed");
    assert_eq!(
        semantic_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("imported_semantic_entry_seed_is_sealed")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

struct ImportRoundTripFixture {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    database_url: String,
    winner: ImportedConversationId,
    inserted: ImportConversationOutcome,
    replayed: ImportConversationOutcome,
    stored: ImportedConversation,
}

impl ImportRoundTripFixture {
    async fn finish(self) {
        self.pool.close().await;
        drop(self.container);
    }
}

async fn import_round_trip_fixture() -> Result<ImportRoundTripFixture, Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let source_result_kind = "future-result-kind";
    let source = concat!(
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",",
        "\"content\":[{\"type\":\"<source-result-kind>\",\"payload\":{\"exact\":1}}]}]}}\r\n",
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",",
        "\"content\":[{\"type\":\"<source-result-kind>\",\"payload\":{\"exact\":1}}]}]}}"
    )
    .replace("<source-result-kind>", source_result_kind);
    let winner = ImportedConversationId::from_uuid(Uuid::from_u128(0x100));
    let repository = ImportedConversationRepository::new(pool.clone());
    let mut service = ImportConversationService::new(
        FixedIds::new(&[0x100, 0x200], 0x300..0x304),
        ClaudeCodeJsonlConverter,
        repository,
    );

    let inserted = service.execute(source.as_bytes()).await?;
    let replayed = service.execute(source.as_bytes()).await?;
    let (_, _, repository) = service.into_parts();
    let stored = repository
        .load(winner)
        .await?
        .expect("inserted imported conversation must load");

    Ok(ImportRoundTripFixture {
        container,
        pool,
        database_url,
        winner,
        inserted,
        replayed,
        stored,
    })
}

/// S28: exact reingestion resolves the immutable imported winner.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_exact_reingestion_resolves_the_immutable_winner() -> Result<(), Box<dyn Error>> {
    let fixture = import_round_trip_fixture().await?;

    assert_eq!(
        fixture.inserted,
        ImportConversationOutcome::Inserted {
            conversation: fixture.winner
        }
    );
    assert_eq!(
        fixture.replayed,
        ImportConversationOutcome::AlreadyImported {
            conversation: fixture.winner
        }
    );

    fixture.finish().await;
    Ok(())
}

/// S28: imported raw bytes deduplicate by content identity while
/// every ordered occurrence and semantic frontier reconstitutes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_imported_raw_blobs_deduplicate_and_reconstitute() -> Result<(), Box<dyn Error>> {
    let fixture = import_round_trip_fixture().await?;
    let source_result_kind = "future-result-kind";

    let stored = &fixture.stored;
    assert_eq!(stored.raw_records().len(), 2);
    assert_eq!(stored.entries().len(), 2);
    assert_eq!(stored.frontiers().count(), 2);
    assert_attested_source_result_kind(stored.entries()[0].content(), source_result_kind);
    assert_eq!(
        stored.raw_records()[0].bytes(),
        stored.raw_records()[1].bytes()
    );

    let counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM imported_raw_source_record),
            (SELECT count(*) FROM imported_conversation),
            (SELECT count(*) FROM imported_conversation_raw_record),
            (SELECT count(*) FROM imported_transcript_entry)",
    )
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(counts, (1, 1, 2, 2));

    fixture.finish().await;
    Ok(())
}

/// the final imported raw-record and entry relations remain
/// append-only after blob convergence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn converged_import_relations_remain_append_only() -> Result<(), Box<dyn Error>> {
    let fixture = import_round_trip_fixture().await?;

    assert!(
        sqlx::query(
            "UPDATE imported_raw_source_record
                SET content_hash = content_hash",
        )
        .execute(&fixture.pool)
        .await
        .is_err(),
        "raw source records must reject updates"
    );
    assert!(
        sqlx::query("TRUNCATE TABLE imported_transcript_entry")
            .execute(&fixture.pool)
            .await
            .is_err(),
        "imported entries must reject statement-level truncate"
    );

    fixture.finish().await;
    Ok(())
}

/// S28: restart loading reconstructs the exact imported aggregate
/// from catalogued raw blobs.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_imported_blob_round_trip_survives_pool_restart() -> Result<(), Box<dyn Error>> {
    let fixture = import_round_trip_fixture().await?;

    fixture.pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(local_test_connection_options(&fixture.database_url)?)
        .await?;
    let restarted = ImportedConversationRepository::new(restarted_pool.clone())
        .load(fixture.winner)
        .await?
        .expect("durable imported conversation must survive pool restart");
    assert_eq!(restarted, fixture.stored);

    restarted_pool.close().await;
    drop(fixture.container);
    Ok(())
}

/// S28: appending Claude Code records creates a distinct exact
/// snapshot while shared raw records remain content-addressed once and source
/// session evidence groups both snapshots without identifying them.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_grown_claude_source_is_new_snapshot_with_shared_lineage() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let leading_record = concat!(
        "{\"sessionId\":\"claude-lineage\",\"uuid\":\"record-1\",",
        "\"type\":\"user\",\"message\":{\"content\":\"first\"}}"
    );
    let appended_record = concat!(
        "{\"sessionId\":\"claude-lineage\",\"uuid\":\"record-2\",",
        "\"type\":\"assistant\",\"message\":{\"content\":\"second\"}}"
    );
    let grown_source = format!("{leading_record}\n{appended_record}");
    let first_snapshot = ImportedConversationId::from_uuid(Uuid::from_u128(0x1100));
    let grown_snapshot = ImportedConversationId::from_uuid(Uuid::from_u128(0x1200));
    let mut service = ImportConversationService::new(
        FixedIds::for_conversations([first_snapshot, grown_snapshot]),
        ClaudeCodeJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );

    assert_eq!(
        service.execute(leading_record.as_bytes()).await?,
        ImportConversationOutcome::Inserted {
            conversation: first_snapshot
        }
    );
    assert_eq!(
        service.execute(grown_source.as_bytes()).await?,
        ImportConversationOutcome::Inserted {
            conversation: grown_snapshot
        }
    );
    assert_ne!(first_snapshot, grown_snapshot);

    let (_, _, repository) = service.into_parts();
    let first = repository
        .load(first_snapshot)
        .await?
        .expect("the leading Claude snapshot must reconstitute");
    let grown = repository
        .load(grown_snapshot)
        .await?
        .expect("the grown Claude snapshot must reconstitute");
    assert_eq!(first.raw_records().len(), 1);
    assert_eq!(first.entries().len(), 1);
    assert_eq!(first.raw_records()[0].bytes(), leading_record.as_bytes());
    assert_eq!(grown.raw_records().len(), 2);
    assert_eq!(grown.entries().len(), 2);
    assert_eq!(grown.raw_records()[0], first.raw_records()[0]);
    assert_eq!(grown.raw_records()[1].bytes(), appended_record.as_bytes());
    assert_ne!(first.source_digest(), grown.source_digest());

    let storage_counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM imported_raw_source_record),
            (SELECT count(*) FROM imported_conversation),
            (SELECT count(*) FROM imported_conversation_raw_record)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(storage_counts, (2, 2, 3));
    let lineage: Vec<Uuid> = sqlx::query_scalar(
        "SELECT imported_conversation_id
           FROM imported_conversation
          WHERE source_session_id = $1
          ORDER BY imported_conversation_id",
    )
    .bind(b"claude-lineage".as_slice())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        lineage,
        vec![first_snapshot.into_uuid(), grown_snapshot.into_uuid()]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: appending Codex records creates a distinct exact snapshot
/// while shared raw records remain content-addressed once and source session
/// evidence groups both snapshots without identifying them.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_grown_codex_source_is_new_snapshot_with_shared_lineage() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let leading_record = concat!(
        "{\"timestamp\":\"t0\",\"type\":\"response_item\",\"payload\":",
        "{\"id\":\"item-1\",\"session_id\":\"codex-lineage\",",
        "\"type\":\"message\",\"role\":\"user\",\"content\":\"first\"}}"
    );
    let appended_record = concat!(
        "{\"timestamp\":\"t1\",\"type\":\"response_item\",\"payload\":",
        "{\"id\":\"item-2\",\"session_id\":\"codex-lineage\",",
        "\"type\":\"message\",\"role\":\"assistant\",\"content\":\"second\"}}"
    );
    let grown_source = format!("{leading_record}\n{appended_record}");
    let first_snapshot = ImportedConversationId::from_uuid(Uuid::from_u128(0x2100));
    let grown_snapshot = ImportedConversationId::from_uuid(Uuid::from_u128(0x2200));
    let mut service = ImportConversationService::new(
        FixedIds::for_conversations([first_snapshot, grown_snapshot]),
        CodexRolloutJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );

    assert_eq!(
        service.execute(leading_record.as_bytes()).await?,
        ImportConversationOutcome::Inserted {
            conversation: first_snapshot
        }
    );
    assert_eq!(
        service.execute(grown_source.as_bytes()).await?,
        ImportConversationOutcome::Inserted {
            conversation: grown_snapshot
        }
    );
    assert_ne!(first_snapshot, grown_snapshot);

    let (_, _, repository) = service.into_parts();
    let first = repository
        .load(first_snapshot)
        .await?
        .expect("the leading Codex snapshot must reconstitute");
    let grown = repository
        .load(grown_snapshot)
        .await?
        .expect("the grown Codex snapshot must reconstitute");
    assert_eq!(first.raw_records().len(), 1);
    assert_eq!(first.entries().len(), 1);
    assert_eq!(first.raw_records()[0].bytes(), leading_record.as_bytes());
    assert_eq!(grown.raw_records().len(), 2);
    assert_eq!(grown.entries().len(), 2);
    assert_eq!(grown.raw_records()[0], first.raw_records()[0]);
    assert_eq!(grown.raw_records()[1].bytes(), appended_record.as_bytes());
    assert_ne!(first.source_digest(), grown.source_digest());

    let storage_counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM imported_raw_source_record),
            (SELECT count(*) FROM imported_conversation),
            (SELECT count(*) FROM imported_conversation_raw_record)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(storage_counts, (2, 2, 3));
    let lineage: Vec<Uuid> = sqlx::query_scalar(
        "SELECT imported_conversation_id
           FROM imported_conversation
          WHERE source_session_id = $1
          ORDER BY imported_conversation_id",
    )
    .bind(b"codex-lineage".as_slice())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        lineage,
        vec![first_snapshot.into_uuid(), grown_snapshot.into_uuid()]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: source-session lineage remains unknown when no record attests an
/// identifier or when records attest conflicting identifiers.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_source_session_lineage_is_null_without_one_consistent_attestation()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let missing_source = "{\"type\":\"summary\",\"value\":\"missing\"}";
    let conflicting_source = concat!(
        "{\"sessionId\":\"lineage-a\",\"type\":\"summary\",\"value\":\"first\"}\n",
        "{\"sessionId\":\"lineage-b\",\"type\":\"summary\",\"value\":\"second\"}"
    );
    let missing_snapshot = ImportedConversationId::from_uuid(Uuid::from_u128(0x3100));
    let conflicting_snapshot = ImportedConversationId::from_uuid(Uuid::from_u128(0x3200));
    let mut service = ImportConversationService::new(
        FixedIds::for_conversations([missing_snapshot, conflicting_snapshot]),
        ClaudeCodeJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );

    assert_eq!(
        service.execute(missing_source.as_bytes()).await?,
        ImportConversationOutcome::Inserted {
            conversation: missing_snapshot
        }
    );
    assert_eq!(
        service.execute(conflicting_source.as_bytes()).await?,
        ImportConversationOutcome::Inserted {
            conversation: conflicting_snapshot
        }
    );
    let evidence: Vec<(Uuid, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT imported_conversation_id, source_session_id
           FROM imported_conversation
          ORDER BY imported_conversation_id",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        evidence,
        vec![
            (missing_snapshot.into_uuid(), None),
            (conflicting_snapshot.into_uuid(), None),
        ]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: checked loading and exact reingestion reject
/// non-null lineage evidence that disagrees with the reconstructed entries.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_corrupt_source_session_lineage_fails_closed() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let source = concat!(
        "{\"sessionId\":\"lineage-original\",\"uuid\":\"record-1\",",
        "\"type\":\"user\",\"message\":{\"content\":\"first\"}}"
    );
    let winner = ImportedConversationId::from_uuid(Uuid::from_u128(0x3300));
    let repository = ImportedConversationRepository::new(pool.clone());
    let mut initial_import = ImportConversationService::new(
        FixedIds::for_conversations([winner]),
        ClaudeCodeJsonlConverter,
        repository.clone(),
    );
    assert_eq!(
        initial_import.execute(source.as_bytes()).await?,
        ImportConversationOutcome::Inserted {
            conversation: winner
        }
    );

    sqlx::query("ALTER TABLE imported_conversation DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE imported_conversation
            SET source_session_id = $1
          WHERE imported_conversation_id = $2",
    )
    .bind(b"lineage-corrupt".as_slice())
    .bind(winner.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE imported_conversation ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    assert!(matches!(
        repository
            .load(winner)
            .await
            .expect_err("corrupt lineage evidence must not load"),
        ImportedConversationRepositoryError::Corruption(
            ImportedConversationCorruption::SourceSessionLineageMismatch
        )
    ));

    let mut exact_reingestion = ImportConversationService::new(
        FixedIds::new(&[0x3400], [0x3410]),
        ClaudeCodeJsonlConverter,
        repository.clone(),
    );
    assert!(matches!(
        exact_reingestion
            .execute(source.as_bytes())
            .await
            .expect_err("exact reingestion must expose corrupt lineage evidence"),
        ImportConversationError::Store(ImportedConversationRepositoryError::Corruption(
            ImportedConversationCorruption::SourceSessionLineageMismatch
        ))
    ));

    sqlx::query("ALTER TABLE imported_conversation DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE imported_conversation
            SET source_session_id = NULL
          WHERE imported_conversation_id = $1",
    )
    .bind(winner.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE imported_conversation ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(
        repository.load(winner).await?.is_some(),
        "NULL lineage must remain unknown for rows predating the column"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: Codex rollout entries use the same append-only,
/// content-addressed persistence boundary as every imported conversation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_codex_rollout_round_trip_is_idempotent_and_restart_safe() -> Result<(), Box<dyn Error>>
{
    let (container, pool, database_url) = migrated_postgres().await?;
    let source = concat!(
        "{\"timestamp\":\"t0\",\"type\":\"response_item\",\"payload\":",
        "{\"type\":\"message\",\"role\":\"user\",\"content\":",
        "[{\"type\":\"input_text\",\"text\":\"question\"}]}}\n",
        "{\"timestamp\":\"t1\",\"type\":\"response_item\",\"payload\":",
        "{\"type\":\"function_call\",\"call_id\":\"call-1\",",
        "\"name\":\"lookup\",\"arguments\":\"{\\\"key\\\":\\\"value\\\"}\"}}\n",
        "{\"timestamp\":\"t2\",\"type\":\"response_item\",\"payload\":",
        "{\"type\":\"function_call_output\",\"call_id\":\"call-1\",",
        "\"output\":\"result\"}}"
    );
    let winner = ImportedConversationId::from_uuid(Uuid::from_u128(0x8100));
    let repository = ImportedConversationRepository::new(pool.clone());
    let mut service = ImportConversationService::new(
        FixedIds::new(&[0x8100, 0x8200], 0x8300..0x8306),
        CodexRolloutJsonlConverter,
        repository,
    );

    assert_eq!(
        service.execute(source.as_bytes()).await?,
        ImportConversationOutcome::Inserted {
            conversation: winner
        }
    );
    assert_eq!(
        service.execute(source.as_bytes()).await?,
        ImportConversationOutcome::AlreadyImported {
            conversation: winner
        }
    );
    let (_, _, repository) = service.into_parts();
    let stored = repository
        .load(winner)
        .await?
        .expect("inserted Codex rollout must load");
    assert_eq!(
        stored.format(),
        ImportedConversationFormat::CodexRolloutJsonlV1
    );
    assert_eq!(stored.raw_records().len(), 3);
    assert_eq!(stored.entries().len(), 3);
    assert_eq!(stored.frontiers().count(), 3);

    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let restarted = ImportedConversationRepository::new(restarted_pool.clone())
        .load(winner)
        .await?
        .expect("durable Codex rollout must survive pool restart");
    assert_eq!(restarted, stored);

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S28: equal source bytes cannot resolve as replay when a drifting
/// converter supplies a different normalized record and semantic projection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_reingestion_rejects_converter_projection_drift() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let source = br#"{"type":"user","message":{"content":"original"}}"#;
    let winner = ImportedConversationId::from_uuid(Uuid::from_u128(0x100));
    let repository = ImportedConversationRepository::new(pool.clone());
    let mut service = ImportConversationService::new(
        FixedIds::new(&[0x100], [0x200]),
        ClaudeCodeJsonlConverter,
        repository,
    );
    assert_eq!(
        service.execute(source).await?,
        ImportConversationOutcome::Inserted {
            conversation: winner
        }
    );
    let (_, _, repository) = service.into_parts();

    let candidate = ImportedConversationId::from_uuid(Uuid::from_u128(0x300));
    let text = |value: &str| ImportedText::new(String::from(value));
    let member = |name: &str, value| {
        ImportedStructuredObjectMember::new(ImportedText::new(String::from(name)), value)
    };
    let normalized = ImportedStructuredValue::Object(
        vec![
            member("type", ImportedStructuredValue::String(text("user"))),
            member(
                "message",
                ImportedStructuredValue::Object(
                    vec![member(
                        "content",
                        ImportedStructuredValue::String(text("drifted")),
                    )]
                    .into_boxed_slice(),
                ),
            ),
        ]
        .into_boxed_slice(),
    );
    let raw = ImportedRawSourceRecord::from_converted(source.to_vec(), normalized);
    let metadata = ImportedSourceMetadata::new(
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
    );
    let projected = ImportedTranscriptEntryInput::new(
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x400)),
        candidate,
        ImportedTranscriptPosition::first(),
        ImportedRawRecordPosition::first(),
        ImportedRecordEntryPosition::first(),
        ImportedSourceAttestation::Attested(ImportedSpeaker::User),
        ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text("drifted"))),
        metadata,
    );
    let drifted = ImportedConversation::from_converted_records(
        candidate,
        ImportedConversationFormat::ClaudeCodeSessionJsonlV2,
        vec![raw],
        vec![projected],
    )
    .expect("the drifting projection is internally coherent");

    let error = repository
        .resolve_or_insert(drifted)
        .await
        .expect_err("the same source digest cannot replay with new semantics");
    assert!(matches!(
        error,
        ImportedConversationRepositoryError::Corruption(
            ImportedConversationCorruption::ExistingSnapshotMismatch
        )
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: exact reingestion checks an existing snapshot
/// before the new-digest blob path and cannot conceal durable raw corruption.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_reingestion_does_not_mask_raw_corruption() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let source = br#"{"type":"summary","summary":"corruption-only"}"#;
    let winner = ImportedConversationId::from_uuid(Uuid::from_u128(0x750));
    let repository = ImportedConversationRepository::new(pool.clone());
    let mut service = ImportConversationService::new(
        FixedIds::new(&[0x750, 0x760], [0x751, 0x761]),
        ClaudeCodeJsonlConverter,
        repository,
    );
    assert_eq!(
        service.execute(source).await?,
        ImportConversationOutcome::Inserted {
            conversation: winner
        }
    );

    let digest: Vec<u8> = sqlx::query_scalar("SELECT content_hash FROM imported_raw_source_record")
        .fetch_one(&pool)
        .await?;
    let digest = BlobDigest::from_bytes(digest.try_into().map_err(|_| "fixture digest size")?);
    corrupt_integration_imported_blob(digest, Arc::from(b"corrupt".as_slice()))?;

    let error = service
        .execute(source)
        .await
        .expect_err("reingestion must expose existing raw corruption");
    corrupt_integration_imported_blob(digest, Arc::from(source.as_slice()))?;
    assert!(matches!(
        error,
        ImportConversationError::Store(ImportedConversationRepositoryError::BlobStorage(_))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: imports sharing raw blobs acquire their global content keys
/// in one stable order even when the source occurrences are reversed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_concurrent_reversed_raws_use_stable_blob_order() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let forward_source = concat!(
        "{\"type\":\"summary\",\"value\":\"first\"}\n",
        "{\"type\":\"summary\",\"value\":\"second\"}"
    );
    let reverse_source = concat!(
        "{\"type\":\"summary\",\"value\":\"second\"}\n",
        "{\"type\":\"summary\",\"value\":\"first\"}"
    );
    let forward_id = ImportedConversationId::from_uuid(Uuid::from_u128(0x800));
    let reverse_id = ImportedConversationId::from_uuid(Uuid::from_u128(0x900));
    let mut forward = ImportConversationService::new(
        FixedIds::new(&[0x800], [0x801, 0x802]),
        ClaudeCodeJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );
    let mut reverse = ImportConversationService::new(
        FixedIds::new(&[0x900], [0x901, 0x902]),
        ClaudeCodeJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );

    let (forward_result, reverse_result) = tokio::join!(
        forward.execute(forward_source.as_bytes()),
        reverse.execute(reverse_source.as_bytes())
    );
    assert_eq!(
        forward_result?,
        ImportConversationOutcome::Inserted {
            conversation: forward_id
        }
    );
    assert_eq!(
        reverse_result?,
        ImportConversationOutcome::Inserted {
            conversation: reverse_id
        }
    );
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM imported_raw_source_record),
            (SELECT count(*) FROM imported_conversation)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (2, 2));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: overlapping imported-entry identity keys are
/// acquired in one stable order even when transcript positions reverse them.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_concurrent_reversed_entry_ids_return_typed_collision() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let forward_source = concat!(
        "{\"type\":\"summary\",\"value\":\"forward-first\"}\n",
        "{\"type\":\"summary\",\"value\":\"forward-second\"}"
    );
    let reverse_source = concat!(
        "{\"type\":\"summary\",\"value\":\"reverse-first\"}\n",
        "{\"type\":\"summary\",\"value\":\"reverse-second\"}"
    );
    let mut forward = ImportConversationService::new(
        FixedIds::new(&[0xb00], [0xc00, 0xc01]),
        ClaudeCodeJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );
    let mut reverse = ImportConversationService::new(
        FixedIds::new(&[0xb10], [0xc01, 0xc00]),
        ClaudeCodeJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );

    let (forward_result, reverse_result) = tokio::join!(
        forward.execute(forward_source.as_bytes()),
        reverse.execute(reverse_source.as_bytes())
    );
    let forward_inserted = matches!(
        &forward_result,
        Ok(ImportConversationOutcome::Inserted { .. })
    );
    let reverse_inserted = matches!(
        &reverse_result,
        Ok(ImportConversationOutcome::Inserted { .. })
    );
    let forward_collision = matches!(
        &forward_result,
        Err(ImportConversationError::Store(
            ImportedConversationRepositoryError::IdentityCollision(
                ImportedConversationIdentityCollision::TranscriptEntry
            )
        ))
    );
    let reverse_collision = matches!(
        &reverse_result,
        Err(ImportConversationError::Store(
            ImportedConversationRepositoryError::IdentityCollision(
                ImportedConversationIdentityCollision::TranscriptEntry
            )
        ))
    );
    assert!(
        (forward_inserted && reverse_collision) || (reverse_inserted && forward_collision),
        "one transaction must insert and the other must return a typed collision"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// the late unique-constraint path reached after a concurrent
/// precheck race retains the repository's typed imported-entry collision.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn late_entry_identity_constraint_is_typed_collision() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let existing_entry = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0xa01));
    let mut service = ImportConversationService::new(
        FixedIds::new(&[0xa00], [0xa01]),
        ClaudeCodeJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );
    service
        .execute(br#"{"type":"summary","value":null}"#)
        .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO imported_conversation
            (imported_conversation_id, storage_version, source_format,
             converter_version, source_digest, declared_raw_record_count,
             declared_entry_count, display_title, display_title_state)
         VALUES ($1, 1, 'claude_code_session_jsonl', 1, $2, 1, 1,
                 NULL, 'underivable')",
    )
    .bind(Uuid::from_u128(0xa10))
    .bind(vec![0x10_u8; 32])
    .execute(&mut *transaction)
    .await?;
    insert_catalogued_raw_source(&mut transaction, vec![0x11_u8; 32], 1).await?;
    sqlx::query(
        "INSERT INTO imported_conversation_raw_record
            (imported_conversation_id, raw_record_position, content_hash,
             conversion_digest, normalized_value_encoding,
             declared_entry_count)
         VALUES ($1, 1, $2, $3, $4, 1)",
    )
    .bind(Uuid::from_u128(0xa10))
    .bind(vec![0x11_u8; 32])
    .bind(vec![0x12_u8; 32])
    .bind(vec![0x13_u8])
    .execute(&mut *transaction)
    .await?;
    let database_error = sqlx::query(
        "INSERT INTO imported_transcript_entry
            (imported_conversation_id, imported_entry_position,
             imported_transcript_entry_id, raw_record_position,
             record_entry_position, source_speaker_kind, content_encoding,
             source_metadata_encoding)
         VALUES ($1, 1, $2, 1, 1, 'not_attested', $3, $4)",
    )
    .bind(Uuid::from_u128(0xa10))
    .bind(existing_entry.into_uuid())
    .bind(vec![2_u8, 1, 1, 0])
    .bind(vec![1_u8])
    .execute(&mut *transaction)
    .await
    .expect_err("duplicate imported-entry identity must violate its unique constraint");
    assert_eq!(
        database_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("imported_transcript_entry_identity_unique")
    );
    let error = ImportedConversationRepositoryError::from(database_error);
    assert!(matches!(
        error,
        ImportedConversationRepositoryError::IdentityCollision(
            ImportedConversationIdentityCollision::TranscriptEntry
        )
    ));
    transaction.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(())
}

/// a header cannot commit without its exact declared contiguous raw
/// and normalized-entry membership.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn incomplete_import_header_cannot_commit() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO imported_conversation
            (imported_conversation_id, storage_version, source_format,
             converter_version, source_digest, declared_raw_record_count,
             declared_entry_count, display_title, display_title_state)
         VALUES ($1, 1, 'claude_code_session_jsonl', 1, $2, 1, 1,
                 NULL, 'underivable')",
    )
    .bind(Uuid::from_u128(0x400))
    .bind(vec![0_u8; 32])
    .execute(&mut *transaction)
    .await?;
    assert!(
        transaction.commit().await.is_err(),
        "deferred complete-membership constraint must reject a partial aggregate"
    );
    let headers: i64 = sqlx::query_scalar("SELECT count(*) FROM imported_conversation")
        .fetch_one(&pool)
        .await?;
    assert_eq!(headers, 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28: a newly inserted content-addressed raw blob cannot commit
/// without at least one conversation-owned occurrence.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_unowned_raw_source_record_cannot_commit() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    insert_catalogued_raw_source(&mut transaction, vec![0x41_u8; 32], 1).await?;

    assert!(
        transaction.commit().await.is_err(),
        "deferred ownership constraint must reject an unowned raw blob"
    );
    let raw_blobs: i64 = sqlx::query_scalar("SELECT count(*) FROM imported_raw_source_record")
        .fetch_one(&pool)
        .await?;
    assert_eq!(raw_blobs, 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// catalogued raw records are nonempty at the schema boundary.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn empty_raw_record_is_schema_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let error = sqlx::query(
        "INSERT INTO blob (digest, byte_length)
         VALUES ($1, $2)",
    )
    .bind(vec![0_u8; 32])
    .bind(0_i64)
    .execute(&pool)
    .await
    .expect_err("empty raw source records must violate the schema");
    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("blob_byte_length_positive_u64")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// the schema admits only implemented format/version pairs
/// and rejects every unimplemented combination before storing a header.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unsupported_format_version_pair_is_schema_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let error = sqlx::query(
        "INSERT INTO imported_conversation
            (imported_conversation_id, storage_version, source_format,
             converter_version, source_digest, declared_raw_record_count,
             declared_entry_count, display_title, display_title_state)
         VALUES ($1, 1, 'claude_code_session_jsonl', 3, $2, 1, 1,
                 NULL, 'underivable')",
    )
    .bind(Uuid::from_u128(0x4ff))
    .bind(vec![0_u8; 32])
    .execute(&pool)
    .await
    .expect_err("an unimplemented converter version must violate the schema");
    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("imported_conversation_converter_version_supported")
    );
    let pair_error = sqlx::query(
        "INSERT INTO imported_conversation
            (imported_conversation_id, storage_version, source_format,
             converter_version, source_digest, declared_raw_record_count,
             declared_entry_count, display_title, display_title_state)
         VALUES ($1, 1, 'codex_rollout_jsonl', 2, $2, 1, 1,
                 NULL, 'underivable')",
    )
    .bind(Uuid::from_u128(0x4fe))
    .bind(vec![1_u8; 32])
    .execute(&pool)
    .await
    .expect_err("an unimplemented format/version pair must violate the schema");
    assert_eq!(
        pair_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("imported_conversation_format_version_supported")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// adapter and domain reconstruction fail closed when
/// durable declared counts are corrupted behind append-only guards.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn corrupt_import_fails_typed_load() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let winner = ImportedConversationId::from_uuid(Uuid::from_u128(0x500));
    let repository = ImportedConversationRepository::new(pool.clone());
    let mut service = ImportConversationService::new(
        FixedIds::new(&[0x500], [0x501]),
        ClaudeCodeJsonlConverter,
        repository.clone(),
    );
    service
        .execute(br#"{"type":"summary","value":null}"#)
        .await?;

    sqlx::query("ALTER TABLE imported_conversation DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE imported_conversation
            SET declared_entry_count = $1
          WHERE imported_conversation_id = $2",
    )
    .bind(Decimal::from(2_u64))
    .bind(winner.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE imported_conversation ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    let error = repository
        .load(winner)
        .await
        .expect_err("corrupt imported conversation must not load");
    assert!(matches!(
        error,
        ImportedConversationRepositoryError::Corruption(ImportedConversationCorruption::Domain(
            ImportedConversationReconstitutionFailure::DeclaredEntryCountMismatch { .. }
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// normalized storage cannot be replaced independently from
/// the exact raw record and its conversion authentication.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn corrupt_normalized_record_fails_typed_load() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let winner = ImportedConversationId::from_uuid(Uuid::from_u128(0x520));
    let donor = ImportedConversationId::from_uuid(Uuid::from_u128(0x530));
    let repository = ImportedConversationRepository::new(pool.clone());
    let mut service = ImportConversationService::new(
        FixedIds::new(&[0x520, 0x530], [0x521, 0x531]),
        ClaudeCodeJsonlConverter,
        repository.clone(),
    );
    service
        .execute(br#"{"type":"summary","value":"original"}"#)
        .await?;
    service
        .execute(br#"{"type":"summary","value":"changed"}"#)
        .await?;

    sqlx::query("ALTER TABLE imported_conversation_raw_record DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE imported_conversation_raw_record AS target
            SET normalized_value_encoding = donor.normalized_value_encoding
           FROM imported_conversation_raw_record AS donor
          WHERE target.imported_conversation_id = $1
            AND target.raw_record_position = 1
            AND donor.imported_conversation_id = $2
            AND donor.raw_record_position = 1",
    )
    .bind(winner.into_uuid())
    .bind(donor.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE imported_conversation_raw_record ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    let error = repository
        .load(winner)
        .await
        .expect_err("normalized record contradicting its raw conversion must not load");
    assert!(matches!(
        error,
        ImportedConversationRepositoryError::Corruption(ImportedConversationCorruption::Domain(
            ImportedConversationReconstitutionFailure::RawRecordConversionDigestMismatch { .. }
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// each raw occurrence's declared normalized-entry count is
/// checked against the complete reconstructed membership.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn corrupt_raw_entry_count_fails_typed_load() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let winner = ImportedConversationId::from_uuid(Uuid::from_u128(0x550));
    let repository = ImportedConversationRepository::new(pool.clone());
    let mut service = ImportConversationService::new(
        FixedIds::new(&[0x550], [0x551]),
        ClaudeCodeJsonlConverter,
        repository.clone(),
    );
    service
        .execute(br#"{"type":"summary","value":null}"#)
        .await?;

    sqlx::query("ALTER TABLE imported_conversation_raw_record DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE imported_conversation_raw_record
            SET declared_entry_count = $1
          WHERE imported_conversation_id = $2
            AND raw_record_position = 1",
    )
    .bind(Decimal::from(2_u64))
    .bind(winner.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE imported_conversation_raw_record ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    let error = repository
        .load(winner)
        .await
        .expect_err("corrupt raw-record entry count must not load");
    assert!(matches!(
        error,
        ImportedConversationRepositoryError::Corruption(
            ImportedConversationCorruption::RawRecordDeclaredEntryCountMismatch {
                declared: 2,
                actual: 1,
                ..
            }
        )
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// Local-only validation of conversion, raw hash round-trip, frontier
/// addressing, Postgres reconstitution, and second-import idempotency. The test
/// deliberately emits no paths, content, identities, raw bytes, or parser data.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires explicit local real-transcript and PostgreSQL opt-in"]
async fn opt_in_real_transcript_postgres_round_trip() -> Result<(), Box<dyn Error>> {
    validate_opt_in_real_transcript_postgres_round_trip().await
}

/// Local-only Codex validation with the same content-silent contract as the
/// Claude Code real-transcript test above.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires explicit local real-rollout and PostgreSQL opt-in"]
async fn opt_in_real_codex_rollout_postgres_round_trip() -> Result<(), Box<dyn Error>> {
    validate_opt_in_real_codex_rollout_postgres_round_trip().await
}

async fn validate_opt_in_real_transcript_postgres_round_trip() -> Result<(), Box<dyn Error>> {
    if env::var("SIGNALBOX_RUN_REAL_CLAUDE_IMPORT").as_deref() != Ok("1") {
        return Ok(());
    }
    let Some(root) = env::var_os("SIGNALBOX_REAL_CLAUDE_TRANSCRIPTS") else {
        return Err("real transcript inputs were not configured".into());
    };
    let mut paths = Vec::new();
    for root in env::split_paths(&root) {
        collect_transcripts(&root, &mut paths).map_err(|()| "real inputs unavailable")?;
    }
    paths.sort();
    if paths.is_empty() {
        return Err("real transcript directory contained no JSONL files".into());
    }
    let (container, pool, _database_url) = migrated_postgres().await?;
    for (file_index, path) in paths.into_iter().enumerate() {
        let source = fs::read(path).map_err(|_| "real input unavailable")?;
        validate_real_source(&pool, &source, file_index, ClaudeCodeJsonlConverter).await?;
    }

    pool.close().await;
    drop(container);
    Ok(())
}

async fn validate_opt_in_real_codex_rollout_postgres_round_trip() -> Result<(), Box<dyn Error>> {
    if env::var("SIGNALBOX_RUN_REAL_CODEX_IMPORT").as_deref() != Ok("1") {
        return Ok(());
    }
    let Some(root) = env::var_os("SIGNALBOX_REAL_CODEX_ROLLOUTS") else {
        return Err("real rollout inputs were not configured".into());
    };
    let mut paths = Vec::new();
    for root in env::split_paths(&root) {
        collect_transcripts(&root, &mut paths).map_err(|()| "real inputs unavailable")?;
    }
    paths.sort();
    if paths.is_empty() {
        return Err("real rollout directory contained no JSONL files".into());
    }
    let (container, pool, _database_url) = migrated_postgres().await?;
    for (file_index, path) in paths.into_iter().enumerate() {
        let source = fs::read(path).map_err(|_| "real input unavailable")?;
        validate_real_source(&pool, &source, file_index, CodexRolloutJsonlConverter).await?;
    }

    pool.close().await;
    drop(container);
    Ok(())
}

async fn validate_real_source<Converter>(
    pool: &PgPool,
    source: &[u8],
    file_index: usize,
    converter: Converter,
) -> Result<(), Box<dyn Error>>
where
    Converter: ImportedConversationConverter,
{
    let ordinal = u128::try_from(file_index)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or("too many real transcript inputs")?;
    let first_candidate = ordinal
        .checked_mul(2)
        .and_then(|value| value.checked_add(0x600))
        .ok_or("too many real transcript inputs")?;
    let second_candidate = first_candidate
        .checked_add(1)
        .ok_or("too many real transcript inputs")?;
    let first_entry = ordinal
        .checked_mul(1_u128 << 64)
        .ok_or("too many real transcript inputs")?;
    let repository = ImportedConversationRepository::new(pool.clone());
    let mut service = ImportConversationService::new(
        SequentialIds::new([first_candidate, second_candidate], first_entry),
        converter,
        repository,
    );
    let winner = match service
        .execute(source)
        .await
        .map_err(|_| "real source first import failed")?
    {
        ImportConversationOutcome::Inserted { conversation }
        | ImportConversationOutcome::AlreadyImported { conversation } => conversation,
    };
    match service
        .execute(source)
        .await
        .map_err(|_| "real source repeat import failed")?
    {
        ImportConversationOutcome::AlreadyImported { conversation } if conversation == winner => {}
        ImportConversationOutcome::AlreadyImported { .. } => {
            return Err("real source reimport resolved a different identity".into());
        }
        ImportConversationOutcome::Inserted { .. } => {
            return Err("real source reimport was not idempotent".into());
        }
    }
    let (_, _, repository) = service.into_parts();
    let stored = repository
        .load(winner)
        .await
        .map_err(|_| "real imported conversation could not be loaded")?
        .ok_or("real imported conversation disappeared")?;
    assert_eq!(stored.frontiers().count(), stored.entries().len());
    assert!(
        stored.raw_records().iter().all(|record| {
            record.content_hash() == ImportedRawRecordHash::digest(record.bytes())
        })
    );

    Ok(())
}

fn collect_transcripts(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), ()> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ())?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        files.push(path.to_path_buf());
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    for child in fs::read_dir(path).map_err(|_| ())? {
        let child = child.map_err(|_| ())?.path();
        let child_metadata = fs::symlink_metadata(&child).map_err(|_| ())?;
        if child_metadata.is_dir() {
            collect_transcripts(&child, files)?;
        } else if child_metadata.is_file()
            && child.extension().and_then(|value| value.to_str()) == Some("jsonl")
        {
            files.push(child);
        }
    }
    Ok(())
}

fn jsonl_record_bytes(source: &str) -> usize {
    source.lines().map(str::len).sum()
}

/// Issue #995: an exclusive UUID keyset remains bounded and duplicate-free
/// when imports are committed on both sides of its cursor.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn imported_discovery_pages_stay_stable_under_concurrent_additions()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_conversation = ImportedConversationId::from_uuid(Uuid::from_u128(0x100));
    let second_conversation = ImportedConversationId::from_uuid(Uuid::from_u128(0x300));
    let third_conversation = ImportedConversationId::from_uuid(Uuid::from_u128(0x500));
    let behind_cursor = ImportedConversationId::from_uuid(Uuid::from_u128(0x200));
    let ahead_of_cursor = ImportedConversationId::from_uuid(Uuid::from_u128(0x400));
    let page_limit = NonZeroU32::new(2).ok_or("page fixture limit must be nonzero")?;
    let mut importer = ImportConversationService::new(
        FixedIds::for_conversations([
            first_conversation,
            second_conversation,
            third_conversation,
            behind_cursor,
            ahead_of_cursor,
        ]),
        ClaudeCodeJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );
    importer
        .execute(br#"{"type":"user","message":{"content":"first"}}"#)
        .await?;
    importer
        .execute(br#"{"type":"user","message":{"content":"second"}}"#)
        .await?;
    importer
        .execute(br#"{"type":"user","message":{"content":"third"}}"#)
        .await?;
    let discovery = ImportedConversationDiscoveryRepository::new(pool.clone());
    let first_page = discovery
        .list(ImportedConversationPageRequest {
            after: None,
            format: None,
            source_session_id: None,
            source_session_maximum_bytes: NonZeroU32::new(512)
                .ok_or("source-session fixture bound must be nonzero")?,
            limit: page_limit,
        })
        .await?;

    assert_eq!(first_page.items.len(), page_limit.get() as usize);
    assert_eq!(first_page.items[0].conversation, first_conversation);
    assert_eq!(first_page.items[1].conversation, second_conversation);
    assert_eq!(first_page.next_after, Some(second_conversation));

    importer
        .execute(br#"{"type":"user","message":{"content":"behind"}}"#)
        .await?;
    importer
        .execute(br#"{"type":"user","message":{"content":"ahead"}}"#)
        .await?;
    let second_page = discovery
        .list(ImportedConversationPageRequest {
            after: first_page.next_after,
            format: None,
            source_session_id: None,
            source_session_maximum_bytes: NonZeroU32::new(512)
                .ok_or("source-session fixture bound must be nonzero")?,
            limit: page_limit,
        })
        .await?;

    assert_eq!(second_page.items.len(), page_limit.get() as usize);
    assert_eq!(second_page.items[0].conversation, ahead_of_cursor);
    assert_eq!(second_page.items[1].conversation, third_conversation);
    assert_ne!(second_page.items[0].conversation, behind_cursor);
    assert_eq!(second_page.next_after, None);

    pool.close().await;
    drop(container);
    Ok(())
}

/// Issue #995: descriptors project size and source facts while an arbitrary
/// entry read decodes only its requested immutable region.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn imported_discovery_describes_and_windows_without_complete_reconstitution()
-> Result<(), Box<dyn Error>> {
    const FIRST_TEXT: &str = "first imported observation";
    const SECOND_TEXT: &str = "second imported observation";
    const THIRD_TEXT: &str = "third imported observation";
    const SOURCE_SESSION: &str = "discovery-source";
    let (container, pool, _database_url) = migrated_postgres().await?;
    let conversation = ImportedConversationId::from_uuid(Uuid::from_u128(0x900));
    let source = format!(
        "{{\"sessionId\":\"{SOURCE_SESSION}\",\"type\":\"user\",\"message\":{{\"content\":\"{FIRST_TEXT}\"}}}}\n\
         {{\"sessionId\":\"{SOURCE_SESSION}\",\"type\":\"assistant\",\"message\":{{\"content\":\"{SECOND_TEXT}\"}}}}\n\
         {{\"sessionId\":\"{SOURCE_SESSION}\",\"type\":\"user\",\"message\":{{\"content\":\"{THIRD_TEXT}\"}}}}"
    );
    let mut importer = ImportConversationService::new(
        FixedIds::for_conversations([conversation]),
        ClaudeCodeJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );
    importer.execute(source.as_bytes()).await?;
    let discovery = ImportedConversationDiscoveryRepository::new(pool.clone());
    let descriptor = discovery
        .descriptor(
            conversation,
            NonZeroU32::new(512).ok_or("source-session fixture bound must be nonzero")?,
        )
        .await?
        .ok_or("descriptor fixture import must exist")?;

    assert_eq!(descriptor.conversation, conversation);
    assert_eq!(
        descriptor
            .source_session_id
            .as_ref()
            .map(|evidence| evidence.leading_text.as_str()),
        Some(SOURCE_SESSION)
    );
    assert_eq!(descriptor.raw_record_count, 3);
    assert_eq!(descriptor.entry_count, 3);
    assert_eq!(
        descriptor.sizes.raw_source_bytes,
        jsonl_record_bytes(&source) as u64
    );
    assert!(descriptor.sizes.normalized_source_record_bytes > 0);
    assert!(descriptor.sizes.normalized_entry_bytes > 0);
    assert_eq!(descriptor.first.position, 1);
    assert_eq!(descriptor.latest.position, descriptor.entry_count);

    let exact_source_page = discovery
        .list(ImportedConversationPageRequest {
            after: None,
            format: None,
            source_session_id: Some(SOURCE_SESSION.as_bytes().to_vec()),
            source_session_maximum_bytes: NonZeroU32::new(512)
                .ok_or("source-session fixture bound must be nonzero")?,
            limit: NonZeroU32::new(1).ok_or("exact-source fixture limit must be nonzero")?,
        })
        .await?;

    assert_eq!(exact_source_page.items.len(), 1);
    assert_eq!(exact_source_page.items[0].conversation, conversation);
    assert_eq!(
        exact_source_page.items[0]
            .source_session_id
            .as_ref()
            .map(|evidence| evidence.leading_text.as_str()),
        Some(SOURCE_SESSION)
    );
    assert!(exact_source_page.items[0].source_session_digest.is_some());
    assert_eq!(exact_source_page.next_after, None);

    let maximum_items = NonZeroU32::new(3).ok_or("window fixture bound must be nonzero")?;
    let window = discovery
        .entry_window(
            conversation,
            ImportedEntryWindowAnchor::Position(2),
            1,
            1,
            maximum_items,
            NonZeroU32::new(512).ok_or("entry-text fixture bound must be nonzero")?,
        )
        .await?
        .ok_or("window fixture import must exist")?;

    assert_eq!(window.items.len(), maximum_items.get() as usize);
    assert_eq!(window.first_position, descriptor.first.position);
    assert_eq!(window.anchor_position, 2);
    assert_eq!(window.last_position, descriptor.latest.position);
    assert_eq!(window.items[0].frontier, descriptor.first);
    assert_eq!(window.items[2].frontier, descriptor.latest);
    assert!(!window.has_before);
    assert!(!window.has_after);

    pool.close().await;
    drop(container);
    Ok(())
}
