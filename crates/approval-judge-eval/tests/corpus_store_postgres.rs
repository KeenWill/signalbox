//! Real-PostgreSQL conformance and import coverage for corpus stores.

#![allow(
    clippy::expect_used,
    reason = "this integration test uses fixed synthetic fixtures"
)]

use std::{error::Error, path::Path};

use signalbox_approval_judge_eval::{
    ApprovalDisposition, ApprovalJudgeCorpus, CorpusKey, CorpusRegistration,
    CorpusSourceDescriptor, CorpusStore, CorpusStoreCorruption, CorpusStoreError,
    DatabaseCorpusStore, DiskCorpusStore, Sha256Digest, score_corpus,
};
use signalbox_domain::{
    DirectModelSelection, ModelCallId, ProviderModelIdentity, ResolvedProviderTarget,
};
use signalbox_model_provider_runtime::{
    RuntimeApprovalJudgeModel, RuntimeModelCatalog, RuntimeModelDefinition,
};
use signalbox_model_runtime::{
    AssistantPart, CompletionEvidence, CompletionFinish, ExchangeFacts, ProviderReportedModel,
    Script, ScriptedModel, TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal, ToolName,
};
use signalbox_persistence::{
    disposable_postgres_state_tmpfs, disposable_test_container_labels,
    local_test_connection_options, migrate,
};
use signalboxd::approval_judge_eval::ApprovalJudgeEvalBinding;
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use uuid::Uuid;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_eval_corpus";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const PROVIDER_MODEL: &str = "offline-fixture-judge";
const FIXTURE_MAX_OUTPUT_TOKENS: u32 = 256;
const FIXTURE_CONTEXT_WINDOW_TOKENS: u32 = 4_096;

fn seed_manifest_path() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/corpora/seed-v1.manifest.json"
    ))
}

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_fsync_enabled()
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
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

async fn observe_store(
    store: &dyn CorpusStore,
    key: &CorpusKey,
) -> Result<(Vec<CorpusRegistration>, ApprovalJudgeCorpus), Box<dyn Error>> {
    Ok((store.enumerate().await?, store.load(key).await?))
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn manifest_import_conforms_with_disk_and_scores_identically() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let disk = DiskCorpusStore::open(seed_manifest_path())?;
    let disk_registrations = disk.enumerate().await?;
    let key = disk_registrations
        .first()
        .expect("the manifest-backed disk store has one registration")
        .key()
        .clone();
    let database = DatabaseCorpusStore::new(pool.clone());
    let imported = database.import_manifest(seed_manifest_path()).await?;
    let disk_observation = observe_store(&disk, &key).await?;
    let database_observation = observe_store(&database, &key).await?;
    let (disk_model, disk_binding) = fixture_model();
    let (database_model, database_binding) = fixture_model();
    let disk_score = score_corpus(&disk_model, &disk_binding, &disk_observation.1).await?;
    let database_score =
        score_corpus(&database_model, &database_binding, &database_observation.1).await?;

    assert_eq!(imported, disk_observation.0[0]);
    assert_eq!(database_observation, disk_observation);
    assert_eq!(database_score, disk_score);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn repeated_manifest_import_is_idempotent() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let database = DatabaseCorpusStore::new(pool.clone());

    let imported = database.import_manifest(seed_manifest_path()).await?;
    let repeated_import = database.import_manifest(seed_manifest_path()).await?;

    assert_eq!(repeated_import, imported);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn repository_import_conflicts_on_changed_source_identity() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let database = DatabaseCorpusStore::new(pool.clone());
    let imported = database.import_manifest(seed_manifest_path()).await?;
    sqlx::query(
        "UPDATE evaluation_corpus
            SET source_sha256 = decode(repeat('00', 32), 'hex')
          WHERE corpus_name = $1 AND corpus_version = $2",
    )
    .bind(&imported.key().name)
    .bind(&imported.key().version)
    .execute(&pool)
    .await?;

    let error = database
        .import_manifest(seed_manifest_path())
        .await
        .expect_err("a different durable source identity conflicts");

    assert!(matches!(
        error,
        CorpusStoreError::CorruptRegistration(CorpusStoreCorruption::RegistrationConflict)
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blob_source_constraint_requires_digest() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;

    insert_blob_registration(&pool, "missing-digest", None, Some("1"))
        .await
        .expect_err("a blob registration without a digest is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blob_source_constraint_requires_byte_length() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let digest = [0_u8; 32];

    insert_blob_registration(&pool, "missing-byte-length", Some(&digest), None)
        .await
        .expect_err("a blob registration without a byte length is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blob_source_constraint_requires_canonical_store_binding() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let corpus_digest = [0_u8; 32];
    let replay_digest = [0_u8; 32];
    let blob_digest = [0_u8; 32];

    sqlx::query(
        "INSERT INTO evaluation_corpus (
            corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
            source_kind, source_blob_store, source_blob_digest, source_blob_byte_length
         ) VALUES ('invalid-blob-store', 'v1', 1, $1, $2, 1,
                   'blob_reference', 'UPPER', $3, 1)",
    )
    .bind(corpus_digest.as_slice())
    .bind(replay_digest.as_slice())
    .bind(blob_digest.as_slice())
    .execute(&pool)
    .await
    .expect_err("a noncanonical durable blob-store binding is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn corpus_registration_requires_positive_case_count() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let corpus_digest = [0_u8; 32];
    let replay_digest = [0_u8; 32];

    sqlx::query(
        "INSERT INTO evaluation_corpus (
            corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
            source_kind
         ) VALUES ('zero-case-count', 'v1', 1, $1, $2, 0, 'database_native')",
    )
    .bind(corpus_digest.as_slice())
    .bind(replay_digest.as_slice())
    .execute(&pool)
    .await
    .expect_err("a durable registration without cases is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn corpus_registration_requires_supported_format_version() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let corpus_digest = [0_u8; 32];
    let replay_digest = [0_u8; 32];

    sqlx::query(
        "INSERT INTO evaluation_corpus (
            corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
            source_kind
         ) VALUES ('unsupported-format', 'v1', 2, $1, $2, 1, 'database_native')",
    )
    .bind(corpus_digest.as_slice())
    .bind(replay_digest.as_slice())
    .execute(&pool)
    .await
    .expect_err("a durable registration with an unsupported format version is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn corpus_registration_requires_nonblank_name() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let corpus_digest = [0_u8; 32];
    let replay_digest = [0_u8; 32];

    sqlx::query(
        "INSERT INTO evaluation_corpus (
            corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
            source_kind
         ) VALUES ('   ', 'v1', 1, $1, $2, 1, 'database_native')",
    )
    .bind(corpus_digest.as_slice())
    .bind(replay_digest.as_slice())
    .execute(&pool)
    .await
    .expect_err("a durable registration with a blank name is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn corpus_registration_requires_nonblank_version() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let corpus_digest = [0_u8; 32];
    let replay_digest = [0_u8; 32];

    sqlx::query(
        "INSERT INTO evaluation_corpus (
            corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
            source_kind
         ) VALUES ('valid-name', '   ', 1, $1, $2, 1, 'database_native')",
    )
    .bind(corpus_digest.as_slice())
    .bind(replay_digest.as_slice())
    .execute(&pool)
    .await
    .expect_err("a durable registration with a blank version is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn corpus_registration_rejects_unicode_whitespace_name() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let corpus_digest = [0_u8; 32];
    let replay_digest = [0_u8; 32];

    sqlx::query(
        "INSERT INTO evaluation_corpus (
            corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
            source_kind
         ) VALUES (U&'\00A0', 'v1', 1, $1, $2, 1, 'database_native')",
    )
    .bind(corpus_digest.as_slice())
    .bind(replay_digest.as_slice())
    .execute(&pool)
    .await
    .expect_err("a Unicode-whitespace-only durable name is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn corpus_registration_rejects_unicode_control_version() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let corpus_digest = [0_u8; 32];
    let replay_digest = [0_u8; 32];

    sqlx::query(
        "INSERT INTO evaluation_corpus (
            corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
            source_kind
         ) VALUES ('valid-name', U&'v1\0085', 1, $1, $2, 1, 'database_native')",
    )
    .bind(corpus_digest.as_slice())
    .bind(replay_digest.as_slice())
    .execute(&pool)
    .await
    .expect_err("a C1-control-bearing durable version is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn repository_registration_requires_nonblank_repository() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let corpus_digest = [0_u8; 32];
    let replay_digest = [0_u8; 32];
    let source_digest = [0_u8; 32];

    sqlx::query(
        "INSERT INTO evaluation_corpus (
            corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
            source_kind, source_repository, source_path, source_sha256
         ) VALUES ('invalid-repository', 'v1', 1, $1, $2, 1,
                   'repository', '   ', 'cases.json', $3)",
    )
    .bind(corpus_digest.as_slice())
    .bind(replay_digest.as_slice())
    .bind(source_digest.as_slice())
    .execute(&pool)
    .await
    .expect_err("a durable repository registration with blank provenance is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn repository_registration_rejects_unicode_whitespace_repository()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let corpus_digest = [0_u8; 32];
    let replay_digest = [0_u8; 32];
    let source_digest = [0_u8; 32];

    sqlx::query(
        "INSERT INTO evaluation_corpus (
            corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
            source_kind, source_repository, source_path, source_sha256
         ) VALUES ('invalid-unicode-repository', 'v1', 1, $1, $2, 1,
                   'repository', U&'\00A0', 'cases.json', $3)",
    )
    .bind(corpus_digest.as_slice())
    .bind(replay_digest.as_slice())
    .bind(source_digest.as_slice())
    .execute(&pool)
    .await
    .expect_err("Unicode-whitespace-only durable repository provenance is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn repository_registration_requires_portable_relative_path() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let corpus_digest = [0_u8; 32];
    let replay_digest = [0_u8; 32];
    let source_digest = [0_u8; 32];

    sqlx::query(
        "INSERT INTO evaluation_corpus (
            corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
            source_kind, source_repository, source_path, source_sha256
         ) VALUES ('invalid-repository-path', 'v1', 1, $1, $2, 1,
                   'repository', 'KeenWill/signalbox', '../cases.json', $3)",
    )
    .bind(corpus_digest.as_slice())
    .bind(replay_digest.as_slice())
    .bind(source_digest.as_slice())
    .execute(&pool)
    .await
    .expect_err("a durable repository registration with parent traversal is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn repository_registration_rejects_windows_console_device_path() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    let corpus_digest = [0_u8; 32];
    let replay_digest = [0_u8; 32];
    let source_digest = [0_u8; 32];

    sqlx::query(
        "INSERT INTO evaluation_corpus (
            corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
            source_kind, source_repository, source_path, source_sha256
         ) VALUES ('invalid-console-device-path', 'v1', 1, $1, $2, 1,
                   'repository', 'KeenWill/signalbox', 'corpora/CONIN$.json', $3)",
    )
    .bind(corpus_digest.as_slice())
    .bind(replay_digest.as_slice())
    .bind(source_digest.as_slice())
    .execute(&pool)
    .await
    .expect_err("a durable Windows console-device path is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn repository_registration_rejects_oversized_path_component() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let corpus_digest = [0_u8; 32];
    let replay_digest = [0_u8; 32];
    let source_digest = [0_u8; 32];
    let oversized_component = "a".repeat(256);
    let source_path = format!("corpora/{oversized_component}");

    sqlx::query(
        "INSERT INTO evaluation_corpus (
            corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
            source_kind, source_repository, source_path, source_sha256
         ) VALUES ('invalid-oversized-path-component', 'v1', 1, $1, $2, 1,
                   'repository', 'KeenWill/signalbox', $3, $4)",
    )
    .bind(corpus_digest.as_slice())
    .bind(replay_digest.as_slice())
    .bind(&source_path)
    .bind(source_digest.as_slice())
    .execute(&pool)
    .await
    .expect_err("a durable path component beyond the portable byte bound is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stored_case_identity_requires_nonblank_text() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let database = DatabaseCorpusStore::new(pool.clone());
    let imported = database.import_manifest(seed_manifest_path()).await?;

    sqlx::query(
        "UPDATE evaluation_corpus_case
            SET case_id = '   '
          WHERE corpus_name = $1 AND corpus_version = $2 AND replay_position = 0",
    )
    .bind(&imported.key().name)
    .bind(&imported.key().version)
    .execute(&pool)
    .await
    .expect_err("a whitespace-only durable case identity is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stored_case_identity_rejects_control_characters() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let database = DatabaseCorpusStore::new(pool.clone());
    let imported = database.import_manifest(seed_manifest_path()).await?;

    sqlx::query(
        "UPDATE evaluation_corpus_case
            SET case_id = E'control\nidentity'
          WHERE corpus_name = $1 AND corpus_version = $2 AND replay_position = 0",
    )
    .bind(&imported.key().name)
    .bind(&imported.key().version)
    .execute(&pool)
    .await
    .expect_err("a control-bearing durable case identity is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stored_case_identity_rejects_unicode_control_character() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let database = DatabaseCorpusStore::new(pool.clone());
    let imported = database.import_manifest(seed_manifest_path()).await?;

    sqlx::query(
        "UPDATE evaluation_corpus_case
            SET case_id = U&'control\0085identity'
          WHERE corpus_name = $1 AND corpus_version = $2 AND replay_position = 0",
    )
    .bind(&imported.key().name)
    .bind(&imported.key().version)
    .execute(&pool)
    .await
    .expect_err("a C1-control-bearing durable case identity is rejected");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stored_case_decode_error_retains_serde_context() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let disk = DiskCorpusStore::open(seed_manifest_path())?;
    let registration = disk
        .enumerate()
        .await?
        .into_iter()
        .next()
        .expect("the seed store has one registration");
    let database = DatabaseCorpusStore::new(pool.clone());
    database.import_manifest(seed_manifest_path()).await?;
    sqlx::query(
        "UPDATE evaluation_corpus_case
            SET case_json = '{}'::jsonb
          WHERE corpus_name = $1 AND corpus_version = $2 AND replay_position = 0",
    )
    .bind(&registration.key().name)
    .bind(&registration.key().version)
    .execute(&pool)
    .await?;

    let error = database
        .load(registration.key())
        .await
        .expect_err("a malformed durable case fails closed");

    assert!(
        error.source().is_some(),
        "the serde decode source is retained"
    );
    assert!(error.to_string().contains("missing field"));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stored_case_identity_must_match_its_row_key() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let disk = DiskCorpusStore::open(seed_manifest_path())?;
    let registration = disk
        .enumerate()
        .await?
        .into_iter()
        .next()
        .expect("the seed store has one registration");
    let database = DatabaseCorpusStore::new(pool.clone());
    database.import_manifest(seed_manifest_path()).await?;
    sqlx::query(
        "UPDATE evaluation_corpus_case
            SET case_id = 'different-row-key'
          WHERE corpus_name = $1 AND corpus_version = $2 AND replay_position = 0",
    )
    .bind(&registration.key().name)
    .bind(&registration.key().version)
    .execute(&pool)
    .await?;

    let error = database
        .load(registration.key())
        .await
        .expect_err("a row key that disagrees with the case payload fails closed");

    assert!(error.to_string().contains("does not match its row key"));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "selected by the PostgreSQL integration suite"]
async fn direct_put_rejects_unverified_blob_registration() -> Result<(), Box<dyn Error>> {
    let disk = DiskCorpusStore::open(seed_manifest_path())?;
    let registration = disk
        .enumerate()
        .await?
        .into_iter()
        .next()
        .expect("the seed store has one registration");
    let corpus = disk.load(registration.key()).await?;
    let blob_registration = CorpusRegistration::new(
        CorpusKey {
            name: String::from("unverified-blob"),
            version: String::from("v1"),
        },
        CorpusSourceDescriptor::BlobReference {
            store: None,
            digest: Sha256Digest::from_bytes([0_u8; 32]),
            byte_length: 1,
        },
        &corpus,
    )?;
    let pool = PgPoolOptions::new().connect_lazy("postgres://localhost/signalbox_eval_corpus")?;
    let database = DatabaseCorpusStore::new(pool);

    let error = database
        .put(blob_registration, &corpus)
        .await
        .expect_err("blob metadata is rejected before database access");

    assert!(error.to_string().contains("no blob corpus backend"));
    Ok(())
}

#[tokio::test]
#[ignore = "selected by the PostgreSQL integration suite"]
async fn direct_put_rejects_unverified_repository_registration() -> Result<(), Box<dyn Error>> {
    let disk = DiskCorpusStore::open(seed_manifest_path())?;
    let registration = disk
        .enumerate()
        .await?
        .into_iter()
        .next()
        .expect("the seed store has one registration");
    let corpus = disk.load(registration.key()).await?;
    let repository_registration = CorpusRegistration::new(
        CorpusKey {
            name: String::from("unverified-repository"),
            version: String::from("v1"),
        },
        CorpusSourceDescriptor::Repository {
            repository: String::from("KeenWill/signalbox"),
            path: String::from("corpora/seed-v1.json"),
        },
        &corpus,
    )?;
    let pool = PgPoolOptions::new().connect_lazy("postgres://localhost/signalbox_eval_corpus")?;
    let database = DatabaseCorpusStore::new(pool);

    let error = database
        .put(repository_registration, &corpus)
        .await
        .expect_err("repository metadata is rejected before database access");

    assert!(error.to_string().contains("verified manifest"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn enumeration_does_not_decode_stored_cases() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let database = DatabaseCorpusStore::new(pool.clone());
    let imported = database.import_manifest(seed_manifest_path()).await?;
    sqlx::query(
        "UPDATE evaluation_corpus_case
            SET case_json = '{}'::jsonb
          WHERE corpus_name = $1 AND corpus_version = $2 AND replay_position = 0",
    )
    .bind(&imported.key().name)
    .bind(&imported.key().version)
    .execute(&pool)
    .await?;

    let registrations = database.enumerate().await?;

    assert_eq!(registrations, vec![imported]);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stored_blob_registration_is_rejected_before_case_loading() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let digest = [0_u8; 32];
    insert_blob_registration(&pool, "stored-blob", Some(&digest), Some("1")).await?;
    let database = DatabaseCorpusStore::new(pool.clone());
    let key = CorpusKey {
        name: String::from("constraint-fixture"),
        version: String::from("stored-blob"),
    };

    let error = database
        .load(&key)
        .await
        .expect_err("blob registrations are rejected before stored cases are read");

    assert!(error.to_string().contains("no blob corpus backend"));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stored_blob_registration_metadata_can_be_looked_up() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let digest = [0_u8; 32];
    let blob_byte_length = "1";
    let expected_blob_byte_length = blob_byte_length.parse::<u64>()?;
    insert_blob_registration(
        &pool,
        "metadata-only-blob",
        Some(&digest),
        Some(blob_byte_length),
    )
    .await?;
    let database = DatabaseCorpusStore::new(pool.clone());
    let key = CorpusKey {
        name: String::from("constraint-fixture"),
        version: String::from("metadata-only-blob"),
    };

    let registration = database.registration(&key).await?;
    let expected_source = CorpusSourceDescriptor::BlobReference {
        store: None,
        digest: Sha256Digest::from_bytes(digest),
        byte_length: expected_blob_byte_length,
    };

    assert_eq!(registration.key(), &key);
    assert_eq!(registration.source(), &expected_source);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stored_replay_order_must_match_its_durable_digest() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let disk = DiskCorpusStore::open(seed_manifest_path())?;
    let registration = disk
        .enumerate()
        .await?
        .into_iter()
        .next()
        .expect("the seed store has one registration");
    let database = DatabaseCorpusStore::new(pool.clone());
    database.import_manifest(seed_manifest_path()).await?;
    sqlx::query(
        "UPDATE evaluation_corpus_case
            SET replay_position = replay_position + 10
          WHERE corpus_name = $1 AND corpus_version = $2",
    )
    .bind(&registration.key().name)
    .bind(&registration.key().version)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE evaluation_corpus_case
            SET replay_position = 12 - replay_position
          WHERE corpus_name = $1 AND corpus_version = $2",
    )
    .bind(&registration.key().name)
    .bind(&registration.key().version)
    .execute(&pool)
    .await?;

    let error = database
        .load(registration.key())
        .await
        .expect_err("reordered durable cases fail their replay identity");

    assert!(error.to_string().contains("replay order"));

    pool.close().await;
    drop(container);
    Ok(())
}

async fn insert_blob_registration(
    pool: &PgPool,
    corpus_version: &str,
    blob_digest: Option<&[u8]>,
    blob_byte_length: Option<&str>,
) -> Result<(), sqlx::Error> {
    let corpus_digest = [0_u8; 32];
    let replay_digest = [0_u8; 32];
    sqlx::query(
        "INSERT INTO evaluation_corpus (
            corpus_name, corpus_version, format_version, corpus_digest, replay_digest, case_count,
            source_kind, source_blob_digest, source_blob_byte_length
         ) VALUES ('constraint-fixture', $1, 1, $2, $3, 1, 'blob_reference', $4, $5::numeric)",
    )
    .bind(corpus_version)
    .bind(corpus_digest.as_slice())
    .bind(replay_digest.as_slice())
    .bind(blob_digest)
    .bind(blob_byte_length)
    .execute(pool)
    .await
    .map(|_| ())
}

fn fixture_model() -> (
    RuntimeApprovalJudgeModel<ScriptedModel<ModelCallId>>,
    ApprovalJudgeEvalBinding,
) {
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(40)));
    let catalog = RuntimeModelCatalog::try_from_definitions([RuntimeModelDefinition::try_new(
        target,
        String::from(PROVIDER_MODEL),
        FIXTURE_MAX_OUTPUT_TOKENS,
        FIXTURE_CONTEXT_WINDOW_TOKENS,
    )
    .expect("the fixture model definition is request-safe")])
    .expect("the fixture catalog names one target once");
    let scripts = [
        scripted_decision(
            ApprovalDisposition::Approve,
            "The exact read is plainly within the grant.",
        ),
        scripted_decision(
            ApprovalDisposition::Deny,
            "The request crosses the named branch boundary.",
        ),
        scripted_decision(
            ApprovalDisposition::EscalateToHuman,
            "The goal is absent, so the request stays parked.",
        ),
    ];
    (
        RuntimeApprovalJudgeModel::new(ScriptedModel::following(scripts), catalog),
        ApprovalJudgeEvalBinding {
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(41)),
            target,
            credential_reference: String::from("offline-fixture-credential"),
        },
    )
}

fn scripted_decision(disposition: ApprovalDisposition, rationale: &str) -> Script {
    let arguments_json = serde_json::json!({
        "recommendation": disposition.as_str(),
        "rationale": rationale,
    })
    .to_string();
    Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new(PROVIDER_MODEL)),
        finish: CompletionFinish::ToolUse,
        content: vec![AssistantPart::ToolCall(ToolCallProposal {
            id: ToolCallId::new("offline_fixture_decision"),
            name: ToolName::new("tool_approval_decision"),
            arguments_json,
        })],
        usage: TokenUsage::unreported(),
    }))
}
