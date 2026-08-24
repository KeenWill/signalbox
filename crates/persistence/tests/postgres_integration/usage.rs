//! PostgreSQL integration proof for bounded dedicated usage projections.

use std::collections::BTreeMap;

use crate::*;
use signalbox_application::{
    UsageAggregateReport, UsageCallEvidence, UsageCallOrder, UsageCallPage, UsageCallPageLimit,
    UsageCallQuery, UsageProvenance, UsageQuery, UsageSelection, UsageTimeRange,
};
use signalbox_persistence::usage::UsageRepository;

const FIRST_INPUT_TOKENS: u64 = 11;
const SECOND_INPUT_TOKENS: u64 = 17;
const THIRD_INPUT_TOKENS: u64 = 23;
const SELECTED_OUTPUT_TOKENS: u64 = 29;

async fn terminal_reported_usage_call(
    pool: &PgPool,
    seed: u128,
    usage: ProviderReportedTokenUsage,
) -> Result<RestartModelCallFixture, Box<dyn Error>> {
    let (fixture, mut repository, authorized) =
        authorize_checkpointed_model_call(pool, seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation_with_usage(ModelCallTerminalObservation::KnownFailed, usage);
    let outcome = repository
        .commit_observation(
            fixture.session,
            observation,
            signalbox_application::ModelCallTerminalIdentityCandidates::Exact(
                ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x40)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x41)),
                )),
            ),
            |_| TurnId::from_uuid(Uuid::from_u128(seed + 0x42)),
        )
        .await?;
    assert!(matches!(
        outcome,
        Some(ModelCallObservationCommitOutcome::Terminal(_))
    ));
    Ok(fixture)
}

async fn terminal_estimated_usage_call(
    pool: &PgPool,
    seed: u128,
    usage: ProviderReportedTokenUsage,
) -> Result<RestartModelCallFixture, Box<dyn Error>> {
    let (fixture, mut repository, authorized) =
        authorize_checkpointed_model_call(pool, seed).await?;
    install_estimator_fixture(pool, fixture.call).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation_with_usage(ModelCallTerminalObservation::KnownFailed, usage);
    let outcome = repository
        .commit_observation(
            fixture.session,
            observation,
            signalbox_application::ModelCallTerminalIdentityCandidates::Exact(
                ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x40)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x41)),
                )),
            ),
            |_| TurnId::from_uuid(Uuid::from_u128(seed + 0x42)),
        )
        .await?;
    assert!(matches!(
        outcome,
        Some(ModelCallObservationCommitOutcome::Terminal(_))
    ));
    Ok(fixture)
}

async fn install_estimator_fixture(pool: &PgPool, call: ModelCallId) -> Result<(), sqlx::Error> {
    let function = format!(
        "CREATE FUNCTION fixture_mark_{suffix}_estimated() RETURNS trigger
         LANGUAGE plpgsql AS $$
         BEGIN
             IF NEW.model_call_id = '{call_id}'::uuid AND NEW.state_kind = 'terminal' THEN
                 NEW.usage_provenance_kind = 'estimated';
             END IF;
             RETURN NEW;
         END;
         $$",
        suffix = call.into_uuid().simple(),
        call_id = call.into_uuid(),
    );
    let trigger = format!(
        "CREATE TRIGGER aaa_fixture_mark_{suffix}_estimated
         BEFORE UPDATE ON model_call FOR EACH ROW
         EXECUTE FUNCTION fixture_mark_{suffix}_estimated()",
        suffix = call.into_uuid().simple(),
    );
    // The only interpolated values are canonical UUID renderings produced by
    // this fixture; no caller-provided SQL text reaches either statement.
    sqlx::query(sqlx::AssertSqlSafe(function.as_str()))
        .execute(pool)
        .await?;
    sqlx::query(sqlx::AssertSqlSafe(trigger.as_str()))
        .execute(pool)
        .await?;
    Ok(())
}

fn all_usage_query() -> UsageQuery {
    UsageQuery {
        time: UsageTimeRange::all(),
        selection: UsageSelection::all(),
    }
}

fn call_query(limit: u16, after: Option<signalbox_application::UsageCallCursor>) -> UsageCallQuery {
    UsageCallQuery {
        scope: all_usage_query(),
        order: UsageCallOrder::NewestFirst,
        limit: UsageCallPageLimit::new(limit).expect("fixture page limit fits"),
        after,
    }
}

fn evidence_signature(
    calls: &[UsageCallEvidence],
) -> BTreeMap<ModelCallId, (UsageProvenance, Option<u64>)> {
    calls
        .iter()
        .map(|call| (call.call, (call.provenance, call.tokens.input)))
        .collect()
}

fn aggregate_signature(
    report: &UsageAggregateReport,
) -> BTreeMap<(ProviderModelIdentity, UsageProvenance), (u64, Option<u128>)> {
    report
        .groups
        .iter()
        .map(|group| {
            (
                (group.key.model.identity(), group.key.provenance),
                (group.call_count, group.tokens.input),
            )
        })
        .collect()
}

fn paged_evidence_signature(
    first: &UsageCallPage,
    second: &UsageCallPage,
) -> BTreeMap<ModelCallId, (UsageProvenance, Option<u64>)> {
    first
        .calls
        .iter()
        .chain(&second.calls)
        .map(|call| (call.call, (call.provenance, call.tokens.input)))
        .collect()
}

fn expected_aggregate_signature(
    calls: &[UsageCallEvidence],
) -> BTreeMap<(ProviderModelIdentity, UsageProvenance), (u64, Option<u128>)> {
    calls
        .iter()
        .map(|call| {
            (
                (call.model.identity(), call.provenance),
                (1, call.tokens.input.map(u128::from)),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn mixed_provenance_aggregates_reconcile_with_exact_paged_call_evidence()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first = terminal_reported_usage_call(
        &pool,
        0x91_000,
        ProviderReportedTokenUsage::unreported().with_input_tokens(Some(FIRST_INPUT_TOKENS)),
    )
    .await?;
    let second = terminal_estimated_usage_call(
        &pool,
        0x92_000,
        ProviderReportedTokenUsage::unreported().with_input_tokens(Some(SECOND_INPUT_TOKENS)),
    )
    .await?;
    let third = terminal_reported_usage_call(
        &pool,
        0x93_000,
        ProviderReportedTokenUsage::unreported().with_input_tokens(Some(THIRD_INPUT_TOKENS)),
    )
    .await?;
    let repository = UsageRepository::new(pool.clone());
    let first_page = repository.calls(call_query(2, None)).await?;
    let second_page = repository.calls(call_query(2, first_page.next)).await?;
    let report = repository.aggregate(all_usage_query()).await?;
    let all_calls = [
        first_page.calls[0].clone(),
        first_page.calls[1].clone(),
        second_page.calls[0].clone(),
    ];

    assert_eq!(first_page.calls.len(), 2);
    assert!(first_page.next.is_some());
    assert_eq!(second_page.calls.len(), 1);
    assert_eq!(second_page.next, None);
    assert_eq!(
        paged_evidence_signature(&first_page, &second_page),
        evidence_signature(&all_calls)
    );
    assert_eq!(
        aggregate_signature(&report),
        expected_aggregate_signature(&all_calls)
    );
    assert_eq!(
        evidence_signature(&all_calls),
        BTreeMap::from([
            (
                first.call,
                (UsageProvenance::Reported, Some(FIRST_INPUT_TOKENS))
            ),
            (
                second.call,
                (UsageProvenance::Estimated, Some(SECOND_INPUT_TOKENS))
            ),
            (
                third.call,
                (UsageProvenance::Reported, Some(THIRD_INPUT_TOKENS))
            ),
        ])
    );
    assert!(!report.truncated);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn usage_exact_selection_filters_call_evidence() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = terminal_reported_usage_call(
        &pool,
        0x94_000,
        ProviderReportedTokenUsage::unreported().with_output_tokens(Some(SELECTED_OUTPUT_TOKENS)),
    )
    .await?;
    let repository = UsageRepository::new(pool.clone());
    let page = repository
        .calls(UsageCallQuery {
            scope: UsageQuery {
                time: UsageTimeRange::all(),
                selection: UsageSelection {
                    session: Some(fixture.session),
                    turn: Some(fixture.turn),
                    model: None,
                    provenance: Some(UsageProvenance::Reported),
                    call_kind: Some(signalbox_application::UsageCallKind::ModelCall),
                },
            },
            order: UsageCallOrder::NewestFirst,
            limit: UsageCallPageLimit::new(1).expect("fixture page limit fits"),
            after: None,
        })
        .await?;
    assert_eq!(page.calls.len(), 1);
    assert_eq!(page.calls[0].call, fixture.call);
    assert_eq!(page.calls[0].tokens.output, Some(SELECTED_OUTPUT_TOKENS));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn usage_half_open_time_range_excludes_earlier_evidence() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = terminal_reported_usage_call(
        &pool,
        0x95_000,
        ProviderReportedTokenUsage::unreported().with_output_tokens(Some(SELECTED_OUTPUT_TOKENS)),
    )
    .await?;
    let repository = UsageRepository::new(pool.clone());
    let page = repository
        .calls(UsageCallQuery {
            scope: all_usage_query(),
            order: UsageCallOrder::NewestFirst,
            limit: UsageCallPageLimit::new(1).expect("fixture page limit fits"),
            after: None,
        })
        .await?;
    let next_microsecond =
        signalbox_application::UsageTimestampMicros::new(page.calls[0].recorded_at.get() + 1)?;
    let excluded = repository
        .aggregate(UsageQuery {
            time: UsageTimeRange::new(Some(next_microsecond), None)?,
            selection: UsageSelection::all(),
        })
        .await?;

    assert_eq!(page.calls[0].call, fixture.call);
    assert_eq!(excluded.groups, Vec::new());

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn usage_projection_has_session_terminal_order_index() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let index_definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND tablename = 'web_usage_call_projection'
            AND indexname = 'web_usage_by_session_recorded_call'",
    )
    .fetch_one(&pool)
    .await?;

    assert!(index_definition.contains("session_id, recorded_at DESC, model_call_id DESC"));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oversized_credential_references_receive_bounded_distinct_usage_labels()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first = "a".repeat(257);
    let second = format!("{}b", "a".repeat(256));
    let labels: (String, String) =
        sqlx::query_as("SELECT bounded_web_usage_profile($1), bounded_web_usage_profile($2)")
            .bind(&first)
            .bind(&second)
            .fetch_one(&pool)
            .await?;

    assert!(labels.0.len() <= 256);
    assert!(labels.1.len() <= 256);
    assert_ne!(labels.0, labels.1);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT bounded_web_usage_profile($1)")
            .bind("within-bound")
            .fetch_one(&pool)
            .await?,
        "within-bound"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_approval_judge_usage_enters_dedicated_call_evidence() -> Result<(), Box<dyn Error>>
{
    const JUDGE_INPUT_TOKENS: u64 = 31;
    const JUDGE_OUTPUT_TOKENS: u64 = 7;

    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x96_000;
    let (fixture, model_repository, _, _) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let repository = model_repository.approval_judge_repository();
    let judge_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0));
    let prepared = ready_approval_judge(
        repository
            .prepare(fixture.session, fixture.turn, judge_call, None)
            .await?,
    );
    let rationale = ToolDecisionRationale::try_new(String::from(APPROVAL_JUDGE_RATIONALE))?;

    repository.authorize(&prepared).await?;
    repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::Approve,
            rationale,
            ProviderReportedTokenUsage::unreported()
                .with_input_tokens(Some(JUDGE_INPUT_TOKENS))
                .with_output_tokens(Some(JUDGE_OUTPUT_TOKENS)),
            ApprovalJudgeCompletionIdentities::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0xe2)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xe3)),
            ),
            |request| {
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    request.as_uuid().as_u128() + 0x2_000_000,
                ))
            },
        )
        .await?;
    let page = UsageRepository::new(pool.clone())
        .calls(UsageCallQuery {
            scope: UsageQuery {
                time: UsageTimeRange::all(),
                selection: UsageSelection {
                    session: Some(fixture.session),
                    turn: Some(fixture.turn),
                    model: None,
                    provenance: Some(UsageProvenance::Reported),
                    call_kind: Some(signalbox_application::UsageCallKind::ApprovalJudge),
                },
            },
            order: UsageCallOrder::NewestFirst,
            limit: UsageCallPageLimit::new(1).expect("fixture page limit fits"),
            after: None,
        })
        .await?;

    assert_eq!(page.calls.len(), 1);
    assert_eq!(page.calls[0].call, judge_call);
    assert_eq!(page.calls[0].tokens.input, Some(JUDGE_INPUT_TOKENS));
    assert_eq!(page.calls[0].tokens.output, Some(JUDGE_OUTPUT_TOKENS));
    assert_eq!(page.calls[0].tokens.cache_creation_input, None);
    assert_eq!(page.calls[0].tokens.cache_read_input, None);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_context_compaction_usage_enters_session_level_call_evidence()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x97_000;
    let fixture =
        terminal_reported_usage_call(&pool, seed, ProviderReportedTokenUsage::unreported()).await?;
    let source_frontier = Uuid::from_u128(seed + 0x80);
    let compaction_call = Uuid::from_u128(seed + 0x81);
    let mut connection = pool.acquire().await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(fixture.session.into_uuid())
    .bind(source_frontier)
    .execute(&mut *connection)
    .await?;
    insert_completed_context_compaction_call(
        &mut connection,
        compaction_call,
        fixture.session.into_uuid(),
        Uuid::from_u128(seed + 0x82),
        Uuid::from_u128(seed + 0x83),
        source_frontier,
    )
    .await?;

    let page = UsageRepository::new(pool.clone())
        .calls(UsageCallQuery {
            scope: UsageQuery {
                time: UsageTimeRange::all(),
                selection: UsageSelection {
                    session: Some(fixture.session),
                    turn: None,
                    model: None,
                    provenance: Some(UsageProvenance::Reported),
                    call_kind: Some(signalbox_application::UsageCallKind::ContextCompaction),
                },
            },
            order: UsageCallOrder::NewestFirst,
            limit: UsageCallPageLimit::new(1).expect("fixture page limit fits"),
            after: None,
        })
        .await?;

    assert_eq!(page.calls.len(), 1);
    assert_eq!(page.calls[0].call.into_uuid(), compaction_call);
    assert_eq!(page.calls[0].turn, None);
    assert_eq!(page.calls[0].tokens.input, Some(17));
    assert_eq!(page.calls[0].tokens.output, Some(5));

    drop(connection);
    pool.close().await;
    drop(container);
    Ok(())
}
