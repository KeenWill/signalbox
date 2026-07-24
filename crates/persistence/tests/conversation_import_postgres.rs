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
    path::{Path, PathBuf},
};

use rust_decimal::Decimal;
use signalbox_application::{
    ImportConversationError, ImportConversationOutcome, ImportConversationService,
    ImportedConversationIdGenerator,
};
use signalbox_conversation_import_claude_code::ClaudeCodeJsonlConverter;
use signalbox_domain::{
    ImportedConversation, ImportedConversationFormat, ImportedConversationId,
    ImportedConversationReconstitutionFailure, ImportedRawRecordHash, ImportedRawRecordPosition,
    ImportedRawSourceRecord, ImportedRecordEntryPosition, ImportedSourceAttestation,
    ImportedSourceMetadata, ImportedSpeaker, ImportedStructuredObjectMember,
    ImportedStructuredValue, ImportedText, ImportedTranscriptContent, ImportedTranscriptEntryId,
    ImportedTranscriptEntryInput, ImportedTranscriptPosition,
};
use signalbox_persistence::{
    conversation_import::{
        ImportedConversationCorruption, ImportedConversationIdentityCollision,
        ImportedConversationRepository, ImportedConversationRepositoryError,
    },
    local_test_connection_options, migrate,
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

struct FixedIds {
    conversations: VecDeque<ImportedConversationId>,
    entries: VecDeque<ImportedTranscriptEntryId>,
}

impl FixedIds {
    fn new(conversations: &[u128], entries: impl IntoIterator<Item = u128>) -> Self {
        Self {
            conversations: conversations
                .iter()
                .copied()
                .map(|value| ImportedConversationId::from_uuid(Uuid::from_u128(value)))
                .collect(),
            entries: entries
                .into_iter()
                .map(|value| ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(value)))
                .collect(),
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
        self.entries
            .pop_front()
            .expect("fixture supplies every imported-entry identity")
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
        .with_fsync_enabled()
        .with_tag(POSTGRES_IMAGE_TAG)
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

async fn insert_imported_source_scaffolding(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
) -> Result<ImportedSeedFacts, sqlx::Error> {
    let facts = imported_seed_facts();
    sqlx::raw_sql(
        "INSERT INTO imported_raw_source_record (content_hash, raw_bytes)
         VALUES (decode(repeat('11', 32), 'hex'), decode('01', 'hex'));
         INSERT INTO imported_conversation
            (imported_conversation_id, storage_version, source_format,
             converter_version, source_digest, declared_raw_record_count,
             declared_entry_count)
         VALUES
            ('10000000-0000-4000-8000-000000000039', 1,
             'claude_code_session_jsonl', 1,
             decode(repeat('22', 32), 'hex'), 1, 3);
         INSERT INTO imported_conversation_raw_record
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
             'attested_user', decode('01', 'hex'), decode('01', 'hex')),
            ('10000000-0000-4000-8000-000000000039', 2,
             '20000000-0000-4000-8000-000000000040', 1, 2,
             'attested_assistant', decode('02', 'hex'), decode('02', 'hex')),
            ('10000000-0000-4000-8000-000000000039', 3,
             '20000000-0000-4000-8000-000000000041', 1, 3,
             'attested_user', decode('03', 'hex'), decode('03', 'hex'));",
    )
    .execute(&mut **transaction)
    .await?;
    Ok(facts)
}

async fn insert_imported_session_scaffolding(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('30000000-0000-4000-8000-000000000039',
             'create_session_from_imported_frontier', 1,
             transaction_timestamp());
         INSERT INTO session
            (session_id, creation_cause, ancestry_kind,
             imported_conversation_id, imported_frontier_entry_id,
             imported_frontier_position, imported_relationship_kind)
         VALUES
            ('40000000-0000-4000-8000-000000000039',
             'owner_initiated', 'imported_conversation',
             '10000000-0000-4000-8000-000000000039',
             '20000000-0000-4000-8000-000000000040', 2, 'resume');
         INSERT INTO session_scheduler (session_id)
         VALUES ('40000000-0000-4000-8000-000000000039');
         INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('40000000-0000-4000-8000-000000000039', 1, 'direct',
             '50000000-0000-4000-8000-000000000039', NULL);
         INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ('40000000-0000-4000-8000-000000000039', 1);
         INSERT INTO create_session_from_imported_frontier_command
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
             'owner_initiated', 'imported_conversation', 1,
             'direct', '50000000-0000-4000-8000-000000000039', NULL,
             'applied', '40000000-0000-4000-8000-000000000039');
         INSERT INTO context_frontier
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
    let facts = insert_imported_source_scaffolding(transaction).await?;
    insert_imported_session_scaffolding(transaction).await?;
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
        "INSERT INTO context_frontier_member
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

/// S28 / INV-039: one applied imported-frontier command can commit only with its
/// exact ancestry, imported semantic prefix, and one-to-one seed frontier.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv039_exact_imported_session_seed_commits() -> Result<(), Box<dyn Error>> {
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

/// S28 / INV-039: the complete seed can be assembled in any in-transaction order;
/// inserting its one-to-one link before the semantic prefix remains valid.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv039_seed_link_can_precede_semantic_prefix() -> Result<(), Box<dyn Error>> {
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

/// S28 / INV-039: the one-to-one seed link can precede its imported session;
/// the deferred ancestry check validates the final cross-table facts.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv039_seed_link_can_precede_imported_session() -> Result<(), Box<dyn Error>> {
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

/// S28 / INV-039: once the complete same-transaction seed check is discharged,
/// another imported semantic row cannot extend the selected prefix.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv039_immediate_seed_check_seals_same_transaction_prefix()
-> Result<(), Box<dyn Error>> {
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

/// S28 / INV-039: imported ancestry cannot commit without the separate one-to-one
/// seed record, even when the materialized frontier content is exact.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv039_imported_ancestry_without_seed_is_rejected() -> Result<(), Box<dyn Error>> {
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

/// S28 / INV-039: equal imported members in the wrong order are not the selected
/// imported prefix.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv039_reordered_imported_seed_members_are_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    let seed = insert_imported_resume_seed_scaffolding(&mut transaction).await?;
    insert_imported_semantic_prefix(&mut transaction, seed).await?;
    sqlx::query(
        "INSERT INTO context_frontier_member
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

/// S28 / INV-039: an imported semantic payload cannot fabricate any native
/// accepted-input, turn, call, or tool evidence.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv039_imported_semantic_entry_rejects_native_payload_columns()
-> Result<(), Box<dyn Error>> {
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

/// S28 / INV-039: the new durable command discriminator still requires its complete
/// typed record at the transaction boundary.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv039_imported_creation_registry_claim_requires_typed_record()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('30000000-0000-4000-8000-000000000039',
             'create_session_from_imported_frontier', 1,
             transaction_timestamp())",
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

/// S28 / INV-039: replacing the native-only reverse creation FK does not make the
/// preexisting native command table truncatable.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv039_native_creation_command_truncate_remains_rejected() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let error = sqlx::query("TRUNCATE TABLE create_session_command")
        .execute(&pool)
        .await
        .expect_err("native creation commands must remain protected from truncate");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some(std::borrow::Cow::Borrowed("23514"))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28 / INV-039: row-level immutability cannot be bypassed by truncating the table
/// that carries exact seed-frontier membership.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv039_seed_frontier_member_truncate_is_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let error = sqlx::query("TRUNCATE TABLE context_frontier_member")
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

/// S28 / INV-039: seed construction is ordered once per session; after the seed link
/// exists, its imported semantic prefix cannot grow.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv039_committed_seed_rejects_late_prefix_inserts() -> Result<(), Box<dyn Error>> {
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

/// S28 / INV-038: exact reingestion resolves the immutable winner, raw blobs
/// deduplicate by content hash, and restart loading reconstructs every
/// addressable imported-conversation frontier.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv038_import_round_trip_is_idempotent_and_restart_safe() -> Result<(), Box<dyn Error>>
{
    let (container, pool, database_url) = migrated_postgres().await?;
    let source = concat!(
        "{\"type\":\"summary\",\"value\":null}\r\n",
        "{\"type\":\"summary\",\"value\":null}"
    );
    let winner = ImportedConversationId::from_uuid(Uuid::from_u128(0x100));
    let repository = ImportedConversationRepository::new(pool.clone());
    let mut service = ImportConversationService::new(
        FixedIds::new(&[0x100, 0x200], 0x300..0x304),
        ClaudeCodeJsonlConverter,
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
        .expect("inserted imported conversation must load");
    assert_eq!(stored.raw_records().len(), 2);
    assert_eq!(stored.entries().len(), 2);
    assert_eq!(stored.frontiers().count(), 2);
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
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (1, 1, 2, 2));
    assert!(
        sqlx::query(
            "UPDATE imported_raw_source_record
                SET raw_bytes = raw_bytes",
        )
        .execute(&pool)
        .await
        .is_err(),
        "raw source records must reject updates"
    );
    assert!(
        sqlx::query("TRUNCATE TABLE imported_transcript_entry")
            .execute(&pool)
            .await
            .is_err(),
        "imported entries must reject statement-level truncate"
    );

    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let restarted = ImportedConversationRepository::new(restarted_pool.clone())
        .load(winner)
        .await?
        .expect("durable imported conversation must survive pool restart");
    assert_eq!(restarted, stored);

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S28 / INV-038: equal source bytes cannot resolve as replay when a drifting
/// converter supplies a different normalized record and semantic projection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv038_reingestion_rejects_converter_projection_drift() -> Result<(), Box<dyn Error>> {
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
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
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

/// S28 / INV-002 / INV-038: exact reingestion checks an existing snapshot
/// before the new-digest blob path and cannot conceal durable raw corruption.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv002_inv038_reingestion_does_not_mask_raw_corruption() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let source = br#"{"type":"summary","value":null}"#;
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

    sqlx::query("ALTER TABLE imported_raw_source_record DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE imported_raw_source_record
            SET raw_bytes = raw_bytes || $1",
    )
    .bind(vec![b' '])
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE imported_raw_source_record ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    let error = service
        .execute(source)
        .await
        .expect_err("reingestion must expose existing raw corruption");
    assert!(matches!(
        error,
        ImportConversationError::Store(ImportedConversationRepositoryError::Corruption(
            ImportedConversationCorruption::Domain(
                ImportedConversationReconstitutionFailure::RawRecordHashMismatch { .. }
            )
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S28 / INV-038: imports sharing raw blobs acquire their global content keys
/// in one stable order even when the source occurrences are reversed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv038_concurrent_reversed_raws_use_stable_blob_order() -> Result<(), Box<dyn Error>> {
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

/// S28 / INV-001 / INV-038: overlapping imported-entry identity keys are
/// acquired in one stable order even when transcript positions reverse them.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv001_inv038_concurrent_reversed_entry_ids_return_typed_collision()
-> Result<(), Box<dyn Error>> {
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

/// INV-001: the late unique-constraint path reached after a concurrent
/// precheck race retains the repository's typed imported-entry collision.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv001_late_entry_identity_constraint_is_typed_collision() -> Result<(), Box<dyn Error>> {
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
             declared_entry_count)
         VALUES ($1, 1, 'claude_code_session_jsonl', 1, $2, 1, 1)",
    )
    .bind(Uuid::from_u128(0xa10))
    .bind(vec![0x10_u8; 32])
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO imported_raw_source_record (content_hash, raw_bytes)
         VALUES ($1, $2)",
    )
    .bind(vec![0x11_u8; 32])
    .bind(vec![0x12_u8])
    .execute(&mut *transaction)
    .await?;
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
    .bind(vec![1_u8])
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

/// INV-038: a header cannot commit without its exact declared contiguous raw
/// and normalized-entry membership.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv038_incomplete_import_header_cannot_commit() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO imported_conversation
            (imported_conversation_id, storage_version, source_format,
             converter_version, source_digest, declared_raw_record_count,
             declared_entry_count)
         VALUES ($1, 1, 'claude_code_session_jsonl', 1, $2, 1, 1)",
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

/// S28 / INV-038: a newly inserted content-addressed raw blob cannot commit
/// without at least one conversation-owned occurrence.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv038_unowned_raw_source_record_cannot_commit() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO imported_raw_source_record (content_hash, raw_bytes)
         VALUES ($1, $2)",
    )
    .bind(vec![0x41_u8; 32])
    .bind(vec![0x42_u8])
    .execute(&mut *transaction)
    .await?;

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

/// INV-038: physical raw records are nonempty at the schema boundary.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv038_empty_raw_record_is_schema_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let error = sqlx::query(
        "INSERT INTO imported_raw_source_record (content_hash, raw_bytes)
         VALUES ($1, $2)",
    )
    .bind(vec![0_u8; 32])
    .bind(Vec::<u8>::new())
    .execute(&pool)
    .await
    .expect_err("empty raw source records must violate the schema");
    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("imported_raw_source_record_bytes_nonempty")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-002 / INV-038: adapter and domain reconstruction fail closed when
/// durable declared counts are corrupted behind append-only guards.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv002_inv038_corrupt_import_fails_typed_load() -> Result<(), Box<dyn Error>> {
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

/// INV-002 / INV-038: normalized storage cannot be replaced independently from
/// the exact raw record and its conversion authentication.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv002_inv038_corrupt_normalized_record_fails_typed_load() -> Result<(), Box<dyn Error>> {
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

/// INV-002 / INV-038: each raw occurrence's declared normalized-entry count is
/// checked against the complete reconstructed membership.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv002_inv038_corrupt_raw_entry_count_fails_typed_load() -> Result<(), Box<dyn Error>> {
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
        validate_real_transcript(&pool, &source, file_index).await?;
    }

    pool.close().await;
    drop(container);
    Ok(())
}

async fn validate_real_transcript(
    pool: &PgPool,
    source: &[u8],
    file_index: usize,
) -> Result<(), Box<dyn Error>> {
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
        ClaudeCodeJsonlConverter,
        repository,
    );
    let winner = match service
        .execute(source)
        .await
        .map_err(|_| "real transcript first import failed")?
    {
        ImportConversationOutcome::Inserted { conversation }
        | ImportConversationOutcome::AlreadyImported { conversation } => conversation,
    };
    match service
        .execute(source)
        .await
        .map_err(|_| "real transcript repeat import failed")?
    {
        ImportConversationOutcome::AlreadyImported { conversation } if conversation == winner => {}
        ImportConversationOutcome::AlreadyImported { .. } => {
            return Err("real transcript reimport resolved a different identity".into());
        }
        ImportConversationOutcome::Inserted { .. } => {
            return Err("real transcript reimport was not idempotent".into());
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
