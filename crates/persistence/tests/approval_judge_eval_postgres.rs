//! Feature-gated PostgreSQL coverage for approval-judge eval-run recording.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::error::Error;

use rust_decimal::Decimal;
use signalbox_domain::{
    DelegateApprovalRecommendation, DirectModelSelection, ProviderModelIdentity,
    ProviderReportedTokenUsage, ResolvedProviderTarget, ToolDecisionRationale,
};
use signalbox_persistence::{
    approval_judge_eval::{
        APPROVAL_JUDGE_EVAL_SCORING_SEMANTICS_VERSION, ApprovalJudgeEvalCallRecord,
        ApprovalJudgeEvalRecordingError, ApprovalJudgeEvalRunId, ApprovalJudgeEvalRunRecord,
        record_eval_run as record_eval_run_in_schema, verify_recording_schema,
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
const SECOND_RUN_IDENTITY: u128 = 0xae11;
const SELECTION_IDENTITY: u128 = 0xae20;
const TARGET_IDENTITY: u128 = 0xae21;
const ABSENT_RUN_IDENTITY: u128 = 0xae30;
const PROVIDER_MODEL: &str = "fixture-judge-model";
const CREDENTIAL_REFERENCE: &str = "fixture-credential";
// The ordinal a post-commit append would claim: still inside the configured
// repeats and unoccupied, so only the sealing trigger can reject it.
const LATE_CALL_ORDINAL: u32 = 2;
// A spelling the recorded run never carries, for contradiction fixtures.
const FOREIGN_PROVIDER_MODEL: &str = "some-other-model";
// The login role granted SELECT but not INSERT on the eval tables.
const RESTRICTED_ROLE: &str = "eval_reader";
// The login role granted INSERT but not the SELECT the sealing trigger reads.
const INSERT_ONLY_ROLE: &str = "eval_writer";
// A transaction identity no fixture transaction ever holds, for forgery
// fixtures; the stamping trigger must overwrite it.
const FOREIGN_TRANSACTION_ID: &str = "42";
// The ERRCODE reject_immutable_record_change raises for every refused change.
const CHECK_VIOLATION_CODE: &str = "23514";
const CORPUS_DIGEST: &str = "fnv1a128:00000000000000000000000000000001";
const CONTRACT_DIGEST: &str = "fnv1a128:00000000000000000000000000000002";
const RENDERED_DIGEST: &str = "fnv1a128:00000000000000000000000000000003";
const REPEATS: u32 = 3;
// One past the configured repeats: the smallest ordinal the recording
// repository must reject as evidence for an unconfigured attempt.
const OVERFLOWING_ORDINAL: u32 = REPEATS + 1;
const CACHE_INCLUSIVE_INPUT: bool = true;
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
const FAILURE_CAUSES: [&str; 2] = ["fixture timeout", "fixture transport failure"];
const SHADOW_SCHEMA: &str = "shadow_approval_judge_eval";

async fn unmigrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
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
    Ok((container, pool))
}

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let (container, pool) = unmigrated_postgres().await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

async fn migrated_postgres_in_configured_schema()
-> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let (container, bootstrap) = unmigrated_postgres().await?;
    sqlx::query("CREATE SCHEMA configured_approval_judge_eval AUTHORIZATION signalbox")
        .execute(&bootstrap)
        .await?;
    sqlx::query(
        "ALTER ROLE signalbox
         SET search_path TO configured_approval_judge_eval",
    )
    .execute(&bootstrap)
    .await?;
    let connection_options = bootstrap.connect_options().clone();
    bootstrap.close().await;

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with((*connection_options).clone())
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

struct VerdictFixture<'a> {
    recommendation: &'a str,
    rationale: &'a str,
}

struct CaseSummaryExpectations {
    verdict_counts: serde_json::Value,
    majority: Option<&'static str>,
    measured: bool,
    complete: bool,
    stable: Option<bool>,
    tied: bool,
    correct: bool,
}

fn first_partial_case_summary() -> CaseSummaryExpectations {
    CaseSummaryExpectations {
        verdict_counts: serde_json::json!({"approve": 1}),
        majority: None,
        measured: true,
        complete: false,
        stable: None,
        tied: false,
        correct: false,
    }
}

fn second_partial_case_summary() -> CaseSummaryExpectations {
    CaseSummaryExpectations {
        verdict_counts: serde_json::json!({"deny": 1}),
        majority: None,
        measured: true,
        complete: false,
        stable: None,
        tied: false,
        correct: false,
    }
}

fn verdict_entry(fixture: VerdictFixture<'_>) -> serde_json::Value {
    serde_json::json!({
        "recommendation": fixture.recommendation,
        "rationale": fixture.rationale,
    })
}

fn scorecard_case(
    name: &str,
    repeats: &[serde_json::Value],
    failed_calls: u32,
    failure_causes: &[&str],
    summary: CaseSummaryExpectations,
) -> serde_json::Value {
    let (category, expected) = match name {
        FIRST_CASE => ("git_push", APPROVE_SPELLING),
        SECOND_CASE => ("credential_access", DENY_SPELLING),
        other => panic!("unknown scorecard case fixture: {other}"),
    };
    serde_json::json!({
        "name": name,
        "category": category,
        "expected": expected,
        "repeats": repeats,
        "failed_calls": failed_calls,
        "failure_causes": failure_causes,
        "verdict_counts": summary.verdict_counts,
        "majority": summary.majority,
        "measured": summary.measured,
        "complete": summary.complete,
        "stable": summary.stable,
        "tied": summary.tied,
        "correct": summary.correct,
    })
}

struct AggregateExpectations {
    total_cases: u64,
    correct_majorities: u64,
    unstable_cases: u64,
    stability_unmeasured_cases: u64,
    partial_cases: u64,
    unmeasured_cases: u64,
    categories: Vec<serde_json::Value>,
}

fn no_case_aggregates() -> AggregateExpectations {
    AggregateExpectations {
        total_cases: 0,
        correct_majorities: 0,
        unstable_cases: 0,
        stability_unmeasured_cases: 0,
        partial_cases: 0,
        unmeasured_cases: 0,
        categories: vec![],
    }
}

fn first_partial_case_aggregates() -> AggregateExpectations {
    AggregateExpectations {
        total_cases: 1,
        correct_majorities: 0,
        unstable_cases: 0,
        stability_unmeasured_cases: 1,
        partial_cases: 1,
        unmeasured_cases: 0,
        categories: vec![serde_json::json!({
            "category": "git_push",
            "cases": 1,
            "correct_majorities": 0,
            "unstable_cases": 0,
            "stability_unmeasured_cases": 1,
            "partial_cases": 1,
            "unmeasured_cases": 0,
            "failed_calls": 2,
        })],
    }
}

fn second_partial_case_aggregates() -> AggregateExpectations {
    AggregateExpectations {
        total_cases: 1,
        correct_majorities: 0,
        unstable_cases: 0,
        stability_unmeasured_cases: 1,
        partial_cases: 1,
        unmeasured_cases: 0,
        categories: vec![serde_json::json!({
            "category": "credential_access",
            "cases": 1,
            "correct_majorities": 0,
            "unstable_cases": 0,
            "stability_unmeasured_cases": 1,
            "partial_cases": 1,
            "unmeasured_cases": 0,
            "failed_calls": 2,
        })],
    }
}

fn both_partial_case_aggregates() -> AggregateExpectations {
    AggregateExpectations {
        total_cases: 2,
        correct_majorities: 0,
        unstable_cases: 0,
        stability_unmeasured_cases: 2,
        partial_cases: 2,
        unmeasured_cases: 0,
        categories: vec![
            serde_json::json!({
                "category": "credential_access",
                "cases": 1,
                "correct_majorities": 0,
                "unstable_cases": 0,
                "stability_unmeasured_cases": 1,
                "partial_cases": 1,
                "unmeasured_cases": 0,
                "failed_calls": 2,
            }),
            serde_json::json!({
                "category": "git_push",
                "cases": 1,
                "correct_majorities": 0,
                "unstable_cases": 0,
                "stability_unmeasured_cases": 1,
                "partial_cases": 1,
                "unmeasured_cases": 0,
                "failed_calls": 2,
            }),
        ],
    }
}

// The scorecard restates run headers and verdicts from the fixture constants,
// while each scenario supplies its aggregate expectations as explicit values.
fn scorecard_with_cases(
    cases: &[serde_json::Value],
    aggregates: AggregateExpectations,
) -> serde_json::Value {
    serde_json::json!({
        "judge_selection": Uuid::from_u128(SELECTION_IDENTITY).to_string(),
        "provider_model": PROVIDER_MODEL,
        "corpus_digest": CORPUS_DIGEST,
        "contract_digest": CONTRACT_DIGEST,
        "rendered_digest": RENDERED_DIGEST,
        "repeats": REPEATS,
        "scoring_semantics_version": APPROVAL_JUDGE_EVAL_SCORING_SEMANTICS_VERSION,
        "total_cases": aggregates.total_cases,
        "correct_majorities": aggregates.correct_majorities,
        "unstable_cases": aggregates.unstable_cases,
        "stability_unmeasured_cases": aggregates.stability_unmeasured_cases,
        "partial_cases": aggregates.partial_cases,
        "unmeasured_cases": aggregates.unmeasured_cases,
        "escalation_calibration": {
            "expected_cases": 0,
            "observed_majorities": 0,
            "missed": 0,
            "excess": 0,
        },
        "categories": aggregates.categories,
        "cases": cases,
    })
}

fn both_call_scorecard_cases() -> Vec<serde_json::Value> {
    vec![
        scorecard_case(
            FIRST_CASE,
            &[verdict_entry(VerdictFixture {
                recommendation: APPROVE_SPELLING,
                rationale: FIRST_RATIONALE,
            })],
            2,
            &FAILURE_CAUSES,
            first_partial_case_summary(),
        ),
        scorecard_case(
            SECOND_CASE,
            &[verdict_entry(VerdictFixture {
                recommendation: DENY_SPELLING,
                rationale: SECOND_RATIONALE,
            })],
            2,
            &FAILURE_CAUSES,
            second_partial_case_summary(),
        ),
    ]
}

fn run_record_with(
    id: u128,
    cases: &[serde_json::Value],
    aggregates: AggregateExpectations,
) -> ApprovalJudgeEvalRunRecord {
    ApprovalJudgeEvalRunRecord {
        run: ApprovalJudgeEvalRunId::from_uuid(Uuid::from_u128(id)),
        selection: DirectModelSelection::from_uuid(Uuid::from_u128(SELECTION_IDENTITY)),
        target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(
            TARGET_IDENTITY,
        ))),
        provider_model: String::from(PROVIDER_MODEL),
        credential_reference: String::from(CREDENTIAL_REFERENCE),
        usage_input_includes_cache_tokens: CACHE_INCLUSIVE_INPUT,
        corpus_digest: String::from(CORPUS_DIGEST),
        contract_digest: String::from(CONTRACT_DIGEST),
        rendered_digest: String::from(RENDERED_DIGEST),
        repeats: REPEATS,
        scorecard: scorecard_with_cases(cases, aggregates),
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

async fn record_eval_run(
    pool: &PgPool,
    run: &ApprovalJudgeEvalRunRecord,
    calls: &[ApprovalJudgeEvalCallRecord],
) -> Result<(), ApprovalJudgeEvalRecordingError> {
    let schema = verify_recording_schema(pool).await?;
    record_eval_run_in_schema(pool, &schema, run, calls).await
}

#[track_caller]
fn constraint_name(error: &ApprovalJudgeEvalRecordingError) -> Option<String> {
    match error {
        ApprovalJudgeEvalRecordingError::Database { source, .. } => source
            .as_database_error()
            .and_then(|database| database.constraint())
            .map(ToOwned::to_owned),
        other => panic!("expected a database constraint failure, found {other:?}"),
    }
}

#[track_caller]
fn expect_tables_absent(error: &ApprovalJudgeEvalRecordingError) {
    match error {
        ApprovalJudgeEvalRecordingError::TablesAbsent => (),
        other => panic!("expected absent recording tables, found {other:?}"),
    }
}

#[track_caller]
fn expect_tables_unwritable(error: &ApprovalJudgeEvalRecordingError) {
    match error {
        ApprovalJudgeEvalRecordingError::TablesUnwritable => (),
        other => panic!("expected unwritable recording tables, found {other:?}"),
    }
}

#[track_caller]
fn expect_call_outside_configured_repeats(error: &ApprovalJudgeEvalRecordingError) {
    match error {
        ApprovalJudgeEvalRecordingError::CallOutsideConfiguredRepeats => (),
        other => panic!("expected an out-of-range call ordinal rejection, found {other:?}"),
    }
}

#[track_caller]
fn expect_scorecard_header_mismatch(error: &ApprovalJudgeEvalRecordingError, field: &str) {
    match error {
        ApprovalJudgeEvalRecordingError::ScorecardHeaderMismatch { field: diverged } => {
            assert_eq!(*diverged, field);
        }
        other => panic!("expected a scorecard header mismatch, found {other:?}"),
    }
}

#[track_caller]
fn expect_scorecard_verdict_mismatch(error: &ApprovalJudgeEvalRecordingError, case: &str) {
    match error {
        ApprovalJudgeEvalRecordingError::ScorecardVerdictMismatch { case: diverged } => {
            assert_eq!(diverged, case);
        }
        other => panic!("expected a scorecard verdict mismatch, found {other:?}"),
    }
}

fn raw_constraint_name(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(|database| database.constraint())
}

fn raw_error_code(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|database| database.code())
        .map(|code| code.into_owned())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recorded_run_and_calls_reread_exactly() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let run = run_record_with(
        RUN_IDENTITY,
        &both_call_scorecard_cases(),
        both_partial_case_aggregates(),
    );
    record_eval_run(&pool, &run, &[first_call(), second_call()]).await?;

    let stored_run = sqlx::query(
        "SELECT direct_model_selection_id, resolved_provider_model_identity_id,
                provider_model, credential_reference,
                usage_input_includes_cache_tokens, corpus_digest,
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
        stored_run.try_get::<Uuid, _>("resolved_provider_model_identity_id")?,
        run.target.identity().into_uuid()
    );
    assert_eq!(
        stored_run.try_get::<String, _>("provider_model")?,
        run.provider_model
    );
    assert_eq!(
        stored_run.try_get::<String, _>("credential_reference")?,
        run.credential_reference
    );
    assert_eq!(
        stored_run.try_get::<bool, _>("usage_input_includes_cache_tokens")?,
        run.usage_input_includes_cache_tokens
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
    let run = run_record_with(
        RUN_IDENTITY,
        &[scorecard_case(
            FIRST_CASE,
            &[verdict_entry(VerdictFixture {
                recommendation: APPROVE_SPELLING,
                rationale: "",
            })],
            2,
            &FAILURE_CAUSES,
            first_partial_case_summary(),
        )],
        first_partial_case_aggregates(),
    );
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
    // The sealing trigger fires before the foreign key can, so the missing
    // run surfaces as its check violation rather than the FK constraint.
    assert_eq!(
        raw_error_code(&error).as_deref(),
        Some(CHECK_VIOLATION_CODE)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recording_requires_the_daemon_applied_schema() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = unmigrated_postgres().await?;
    let absent = verify_recording_schema(&pool)
        .await
        .expect_err("an unmigrated database has no recording tables");
    expect_tables_absent(&absent);
    migrate(&pool).await?;
    verify_recording_schema(&pool).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recording_requires_every_evidence_trigger_to_be_active() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    sqlx::query(
        "ALTER TABLE approval_judge_eval_run
         DISABLE TRIGGER approval_judge_eval_run_is_append_only",
    )
    .execute(&pool)
    .await?;
    let absent = verify_recording_schema(&pool)
        .await
        .expect_err("a disabled append-only trigger makes recording unavailable");
    expect_tables_absent(&absent);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recording_uses_the_configured_migration_schema() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres_in_configured_schema().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE SCHEMA {SHADOW_SCHEMA} AUTHORIZATION signalbox"
    )))
    .execute(&pool)
    .await?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE {SHADOW_SCHEMA}.approval_judge_eval_run
             (LIKE configured_approval_judge_eval.approval_judge_eval_run INCLUDING ALL);
         CREATE TABLE {SHADOW_SCHEMA}.approval_judge_eval_call
             (LIKE configured_approval_judge_eval.approval_judge_eval_call INCLUDING ALL);
         ALTER ROLE signalbox SET search_path TO {SHADOW_SCHEMA}"
    )))
    .execute(&pool)
    .await?;
    let connection_options = pool.connect_options().clone();
    pool.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with((*connection_options).clone())
        .await?;
    let schema = verify_recording_schema(&pool).await?;
    let run = run_record_with(
        RUN_IDENTITY,
        &both_call_scorecard_cases(),
        both_partial_case_aggregates(),
    );
    record_eval_run_in_schema(&pool, &schema, &run, &[first_call(), second_call()]).await?;

    let current_schema: String = sqlx::query_scalar("SELECT current_schema()")
        .fetch_one(&pool)
        .await?;
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM configured_approval_judge_eval.approval_judge_eval_run
          WHERE eval_run_id = $1",
    )
    .bind(run.run.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(current_schema, SHADOW_SCHEMA);
    assert_eq!(stored, 1);
    let shadow_query = sqlx::AssertSqlSafe(format!(
        "SELECT count(*)
           FROM {SHADOW_SCHEMA}.approval_judge_eval_run
          WHERE eval_run_id = $1"
    ));
    let shadow_stored: i64 = sqlx::query_scalar(shadow_query)
        .bind(run.run.into_uuid())
        .fetch_one(&pool)
        .await?;
    assert_eq!(shadow_stored, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn call_ordinals_outside_the_run_repeats_are_rejected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let run = run_record_with(
        RUN_IDENTITY,
        &[scorecard_case(
            FIRST_CASE,
            &[verdict_entry(VerdictFixture {
                recommendation: APPROVE_SPELLING,
                rationale: FIRST_RATIONALE,
            })],
            2,
            &FAILURE_CAUSES,
            first_partial_case_summary(),
        )],
        first_partial_case_aggregates(),
    );
    let mut overflowing = first_call();
    overflowing.repeat_ordinal = OVERFLOWING_ORDINAL;
    let error = record_eval_run(&pool, &run, &[overflowing])
        .await
        .expect_err("an ordinal past the run's repeats is rejected");
    expect_call_outside_configured_repeats(&error);
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
async fn recording_requires_insert_privileges() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    // Fixture DDL interpolates only this file's constants, never external
    // input, so asserting SQL safety is sound here.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE ROLE {RESTRICTED_ROLE} LOGIN PASSWORD '{DATABASE_PASSWORD}'"
    )))
    .execute(&pool)
    .await?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "GRANT SELECT ON approval_judge_eval_run, approval_judge_eval_call TO {RESTRICTED_ROLE}"
    )))
    .execute(&pool)
    .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let restricted_url =
        format!("postgres://{RESTRICTED_ROLE}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let restricted = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(local_test_connection_options(&restricted_url)?)
        .await?;
    let unwritable = verify_recording_schema(&restricted)
        .await
        .expect_err("a role without INSERT cannot claim a recordable schema");
    expect_tables_unwritable(&unwritable);
    // INSERT alone is not enough either: the sealing trigger reads the run
    // row while admitting each call, so recording also needs SELECT there.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE ROLE {INSERT_ONLY_ROLE} LOGIN PASSWORD '{DATABASE_PASSWORD}'"
    )))
    .execute(&pool)
    .await?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "GRANT INSERT ON approval_judge_eval_run, approval_judge_eval_call TO {INSERT_ONLY_ROLE}"
    )))
    .execute(&pool)
    .await?;
    let insert_only_url =
        format!("postgres://{INSERT_ONLY_ROLE}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let insert_only = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(local_test_connection_options(&insert_only_url)?)
        .await?;
    let unreadable = verify_recording_schema(&insert_only)
        .await
        .expect_err("a role without SELECT on the run table cannot record sealed calls");
    expect_tables_unwritable(&unreadable);
    verify_recording_schema(&pool).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn contradictory_scorecard_headers_are_rejected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let mut run = run_record_with(RUN_IDENTITY, &[], no_case_aggregates());
    run.scorecard["provider_model"] = serde_json::json!(FOREIGN_PROVIDER_MODEL);
    let error = record_eval_run(&pool, &run, &[])
        .await
        .expect_err("a scorecard contradicting the typed record is rejected");
    expect_scorecard_header_mismatch(&error, "provider_model");
    let stored_runs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM approval_judge_eval_run WHERE eval_run_id = $1")
            .bind(run.run.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(stored_runs, 0);

    let mut wrong_semantics = run_record_with(RUN_IDENTITY, &[], no_case_aggregates());
    wrong_semantics.scorecard["scoring_semantics_version"] = serde_json::json!(0);
    let error = record_eval_run(&pool, &wrong_semantics, &[])
        .await
        .expect_err("a scorecard with foreign scoring semantics is rejected");
    expect_scorecard_header_mismatch(&error, "scoring_semantics_version");
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recorded_evidence_is_append_only() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let run = run_record_with(
        RUN_IDENTITY,
        &[scorecard_case(
            FIRST_CASE,
            &[verdict_entry(VerdictFixture {
                recommendation: APPROVE_SPELLING,
                rationale: FIRST_RATIONALE,
            })],
            2,
            &FAILURE_CAUSES,
            first_partial_case_summary(),
        )],
        first_partial_case_aggregates(),
    );
    record_eval_run(&pool, &run, &[first_call()]).await?;

    let updated_run = sqlx::query("UPDATE approval_judge_eval_run SET provider_model = $1")
        .bind(FOREIGN_PROVIDER_MODEL)
        .execute(&pool)
        .await
        .expect_err("a recorded run refuses updates");
    assert_eq!(
        raw_error_code(&updated_run).as_deref(),
        Some(CHECK_VIOLATION_CODE)
    );
    let deleted_run = sqlx::query("DELETE FROM approval_judge_eval_run")
        .execute(&pool)
        .await
        .expect_err("a recorded run refuses deletion");
    assert_eq!(
        raw_error_code(&deleted_run).as_deref(),
        Some(CHECK_VIOLATION_CODE)
    );
    let truncated_run = sqlx::query("TRUNCATE approval_judge_eval_run CASCADE")
        .execute(&pool)
        .await
        .expect_err("a recorded run refuses truncation");
    assert_eq!(
        raw_error_code(&truncated_run).as_deref(),
        Some(CHECK_VIOLATION_CODE)
    );
    let updated_call = sqlx::query("UPDATE approval_judge_eval_call SET rationale = $1")
        .bind(SECOND_RATIONALE)
        .execute(&pool)
        .await
        .expect_err("a recorded call refuses updates");
    assert_eq!(
        raw_error_code(&updated_call).as_deref(),
        Some(CHECK_VIOLATION_CODE)
    );
    let deleted_call = sqlx::query("DELETE FROM approval_judge_eval_call")
        .execute(&pool)
        .await
        .expect_err("a recorded call refuses deletion");
    assert_eq!(
        raw_error_code(&deleted_call).as_deref(),
        Some(CHECK_VIOLATION_CODE)
    );
    let truncated_call = sqlx::query("TRUNCATE approval_judge_eval_call")
        .execute(&pool)
        .await
        .expect_err("a recorded call refuses truncation");
    assert_eq!(
        raw_error_code(&truncated_call).as_deref(),
        Some(CHECK_VIOLATION_CODE)
    );

    let surviving_runs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM approval_judge_eval_run WHERE eval_run_id = $1")
            .bind(run.run.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(surviving_runs, 1);
    let surviving_calls: i64 =
        sqlx::query_scalar("SELECT count(*) FROM approval_judge_eval_call WHERE eval_run_id = $1")
            .bind(run.run.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(surviving_calls, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn foreign_recommendation_spellings_are_rejected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    // The sealing trigger admits call rows only in the run's own recording
    // transaction, so reaching the recommendation constraint requires
    // inserting the run row and the bad call in one raw transaction.
    let run = run_record_with(RUN_IDENTITY, &[], no_case_aggregates());
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO approval_judge_eval_run
            (eval_run_id, direct_model_selection_id,
             resolved_provider_model_identity_id, provider_model,
             credential_reference, usage_input_includes_cache_tokens,
             corpus_digest, contract_digest, rendered_digest, repeats,
             scorecard)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(run.run.into_uuid())
    .bind(run.selection.into_uuid())
    .bind(run.target.identity().into_uuid())
    .bind(run.provider_model.as_str())
    .bind(run.credential_reference.as_str())
    .bind(run.usage_input_includes_cache_tokens)
    .bind(run.corpus_digest.as_str())
    .bind(run.contract_digest.as_str())
    .bind(run.rendered_digest.as_str())
    .bind(Decimal::from(run.repeats))
    .bind(Json(&run.scorecard))
    .execute(&mut *transaction)
    .await?;
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
    .execute(&mut *transaction)
    .await
    .expect_err("a recommendation outside the closed set is rejected");
    assert_eq!(
        raw_constraint_name(&error),
        Some("approval_judge_eval_call_recommendation_closed")
    );
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn run_rows_pin_their_recording_transaction() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let run = run_record_with(RUN_IDENTITY, &[], no_case_aggregates());
    let mut transaction = pool.begin().await?;
    // The insert names a foreign transaction identity outright; the stamping
    // trigger must replace it with the inserting transaction's own.
    sqlx::query(
        "INSERT INTO approval_judge_eval_run
            (eval_run_id, direct_model_selection_id,
             resolved_provider_model_identity_id, provider_model,
             credential_reference, usage_input_includes_cache_tokens,
             corpus_digest, contract_digest, rendered_digest, repeats,
             scorecard, recording_transaction_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12::xid8)",
    )
    .bind(run.run.into_uuid())
    .bind(run.selection.into_uuid())
    .bind(run.target.identity().into_uuid())
    .bind(run.provider_model.as_str())
    .bind(run.credential_reference.as_str())
    .bind(run.usage_input_includes_cache_tokens)
    .bind(run.corpus_digest.as_str())
    .bind(run.contract_digest.as_str())
    .bind(run.rendered_digest.as_str())
    .bind(Decimal::from(run.repeats))
    .bind(Json(&run.scorecard))
    .bind(FOREIGN_TRANSACTION_ID)
    .execute(&mut *transaction)
    .await?;
    let stamped: bool = sqlx::query_scalar(
        "SELECT recording_transaction_id = pg_current_xact_id()
            AND recording_transaction_id::text <> $2
           FROM approval_judge_eval_run WHERE eval_run_id = $1",
    )
    .bind(run.run.into_uuid())
    .bind(FOREIGN_TRANSACTION_ID)
    .fetch_one(&mut *transaction)
    .await?;
    assert!(stamped);
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unaccounted_attempts_are_rejected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let mut run = run_record_with(
        RUN_IDENTITY,
        &[scorecard_case(
            FIRST_CASE,
            &[verdict_entry(VerdictFixture {
                recommendation: APPROVE_SPELLING,
                rationale: FIRST_RATIONALE,
            })],
            2,
            &FAILURE_CAUSES,
            first_partial_case_summary(),
        )],
        first_partial_case_aggregates(),
    );
    // One verdict plus zero failures leaves two configured attempts
    // unaccounted for.
    run.scorecard["cases"][0]["failed_calls"] = serde_json::json!(0);
    let error = record_eval_run(&pool, &run, &[first_call()])
        .await
        .expect_err("a scorecard dropping attempts is rejected");
    expect_scorecard_verdict_mismatch(&error, FIRST_CASE);
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
async fn failed_calls_require_one_failure_cause_each() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let mut missing = run_record_with(
        RUN_IDENTITY,
        &[scorecard_case(
            FIRST_CASE,
            &[verdict_entry(VerdictFixture {
                recommendation: APPROVE_SPELLING,
                rationale: FIRST_RATIONALE,
            })],
            2,
            &FAILURE_CAUSES,
            first_partial_case_summary(),
        )],
        first_partial_case_aggregates(),
    );
    missing.scorecard["cases"][0]
        .as_object_mut()
        .expect("fixture case is an object")
        .remove("failure_causes");
    let error = record_eval_run(&pool, &missing, &[first_call()])
        .await
        .expect_err("failed calls require failure causes");
    expect_scorecard_verdict_mismatch(&error, FIRST_CASE);

    let mut empty = run_record_with(
        RUN_IDENTITY,
        &[scorecard_case(
            FIRST_CASE,
            &[verdict_entry(VerdictFixture {
                recommendation: APPROVE_SPELLING,
                rationale: FIRST_RATIONALE,
            })],
            2,
            &FAILURE_CAUSES,
            first_partial_case_summary(),
        )],
        first_partial_case_aggregates(),
    );
    empty.scorecard["cases"][0]["failure_causes"] = serde_json::json!([]);
    let error = record_eval_run(&pool, &empty, &[first_call()])
        .await
        .expect_err("two failed calls cannot have no failure causes");
    expect_scorecard_verdict_mismatch(&error, FIRST_CASE);

    let mut too_few = run_record_with(
        RUN_IDENTITY,
        &[scorecard_case(
            FIRST_CASE,
            &[verdict_entry(VerdictFixture {
                recommendation: APPROVE_SPELLING,
                rationale: FIRST_RATIONALE,
            })],
            2,
            &FAILURE_CAUSES,
            first_partial_case_summary(),
        )],
        first_partial_case_aggregates(),
    );
    too_few.scorecard["cases"][0]["failure_causes"] = serde_json::json!(["only one cause"]);
    let error = record_eval_run(&pool, &too_few, &[first_call()])
        .await
        .expect_err("two failed calls cannot have only one failure cause");
    expect_scorecard_verdict_mismatch(&error, FIRST_CASE);

    let mut too_many = run_record_with(
        RUN_IDENTITY,
        &[scorecard_case(
            FIRST_CASE,
            &[verdict_entry(VerdictFixture {
                recommendation: APPROVE_SPELLING,
                rationale: FIRST_RATIONALE,
            })],
            2,
            &FAILURE_CAUSES,
            first_partial_case_summary(),
        )],
        first_partial_case_aggregates(),
    );
    too_many.scorecard["cases"][0]["failure_causes"] =
        serde_json::json!(["first", "second", "third"]);
    let error = record_eval_run(&pool, &too_many, &[first_call()])
        .await
        .expect_err("two failed calls cannot have three failure causes");
    expect_scorecard_verdict_mismatch(&error, FIRST_CASE);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn late_call_rows_are_rejected_after_the_run_commits() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let run = run_record_with(
        RUN_IDENTITY,
        &[scorecard_case(
            FIRST_CASE,
            &[verdict_entry(VerdictFixture {
                recommendation: APPROVE_SPELLING,
                rationale: FIRST_RATIONALE,
            })],
            2,
            &FAILURE_CAUSES,
            first_partial_case_summary(),
        )],
        first_partial_case_aggregates(),
    );
    record_eval_run(&pool, &run, &[first_call()]).await?;
    let late = first_call();
    let error = sqlx::query(
        "INSERT INTO approval_judge_eval_call
            (eval_run_id, case_name, repeat_ordinal, recommendation_kind,
             rationale)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(run.run.into_uuid())
    .bind(late.case_name.as_str())
    .bind(Decimal::from(LATE_CALL_ORDINAL))
    .bind(APPROVE_SPELLING)
    .bind(late.rationale.as_str())
    .execute(&pool)
    .await
    .expect_err("a call row appended after the run commits is rejected");
    assert_eq!(
        raw_error_code(&error).as_deref(),
        Some(CHECK_VIOLATION_CODE)
    );
    let surviving_calls: i64 =
        sqlx::query_scalar("SELECT count(*) FROM approval_judge_eval_call WHERE eval_run_id = $1")
            .bind(run.run.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(surviving_calls, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn contradictory_scorecard_verdicts_are_rejected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let run = run_record_with(
        RUN_IDENTITY,
        &[scorecard_case(
            FIRST_CASE,
            &[verdict_entry(VerdictFixture {
                recommendation: APPROVE_SPELLING,
                rationale: FIRST_RATIONALE,
            })],
            2,
            &FAILURE_CAUSES,
            first_partial_case_summary(),
        )],
        first_partial_case_aggregates(),
    );
    let error = record_eval_run(&pool, &run, &[])
        .await
        .expect_err("a scorecard stating verdicts with no matching calls is rejected");
    expect_scorecard_verdict_mismatch(&error, FIRST_CASE);
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
async fn rationale_bound_follows_the_domain_constant() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let widest = "a".repeat(ToolDecisionRationale::MAX_UTF8_BYTES);
    let mut at_bound = first_call();
    at_bound.rationale = widest.clone();
    let run = run_record_with(
        RUN_IDENTITY,
        &[scorecard_case(
            FIRST_CASE,
            &[verdict_entry(VerdictFixture {
                recommendation: APPROVE_SPELLING,
                rationale: &widest,
            })],
            2,
            &FAILURE_CAUSES,
            first_partial_case_summary(),
        )],
        first_partial_case_aggregates(),
    );
    record_eval_run(&pool, &run, &[at_bound]).await?;

    let overlong = "a".repeat(ToolDecisionRationale::MAX_UTF8_BYTES + 1);
    let mut past_bound = second_call();
    past_bound.rationale = overlong.clone();
    let second_run = run_record_with(
        SECOND_RUN_IDENTITY,
        &[scorecard_case(
            SECOND_CASE,
            &[verdict_entry(VerdictFixture {
                recommendation: DENY_SPELLING,
                rationale: &overlong,
            })],
            2,
            &FAILURE_CAUSES,
            second_partial_case_summary(),
        )],
        second_partial_case_aggregates(),
    );
    let error = record_eval_run(&pool, &second_run, &[past_bound])
        .await
        .expect_err("one byte past the domain bound is rejected");
    assert_eq!(
        constraint_name(&error).as_deref(),
        Some("approval_judge_eval_call_rationale_bounded")
    );
    Ok(())
}
