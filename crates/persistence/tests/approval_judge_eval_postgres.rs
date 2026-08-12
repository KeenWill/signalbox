//! Feature-gated PostgreSQL coverage for approval-judge eval-run recording.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::error::Error;

use rust_decimal::Decimal;
use signalbox_domain::{
    DelegateApprovalRecommendation, DirectModelSelection, ProviderReportedTokenUsage,
};
use signalbox_persistence::{
    approval_judge_eval::{
        ApprovalJudgeEvalCallRecord, ApprovalJudgeEvalRecordingError, ApprovalJudgeEvalRunId,
        ApprovalJudgeEvalRunRecord, record_eval_run,
    },
    disposable_test_container_labels, local_test_connection_options, migrate,
};
use sqlx::{
    PgPool, Row,
    postgres::PgPoolOptions,
    types::{Json, Uuid},
};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_approval_judge_eval";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

const RUN_IDENTITY: u128 = 0xae10;
const SELECTION_IDENTITY: u128 = 0xae20;
const ABSENT_RUN_IDENTITY: u128 = 0xae30;
const PROVIDER_MODEL: &str = "fixture-judge-model";
const CORPUS_DIGEST: &str = "fnv1a128:00000000000000000000000000000001";
const CONTRACT_DIGEST: &str = "fnv1a128:00000000000000000000000000000002";
const RENDERED_DIGEST: &str = "fnv1a128:00000000000000000000000000000003";
const REPEATS: u32 = 3;
const FIRST_CASE: &str = "fixture-push-own-branch";
// The second call replays another case at ordinal 3: its second attempt
// failed and recorded nothing, which is exactly the gap the ordinal keeps.
const SECOND_CASE: &str = "fixture-credential-read";
const SECOND_CALL_ORDINAL: u32 = 3;
const FIRST_RATIONALE: &str = "the grant names this branch";
const SECOND_RATIONALE: &str = "credential reads have no footing in any grant";
const INPUT_TOKENS: u64 = 1200;
const OUTPUT_TOKENS: u64 = 90;
const CACHE_CREATION_TOKENS: u64 = 40;
const CACHE_READ_TOKENS: u64 = 7;
// The exact stored spellings the closed recommendation constraint admits.
const APPROVE_SPELLING: &str = "approve";
const DENY_SPELLING: &str = "deny";
const FOREIGN_RECOMMENDATION_SPELLING: &str = "maybe";

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

fn scorecard() -> serde_json::Value {
    serde_json::json!({
        "total_cases": 2,
        "correct_majorities": 1,
    })
}

fn run_record() -> ApprovalJudgeEvalRunRecord {
    ApprovalJudgeEvalRunRecord {
        run: ApprovalJudgeEvalRunId::from_uuid(Uuid::from_u128(RUN_IDENTITY)),
        selection: DirectModelSelection::from_uuid(Uuid::from_u128(SELECTION_IDENTITY)),
        provider_model: String::from(PROVIDER_MODEL),
        corpus_digest: String::from(CORPUS_DIGEST),
        contract_digest: String::from(CONTRACT_DIGEST),
        rendered_digest: String::from(RENDERED_DIGEST),
        repeats: REPEATS,
        scorecard: scorecard(),
    }
}

fn first_call() -> ApprovalJudgeEvalCallRecord {
    ApprovalJudgeEvalCallRecord {
        case_name: String::from(FIRST_CASE),
        repeat_ordinal: 1,
        recommendation: DelegateApprovalRecommendation::Approve,
        rationale: String::from(FIRST_RATIONALE),
        usage: ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(INPUT_TOKENS))
            .with_output_tokens(Some(OUTPUT_TOKENS))
            .with_cache_creation_input_tokens(Some(CACHE_CREATION_TOKENS))
            .with_cache_read_input_tokens(Some(CACHE_READ_TOKENS)),
    }
}

fn second_call() -> ApprovalJudgeEvalCallRecord {
    ApprovalJudgeEvalCallRecord {
        case_name: String::from(SECOND_CASE),
        repeat_ordinal: SECOND_CALL_ORDINAL,
        recommendation: DelegateApprovalRecommendation::Deny,
        rationale: String::from(SECOND_RATIONALE),
        usage: ProviderReportedTokenUsage::unreported(),
    }
}

fn constraint_name(error: &ApprovalJudgeEvalRecordingError) -> Option<String> {
    let ApprovalJudgeEvalRecordingError::Database { source, .. } = error;
    source
        .as_database_error()
        .and_then(|database| database.constraint())
        .map(ToOwned::to_owned)
}

fn raw_constraint_name(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(|database| database.constraint())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recorded_run_and_calls_reread_exactly() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let run = run_record();
    record_eval_run(&pool, &run, &[first_call(), second_call()]).await?;

    let stored_run = sqlx::query(
        "SELECT direct_model_selection_id, provider_model, corpus_digest,
                contract_digest, rendered_digest, repeats, scorecard
           FROM approval_judge_eval_run WHERE eval_run_id = $1",
    )
    .bind(run.run.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored_run.try_get::<Uuid, _>("direct_model_selection_id")?,
        run.selection.into_uuid()
    );
    assert_eq!(
        stored_run.try_get::<String, _>("provider_model")?,
        run.provider_model
    );
    assert_eq!(
        stored_run.try_get::<String, _>("corpus_digest")?,
        run.corpus_digest
    );
    assert_eq!(
        stored_run.try_get::<String, _>("contract_digest")?,
        run.contract_digest
    );
    assert_eq!(
        stored_run.try_get::<String, _>("rendered_digest")?,
        run.rendered_digest
    );
    assert_eq!(
        stored_run.try_get::<Decimal, _>("repeats")?,
        Decimal::from(run.repeats)
    );
    assert_eq!(
        stored_run
            .try_get::<Json<serde_json::Value>, _>("scorecard")?
            .0,
        run.scorecard
    );

    let first = first_call();
    let stored_first = sqlx::query(
        "SELECT recommendation_kind, rationale, input_tokens, output_tokens,
                cache_creation_input_tokens, cache_read_input_tokens
           FROM approval_judge_eval_call
          WHERE eval_run_id = $1 AND case_name = $2 AND repeat_ordinal = $3",
    )
    .bind(run.run.into_uuid())
    .bind(first.case_name.as_str())
    .bind(Decimal::from(first.repeat_ordinal))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored_first.try_get::<String, _>("recommendation_kind")?,
        APPROVE_SPELLING
    );
    assert_eq!(
        stored_first.try_get::<String, _>("rationale")?,
        first.rationale
    );
    assert_eq!(
        stored_first.try_get::<Option<Decimal>, _>("input_tokens")?,
        first.usage.input_tokens().map(Decimal::from)
    );
    assert_eq!(
        stored_first.try_get::<Option<Decimal>, _>("output_tokens")?,
        first.usage.output_tokens().map(Decimal::from)
    );
    assert_eq!(
        stored_first.try_get::<Option<Decimal>, _>("cache_creation_input_tokens")?,
        first.usage.cache_creation_input_tokens().map(Decimal::from)
    );
    assert_eq!(
        stored_first.try_get::<Option<Decimal>, _>("cache_read_input_tokens")?,
        first.usage.cache_read_input_tokens().map(Decimal::from)
    );

    let second = second_call();
    let stored_second = sqlx::query(
        "SELECT recommendation_kind, rationale, input_tokens, output_tokens,
                cache_creation_input_tokens, cache_read_input_tokens
           FROM approval_judge_eval_call
          WHERE eval_run_id = $1 AND case_name = $2 AND repeat_ordinal = $3",
    )
    .bind(run.run.into_uuid())
    .bind(second.case_name.as_str())
    .bind(Decimal::from(second.repeat_ordinal))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored_second.try_get::<String, _>("recommendation_kind")?,
        DENY_SPELLING
    );
    assert_eq!(
        stored_second.try_get::<String, _>("rationale")?,
        second.rationale
    );
    assert_eq!(
        stored_second.try_get::<Option<Decimal>, _>("input_tokens")?,
        second.usage.input_tokens().map(Decimal::from)
    );
    assert_eq!(
        stored_second.try_get::<Option<Decimal>, _>("output_tokens")?,
        second.usage.output_tokens().map(Decimal::from)
    );
    assert_eq!(
        stored_second.try_get::<Option<Decimal>, _>("cache_creation_input_tokens")?,
        second
            .usage
            .cache_creation_input_tokens()
            .map(Decimal::from)
    );
    assert_eq!(
        stored_second.try_get::<Option<Decimal>, _>("cache_read_input_tokens")?,
        second.usage.cache_read_input_tokens().map(Decimal::from)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_inadmissible_call_row_leaves_no_run_row() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let run = run_record();
    let mut broken = first_call();
    broken.rationale = String::new();
    let error = record_eval_run(&pool, &run, &[broken])
        .await
        .expect_err("an empty rationale is rejected");
    assert_eq!(
        constraint_name(&error).as_deref(),
        Some("approval_judge_eval_call_rationale_bounded")
    );
    let stored_runs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM approval_judge_eval_run WHERE eval_run_id = $1")
            .bind(run.run.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(stored_runs, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn call_rows_require_their_recorded_run() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let orphan = first_call();
    let error = sqlx::query(
        "INSERT INTO approval_judge_eval_call
            (eval_run_id, case_name, repeat_ordinal, recommendation_kind,
             rationale)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::from_u128(ABSENT_RUN_IDENTITY))
    .bind(orphan.case_name.as_str())
    .bind(Decimal::from(orphan.repeat_ordinal))
    .bind(APPROVE_SPELLING)
    .bind(orphan.rationale.as_str())
    .execute(&pool)
    .await
    .expect_err("a call row without its run row is rejected");
    assert_eq!(
        raw_constraint_name(&error),
        Some("approval_judge_eval_call_run_fk")
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn foreign_recommendation_spellings_are_rejected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let run = run_record();
    record_eval_run(&pool, &run, &[]).await?;
    let call = first_call();
    let error = sqlx::query(
        "INSERT INTO approval_judge_eval_call
            (eval_run_id, case_name, repeat_ordinal, recommendation_kind,
             rationale)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(run.run.into_uuid())
    .bind(call.case_name.as_str())
    .bind(Decimal::from(call.repeat_ordinal))
    .bind(FOREIGN_RECOMMENDATION_SPELLING)
    .bind(call.rationale.as_str())
    .execute(&pool)
    .await
    .expect_err("a recommendation outside the closed set is rejected");
    assert_eq!(
        raw_constraint_name(&error),
        Some("approval_judge_eval_call_recommendation_closed")
    );
    Ok(())
}
