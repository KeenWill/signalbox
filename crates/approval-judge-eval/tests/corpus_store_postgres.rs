//! Real-PostgreSQL conformance and import coverage for corpus stores.

#![allow(
    clippy::expect_used,
    reason = "this integration test uses fixed synthetic fixtures"
)]

use std::{error::Error, path::Path};

use signalbox_approval_judge_eval::{
    ApprovalDisposition, ApprovalJudgeCorpus, CorpusKey, CorpusRegistration, CorpusStore,
    DatabaseCorpusStore, DiskCorpusStore, score_corpus,
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
    disposable_test_container_labels, local_test_connection_options, migrate,
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
        .key
        .clone();
    let database = DatabaseCorpusStore::new(pool.clone());
    let imported = database.import_manifest(seed_manifest_path()).await?;
    let repeated_import = database.import_manifest(seed_manifest_path()).await?;
    let disk_observation = observe_store(&disk, &key).await?;
    let database_observation = observe_store(&database, &key).await?;
    let (disk_model, disk_binding) = fixture_model();
    let (database_model, database_binding) = fixture_model();
    let disk_score = score_corpus(&disk_model, &disk_binding, &disk_observation.1).await?;
    let database_score =
        score_corpus(&database_model, &database_binding, &database_observation.1).await?;

    assert_eq!(imported, disk_observation.0[0]);
    assert_eq!(repeated_import, imported);
    assert_eq!(database_observation, disk_observation);
    assert_eq!(database_score, disk_score);

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

async fn insert_blob_registration(
    pool: &PgPool,
    corpus_version: &str,
    blob_digest: Option<&[u8]>,
    blob_byte_length: Option<&str>,
) -> Result<(), sqlx::Error> {
    let corpus_digest = [0_u8; 32];
    sqlx::query(
        "INSERT INTO evaluation_corpus (
            corpus_name, corpus_version, format_version, corpus_digest, case_count,
            source_kind, source_blob_digest, source_blob_byte_length
         ) VALUES ('constraint-fixture', $1, 1, $2, 0, 'blob_reference', $3, $4::numeric)",
    )
    .bind(corpus_version)
    .bind(corpus_digest.as_slice())
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
        256,
        4_096,
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
