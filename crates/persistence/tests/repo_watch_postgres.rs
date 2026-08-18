#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{
    collections::{BTreeSet, HashSet},
    error::Error,
    num::NonZeroU16,
};

use signalbox_application::{
    RepoWatchBranchHead, RepoWatchCheckCompletionGeneration, RepoWatchCheckRunObservation,
    RepoWatchCheckSuiteObservation, RepoWatchConvergenceAssessment,
    RepoWatchConvergenceAssessmentInput, RepoWatchEventContentIdentityV1,
    RepoWatchEventIdGenerator, RepoWatchEventOccurrenceV1, RepoWatchObservation,
    RepoWatchPullRequestLifecycle, RepoWatchPullRequestState, RepoWatchPullRequestStateInput,
    RepoWatchRepositoryState, RepoWatchRepositoryStateInput, RepoWatchReviewDecision,
    derive_repo_watch_events,
};
use signalbox_domain::{
    BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha, GitHubObjectId, LabelName,
    MergeableState, PullRequestBody, PullRequestEventContext, PullRequestEventContextInput,
    PullRequestNumber, PullRequestTitle, ReactionChange, ReactionContent, ReactionSubject,
    RepoWatchAuthorLogin, RepoWatchEvent, RepoWatchEventId, RepoWatchEventKindNameV1,
    RepoWatchEventKindV1, RepositorySlug, ReviewState, ReviewThreadId, WorkflowName,
};
use signalbox_persistence::{
    MIGRATOR, disposable_test_container_labels, local_test_connection_options, migrate,
    repo_watch::{
        PostgresRepoWatchStore, RepoWatchCommitOutcome, RepoWatchCommitRequest,
        RepoWatchCursorCandidate, RepoWatchCursorGeneration, RepoWatchEventPageSize,
        RepoWatchPersistenceCorruption, RepoWatchStoreError,
    },
};
use sqlx::{PgPool, migrate::Migrate, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_repo_watch";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const REPOSITORY: &str = "signalbox/repository";
const HEAD_REPOSITORY: &str = "contributor/repository";
const BASE_BRANCH: &str = "main";
const HEAD_BRANCH: &str = "feature/repo-watch";
const INITIAL_HEAD: &str = "1111111111111111111111111111111111111111";
const CHANGED_HEAD: &str = "2222222222222222222222222222222222222222";
const BASE_REVISION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const STALE_BASE_REVISION: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TITLE: &str = "Persist repository-watch facts";
const BODY: &str = "A complete fixture pull request body.";
const AUTHOR: &str = "fixture-author";
const NON_ASCII_AUTHOR: &str = "é";
const LABEL: &str = "watch-me";
const U64_MAX_NUMERIC: &str = "18446744073709551615";
const U64_OVERFLOW_NUMERIC: &str = "18446744073709551616";
const OVERLONG_LABEL: &str = "123456789012345678901234567890123456789012345678901";
const CHECK_COMPLETION_GENERATION: &str = "2026-08-02T12:00:00Z";
const CHECK_RUN_NAME: &str = "required";
const WORKFLOW_NAME: &str = "required checks";
const REVIEW_THREAD: &str = "review-thread-1";
const REACTION_CONTENT: &str = "+1";
// Distinct from the context's `author` and `head_sha` on purpose. Reusing those
// would let a decoder that read `author` where it should read `review_reviewer`
// — or `head_sha` where it should read `review_commit` — still satisfy the
// round trip, so a cross-wired durable column would be invisible here.
const REVIEW_REVIEWER: &str = "fixture-reviewer";
const REVIEW_COMMIT: &str = "3333333333333333333333333333333333333333";
const REACTOR: &str = "fixture-reactor";
const PULL_REQUEST: u64 = 41;
const CONTENT_IDENTITY_MIGRATION: i64 = 202608150001;
const CHECK_SUITE_ID: u64 = 51;
const CHECK_RUN_ID: u64 = 52;
const ISSUE_COMMENT_ID: u64 = 61;
const REVIEW_COMMENT_ID: u64 = 62;

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

async fn postgres_before_content_identity()
-> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
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
    let mut connection = pool.acquire().await?;
    connection
        .ensure_migrations_table("_sqlx_migrations")
        .await?;
    for migration in MIGRATOR
        .iter()
        .take_while(|migration| migration.version < CONTENT_IDENTITY_MIGRATION)
    {
        connection.apply("_sqlx_migrations", migration).await?;
    }
    drop(connection);
    Ok((container, pool))
}

async fn apply_content_identity_migration(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    let mut connection = pool.acquire().await?;
    connection
        .ensure_migrations_table("_sqlx_migrations")
        .await?;
    for migration in MIGRATOR
        .iter()
        .filter(|migration| migration.version >= CONTENT_IDENTITY_MIGRATION)
    {
        connection.apply("_sqlx_migrations", migration).await?;
    }
    Ok(())
}

#[derive(Default)]
struct FixedEventIds(u128);

impl RepoWatchEventIdGenerator for FixedEventIds {
    fn next_event_id(&mut self) -> RepoWatchEventId {
        self.0 += 1;
        RepoWatchEventId::from_uuid(Uuid::from_u128(self.0))
    }
}

fn repository() -> Result<RepositorySlug, Box<dyn Error>> {
    Ok(RepositorySlug::try_new(REPOSITORY.to_owned())?)
}

fn observation(head: Option<&str>) -> Result<RepoWatchObservation, Box<dyn Error>> {
    let pull_requests = head.map(pull_request).transpose()?.into_iter().collect();
    Ok(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests,
            workflow_runs: Vec::new(),
            branch_heads: vec![RepoWatchBranchHead::new(
                BranchName::try_new(BASE_BRANCH.to_owned())?,
                CommitSha::try_new(BASE_REVISION.to_owned())?,
            )],
        })?,
    ))
}

fn pull_request(head: &str) -> Result<RepoWatchPullRequestState, Box<dyn Error>> {
    Ok(RepoWatchPullRequestState::try_new(
        RepoWatchPullRequestStateInput {
            context: PullRequestEventContext::new(PullRequestEventContextInput {
                number: PullRequestNumber::new(PULL_REQUEST.try_into()?),
                head_sha: CommitSha::try_new(head.to_owned())?,
                head_repository: RepositorySlug::try_new(HEAD_REPOSITORY.to_owned())?,
                base_branch: BranchName::try_new(BASE_BRANCH.to_owned())?,
                head_branch: BranchName::try_new(HEAD_BRANCH.to_owned())?,
                title: PullRequestTitle::try_new(TITLE.to_owned())?,
                body: PullRequestBody::try_new(BODY.to_owned())?,
                labels: Vec::new(),
                draft: false,
                author: Some(RepoWatchAuthorLogin::try_new(AUTHOR.to_owned())?),
            }),
            lifecycle: RepoWatchPullRequestLifecycle::Open,
            mergeable_state: MergeableState::Mergeable,
            completed_check_suites: vec![RepoWatchCheckSuiteObservation::new(
                GitHubObjectId::new(CHECK_SUITE_ID.try_into()?),
                RepoWatchCheckCompletionGeneration::try_new(String::from(
                    CHECK_COMPLETION_GENERATION,
                ))?,
                ChecksOutcome::Success,
            )],
            completed_check_runs: vec![RepoWatchCheckRunObservation::new(
                GitHubObjectId::new(CHECK_RUN_ID.try_into()?),
                RepoWatchCheckCompletionGeneration::try_new(String::from(
                    CHECK_COMPLETION_GENERATION,
                ))?,
                CheckRunName::try_new(String::from(CHECK_RUN_NAME))?,
                CheckConclusion::Success,
            )],
            reviews: Vec::new(),
            threads: Vec::new(),
            reactions: Vec::new(),
        },
    )?)
}

fn candidate(head: Option<&str>) -> Result<RepoWatchCursorCandidate, Box<dyn Error>> {
    Ok(RepoWatchCursorCandidate::new(observation(head)?))
}

fn merge_ready_assessment(
    head: &str,
    base_revision: &str,
) -> Result<RepoWatchConvergenceAssessment, Box<dyn Error>> {
    Ok(RepoWatchConvergenceAssessment::try_new(
        RepoWatchConvergenceAssessmentInput {
            number: PullRequestNumber::new(PULL_REQUEST.try_into()?),
            head_sha: CommitSha::try_new(head.to_owned())?,
            base_branch: BranchName::try_new(BASE_BRANCH.to_owned())?,
            base_revision: CommitSha::try_new(base_revision.to_owned())?,
            mergeable_state: MergeableState::Mergeable,
            review_decision: RepoWatchReviewDecision::None,
            unresolved_threads: Vec::new(),
            gating_check_count: 0,
            non_green_gating_checks: Vec::new(),
        },
    )?)
}

fn committed_generation(outcome: RepoWatchCommitOutcome) -> RepoWatchCursorGeneration {
    match outcome {
        RepoWatchCommitOutcome::Committed(cursor) => cursor.generation(),
        _ => panic!("fixture commit must be newly committed"),
    }
}

async fn seed_legacy_repo_watch_event(pool: &PgPool) -> Result<Uuid, Box<dyn Error>> {
    let event = Uuid::from_u128(0x10_001);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO repo_watch_cursor (
            repository, generation, storage_version, cursor_payload
         ) VALUES ($1, 1, 1, $2)",
    )
    .bind(REPOSITORY)
    .bind(sqlx::types::Json(serde_json::json!({
        "storage_version": 1,
        "signal_reviewers": [],
        "state": {
            "pull_requests": [],
            "workflow_runs": [],
            "branch_heads": []
        }
    })))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, target_kind, event_kind, conclusion,
            workflow_branch, workflow_name
         ) VALUES ($1, $2, 1, 1, 1, 'branch',
             'branch_workflow_run_completed', 'success', $3, $4)",
    )
    .bind(event)
    .bind(REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(WORKFLOW_NAME)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(event)
}

struct CommittedFixture {
    _container: ContainerAsync<Postgres>,
    repository: RepositorySlug,
    store: PostgresRepoWatchStore,
    second_candidate: RepoWatchCursorCandidate,
    events: Vec<RepoWatchEventOccurrenceV1>,
    first_generation: RepoWatchCursorGeneration,
    second_generation: RepoWatchCursorGeneration,
}

async fn committed_fixture() -> Result<CommittedFixture, Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let first_candidate = candidate(None)?;
    let first_generation = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, first_candidate.clone(), Vec::new()),
            )
            .await?,
    );
    let second_observation = observation(Some(INITIAL_HEAD))?;
    let mut identity_frontier = first_candidate.event_identity_frontier().clone();
    let events = derive_repo_watch_events(
        &repository,
        Some(first_candidate.observation()),
        &second_observation,
        &mut identity_frontier,
        &mut FixedEventIds::default(),
    )?;
    let second_candidate = RepoWatchCursorCandidate::with_event_identity_frontier(
        second_observation,
        identity_frontier,
    );
    let second_generation = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    Some(first_generation),
                    second_candidate.clone(),
                    events.clone(),
                ),
            )
            .await?,
    );
    Ok(CommittedFixture {
        _container: container,
        repository,
        store,
        second_candidate,
        events,
        first_generation,
        second_generation,
    })
}

fn replayed_generation(outcome: RepoWatchCommitOutcome) -> RepoWatchCursorGeneration {
    match outcome {
        RepoWatchCommitOutcome::Replayed(cursor) => cursor.generation(),
        _ => panic!("fixture commit must be recognized as a replay"),
    }
}

fn unchanged_generation(outcome: RepoWatchCommitOutcome) -> RepoWatchCursorGeneration {
    match outcome {
        RepoWatchCommitOutcome::Unchanged(cursor) => cursor.generation(),
        _ => panic!("fixture commit must be recognized as unchanged"),
    }
}

fn conflict_generation(outcome: RepoWatchCommitOutcome) -> Option<RepoWatchCursorGeneration> {
    match outcome {
        RepoWatchCommitOutcome::Conflict { current } => current,
        _ => panic!("fixture commit must conflict"),
    }
}

fn corruption_kind(error: RepoWatchStoreError) -> Option<RepoWatchPersistenceCorruption> {
    match error {
        RepoWatchStoreError::Corruption(corruption) => Some(corruption),
        _ => None,
    }
}

fn is_database_failure(result: Result<RepoWatchCommitOutcome, RepoWatchStoreError>) -> bool {
    match result {
        Err(RepoWatchStoreError::Database(_)) => true,
        Ok(_) | Err(_) => false,
    }
}

fn next_generation_value(generation: RepoWatchCursorGeneration) -> u64 {
    generation
        .get()
        .checked_add(1)
        .expect("fixture generation has a successor")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cursor_commits_advance_one_generation_at_a_time() -> Result<(), Box<dyn Error>> {
    let fixture = committed_fixture().await?;

    assert_eq!(fixture.first_generation, RepoWatchCursorGeneration::INITIAL);
    assert_eq!(
        fixture.second_generation.get(),
        next_generation_value(fixture.first_generation)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cursor_round_trip_retains_check_completion_generations() -> Result<(), Box<dyn Error>> {
    let fixture = committed_fixture().await?;

    let loaded = fixture
        .store
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor is present");

    assert_eq!(loaded.candidate(), &fixture.second_candidate);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn content_identity_migration_preserves_legacy_cursor_and_event() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = postgres_before_content_identity().await?;
    let event = seed_legacy_repo_watch_event(&pool).await?;

    apply_content_identity_migration(&pool).await?;

    let cursor_version: i16 =
        sqlx::query_scalar("SELECT storage_version FROM repo_watch_cursor WHERE repository = $1")
            .bind(REPOSITORY)
            .fetch_one(&pool)
            .await?;
    let frontier: serde_json::Value = sqlx::query_scalar(
        "SELECT cursor_payload -> 'event_identity_frontier'
           FROM repo_watch_cursor
          WHERE repository = $1",
    )
    .bind(REPOSITORY)
    .fetch_one(&pool)
    .await?;
    let event_identity: (i16, Vec<u8>, String) = sqlx::query_as(
        "SELECT content_identity_version, content_identity, producer
           FROM repo_watch_event
          WHERE event_id = $1",
    )
    .bind(event)
    .fetch_one(&pool)
    .await?;
    let store = PostgresRepoWatchStore::new(pool);
    let loaded_cursor = store
        .load_cursor(&repository()?)
        .await?
        .expect("migrated cursor remains readable");
    let loaded_event = store
        .load_event(&repository()?, RepoWatchEventId::from_uuid(event))
        .await?
        .expect("migrated event remains readable");

    assert_eq!(cursor_version, 2);
    assert_eq!(frontier, serde_json::json!([]));
    assert_eq!(event_identity.0, 0);
    assert_eq!(event_identity.1.len(), 32);
    assert_eq!(event_identity.2, "poll");
    assert_eq!(
        loaded_cursor
            .candidate()
            .event_identity_frontier()
            .entries()
            .len(),
        0
    );
    assert_eq!(loaded_event.id(), RepoWatchEventId::from_uuid(event));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn exact_retry_finds_its_replay_after_a_later_generation_commits()
-> Result<(), Box<dyn Error>> {
    let fixture = committed_fixture().await?;
    let changed_observation = observation(Some(CHANGED_HEAD))?;
    let mut changed_frontier = fixture.second_candidate.event_identity_frontier().clone();
    let changed_events = derive_repo_watch_events(
        &fixture.repository,
        Some(fixture.second_candidate.observation()),
        &changed_observation,
        &mut changed_frontier,
        &mut FixedEventIds(100),
    )?;
    let changed = RepoWatchCursorCandidate::with_event_identity_frontier(
        changed_observation,
        changed_frontier,
    );
    fixture
        .store
        .commit(
            &fixture.repository,
            RepoWatchCommitRequest::new(Some(fixture.second_generation), changed, changed_events),
        )
        .await?;
    let replay = fixture
        .store
        .commit(
            &fixture.repository,
            RepoWatchCommitRequest::new(
                Some(fixture.first_generation),
                fixture.second_candidate.clone(),
                fixture.events.clone(),
            ),
        )
        .await?;

    assert_eq!(replayed_generation(replay), fixture.second_generation);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stale_non_replay_commit_reports_the_current_generation() -> Result<(), Box<dyn Error>> {
    let fixture = committed_fixture().await?;
    let conflict = fixture
        .store
        .commit(
            &fixture.repository,
            RepoWatchCommitRequest::new(None, fixture.second_candidate, Vec::new()),
        )
        .await?;

    assert_eq!(
        conflict_generation(conflict),
        Some(fixture.second_generation)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unchanged_candidate_without_events_does_not_advance() -> Result<(), Box<dyn Error>> {
    let fixture = committed_fixture().await?;
    let unchanged = fixture
        .store
        .commit(
            &fixture.repository,
            RepoWatchCommitRequest::new(
                Some(fixture.second_generation),
                fixture.second_candidate,
                Vec::new(),
            ),
        )
        .await?;

    assert_eq!(unchanged_generation(unchanged), fixture.second_generation);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn convergence_identity_mismatch_rolls_back_cursor_events_and_evidence()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let baseline = candidate(None)?;
    let first_generation = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, baseline.clone(), Vec::new()),
            )
            .await?,
    );
    let current_observation = observation(Some(INITIAL_HEAD))?;
    let mut current_frontier = baseline.event_identity_frontier().clone();
    let events = derive_repo_watch_events(
        &repository,
        Some(baseline.observation()),
        &current_observation,
        &mut current_frontier,
        &mut FixedEventIds::default(),
    )?;
    let current = RepoWatchCursorCandidate::with_event_identity_frontier(
        current_observation,
        current_frontier,
    );
    let failure = store
        .commit_with_convergence(
            &repository,
            RepoWatchCommitRequest::new(Some(first_generation), current, events),
            &[merge_ready_assessment(CHANGED_HEAD, BASE_REVISION)?],
        )
        .await;
    let cursor = store
        .load_cursor(&repository)
        .await?
        .expect("baseline cursor remains present");
    let cursor_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_cursor")
        .fetch_one(&pool)
        .await?;
    let event_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_event")
        .fetch_one(&pool)
        .await?;
    let assessment_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_pull_request_convergence_assessment")
            .fetch_one(&pool)
            .await?;
    let seal_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_pull_request_convergence")
            .fetch_one(&pool)
            .await?;

    assert!(matches!(
        failure,
        Err(RepoWatchStoreError::ConvergenceEvidenceMismatch)
    ));
    assert_eq!(cursor.generation(), first_generation);
    assert_eq!(cursor_count, 1);
    assert_eq!(event_count, 0);
    assert_eq!(assessment_count, 0);
    assert_eq!(seal_count, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn provider_associated_stale_base_revision_commits_with_cursor_evidence()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let baseline = candidate(None)?;
    let first_generation = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, baseline.clone(), Vec::new()),
            )
            .await?,
    );
    let current_observation = observation(Some(INITIAL_HEAD))?;
    let mut current_frontier = baseline.event_identity_frontier().clone();
    let events = derive_repo_watch_events(
        &repository,
        Some(baseline.observation()),
        &current_observation,
        &mut current_frontier,
        &mut FixedEventIds::default(),
    )?;
    let current = RepoWatchCursorCandidate::with_event_identity_frontier(
        current_observation,
        current_frontier,
    );

    let outcome = store
        .commit_with_convergence(
            &repository,
            RepoWatchCommitRequest::new(Some(first_generation), current, events),
            &[merge_ready_assessment(INITIAL_HEAD, STALE_BASE_REVISION)?],
        )
        .await?;
    let assessment_base: String = sqlx::query_scalar(
        "SELECT base_revision
           FROM repo_watch_pull_request_convergence_assessment",
    )
    .fetch_one(&pool)
    .await?;

    assert!(matches!(outcome, RepoWatchCommitOutcome::Committed(_)));
    assert_eq!(assessment_base, STALE_BASE_REVISION);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cursor_events_assessment_and_seal_commit_atomically() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let baseline = candidate(None)?;
    let first_generation = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, baseline.clone(), Vec::new()),
            )
            .await?,
    );
    let current_observation = observation(Some(INITIAL_HEAD))?;
    let mut current_frontier = baseline.event_identity_frontier().clone();
    let events = derive_repo_watch_events(
        &repository,
        Some(baseline.observation()),
        &current_observation,
        &mut current_frontier,
        &mut FixedEventIds::default(),
    )?;
    let current = RepoWatchCursorCandidate::with_event_identity_frontier(
        current_observation,
        current_frontier,
    );
    let expected_event_count = i64::try_from(events.len())?;

    let committed = store
        .commit_with_convergence(
            &repository,
            RepoWatchCommitRequest::new(Some(first_generation), current, events),
            &[merge_ready_assessment(INITIAL_HEAD, BASE_REVISION)?],
        )
        .await?;
    let cursor_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_cursor")
        .fetch_one(&pool)
        .await?;
    let event_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_event")
        .fetch_one(&pool)
        .await?;
    let assessment_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_pull_request_convergence_assessment")
            .fetch_one(&pool)
            .await?;
    let seal_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_pull_request_convergence")
            .fetch_one(&pool)
            .await?;

    assert_eq!(
        committed_generation(committed).get(),
        next_generation_value(first_generation)
    );
    assert_eq!(cursor_count, 2);
    assert_eq!(event_count, expected_event_count);
    assert_eq!(assessment_count, 1);
    assert_eq!(seal_count, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn evidence_replay_after_a_head_round_trip_uses_the_candidate_head()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let baseline = candidate(None)?;
    let baseline_generation = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, baseline.clone(), Vec::new()),
            )
            .await?,
    );
    let first_observation = observation(Some(INITIAL_HEAD))?;
    let mut first_frontier = baseline.event_identity_frontier().clone();
    let mut ids = FixedEventIds::default();
    let first_events = derive_repo_watch_events(
        &repository,
        Some(baseline.observation()),
        &first_observation,
        &mut first_frontier,
        &mut ids,
    )?;
    let first =
        RepoWatchCursorCandidate::with_event_identity_frontier(first_observation, first_frontier);
    let first_generation = committed_generation(
        store
            .commit_with_convergence(
                &repository,
                RepoWatchCommitRequest::new(Some(baseline_generation), first.clone(), first_events),
                &[merge_ready_assessment(INITIAL_HEAD, BASE_REVISION)?],
            )
            .await?,
    );
    let second_observation = observation(Some(CHANGED_HEAD))?;
    let mut second_frontier = first.event_identity_frontier().clone();
    let second_events = derive_repo_watch_events(
        &repository,
        Some(first.observation()),
        &second_observation,
        &mut second_frontier,
        &mut ids,
    )?;
    let second =
        RepoWatchCursorCandidate::with_event_identity_frontier(second_observation, second_frontier);
    let second_generation = committed_generation(
        store
            .commit_with_convergence(
                &repository,
                RepoWatchCommitRequest::new(Some(first_generation), second.clone(), second_events),
                &[merge_ready_assessment(CHANGED_HEAD, BASE_REVISION)?],
            )
            .await?,
    );
    let replay_observation = observation(Some(INITIAL_HEAD))?;
    let mut replay_frontier = second.event_identity_frontier().clone();
    let replay_events = derive_repo_watch_events(
        &repository,
        Some(second.observation()),
        &replay_observation,
        &mut replay_frontier,
        &mut ids,
    )?;
    let replay =
        RepoWatchCursorCandidate::with_event_identity_frontier(replay_observation, replay_frontier);

    store
        .commit_with_convergence(
            &repository,
            RepoWatchCommitRequest::new(Some(second_generation), replay, replay_events),
            &[merge_ready_assessment(INITIAL_HEAD, BASE_REVISION)?],
        )
        .await?;
    let assessment_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_pull_request_convergence_assessment")
            .fetch_one(&pool)
            .await?;
    let first_head_assessment_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_pull_request_convergence_assessment
          WHERE head_sha = $1",
    )
    .bind(INITIAL_HEAD)
    .fetch_one(&pool)
    .await?;
    let latest_assessment_head: String = sqlx::query_scalar(
        "SELECT head_sha
           FROM repo_watch_pull_request_convergence_assessment
          ORDER BY recorded_at DESC, assessment_id DESC
          LIMIT 1",
    )
    .fetch_one(&pool)
    .await?;

    assert_eq!(assessment_count, 3);
    assert_eq!(first_head_assessment_count, 2);
    assert_eq!(latest_assessment_head, INITIAL_HEAD);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn historical_exact_replay_does_not_append_stale_assessment() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let baseline = candidate(None)?;
    let baseline_generation = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, baseline.clone(), Vec::new()),
            )
            .await?,
    );
    let first_observation = observation(Some(INITIAL_HEAD))?;
    let mut first_frontier = baseline.event_identity_frontier().clone();
    let mut ids = FixedEventIds::default();
    let first_events = derive_repo_watch_events(
        &repository,
        Some(baseline.observation()),
        &first_observation,
        &mut first_frontier,
        &mut ids,
    )?;
    let first =
        RepoWatchCursorCandidate::with_event_identity_frontier(first_observation, first_frontier);
    let first_generation = committed_generation(
        store
            .commit_with_convergence(
                &repository,
                RepoWatchCommitRequest::new(
                    Some(baseline_generation),
                    first.clone(),
                    first_events.clone(),
                ),
                &[merge_ready_assessment(INITIAL_HEAD, BASE_REVISION)?],
            )
            .await?,
    );
    let second_observation = observation(Some(CHANGED_HEAD))?;
    let mut second_frontier = first.event_identity_frontier().clone();
    let second_events = derive_repo_watch_events(
        &repository,
        Some(first.observation()),
        &second_observation,
        &mut second_frontier,
        &mut ids,
    )?;
    let second =
        RepoWatchCursorCandidate::with_event_identity_frontier(second_observation, second_frontier);
    store
        .commit_with_convergence(
            &repository,
            RepoWatchCommitRequest::new(Some(first_generation), second, second_events),
            &[merge_ready_assessment(CHANGED_HEAD, BASE_REVISION)?],
        )
        .await?;

    let replayed = replayed_generation(
        store
            .commit_with_convergence(
                &repository,
                RepoWatchCommitRequest::new(Some(baseline_generation), first, first_events),
                &[merge_ready_assessment(INITIAL_HEAD, BASE_REVISION)?],
            )
            .await?,
    );
    let assessment_heads: Vec<String> = sqlx::query_scalar(
        "SELECT head_sha
           FROM repo_watch_pull_request_convergence_assessment
          ORDER BY cursor_generation",
    )
    .fetch_all(&pool)
    .await?;

    assert_eq!(replayed, first_generation);
    assert_eq!(assessment_heads, vec![INITIAL_HEAD, CHANGED_HEAD]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn event_pages_use_the_last_position_as_the_next_keyset_cursor() -> Result<(), Box<dyn Error>>
{
    let fixture = committed_fixture().await?;
    let first_page = fixture
        .store
        .load_event_page(
            &fixture.repository,
            None,
            RepoWatchEventPageSize::try_new(NonZeroU16::MIN)?,
        )
        .await?;
    let second_page = fixture
        .store
        .load_event_page(
            &fixture.repository,
            first_page.next_after(),
            RepoWatchEventPageSize::try_new(NonZeroU16::MIN)?,
        )
        .await?;

    assert_eq!(
        first_page.events().len(),
        usize::from(NonZeroU16::MIN.get())
    );
    assert!(first_page.next_after().is_some());
    assert_eq!(
        second_page.events().len(),
        usize::from(NonZeroU16::MIN.get())
    );
    assert!(second_page.next_after().is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn event_identity_failure_rolls_back_cursor_and_events() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let baseline = candidate(None)?;
    let first_generation = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, baseline.clone(), Vec::new()),
            )
            .await?,
    );
    let current_observation = observation(Some(INITIAL_HEAD))?;
    let mut current_frontier = baseline.event_identity_frontier().clone();
    let mut ids = FixedEventIds::default();
    let events = derive_repo_watch_events(
        &repository,
        Some(baseline.observation()),
        &current_observation,
        &mut current_frontier,
        &mut ids,
    )?;
    let current = RepoWatchCursorCandidate::with_event_identity_frontier(
        current_observation,
        current_frontier,
    );
    let second_generation = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(Some(first_generation), current.clone(), events),
            )
            .await?,
    );
    let changed_observation = observation(Some(CHANGED_HEAD))?;
    let mut changed_frontier = current.event_identity_frontier().clone();
    let collision = derive_repo_watch_events(
        &repository,
        Some(current.observation()),
        &changed_observation,
        &mut changed_frontier,
        &mut FixedEventIds::default(),
    )?;
    let changed = RepoWatchCursorCandidate::with_event_identity_frontier(
        changed_observation,
        changed_frontier,
    );
    let failure = store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(Some(second_generation), changed, collision),
        )
        .await;
    let cursor = store
        .load_cursor(&repository)
        .await?
        .expect("fixture cursor remains present");
    let cursor_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_cursor WHERE repository = $1")
            .bind(repository.as_str())
            .fetch_one(&pool)
            .await?;

    assert!(is_database_failure(failure));
    assert_eq!(cursor.generation(), second_generation);
    assert_eq!(cursor_rows, 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn content_identity_failure_rolls_back_cursor_and_events() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let baseline = candidate(None)?;
    let first_generation = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, baseline.clone(), Vec::new()),
            )
            .await?,
    );
    let current_observation = observation(Some(INITIAL_HEAD))?;
    let mut current_frontier = baseline.event_identity_frontier().clone();
    let current_events = derive_repo_watch_events(
        &repository,
        Some(baseline.observation()),
        &current_observation,
        &mut current_frontier,
        &mut FixedEventIds::default(),
    )?;
    let duplicated_content_identity = current_events[0].content_identity();
    let current = RepoWatchCursorCandidate::with_event_identity_frontier(
        current_observation,
        current_frontier,
    );
    let second_generation = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    Some(first_generation),
                    current.clone(),
                    current_events,
                ),
            )
            .await?,
    );
    let changed_observation = observation(Some(CHANGED_HEAD))?;
    let mut changed_frontier = current.event_identity_frontier().clone();
    let changed_events = derive_repo_watch_events(
        &repository,
        Some(current.observation()),
        &changed_observation,
        &mut changed_frontier,
        &mut FixedEventIds(100),
    )?;
    let conflicting_event = RepoWatchEventOccurrenceV1::from_parts(
        changed_events[0].event().clone(),
        duplicated_content_identity,
    );
    let changed = RepoWatchCursorCandidate::with_event_identity_frontier(
        changed_observation,
        changed_frontier,
    );

    let failure = store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(Some(second_generation), changed, vec![conflicting_event]),
        )
        .await;
    let cursor = store
        .load_cursor(&repository)
        .await?
        .expect("fixture cursor remains present");
    let cursor_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_cursor WHERE repository = $1")
            .bind(repository.as_str())
            .fetch_one(&pool)
            .await?;

    assert!(is_database_failure(failure));
    assert_eq!(cursor.generation(), second_generation);
    assert_eq!(cursor_rows, 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn append_only_guards_reject_update_delete_and_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    PostgresRepoWatchStore::new(pool.clone())
        .commit(
            &repository,
            RepoWatchCommitRequest::new(None, candidate(None)?, Vec::new()),
        )
        .await?;
    let update = sqlx::query(
        "UPDATE repo_watch_cursor SET recorded_at = transaction_timestamp() WHERE repository = $1",
    )
    .bind(repository.as_str())
    .execute(&pool)
    .await;
    let delete = sqlx::query("DELETE FROM repo_watch_cursor WHERE repository = $1")
        .bind(repository.as_str())
        .execute(&pool)
        .await;
    let truncate = sqlx::query("TRUNCATE repo_watch_cursor CASCADE")
        .execute(&pool)
        .await;

    assert!(update.is_err());
    assert!(delete.is_err());
    assert!(truncate.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn malformed_cursor_document_fails_closed_on_read() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(None, candidate(None)?, Vec::new()),
        )
        .await?;
    let mut corruption_connection = pool.acquire().await?;
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut *corruption_connection)
        .await?;
    sqlx::query(
        "UPDATE repo_watch_cursor SET cursor_payload = '{\"storage_version\":2}'::jsonb WHERE repository = $1",
    )
    .bind(repository.as_str())
    .execute(&mut *corruption_connection)
    .await?;
    sqlx::query("SET session_replication_role = origin")
        .execute(&mut *corruption_connection)
        .await?;
    drop(corruption_connection);
    let corruption = store
        .load_cursor(&repository)
        .await
        .expect_err("malformed cursor is rejected");

    assert_eq!(
        corruption_kind(corruption),
        Some(RepoWatchPersistenceCorruption::MalformedCursorDocument)
    );
    Ok(())
}

async fn migrated_cursor_fixture()
-> Result<(ContainerAsync<Postgres>, PgPool, RepositorySlug), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    PostgresRepoWatchStore::new(pool.clone())
        .commit(
            &repository,
            RepoWatchCommitRequest::new(None, candidate(None)?, Vec::new()),
        )
        .await?;
    Ok((container, pool, repository))
}

async fn begin_next_cursor_transaction<'a>(
    pool: &'a PgPool,
    repository: &RepositorySlug,
) -> Result<(sqlx::Transaction<'a, sqlx::Postgres>, i64), Box<dyn Error>> {
    let generation = RepoWatchCursorGeneration::INITIAL.get() as i64 + 1;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO repo_watch_cursor (
            repository, generation, storage_version, cursor_payload
         )
         SELECT repository, $2, storage_version, cursor_payload
           FROM repo_watch_cursor
          WHERE repository = $1 AND generation = $3",
    )
    .bind(repository.as_str())
    .bind(generation)
    .bind(RepoWatchCursorGeneration::INITIAL.get() as i64)
    .execute(&mut *transaction)
    .await?;
    Ok((transaction, generation))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn event_insert_requires_its_cursor_commit_transaction() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let error = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'pull_request_opened', $4,
            $5, $6, $7, $8, $9, $10, ARRAY[]::text[], false
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(RepoWatchCursorGeneration::INITIAL.get() as i64)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .execute(&pool)
    .await
    .expect_err("an event cannot extend an already-committed cursor generation");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("repo_watch_event_requires_current_cursor_transaction")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cursor_constraint_rejects_a_missing_payload_storage_version() -> Result<(), Box<dyn Error>>
{
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let mut connection = pool.acquire().await?;
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut *connection)
        .await?;
    let update = sqlx::query(
        "UPDATE repo_watch_cursor SET cursor_payload = '{}'::jsonb WHERE repository = $1",
    )
    .bind(repository.as_str())
    .execute(&mut *connection)
    .await;
    sqlx::query("SET session_replication_role = origin")
        .execute(&mut *connection)
        .await?;

    assert!(update.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cursor_constraint_rejects_a_null_payload_storage_version() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let mut connection = pool.acquire().await?;
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut *connection)
        .await?;
    let update = sqlx::query(
        "UPDATE repo_watch_cursor
            SET cursor_payload = '{\"storage_version\": null}'::jsonb
          WHERE repository = $1",
    )
    .bind(repository.as_str())
    .execute(&mut *connection)
    .await;
    sqlx::query("SET session_replication_role = origin")
        .execute(&mut *connection)
        .await?;

    assert!(update.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cursor_constraint_rejects_a_non_numeric_payload_storage_version()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let mut connection = pool.acquire().await?;
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut *connection)
        .await?;
    let update = sqlx::query(
        "UPDATE repo_watch_cursor
            SET cursor_payload = '{\"storage_version\": \"1\"}'::jsonb
          WHERE repository = $1",
    )
    .bind(repository.as_str())
    .execute(&mut *connection)
    .await;
    sqlx::query("SET session_replication_role = origin")
        .execute(&mut *connection)
        .await?;

    assert!(update.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cursor_constraint_rejects_a_fractional_payload_storage_version()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let mut connection = pool.acquire().await?;
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut *connection)
        .await?;
    let update = sqlx::query(
        "UPDATE repo_watch_cursor
            SET cursor_payload = '{\"storage_version\": 0.6}'::jsonb
          WHERE repository = $1",
    )
    .bind(repository.as_str())
    .execute(&mut *connection)
    .await;
    sqlx::query("SET session_replication_role = origin")
        .execute(&mut *connection)
        .await?;

    assert!(update.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pull_request_target_constraint_rejects_a_null_number() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'pull_request_opened', NULL,
            $4, $5, $6, $7, $8, $9, ARRAY[]::text[], false
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pull_request_target_constraint_rejects_a_number_beyond_u64() -> Result<(), Box<dyn Error>>
{
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'pull_request_opened', $4::numeric,
            $5, $6, $7, $8, $9, $10, ARRAY[]::text[], false
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(U64_OVERFLOW_NUMERIC)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pull_request_target_constraint_accepts_u64_maximum() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'pull_request_opened', $4::numeric,
            $5, $6, $7, $8, $9, $10, ARRAY[]::text[], false
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(U64_MAX_NUMERIC)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .execute(&mut *transaction)
    .await?;
    transaction.rollback().await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn comment_reaction_constraint_rejects_a_null_subject_id() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft,
            reaction_subject_kind, reaction_subject_id, reaction_reactor,
            reaction_content, reaction_change
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'reaction_changed', $4,
            $5, $6, $7, $8, $9, $10, ARRAY[]::text[], false,
            'issue_comment', NULL, $11, '+1', 'added'
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .bind(AUTHOR)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn comment_reaction_constraint_rejects_a_subject_id_beyond_u64() -> Result<(), Box<dyn Error>>
{
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft,
            reaction_subject_kind, reaction_subject_id, reaction_reactor,
            reaction_content, reaction_change
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'reaction_changed', $4,
            $5, $6, $7, $8, $9, $10, ARRAY[]::text[], false,
            'issue_comment', $11::numeric, $12, '+1', 'added'
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .bind(U64_OVERFLOW_NUMERIC)
    .bind(AUTHOR)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn comment_reaction_constraint_accepts_a_u64_maximum_subject_id() -> Result<(), Box<dyn Error>>
{
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft,
            reaction_subject_kind, reaction_subject_id, reaction_reactor,
            reaction_content, reaction_change
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'reaction_changed', $4,
            $5, $6, $7, $8, $9, $10, ARRAY[]::text[], false,
            'issue_comment', $11::numeric, $12, '+1', 'added'
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .bind(U64_MAX_NUMERIC)
    .bind(AUTHOR)
    .execute(&mut *transaction)
    .await?;
    transaction.rollback().await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn comment_reaction_constraint_rejects_an_empty_content() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft,
            reaction_subject_kind, reaction_subject_id, reaction_reactor,
            reaction_content, reaction_change
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'reaction_changed', $4,
            $5, $6, $7, $8, $9, $10, ARRAY[]::text[], false,
            'issue_comment', $11::numeric, $12, '', 'added'
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .bind(U64_MAX_NUMERIC)
    .bind(AUTHOR)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn label_array_constraint_rejects_a_null_member() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft, label_name
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'labeled', $4,
            $5, $6, $7, $8, $9, $10, ARRAY[$11, NULL]::text[], false, $11
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .bind(LABEL)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn label_array_constraint_rejects_an_overlong_member() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft, label_name
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'labeled', $4,
            $5, $6, $7, $8, $9, $10, ARRAY[$11, $12]::text[], false, $11
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .bind(LABEL)
    .bind(OVERLONG_LABEL)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn label_array_constraint_rejects_multiple_dimensions() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft, label_name
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'labeled', $4,
            $5, $6, $7, $8, $9, $10, ARRAY[[$11], [$11]]::text[][], false, $11
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .bind(LABEL)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn label_array_constraint_rejects_a_noncanonical_lower_bound() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft, label_name
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'labeled', $4,
            $5, $6, $7, $8, $9, $10, ('[0:0]={' || $11 || '}')::text[], false, $11
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .bind(LABEL)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn label_array_constraint_rejects_noncanonical_order() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'pull_request_opened', $4,
            $5, $6, $7, $8, $9, $10, ARRAY['z-label', 'a-label']::text[], false
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn label_array_constraint_rejects_duplicate_members() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'pull_request_opened', $4,
            $5, $6, $7, $8, $9, $10, ARRAY[$11, $11]::text[], false
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .bind(LABEL)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn actor_login_constraint_rejects_domain_invalid_spelling() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft, author
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'pull_request_opened', $4,
            $5, $6, $7, $8, $9, $10, ARRAY[]::text[], false, 'bad login'
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn actor_login_constraint_rejects_non_ascii_range_members() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft, author
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'pull_request_opened', $4,
            $5, $6, $7, $8, $9, $10, ARRAY[]::text[], false, $11
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .bind(NON_ASCII_AUTHOR)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn head_repository_constraint_rejects_domain_invalid_spelling() -> Result<(), Box<dyn Error>>
{
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'pull_request_opened', $4,
            $5, 'namespace/bad repo', $6, $7, $8, $9, ARRAY[]::text[], false
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn workflow_branch_constraint_rejects_domain_invalid_spelling() -> Result<(), Box<dyn Error>>
{
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, conclusion, workflow_branch, workflow_name
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'branch', 'branch_workflow_run_completed',
            'failure', 'bad branch', 'required checks'
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn label_name_constraint_rejects_an_overlong_value() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let (mut transaction, generation) = begin_next_cursor_transaction(&pool, &repository).await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity, producer,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft, label_name
         ) VALUES (
            $1, $2, $3, 1, 1, 1, sha256(uuid_send($1)), 'poll',
            'pull_request', 'unlabeled', $4,
            $5, $6, $7, $8, $9, $10, ARRAY[]::text[], false, $11
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(generation)
    .bind(PULL_REQUEST as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .bind(OVERLONG_LABEL)
    .execute(&mut *transaction)
    .await;

    assert!(insert.is_err());
    transaction.rollback().await?;
    Ok(())
}

/// The canonical pull-request context every hand-built fixture event carries:
/// the fixture pull request at `CHANGED_HEAD` on `BASE_BRANCH`, carrying
/// exactly `labels`.
fn event_context(labels: Vec<LabelName>) -> Result<PullRequestEventContext, Box<dyn Error>> {
    Ok(PullRequestEventContext::new(PullRequestEventContextInput {
        number: PullRequestNumber::new(PULL_REQUEST.try_into()?),
        head_sha: CommitSha::try_new(CHANGED_HEAD.to_owned())?,
        head_repository: RepositorySlug::try_new(HEAD_REPOSITORY.to_owned())?,
        base_branch: BranchName::try_new(BASE_BRANCH.to_owned())?,
        head_branch: BranchName::try_new(HEAD_BRANCH.to_owned())?,
        title: PullRequestTitle::try_new(TITLE.to_owned())?,
        body: PullRequestBody::try_new(BODY.to_owned())?,
        labels,
        draft: false,
        author: Some(RepoWatchAuthorLogin::try_new(AUTHOR.to_owned())?),
    }))
}

fn label() -> Result<LabelName, Box<dyn Error>> {
    Ok(LabelName::try_new(LABEL.to_owned())?)
}

/// One fixture event of `kind` against the canonical pull request carrying no
/// labels; the generator supplies its durable identity.
fn unlabeled_event(
    ids: &mut FixedEventIds,
    kind: RepoWatchEventKindV1,
) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        ids.next_event_id(),
        repository()?,
        event_context(Vec::new())?,
        kind,
    )?)
}

/// One fixture event of `kind` against the canonical pull request already
/// carrying `LABEL`, which only a `Labeled` fact is admitted against.
fn labeled_event(
    ids: &mut FixedEventIds,
    kind: RepoWatchEventKindV1,
) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        ids.next_event_id(),
        repository()?,
        event_context(vec![label()?])?,
        kind,
    )?)
}

/// One fixture `ReactionChanged` event whose only meaningful variation is the
/// object the configured reactor reacted to. The reactor is `REACTOR`, kept
/// distinct from the context author and from `REVIEW_REVIEWER` on purpose, so
/// a cross-wired durable column stays visible.
fn reaction_event(
    ids: &mut FixedEventIds,
    subject: ReactionSubject,
) -> Result<RepoWatchEvent, Box<dyn Error>> {
    unlabeled_event(
        ids,
        RepoWatchEventKindV1::ReactionChanged {
            subject,
            reactor: RepoWatchAuthorLogin::try_new(REACTOR.to_owned())?,
            content: ReactionContent::try_new(REACTION_CONTENT.to_owned())?,
            change: ReactionChange::Added,
        },
    )
}

/// Commits `events` in the generation that advances the fixture cursor from
/// its baseline, and returns the store they are durable in.
async fn committed_event_fixture(
    events: Vec<RepoWatchEvent>,
) -> Result<
    (
        ContainerAsync<Postgres>,
        PostgresRepoWatchStore,
        RepositorySlug,
    ),
    Box<dyn Error>,
> {
    let (container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool);
    let events = events
        .into_iter()
        .map(fixture_occurrence)
        .collect::<Vec<_>>();
    let baseline = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, candidate(None)?, Vec::new()),
            )
            .await?,
    );
    committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(Some(baseline), candidate(Some(CHANGED_HEAD))?, events),
            )
            .await?,
    );
    Ok((container, store, repository))
}

fn fixture_occurrence(event: RepoWatchEvent) -> RepoWatchEventOccurrenceV1 {
    let mut identity = [0_u8; 32];
    identity[..16].copy_from_slice(event.id().as_uuid().as_bytes());
    identity[16..].copy_from_slice(event.id().as_uuid().as_bytes());
    RepoWatchEventOccurrenceV1::from_parts(
        event,
        RepoWatchEventContentIdentityV1::from_bytes(identity),
    )
}

/// Loads one committed event back through the closed event decoder.
///
/// The comparison is deliberately *not* made here. `#[track_caller]` does not
/// carry through an `async fn`, so a shared assertion would report all fifteen
/// round trips from this one line, and a failure has to name its own call site.
/// Each caller therefore asserts for itself and this helper only hides the load.
///
/// A decode that drops, or silently rewrites, any field of a kind stalls
/// repository-watch dispatch on that event forever, because the evaluation row
/// is only written after a successful dispatch.
async fn loaded_event(
    store: &PostgresRepoWatchStore,
    repository: &RepositorySlug,
    expected: &RepoWatchEvent,
) -> Result<Option<RepoWatchEvent>, Box<dyn Error>> {
    Ok(store.load_event(repository, expected.id()).await?)
}

/// Every event kind the committed samples cover, for the inventory assertion.
fn covered_kind_names(events: &[&RepoWatchEvent]) -> BTreeSet<RepoWatchEventKindNameV1> {
    events.iter().map(|event| event.kind().name()).collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn every_event_kind_survives_a_commit_and_load_round_trip() -> Result<(), Box<dyn Error>> {
    let mut ids = FixedEventIds::default();
    let opened = unlabeled_event(&mut ids, RepoWatchEventKindV1::PullRequestOpened)?;
    let closed = unlabeled_event(&mut ids, RepoWatchEventKindV1::PullRequestClosed)?;
    let merged = unlabeled_event(&mut ids, RepoWatchEventKindV1::PullRequestMerged)?;
    let head_changed = unlabeled_event(
        &mut ids,
        RepoWatchEventKindV1::HeadChanged {
            previous: CommitSha::try_new(INITIAL_HEAD.to_owned())?,
            current: CommitSha::try_new(CHANGED_HEAD.to_owned())?,
        },
    )?;
    let mergeable_state_changed = unlabeled_event(
        &mut ids,
        RepoWatchEventKindV1::MergeableStateChanged {
            current: MergeableState::Conflicting,
        },
    )?;
    let checks_completed = unlabeled_event(
        &mut ids,
        RepoWatchEventKindV1::ChecksCompleted {
            outcome: ChecksOutcome::Failure,
        },
    )?;
    let check_run_completed = unlabeled_event(
        &mut ids,
        RepoWatchEventKindV1::CheckRunCompleted {
            name: CheckRunName::try_new(CHECK_RUN_NAME.to_owned())?,
            conclusion: CheckConclusion::TimedOut,
        },
    )?;
    let review_submitted = unlabeled_event(
        &mut ids,
        RepoWatchEventKindV1::ReviewSubmitted {
            reviewer: RepoWatchAuthorLogin::try_new(REVIEW_REVIEWER.to_owned())?,
            state: ReviewState::ChangesRequested,
            commit: CommitSha::try_new(REVIEW_COMMIT.to_owned())?,
        },
    )?;
    let thread_opened = unlabeled_event(
        &mut ids,
        RepoWatchEventKindV1::ThreadOpened {
            thread: ReviewThreadId::try_new(REVIEW_THREAD.to_owned())?,
        },
    )?;
    let thread_resolved = unlabeled_event(
        &mut ids,
        RepoWatchEventKindV1::ThreadResolved {
            thread: ReviewThreadId::try_new(REVIEW_THREAD.to_owned())?,
        },
    )?;
    let labeled = labeled_event(&mut ids, RepoWatchEventKindV1::Labeled { label: label()? })?;
    let unlabeled = unlabeled_event(
        &mut ids,
        RepoWatchEventKindV1::Unlabeled { label: label()? },
    )?;
    let base_advanced = unlabeled_event(
        &mut ids,
        RepoWatchEventKindV1::BaseAdvanced {
            branch: BranchName::try_new(BASE_BRANCH.to_owned())?,
        },
    )?;
    let reaction_changed = reaction_event(&mut ids, ReactionSubject::PullRequestBody)?;
    let branch_workflow_run_completed = RepoWatchEvent::branch_workflow(
        ids.next_event_id(),
        repository()?,
        BranchName::try_new(BASE_BRANCH.to_owned())?,
        WorkflowName::try_new(WORKFLOW_NAME.to_owned())?,
        CheckConclusion::ActionRequired,
    );
    let (_container, store, repository) = committed_event_fixture(vec![
        opened.clone(),
        closed.clone(),
        merged.clone(),
        head_changed.clone(),
        mergeable_state_changed.clone(),
        checks_completed.clone(),
        check_run_completed.clone(),
        review_submitted.clone(),
        thread_opened.clone(),
        thread_resolved.clone(),
        labeled.clone(),
        unlabeled.clone(),
        base_advanced.clone(),
        reaction_changed.clone(),
        branch_workflow_run_completed.clone(),
    ])
    .await?;

    // The inventory is the compiler's, not this file's: `all()` is produced by
    // an exhaustive match, so a new `RepoWatchEventKindV1` variant fails to
    // compile there, and once it is added this assertion fails until a sample
    // of that kind is committed above. Without it the "every event kind"
    // guarantee in this test's name was only as good as the list below.
    assert_eq!(
        covered_kind_names(&[
            &opened,
            &closed,
            &merged,
            &head_changed,
            &mergeable_state_changed,
            &checks_completed,
            &check_run_completed,
            &review_submitted,
            &thread_opened,
            &thread_resolved,
            &labeled,
            &unlabeled,
            &base_advanced,
            &reaction_changed,
            &branch_workflow_run_completed,
        ]),
        RepoWatchEventKindNameV1::all().into_iter().collect(),
        "every event kind must have a committed round trip"
    );

    assert_eq!(
        loaded_event(&store, &repository, &opened).await?.as_ref(),
        Some(&opened),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &closed).await?.as_ref(),
        Some(&closed),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &merged).await?.as_ref(),
        Some(&merged),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &head_changed)
            .await?
            .as_ref(),
        Some(&head_changed),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &mergeable_state_changed)
            .await?
            .as_ref(),
        Some(&mergeable_state_changed),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &checks_completed)
            .await?
            .as_ref(),
        Some(&checks_completed),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &check_run_completed)
            .await?
            .as_ref(),
        Some(&check_run_completed),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &review_submitted)
            .await?
            .as_ref(),
        Some(&review_submitted),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &thread_opened)
            .await?
            .as_ref(),
        Some(&thread_opened),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &thread_resolved)
            .await?
            .as_ref(),
        Some(&thread_resolved),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &labeled).await?.as_ref(),
        Some(&labeled),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &unlabeled)
            .await?
            .as_ref(),
        Some(&unlabeled),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &base_advanced)
            .await?
            .as_ref(),
        Some(&base_advanced),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &reaction_changed)
            .await?
            .as_ref(),
        Some(&reaction_changed),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &branch_workflow_run_completed)
            .await?
            .as_ref(),
        Some(&branch_workflow_run_completed),
        "a committed repository-watch event must load back unchanged"
    );
    Ok(())
}

/// Every reaction subject, in an inventory the compiler keeps complete.
///
/// Each arm names its successor, so adding a variant makes this `match`
/// non-exhaustive and the test crate stops compiling until the new subject is
/// slotted into the chain — at which point it is in the returned list and the
/// coverage assertion below demands a committed sample for it.
///
/// The same idiom as `RepoWatchEventKindNameV1::all()`, and deliberately
/// dependency-free: no `strum`, no `EnumIter`. A hand-written `vec![..]` is
/// what `docs/style.md` forbids, because it goes stale in silence.
fn every_subject() -> Result<Vec<ReactionSubject>, Box<dyn Error>> {
    let mut subjects = Vec::new();
    let mut next = Some(ReactionSubject::PullRequestBody);
    while let Some(current) = next {
        next = match current {
            ReactionSubject::PullRequestBody => Some(ReactionSubject::IssueComment {
                id: GitHubObjectId::new(ISSUE_COMMENT_ID.try_into()?),
            }),
            ReactionSubject::IssueComment { .. } => Some(ReactionSubject::ReviewComment {
                id: GitHubObjectId::new(REVIEW_COMMENT_ID.try_into()?),
            }),
            ReactionSubject::ReviewComment { .. } => None,
        };
        subjects.push(current);
    }
    Ok(subjects)
}

/// The subject one reaction event carries, for the inventory assertion.
///
/// Exhaustive over `RepoWatchEventKindV1` rather than closed with a wildcard:
/// the inventory test's coverage claim is only as good as the set of kinds
/// this projection considered, and a wildcard would let a new variant join the
/// enum without the claim being revisited. Deliberately dependency-free — a
/// new kind makes this match non-exhaustive and stops the crate compiling.
fn reacted_subject(event: &RepoWatchEvent) -> Result<ReactionSubject, Box<dyn Error>> {
    match event.kind() {
        RepoWatchEventKindV1::ReactionChanged { subject, .. } => Ok(*subject),
        other @ (RepoWatchEventKindV1::PullRequestOpened
        | RepoWatchEventKindV1::PullRequestClosed
        | RepoWatchEventKindV1::PullRequestMerged
        | RepoWatchEventKindV1::HeadChanged { .. }
        | RepoWatchEventKindV1::MergeableStateChanged { .. }
        | RepoWatchEventKindV1::ChecksCompleted { .. }
        | RepoWatchEventKindV1::CheckRunCompleted { .. }
        | RepoWatchEventKindV1::BranchWorkflowRunCompleted { .. }
        | RepoWatchEventKindV1::ReviewSubmitted { .. }
        | RepoWatchEventKindV1::ThreadOpened { .. }
        | RepoWatchEventKindV1::ThreadResolved { .. }
        | RepoWatchEventKindV1::Labeled { .. }
        | RepoWatchEventKindV1::Unlabeled { .. }
        | RepoWatchEventKindV1::BaseAdvanced { .. }) => {
            Err(format!("expected a reaction event, got {other:?}").into())
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn every_reaction_subject_survives_a_commit_and_load_round_trip() -> Result<(), Box<dyn Error>>
{
    let mut ids = FixedEventIds::default();
    let pull_request_body = reaction_event(&mut ids, ReactionSubject::PullRequestBody)?;
    let issue_comment = reaction_event(
        &mut ids,
        ReactionSubject::IssueComment {
            id: GitHubObjectId::new(ISSUE_COMMENT_ID.try_into()?),
        },
    )?;
    let review_comment = reaction_event(
        &mut ids,
        ReactionSubject::ReviewComment {
            id: GitHubObjectId::new(REVIEW_COMMENT_ID.try_into()?),
        },
    )?;
    // Same guard as the event-kind test, one level down. `every_subject` is
    // produced by an exhaustive match, so a new `ReactionSubject` variant fails
    // to compile there; it lives here rather than in the domain crate because
    // its variants carry object ids that only this fixture can supply.
    assert_eq!(
        [&pull_request_body, &issue_comment, &review_comment]
            .into_iter()
            .map(reacted_subject)
            .collect::<Result<HashSet<_>, _>>()?,
        every_subject()?.into_iter().collect(),
        "every reaction subject must have a committed round trip"
    );

    let (_container, store, repository) = committed_event_fixture(vec![
        pull_request_body.clone(),
        issue_comment.clone(),
        review_comment.clone(),
    ])
    .await?;

    assert_eq!(
        loaded_event(&store, &repository, &pull_request_body)
            .await?
            .as_ref(),
        Some(&pull_request_body),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &issue_comment)
            .await?
            .as_ref(),
        Some(&issue_comment),
        "a committed repository-watch event must load back unchanged"
    );
    assert_eq!(
        loaded_event(&store, &repository, &review_comment)
            .await?
            .as_ref(),
        Some(&review_comment),
        "a committed repository-watch event must load back unchanged"
    );
    Ok(())
}
