#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

mod support;

use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use signalbox_application::{
    CreateSessionFromImportedFrontierOutcome, ImportedConversationConverter,
    ImportedConversationStore,
};
use signalbox_conversation_import_claude_code::ClaudeCodeJsonlConverter;
use signalbox_domain::{
    BoundedImportedSessionReconstitutionFailure, ContextFrontierId,
    CreateSessionFromImportedFrontier, DirectModelSelection, DurableCommandId,
    ImportedConversation, ImportedConversationId, ImportedSessionRelationship,
    ImportedTranscriptEntryId, ModelSelectionRequest, SemanticTranscriptEntryId,
    SessionConfigurationDefaults, SessionId, TranscriptAncestry,
};
use signalbox_persistence::{
    conversation_import::ImportedConversationRepository,
    create_session_from_imported_frontier::{
        ImportedSessionCorruption, ImportedSessionIdentityCollision, ImportedSessionRepository,
        ImportedSessionRepositoryError,
    },
    local_test_connection_options,
    mapping::DurableCommandIdMappingError,
    migrate,
    session::{SessionCorruption, SessionRepository, SessionRepositoryError},
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

use support::blocked_backends_reached;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_imported_session_integration";
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
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

fn imported(conversation: u128, first_entry: u128, source: &str) -> ImportedConversation {
    let mut next_entry = first_entry;
    ClaudeCodeJsonlConverter
        .convert(
            ImportedConversationId::from_uuid(Uuid::from_u128(conversation)),
            source.as_bytes(),
            || {
                let identity = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(next_entry));
                next_entry = next_entry
                    .checked_add(1)
                    .expect("synthetic entry range remains bounded");
                identity
            },
        )
        .expect("synthetic JSONL must convert")
}

fn defaults(value: u128) -> SessionConfigurationDefaults {
    SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
        DirectModelSelection::from_uuid(Uuid::from_u128(value)),
    ))
}

fn imported_command(
    command: u128,
    conversation: &ImportedConversation,
    relationship: ImportedSessionRelationship,
) -> CreateSessionFromImportedFrontier {
    CreateSessionFromImportedFrontier::new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        conversation
            .frontiers()
            .last()
            .expect("synthetic conversation has an addressable entry"),
        relationship,
        defaults(0x500),
    )
}

/// S28 / INV-038 / INV-039: first handling commits the exact imported prefix,
/// seed, command result, session, and outbox event atomically.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv038_inv039_first_imported_frontier_creation_commits_exact_seed_atomically()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let conversation = imported(
        0x100,
        0x200,
        concat!(
            "{\"type\":\"summary\",\"value\":null}\n",
            "{\"type\":\"summary\",\"value\":null}"
        ),
    );
    ImportedConversationStore::resolve_or_insert(
        &mut ImportedConversationRepository::new(pool.clone()),
        conversation.clone(),
    )
    .await?;

    let command = imported_command(0x300, &conversation, ImportedSessionRelationship::Resume);
    let repository = ImportedSessionRepository::new(pool.clone());
    let mut next_semantic = 0x600_u128;
    let outcome = repository
        .handle(
            command,
            SessionId::from_uuid(Uuid::from_u128(0x400)),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x700)),
            || {
                let value = next_semantic;
                next_semantic += 1;
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value))
            },
        )
        .await?;
    let CreateSessionFromImportedFrontierOutcome::Applied(applied) = outcome else {
        panic!("first handling must apply")
    };
    assert_eq!(
        applied.session(),
        SessionId::from_uuid(Uuid::from_u128(0x400))
    );
    assert_eq!(next_semantic, 0x602);

    let counts: (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command),
            (SELECT count(*) FROM create_session_from_imported_frontier_command),
            (SELECT count(*) FROM session),
            (SELECT count(*) FROM semantic_transcript_entry),
            (SELECT count(*) FROM context_frontier_member),
            (SELECT count(*) FROM imported_session_seed),
            (SELECT count(*) FROM outbox_event
              WHERE event_kind = 'session_created')",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (1, 1, 1, 2, 2, 1, 1));
    Ok(())
}

/// S28 / INV-012 / INV-039: equal replay returns the recorded result without
/// consuming any fresh semantic identity.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv012_inv039_equal_replay_returns_recorded_session_without_generation()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let conversation = imported(
        0x101,
        0x210,
        concat!(
            "{\"type\":\"summary\",\"value\":null}\n",
            "{\"type\":\"summary\",\"value\":null}"
        ),
    );
    ImportedConversationStore::resolve_or_insert(
        &mut ImportedConversationRepository::new(pool.clone()),
        conversation.clone(),
    )
    .await?;

    let command = imported_command(0x301, &conversation, ImportedSessionRelationship::Resume);
    let repository = ImportedSessionRepository::new(pool);
    let mut next_semantic = 0x610_u128;
    let first = repository
        .handle(
            command,
            SessionId::from_uuid(Uuid::from_u128(0x401)),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x701)),
            || {
                let value = next_semantic;
                next_semantic += 1;
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value))
            },
        )
        .await?;
    let CreateSessionFromImportedFrontierOutcome::Applied(applied) = first else {
        panic!("first handling must apply")
    };

    let mut replay_generation = 0_u64;
    let replay = repository
        .handle(
            command,
            SessionId::from_uuid(Uuid::from_u128(0x402)),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x702)),
            || {
                replay_generation += 1;
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x619))
            },
        )
        .await?;
    assert_eq!(
        replay,
        CreateSessionFromImportedFrontierOutcome::Applied(applied)
    );
    assert_eq!(replay_generation, 0);
    Ok(())
}

/// S28 / INV-002 / INV-038 / INV-039: the purpose-specific command load
/// reconstitutes the complete stored command, result, semantic prefix, and seed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv002_inv038_inv039_command_load_reconstitutes_complete_checked_seed()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let conversation = imported(
        0x102,
        0x220,
        concat!(
            "{\"type\":\"summary\",\"value\":null}\n",
            "{\"type\":\"summary\",\"value\":null}"
        ),
    );
    ImportedConversationStore::resolve_or_insert(
        &mut ImportedConversationRepository::new(pool.clone()),
        conversation.clone(),
    )
    .await?;

    let command = imported_command(0x302, &conversation, ImportedSessionRelationship::Fork);
    let repository = ImportedSessionRepository::new(pool);
    let mut next_semantic = 0x620_u128;
    let created = repository
        .handle(
            command,
            SessionId::from_uuid(Uuid::from_u128(0x403)),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x703)),
            || {
                let value = next_semantic;
                next_semantic += 1;
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value))
            },
        )
        .await?;
    let CreateSessionFromImportedFrontierOutcome::Applied(applied) = created else {
        panic!("fixture creation must apply")
    };

    let recorded = repository
        .load(command.command_id())
        .await?
        .expect("claimed command must completely reconstitute");
    assert_eq!(recorded.command(), &command);
    assert_eq!(recorded.applied_result(), applied);
    assert_eq!(recorded.semantic_entries().len(), 2);
    assert_eq!(recorded.seed_snapshot().entry_count(), 2);
    Ok(())
}

/// S28 / INV-002 / INV-039: ordinary current-session loading returns the
/// imported ancestry after validating the bounded one-to-one seed proof.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv002_inv039_current_session_load_reconstitutes_imported_ancestry()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let conversation = imported(
        0x103,
        0x230,
        concat!(
            "{\"type\":\"summary\",\"value\":null}\n",
            "{\"type\":\"summary\",\"value\":null}"
        ),
    );
    ImportedConversationStore::resolve_or_insert(
        &mut ImportedConversationRepository::new(pool.clone()),
        conversation.clone(),
    )
    .await?;

    let command = imported_command(0x303, &conversation, ImportedSessionRelationship::Resume);
    let repository = ImportedSessionRepository::new(pool.clone());
    let mut next_semantic = 0x630_u128;
    let created = repository
        .handle(
            command,
            SessionId::from_uuid(Uuid::from_u128(0x404)),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x704)),
            || {
                let value = next_semantic;
                next_semantic += 1;
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value))
            },
        )
        .await?;
    let CreateSessionFromImportedFrontierOutcome::Applied(applied) = created else {
        panic!("fixture creation must apply")
    };

    let loaded = SessionRepository::new(pool.clone())
        .load_session(applied.session())
        .await?
        .expect("created imported session must load");
    assert_eq!(loaded.id(), applied.session());
    assert!(matches!(
        loaded.creation_provenance().ancestry(),
        TranscriptAncestry::ImportedConversation {
            relationship: ImportedSessionRelationship::Resume,
            ..
        }
    ));
    Ok(())
}

/// S28 / INV-012 / INV-039: a changed canonical payload under a claimed
/// command identity returns typed conflicting reuse without generating entries.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv012_inv039_conflicting_reuse_is_typed_and_generation_free()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let conversation = imported(
        0x104,
        0x240,
        concat!(
            "{\"type\":\"summary\",\"value\":null}\n",
            "{\"type\":\"summary\",\"value\":null}"
        ),
    );
    ImportedConversationStore::resolve_or_insert(
        &mut ImportedConversationRepository::new(pool.clone()),
        conversation.clone(),
    )
    .await?;

    let command = imported_command(0x304, &conversation, ImportedSessionRelationship::Resume);
    let repository = ImportedSessionRepository::new(pool);
    let mut next_semantic = 0x640_u128;
    let created = repository
        .handle(
            command,
            SessionId::from_uuid(Uuid::from_u128(0x405)),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x705)),
            || {
                let value = next_semantic;
                next_semantic += 1;
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value))
            },
        )
        .await?;
    assert!(matches!(
        created,
        CreateSessionFromImportedFrontierOutcome::Applied(_)
    ));

    let conflict = repository
        .handle(
            imported_command(0x304, &conversation, ImportedSessionRelationship::Fork),
            SessionId::from_uuid(Uuid::from_u128(0x406)),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x706)),
            || panic!("conflicting reuse must not request semantic identities"),
        )
        .await?;
    assert_eq!(
        conflict,
        CreateSessionFromImportedFrontierOutcome::ConflictingReuse {
            command_id: command.command_id()
        }
    );
    Ok(())
}

/// S28 / INV-012 / INV-039: a missing imported conversation is a pre-claim
/// typed outcome and generates no semantic identities.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv012_inv039_missing_conversation_remains_unclaimed_and_generation_free()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = ImportedSessionRepository::new(pool.clone());

    let absent = imported(0x111, 0x211, "{\"type\":\"summary\",\"value\":null}");
    let absent_command = imported_command(0x310, &absent, ImportedSessionRelationship::Resume);
    assert_eq!(
        repository
            .handle(
                absent_command,
                SessionId::from_uuid(Uuid::from_u128(0x410)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x710)),
                || panic!("missing conversation must not generate identities"),
            )
            .await?,
        CreateSessionFromImportedFrontierOutcome::ImportedConversationNotFound {
            conversation: absent.id()
        }
    );

    let claimed: i64 = sqlx::query_scalar("SELECT count(*) FROM durable_command")
        .fetch_one(&pool)
        .await?;
    assert_eq!(claimed, 0);
    Ok(())
}

/// S28 / INV-012 / INV-039: a missing imported frontier is a pre-claim typed
/// outcome and generates no semantic identities.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv012_inv039_missing_frontier_remains_unclaimed_and_generation_free()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let stored = imported(0x110, 0x210, "{\"type\":\"summary\",\"value\":null}");
    ImportedConversationStore::resolve_or_insert(
        &mut ImportedConversationRepository::new(pool.clone()),
        stored.clone(),
    )
    .await?;
    let repository = ImportedSessionRepository::new(pool.clone());

    let alternate = imported(0x110, 0x212, "{\"type\":\"summary\",\"value\":\"other\"}");
    let missing_frontier_command =
        imported_command(0x311, &alternate, ImportedSessionRelationship::Fork);
    assert_eq!(
        repository
            .handle(
                missing_frontier_command,
                SessionId::from_uuid(Uuid::from_u128(0x411)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x711)),
                || panic!("missing frontier must not generate identities"),
            )
            .await?,
        CreateSessionFromImportedFrontierOutcome::ImportedFrontierNotFound {
            frontier: missing_frontier_command.imported_frontier()
        }
    );
    let claimed: i64 = sqlx::query_scalar("SELECT count(*) FROM durable_command")
        .fetch_one(&pool)
        .await?;
    assert_eq!(claimed, 0);
    Ok(())
}

/// S28 / INV-001 / INV-012 / INV-039: concurrent equal first handling
/// converges on one committed seed, and only the command-claim winner consumes
/// semantic identities.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv001_inv012_inv039_concurrent_equal_creation_has_one_identity_consuming_winner()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let conversation = imported(
        0x115,
        0x215,
        concat!(
            "{\"type\":\"summary\",\"value\":null}\n",
            "{\"type\":\"summary\",\"value\":null}"
        ),
    );
    ImportedConversationStore::resolve_or_insert(
        &mut ImportedConversationRepository::new(pool.clone()),
        conversation.clone(),
    )
    .await?;
    let command = imported_command(0x315, &conversation, ImportedSessionRelationship::Fork);
    let first_repository = ImportedSessionRepository::new(pool.clone());
    let second_repository = first_repository.clone();
    let generated = Arc::new(AtomicU64::new(0));
    let first_generated = Arc::clone(&generated);
    let second_generated = Arc::clone(&generated);
    let mut claim_gate = pool.begin().await?;
    sqlx::query("LOCK TABLE durable_command IN SHARE MODE")
        .execute(&mut *claim_gate)
        .await?;

    let first = tokio::spawn(async move {
        let mut first_identity = 0x680_u128;
        first_repository
            .handle(
                command,
                SessionId::from_uuid(Uuid::from_u128(0x415)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x715)),
                || {
                    first_generated.fetch_add(1, Ordering::SeqCst);
                    let value = first_identity;
                    first_identity += 1;
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value))
                },
            )
            .await
    });
    let second = tokio::spawn(async move {
        let mut second_identity = 0x690_u128;
        second_repository
            .handle(
                command,
                SessionId::from_uuid(Uuid::from_u128(0x416)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x716)),
                || {
                    second_generated.fetch_add(1, Ordering::SeqCst);
                    let value = second_identity;
                    second_identity += 1;
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value))
                },
            )
            .await
    });
    assert!(
        blocked_backends_reached(&pool, 2).await?,
        "both handlers must reach the held command-claim insert"
    );
    claim_gate.commit().await?;

    let (
        CreateSessionFromImportedFrontierOutcome::Applied(first),
        CreateSessionFromImportedFrontierOutcome::Applied(second),
    ) = (first.await??, second.await??)
    else {
        panic!("both equal handlers must return the recorded applied result")
    };
    assert_eq!(first, second);
    assert_eq!(generated.load(Ordering::SeqCst), 2);

    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM session),
            (SELECT count(*) FROM semantic_transcript_entry),
            (SELECT count(*) FROM imported_session_seed)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (1, 2, 1));
    Ok(())
}

/// S28 / INV-001 / INV-039: a generated session identity collision is typed
/// and rolls back the command claim.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv001_inv039_generated_session_identity_collision_is_typed()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let conversation = imported(0x116, 0x216, "{\"type\":\"summary\",\"value\":null}");
    ImportedConversationStore::resolve_or_insert(
        &mut ImportedConversationRepository::new(pool.clone()),
        conversation.clone(),
    )
    .await?;
    let repository = ImportedSessionRepository::new(pool);
    let occupied_session = SessionId::from_uuid(Uuid::from_u128(0x416));
    repository
        .handle(
            imported_command(0x316, &conversation, ImportedSessionRelationship::Resume),
            occupied_session,
            ContextFrontierId::from_uuid(Uuid::from_u128(0x716)),
            || SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x616)),
        )
        .await?;

    let colliding_command =
        imported_command(0x317, &conversation, ImportedSessionRelationship::Fork);
    let error = repository
        .handle(
            colliding_command,
            occupied_session,
            ContextFrontierId::from_uuid(Uuid::from_u128(0x717)),
            || SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x617)),
        )
        .await
        .expect_err("occupied session identity must be a typed collision");
    assert!(matches!(
        error,
        ImportedSessionRepositoryError::IdentityCollision(
            ImportedSessionIdentityCollision::Session
        )
    ));
    assert!(
        repository
            .load(colliding_command.command_id())
            .await?
            .is_none()
    );
    Ok(())
}

/// S28 / INV-001 / INV-039: a generated semantic-entry identity collision is
/// typed and rolls back the command claim.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv001_inv039_generated_semantic_entry_identity_collision_is_typed()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let conversation = imported(0x118, 0x218, "{\"type\":\"summary\",\"value\":null}");
    ImportedConversationStore::resolve_or_insert(
        &mut ImportedConversationRepository::new(pool.clone()),
        conversation.clone(),
    )
    .await?;
    let repository = ImportedSessionRepository::new(pool);
    let occupied_semantic = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x618));
    repository
        .handle(
            imported_command(0x318, &conversation, ImportedSessionRelationship::Resume),
            SessionId::from_uuid(Uuid::from_u128(0x418)),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x718)),
            || occupied_semantic,
        )
        .await?;

    let colliding_command =
        imported_command(0x319, &conversation, ImportedSessionRelationship::Fork);
    let error = repository
        .handle(
            colliding_command,
            SessionId::from_uuid(Uuid::from_u128(0x419)),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x719)),
            || occupied_semantic,
        )
        .await
        .expect_err("occupied semantic-entry identity must be a typed collision");
    assert!(matches!(
        error,
        ImportedSessionRepositoryError::IdentityCollision(
            ImportedSessionIdentityCollision::SemanticEntry
        )
    ));
    assert!(
        repository
            .load(colliding_command.command_id())
            .await?
            .is_none()
    );
    Ok(())
}

/// S28 / INV-001 / INV-039: a generated seed-frontier identity collision is
/// typed and rolls back the command claim.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv001_inv039_generated_seed_frontier_identity_collision_is_typed()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let conversation = imported(0x11a, 0x21a, "{\"type\":\"summary\",\"value\":null}");
    ImportedConversationStore::resolve_or_insert(
        &mut ImportedConversationRepository::new(pool.clone()),
        conversation.clone(),
    )
    .await?;
    let repository = ImportedSessionRepository::new(pool);
    let occupied_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0x71a));
    repository
        .handle(
            imported_command(0x31a, &conversation, ImportedSessionRelationship::Resume),
            SessionId::from_uuid(Uuid::from_u128(0x41a)),
            occupied_frontier,
            || SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x61a)),
        )
        .await?;

    let colliding_command =
        imported_command(0x31b, &conversation, ImportedSessionRelationship::Fork);
    let error = repository
        .handle(
            colliding_command,
            SessionId::from_uuid(Uuid::from_u128(0x41b)),
            occupied_frontier,
            || SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x61b)),
        )
        .await
        .expect_err("occupied seed-frontier identity must be a typed collision");
    assert!(matches!(
        error,
        ImportedSessionRepositoryError::IdentityCollision(
            ImportedSessionIdentityCollision::SeedFrontier
        )
    ));
    assert!(
        repository
            .load(colliding_command.command_id())
            .await?
            .is_none()
    );
    Ok(())
}

/// S28 / INV-002: purpose loading rejects a stored sentinel command UUID
/// before reconstructing a domain command.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv002_command_load_rejects_stored_sentinel_command_identity()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let conversation = imported(0x11c, 0x21c, "{\"type\":\"summary\",\"value\":null}");
    ImportedConversationStore::resolve_or_insert(
        &mut ImportedConversationRepository::new(pool.clone()),
        conversation.clone(),
    )
    .await?;
    let repository = ImportedSessionRepository::new(pool.clone());
    let command = imported_command(0x31c, &conversation, ImportedSessionRelationship::Resume);
    repository
        .handle(
            command,
            SessionId::from_uuid(Uuid::from_u128(0x41c)),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x71c)),
            || SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x61c)),
        )
        .await?;

    sqlx::raw_sql(
        "ALTER TABLE durable_command DISABLE TRIGGER ALL;
         ALTER TABLE create_session_from_imported_frontier_command DISABLE TRIGGER ALL;
         UPDATE create_session_from_imported_frontier_command
            SET command_id = '00000000-0000-0000-0000-000000000000';
         UPDATE durable_command
            SET command_id = '00000000-0000-0000-0000-000000000000';
         ALTER TABLE create_session_from_imported_frontier_command ENABLE TRIGGER ALL;
         ALTER TABLE durable_command ENABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;

    let error = repository
        .load(DurableCommandId::from_uuid(Uuid::nil()))
        .await
        .expect_err("stored sentinel command identity must fail closed");
    assert!(matches!(
        error,
        ImportedSessionRepositoryError::Corruption(
            ImportedSessionCorruption::InvalidCommandIdentity {
                field: "typed command identity",
                reason: DurableCommandIdMappingError::SentinelUuid,
            }
        )
    ));
    Ok(())
}

/// S28 / INV-039: an imported session whose one-to-one seed is absent fails
/// closed at the ordinary current-session load boundary.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv039_current_load_rejects_missing_imported_seed() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let conversation = imported(0x120, 0x220, "{\"type\":\"summary\",\"value\":null}");
    ImportedConversationStore::resolve_or_insert(
        &mut ImportedConversationRepository::new(pool.clone()),
        conversation.clone(),
    )
    .await?;
    let repository = ImportedSessionRepository::new(pool.clone());
    let created = repository
        .handle(
            imported_command(0x320, &conversation, ImportedSessionRelationship::Resume),
            SessionId::from_uuid(Uuid::from_u128(0x420)),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x720)),
            || SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x620)),
        )
        .await?;
    let CreateSessionFromImportedFrontierOutcome::Applied(applied) = created else {
        panic!("fixture creation must apply")
    };

    sqlx::raw_sql(
        "ALTER TABLE imported_session_seed DISABLE TRIGGER USER;
         DELETE FROM imported_session_seed;
         ALTER TABLE imported_session_seed ENABLE TRIGGER USER;",
    )
    .execute(&pool)
    .await?;

    let error = SessionRepository::new(pool)
        .load_session(applied.session())
        .await
        .expect_err("missing seed must fail closed");
    assert!(matches!(
        error,
        SessionRepositoryError::Corruption(SessionCorruption::Imported(
            ImportedSessionCorruption::BoundedCurrentDomain(
                BoundedImportedSessionReconstitutionFailure::MissingSeedRecord
            )
        ))
    ));
    Ok(())
}

/// S28 / INV-002 / INV-039: the constant-size current-session proof rejects a
/// seed header whose declared member count differs from the imported boundary.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv002_inv039_current_load_rejects_cross_wired_seed_header_count()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let conversation = imported(0x121, 0x221, "{\"type\":\"summary\",\"value\":null}");
    ImportedConversationStore::resolve_or_insert(
        &mut ImportedConversationRepository::new(pool.clone()),
        conversation.clone(),
    )
    .await?;
    let repository = ImportedSessionRepository::new(pool.clone());
    let created = repository
        .handle(
            imported_command(0x321, &conversation, ImportedSessionRelationship::Fork),
            SessionId::from_uuid(Uuid::from_u128(0x421)),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x721)),
            || SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x621)),
        )
        .await?;
    let CreateSessionFromImportedFrontierOutcome::Applied(applied) = created else {
        panic!("fixture creation must apply")
    };

    sqlx::raw_sql(
        "ALTER TABLE context_frontier DISABLE TRIGGER USER;
         UPDATE context_frontier SET member_count = 2;
         ALTER TABLE context_frontier ENABLE TRIGGER USER;",
    )
    .execute(&pool)
    .await?;

    let error = SessionRepository::new(pool)
        .load_session(applied.session())
        .await
        .expect_err("cross-wired seed header count must fail closed");
    assert!(matches!(
        error,
        SessionRepositoryError::Corruption(SessionCorruption::Imported(
            ImportedSessionCorruption::BoundedCurrentDomain(
                BoundedImportedSessionReconstitutionFailure::SeedMemberCountMismatch
            )
        ))
    ));
    Ok(())
}
