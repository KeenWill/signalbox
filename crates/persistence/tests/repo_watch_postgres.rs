#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{
    collections::{BTreeSet, HashSet},
    error::Error,
    num::{NonZeroU16, NonZeroU64},
    time::Duration,
};

use signalbox_application::{
    RepoWatchBranchHead, RepoWatchCheckCompletionGeneration, RepoWatchCheckRunObservation,
    RepoWatchCheckSuiteObservation, RepoWatchConvergenceAssessment,
    RepoWatchConvergenceAssessmentInput, RepoWatchEventContentIdentityV1,
    RepoWatchEventIdGenerator, RepoWatchEventOccurrenceV1, RepoWatchObservation,
    RepoWatchPullRequestLifecycle, RepoWatchPullRequestState, RepoWatchPullRequestStateInput,
    RepoWatchRepositoryState, RepoWatchRepositoryStateInput, RepoWatchReviewDecision,
    RepoWatchStaleReviewClearanceCandidate, RepoWatchThreadObservation, RepoWatchThreadState,
    RepoWatchWorkflowRunObservation, derive_repo_watch_events,
};
use signalbox_domain::{
    BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha, GitHubObjectId, LabelName,
    MergeableState, PullRequestBody, PullRequestEventContext, PullRequestEventContextInput,
    PullRequestNumber, PullRequestTitle, ReactionChange, ReactionContent, ReactionSubject,
    RepoWatchAuthorLogin, RepoWatchEvent, RepoWatchEventId, RepoWatchEventKindNameV1,
    RepoWatchEventKindV1, RepoWatchWorkflowRunAttempt, RepositorySlug, ReviewState, ReviewThreadId,
    WorkflowName,
};
use signalbox_persistence::{
    attention::AutomaticResumeAttemptBounds,
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
    repo_watch::{
        PostgresRepoWatchStore, RepoWatchCommitOutcome, RepoWatchCommitRequest,
        RepoWatchCursorCandidate, RepoWatchCursorGeneration, RepoWatchEventPageSize,
        RepoWatchPersistenceCorruption, RepoWatchStoreError,
    },
    repo_watch_operations::PostgresRepoWatchOperations,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
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
const COMPRESSIBLE_CURSOR_PADDING_BYTES: i32 = 128 * 1024;
// Distinct from the context's `author` and `head_sha` on purpose. Reusing those
// would let a decoder that read `author` where it should read `review_reviewer`
// — or `head_sha` where it should read `review_commit` — still satisfy the
// round trip, so a cross-wired durable column would be invisible here.
const REVIEW_REVIEWER: &str = "fixture-reviewer";
const REVIEW_NODE: &str = "PRR_fixture_review_node";
const REVIEW_COMMIT: &str = "3333333333333333333333333333333333333333";
const REACTOR: &str = "fixture-reactor";
const PULL_REQUEST: u64 = 41;
const CHECK_SUITE_ID: u64 = 51;
const CHECK_RUN_ID: u64 = 52;
const ISSUE_COMMENT_ID: u64 = 61;
const REVIEW_COMMENT_ID: u64 = 62;
/// This operator read turns on the durable pull-request projection, never on
/// how many automatic resumptions a deployment still owes, so it states the
/// unbounded automatic-resume budget instead of a number its story never uses.
const UNBOUNDED_AUTOMATIC_RESUME_ATTEMPTS: AutomaticResumeAttemptBounds =
    AutomaticResumeAttemptBounds::unbounded();

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
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
    Ok((container, pool))
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
    observation_at_base(head, BASE_REVISION)
}

fn observation_at_base(
    head: Option<&str>,
    base_revision: &str,
) -> Result<RepoWatchObservation, Box<dyn Error>> {
    let pull_requests = head.map(pull_request).transpose()?.into_iter().collect();
    Ok(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests,
            workflow_runs: Vec::new(),
            branch_heads: vec![RepoWatchBranchHead::new(
                BranchName::try_new(BASE_BRANCH.to_owned())?,
                CommitSha::try_new(base_revision.to_owned())?,
            )],
        })?,
    ))
}

fn pull_request(head: &str) -> Result<RepoWatchPullRequestState, Box<dyn Error>> {
    pull_request_with_threads(head, Vec::new())
}

fn pull_request_with_threads(
    head: &str,
    threads: Vec<RepoWatchThreadObservation>,
) -> Result<RepoWatchPullRequestState, Box<dyn Error>> {
    pull_request_state(head, threads, MergeableState::Mergeable)
}

fn pull_request_state(
    head: &str,
    threads: Vec<RepoWatchThreadObservation>,
    mergeable_state: MergeableState,
) -> Result<RepoWatchPullRequestState, Box<dyn Error>> {
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
            mergeable_state,
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
            threads,
            reactions: Vec::new(),
        },
    )?)
}

fn candidate_with_open_thread(head: &str) -> Result<RepoWatchCursorCandidate, Box<dyn Error>> {
    Ok(RepoWatchCursorCandidate::new(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![pull_request_with_threads(
                head,
                vec![RepoWatchThreadObservation::new(
                    ReviewThreadId::try_new(REVIEW_THREAD.to_owned())?,
                    RepoWatchThreadState::Open,
                )],
            )?],
            workflow_runs: Vec::new(),
            branch_heads: vec![RepoWatchBranchHead::new(
                BranchName::try_new(BASE_BRANCH.to_owned())?,
                CommitSha::try_new(BASE_REVISION.to_owned())?,
            )],
        })?,
    )))
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
            settled: true,
            review_decision: RepoWatchReviewDecision::None,
            unresolved_threads: Vec::new(),
            gating_check_count: 1,
            non_green_gating_checks: Vec::new(),
        },
    )?)
}

/// The same stale-review evidence with the gating check the candidate rule
/// requires, so the head's only remaining blocker really is the review.
fn clearable_stale_review_assessment(
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
            settled: true,
            review_decision: RepoWatchReviewDecision::ChangesRequested,
            unresolved_threads: Vec::new(),
            gating_check_count: 1,
            non_green_gating_checks: Vec::new(),
        },
    )?)
}

fn stale_review_clearance_candidate(
    assessment: &RepoWatchConvergenceAssessment,
) -> Result<RepoWatchStaleReviewClearanceCandidate, Box<dyn Error>> {
    Ok(RepoWatchStaleReviewClearanceCandidate::try_new(
        assessment,
        String::from(REVIEW_NODE),
        RepoWatchAuthorLogin::try_new(REVIEW_REVIEWER.to_owned())?,
        CommitSha::try_new(REVIEW_COMMIT.to_owned())?,
    )?)
}

/// The same stale-review evidence for a head that has not finished registering
/// and completing its exact-head checks, so its empty non-green list is the
/// absence of evidence rather than evidence of a green head.
fn unsettled_stale_review_assessment(
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
            settled: false,
            review_decision: RepoWatchReviewDecision::ChangesRequested,
            unresolved_threads: Vec::new(),
            gating_check_count: 1,
            non_green_gating_checks: Vec::new(),
        },
    )?)
}

/// The same stale-review evidence recorded while GitHub had not decided the
/// head's mergeability. `unknown` is that pending state, never affirmative
/// evidence that the head merges.
fn undecided_mergeability_stale_review_assessment(
    head: &str,
    base_revision: &str,
) -> Result<RepoWatchConvergenceAssessment, Box<dyn Error>> {
    Ok(RepoWatchConvergenceAssessment::try_new(
        RepoWatchConvergenceAssessmentInput {
            number: PullRequestNumber::new(PULL_REQUEST.try_into()?),
            head_sha: CommitSha::try_new(head.to_owned())?,
            base_branch: BranchName::try_new(BASE_BRANCH.to_owned())?,
            base_revision: CommitSha::try_new(base_revision.to_owned())?,
            mergeable_state: MergeableState::Unknown,
            settled: true,
            review_decision: RepoWatchReviewDecision::ChangesRequested,
            unresolved_threads: Vec::new(),
            gating_check_count: 1,
            non_green_gating_checks: Vec::new(),
        },
    )?)
}

/// The cursor an assessment carrying `unknown` mergeability is recorded
/// against: recorded evidence must restate the observed mergeable state.
fn undecided_mergeability_candidate(
    head: &str,
) -> Result<RepoWatchCursorCandidate, Box<dyn Error>> {
    Ok(RepoWatchCursorCandidate::new(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![pull_request_state(
                head,
                Vec::new(),
                MergeableState::Unknown,
            )?],
            workflow_runs: Vec::new(),
            branch_heads: vec![RepoWatchBranchHead::new(
                BranchName::try_new(BASE_BRANCH.to_owned())?,
                CommitSha::try_new(BASE_REVISION.to_owned())?,
            )],
        })?,
    )))
}

/// Stale-review evidence for a head that ran no gating check at all. Its empty
/// non-green list is indistinguishable from a fully green head's.
fn stale_review_assessment(
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
            settled: true,
            review_decision: RepoWatchReviewDecision::ChangesRequested,
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

struct CommittedFixture {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
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
        pool,
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
async fn cursor_payload_size_reports_the_latest_stored_document() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());

    let absent = store.load_cursor_payload_bytes(&repository).await?;
    store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(None, candidate(Some(INITIAL_HEAD))?, Vec::new()),
        )
        .await?;
    let mut connection = pool.acquire().await?;
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut *connection)
        .await?;
    sqlx::query(
        "UPDATE repo_watch_cursor
            SET cursor_payload = cursor_payload ||
                jsonb_build_object('sizing_fixture', repeat('x', $2))
          WHERE repository = $1",
    )
    .bind(repository.as_str())
    .bind(COMPRESSIBLE_CURSOR_PADDING_BYTES)
    .execute(&mut *connection)
    .await?;
    sqlx::query("SET session_replication_role = origin")
        .execute(&mut *connection)
        .await?;
    let reported = store.load_cursor_payload_bytes(&repository).await?;
    let logical: i64 = sqlx::query_scalar(
        "SELECT octet_length(cursor_payload::text)::bigint
           FROM repo_watch_cursor
          WHERE repository = $1",
    )
    .bind(repository.as_str())
    .fetch_one(&pool)
    .await?;
    let compressed: i64 = sqlx::query_scalar(
        "SELECT pg_column_size(cursor_payload)::bigint
           FROM repo_watch_cursor
          WHERE repository = $1",
    )
    .bind(repository.as_str())
    .fetch_one(&pool)
    .await?;

    assert_eq!(absent, None);
    assert_eq!(reported, Some(u64::try_from(logical)?));
    assert!(compressed < logical);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pull_request_pages_read_the_current_projection_without_decoding_the_cursor()
-> Result<(), Box<dyn Error>> {
    let fixture = committed_fixture().await?;
    let mut connection = fixture.pool.acquire().await?;
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut *connection)
        .await?;
    // The payload keeps only its storage version, which no decode accepts as a
    // cursor. Copying that version from the column rather than naming a literal
    // keeps the row inside the table's payload/column agreement check, so the
    // corruption stays undecodable across a storage-version bump instead of
    // failing the write the next bump lands.
    sqlx::query(
        "UPDATE repo_watch_cursor
            SET cursor_payload = jsonb_build_object('storage_version', storage_version)
          WHERE repository = $1 AND generation = $2",
    )
    .bind(fixture.repository.as_str())
    .bind(i64::try_from(fixture.second_generation.get())?)
    .execute(&mut *connection)
    .await?;
    sqlx::query("SET session_replication_role = origin")
        .execute(&mut *connection)
        .await?;
    drop(connection);

    let page =
        PostgresRepoWatchOperations::new(fixture.pool.clone(), UNBOUNDED_AUTOMATIC_RESUME_ATTEMPTS)
            .pull_requests(fixture.repository.clone(), None)
            .await?;

    assert_eq!(page.pull_requests.len(), 1);
    assert_eq!(
        page.pull_requests[0].number,
        fixture
            .second_candidate
            .observation()
            .state()
            .pull_requests()[0]
            .context()
            .number()
    );
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
async fn convergence_mismatch_rolls_back_cursor_events_and_evidence() -> Result<(), Box<dyn Error>>
{
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
    let mismatched_base = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    let failure = store
        .commit_with_convergence(
            &repository,
            RepoWatchCommitRequest::new(Some(first_generation), current, events),
            &[merge_ready_assessment(INITIAL_HEAD, mismatched_base)?],
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

/// A restarting daemon measures what is left of its poll cadence from this
/// record, so only the complete provider sweep may write it. The cursor cannot
/// stand in: a targeted webhook refresh commits generations of its own, and a
/// sweep that finds the repository unchanged commits none at all, so neither the
/// newest generation's age nor its existence measures the sweep cadence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn only_a_complete_sweep_records_the_poll_cadence() -> Result<(), Box<dyn Error>> {
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

    let after_webhook_commit = store.load_complete_poll_age(&repository).await?;

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
    let swept_generation = committed_generation(
        store
            .commit_with_convergence(
                &repository,
                RepoWatchCommitRequest::new(Some(first_generation), current.clone(), events),
                &[merge_ready_assessment(INITIAL_HEAD, BASE_REVISION)?],
            )
            .await?,
    );
    let after_sweep = store.load_complete_poll_age(&repository).await?;
    let swept_at = recorded_complete_poll(&pool).await?;

    // A sweep whose commit conflicts never observed the repository completely,
    // so it must leave the previous deadline in force.
    let conflicted = store
        .commit_with_convergence(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                candidate(Some(CHANGED_HEAD))?,
                Vec::new(),
            ),
            &[merge_ready_assessment(CHANGED_HEAD, BASE_REVISION)?],
        )
        .await?;
    let after_conflict = recorded_complete_poll(&pool).await?;

    // A sweep that finds the repository unchanged writes no cursor generation
    // and is still the completed sweep the cadence measures.
    let unchanged = store
        .commit_with_convergence(
            &repository,
            RepoWatchCommitRequest::new(Some(swept_generation), current, Vec::new()),
            &[merge_ready_assessment(INITIAL_HEAD, BASE_REVISION)?],
        )
        .await?;
    let after_unchanged = recorded_complete_poll(&pool).await?;

    assert_eq!(
        after_webhook_commit, None,
        "a cursor generation is not evidence that a sweep completed"
    );
    assert!(
        after_sweep.is_some(),
        "the completed sweep records the cadence the next restart reads"
    );
    assert!(matches!(
        conflicted,
        RepoWatchCommitOutcome::Conflict { .. }
    ));
    assert_eq!(
        after_conflict, swept_at,
        "a rolled-back sweep leaves the previous deadline in force"
    );
    assert!(matches!(unchanged, RepoWatchCommitOutcome::Unchanged(_)));
    assert_ne!(
        after_unchanged, swept_at,
        "an unchanged sweep still completed, and commits no generation to measure"
    );
    Ok(())
}

async fn recorded_complete_poll(pool: &PgPool) -> Result<Option<String>, Box<dyn Error>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT completed_at::text FROM repo_watch_complete_poll WHERE repository = $1",
    )
    .bind(REPOSITORY)
    .fetch_optional(pool)
    .await?)
}

/// Waits until a backend other than this test's is queued behind an advisory
/// lock in this database.
///
/// The observation has to be positive for the caller to be a classifier at all:
/// a fixed delay before marking the clock passes against a transaction-start
/// stamp whenever the spawned sweep is slower to reach the lock than the delay,
/// which is exactly what a loaded runner produces. Waiting on the contention
/// itself makes the marker provably later than the sweep's transaction start.
async fn await_repository_lock_contention(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    const PROBE_INTERVAL: Duration = Duration::from_millis(10);
    // Only reached when the sweep never queues at all, and failing is then the
    // correct outcome, so this buys patience rather than encoding a latency.
    const PROBE_ATTEMPTS: u32 = 3_000;

    for _ in 0..PROBE_ATTEMPTS {
        let contended: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM pg_locks
                 WHERE locktype = 'advisory'
                   AND NOT granted
                   AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
             )",
        )
        .fetch_one(pool)
        .await?;
        if contended {
            return Ok(());
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
    Err("no backend ever queued behind the repository's advisory lock".into())
}

/// The cadence stamp measures when the sweep committed, not when its
/// transaction opened. Those differ by the wait for the per-repository advisory
/// lock — a sweep can queue behind a targeted webhook commit — and a stamp taken
/// from the transaction's start hands that wait to the next restart as elapsed
/// cadence, bringing the following sweep forward by it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_cadence_stamp_excludes_the_wait_for_the_repository_lock() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let baseline = candidate(None)?;
    let mut blocker = pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
        .bind(REPOSITORY)
        .execute(&mut *blocker)
        .await?;

    let sweeping = tokio::spawn({
        let store = store.clone();
        let repository = repository.clone();
        async move {
            store
                .commit_with_convergence(
                    &repository,
                    RepoWatchCommitRequest::new(None, baseline, Vec::new()),
                    &[],
                )
                .await
        }
    });
    await_repository_lock_contention(&pool).await?;
    let held_until: String = sqlx::query_scalar("SELECT clock_timestamp()::text")
        .fetch_one(&mut *blocker)
        .await?;
    let released: bool = sqlx::query_scalar("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
        .bind(REPOSITORY)
        .fetch_one(&mut *blocker)
        .await?;
    let swept = sweeping.await??;

    assert!(released, "the fixture releases its deliberate lock");
    assert!(matches!(swept, RepoWatchCommitOutcome::Committed(_)));
    let stamped_after_the_wait: bool = sqlx::query_scalar(
        "SELECT completed_at > $2::timestamptz
           FROM repo_watch_complete_poll
          WHERE repository = $1",
    )
    .bind(REPOSITORY)
    .bind(&held_until)
    .fetch_one(&pool)
    .await?;
    assert!(
        stamped_after_the_wait,
        "the stamp is taken once the lock is held, not when the transaction opened"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn assessment_threads_must_match_the_committed_cursor() -> Result<(), Box<dyn Error>> {
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
    let current = candidate_with_open_thread(INITIAL_HEAD)?;
    let mut current_frontier = baseline.event_identity_frontier().clone();
    let events = derive_repo_watch_events(
        &repository,
        Some(baseline.observation()),
        current.observation(),
        &mut current_frontier,
        &mut FixedEventIds::default(),
    )?;
    let current = RepoWatchCursorCandidate::with_event_identity_frontier(
        current.observation().clone(),
        current_frontier,
    );

    let failure = store
        .commit_with_convergence(
            &repository,
            RepoWatchCommitRequest::new(Some(baseline_generation), current, events),
            &[merge_ready_assessment(INITIAL_HEAD, BASE_REVISION)?],
        )
        .await;
    let cursor = store
        .load_cursor(&repository)
        .await?
        .expect("the baseline cursor remains present");
    let assessment_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_pull_request_convergence_assessment")
            .fetch_one(&pool)
            .await?;

    assert!(matches!(
        failure,
        Err(RepoWatchStoreError::ConvergenceEvidenceMismatch)
    ));
    assert_eq!(cursor.generation(), baseline_generation);
    assert_eq!(assessment_count, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn superseded_exact_replay_does_not_record_convergence_evidence() -> Result<(), Box<dyn Error>>
{
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
        .commit_with_convergence(
            &fixture.repository,
            RepoWatchCommitRequest::new(
                Some(fixture.first_generation),
                fixture.second_candidate.clone(),
                fixture.events.clone(),
            ),
            &[merge_ready_assessment(INITIAL_HEAD, BASE_REVISION)?],
        )
        .await?;
    let assessment_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_pull_request_convergence_assessment")
            .fetch_one(&fixture.pool)
            .await?;
    let identity_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_pull_request_convergence_identity")
            .fetch_one(&fixture.pool)
            .await?;

    assert_eq!(replayed_generation(replay), fixture.second_generation);
    assert_eq!(assessment_count, 0);
    assert_eq!(identity_count, 0);
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
    let second_observation = observation(Some(CHANGED_HEAD))?;
    let mut ids = FixedEventIds::default();
    let mut first_frontier = baseline.event_identity_frontier().clone();
    let first_events = derive_repo_watch_events(
        &repository,
        Some(baseline.observation()),
        &first_observation,
        &mut first_frontier,
        &mut ids,
    )?;
    let first = RepoWatchCursorCandidate::with_event_identity_frontier(
        first_observation.clone(),
        first_frontier,
    );
    let first_generation = committed_generation(
        store
            .commit_with_convergence(
                &repository,
                RepoWatchCommitRequest::new(Some(baseline_generation), first.clone(), first_events),
                &[merge_ready_assessment(INITIAL_HEAD, BASE_REVISION)?],
            )
            .await?,
    );
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
    let mut replay_frontier = second.event_identity_frontier().clone();
    let replay_events = derive_repo_watch_events(
        &repository,
        Some(second.observation()),
        &first_observation,
        &mut replay_frontier,
        &mut ids,
    )?;
    let replay =
        RepoWatchCursorCandidate::with_event_identity_frontier(first_observation, replay_frontier);

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
    let current_head: String =
        sqlx::query_scalar("SELECT head_sha FROM repo_watch_current_pull_request_convergence")
            .fetch_one(&pool)
            .await?;
    let identity_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_pull_request_convergence_identity")
            .fetch_one(&pool)
            .await?;

    assert_eq!(assessment_count, 3);
    assert_eq!(first_head_assessment_count, 2);
    assert_eq!(current_head, INITIAL_HEAD);
    assert_eq!(identity_count, 3);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn evidence_replay_after_a_base_round_trip_uses_the_candidate_base()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let second_base = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let first_observation = observation_at_base(Some(INITIAL_HEAD), BASE_REVISION)?;
    let second_observation = observation_at_base(Some(INITIAL_HEAD), second_base)?;
    let first = RepoWatchCursorCandidate::new(first_observation.clone());
    let first_generation = committed_generation(
        store
            .commit_with_convergence(
                &repository,
                RepoWatchCommitRequest::new(None, first.clone(), Vec::new()),
                &[merge_ready_assessment(INITIAL_HEAD, BASE_REVISION)?],
            )
            .await?,
    );
    let mut ids = FixedEventIds::default();
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
                &[merge_ready_assessment(INITIAL_HEAD, second_base)?],
            )
            .await?,
    );
    let mut restored_frontier = second.event_identity_frontier().clone();
    let restored_events = derive_repo_watch_events(
        &repository,
        Some(second.observation()),
        &first_observation,
        &mut restored_frontier,
        &mut ids,
    )?;
    let restored = RepoWatchCursorCandidate::with_event_identity_frontier(
        first_observation,
        restored_frontier,
    );

    store
        .commit_with_convergence(
            &repository,
            RepoWatchCommitRequest::new(Some(second_generation), restored, restored_events),
            &[merge_ready_assessment(INITIAL_HEAD, BASE_REVISION)?],
        )
        .await?;
    let assessment_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_pull_request_convergence_assessment")
            .fetch_one(&pool)
            .await?;
    let first_base_assessment_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_pull_request_convergence_assessment
          WHERE base_revision = $1",
    )
    .bind(BASE_REVISION)
    .fetch_one(&pool)
    .await?;
    let current_base: String =
        sqlx::query_scalar("SELECT base_revision FROM repo_watch_current_pull_request_convergence")
            .fetch_one(&pool)
            .await?;
    let identity_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_pull_request_convergence_identity")
            .fetch_one(&pool)
            .await?;

    assert_eq!(assessment_count, 3);
    assert_eq!(first_base_assessment_count, 2);
    assert_eq!(current_base, BASE_REVISION);
    assert_eq!(identity_count, 3);
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
async fn stale_review_clearance_journals_are_append_only() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    store
        .commit_with_convergence(
            &repository,
            RepoWatchCommitRequest::new(None, candidate(Some(INITIAL_HEAD))?, Vec::new()),
            &[stale_review_assessment(INITIAL_HEAD, BASE_REVISION)?],
        )
        .await?;
    let assessment_id: Uuid = sqlx::query_scalar(
        "SELECT assessment_id
           FROM repo_watch_pull_request_convergence_assessment
          WHERE repository = $1 AND head_sha = $2",
    )
    .bind(repository.as_str())
    .bind(INITIAL_HEAD)
    .fetch_one(&pool)
    .await?;
    let clearance_id = Uuid::from_u128(0x70_001);
    sqlx::query(
        "INSERT INTO repo_watch_stale_review_clearance
            (clearance_id, assessment_id, repository, pull_request_number,
             current_head_sha, base_revision, review_node_id, reviewer,
             reviewed_head_sha, dismissal_message)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(clearance_id)
    .bind(assessment_id)
    .bind(repository.as_str())
    .bind(i64::try_from(PULL_REQUEST)?)
    .bind(INITIAL_HEAD)
    .bind(BASE_REVISION)
    .bind("review-node-70")
    .bind(REVIEW_REVIEWER)
    .bind(REVIEW_COMMIT)
    .bind("fixture stale-review dismissal")
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO repo_watch_stale_review_clearance_result
            (clearance_id, outcome_kind, provider_review_state)
         VALUES ($1, 'dismissed', 'dismissed')",
    )
    .bind(clearance_id)
    .execute(&pool)
    .await?;

    let intent_update = sqlx::query(
        "UPDATE repo_watch_stale_review_clearance SET reviewer = $1 WHERE clearance_id = $2",
    )
    .bind(AUTHOR)
    .bind(clearance_id)
    .execute(&pool)
    .await;
    let intent_delete =
        sqlx::query("DELETE FROM repo_watch_stale_review_clearance WHERE clearance_id = $1")
            .bind(clearance_id)
            .execute(&pool)
            .await;
    let intent_truncate = sqlx::query("TRUNCATE repo_watch_stale_review_clearance CASCADE")
        .execute(&pool)
        .await;
    let result_update = sqlx::query(
        "UPDATE repo_watch_stale_review_clearance_result SET observed_at = clock_timestamp() WHERE clearance_id = $1",
    )
    .bind(clearance_id)
    .execute(&pool)
    .await;
    let result_delete =
        sqlx::query("DELETE FROM repo_watch_stale_review_clearance_result WHERE clearance_id = $1")
            .bind(clearance_id)
            .execute(&pool)
            .await;
    let result_truncate = sqlx::query("TRUNCATE repo_watch_stale_review_clearance_result CASCADE")
        .execute(&pool)
        .await;

    assert!(intent_update.is_err());
    assert!(intent_delete.is_err());
    assert!(intent_truncate.is_err());
    assert!(result_update.is_err());
    assert!(result_delete.is_err());
    assert!(result_truncate.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_stale_review_clearance_plans_against_its_recorded_assessment()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let assessment = clearable_stale_review_assessment(INITIAL_HEAD, BASE_REVISION)?;
    let generation = committed_generation(
        store
            .commit_with_convergence(
                &repository,
                RepoWatchCommitRequest::new(None, candidate(Some(INITIAL_HEAD))?, Vec::new()),
                std::slice::from_ref(&assessment),
            )
            .await?,
    );

    let planned = store
        .plan_stale_review_clearances(
            &repository,
            generation,
            &[stale_review_clearance_candidate(&assessment)?],
        )
        .await?;

    assert_eq!(planned.len(), 1);
    assert_eq!(planned[0].review_node_id(), REVIEW_NODE);
    assert_eq!(planned[0].reviewer().as_str(), REVIEW_REVIEWER);
    assert_eq!(planned[0].reviewed_head_sha().as_str(), REVIEW_COMMIT);
    assert_eq!(planned[0].current_head_sha().as_str(), INITIAL_HEAD);
    assert_eq!(planned[0].base_revision().as_str(), BASE_REVISION);
    Ok(())
}

/// The durable gate reads the recorded assessment, not the candidate's own
/// evidence, so it refuses a head whose committed convergence row counted no
/// gating check even when the in-memory candidate was admissible. Without that
/// term the daemon would dismiss a blocking review on a pull request whose only
/// gate was the review itself.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_recorded_assessment_without_a_gating_check_plans_no_clearance()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let generation = committed_generation(
        store
            .commit_with_convergence(
                &repository,
                RepoWatchCommitRequest::new(None, candidate(Some(INITIAL_HEAD))?, Vec::new()),
                &[stale_review_assessment(INITIAL_HEAD, BASE_REVISION)?],
            )
            .await?,
    );
    let candidate = stale_review_clearance_candidate(&clearable_stale_review_assessment(
        INITIAL_HEAD,
        BASE_REVISION,
    )?)?;

    let planned = store
        .plan_stale_review_clearances(&repository, generation, &[candidate])
        .await;

    assert!(matches!(
        planned,
        Err(RepoWatchStoreError::StaleReviewClearanceMismatch)
    ));
    let intents: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_stale_review_clearance")
        .fetch_one(&pool)
        .await?;
    assert_eq!(intents, 0);
    Ok(())
}

/// Another watcher can append a newer assessment for the unchanged cursor while
/// this watcher reconciles the candidate it raised, and the durable gate reads
/// that newest row. An unsettled head has not finished registering and
/// completing its exact-head checks, so planning against it would link a
/// dismissal intent claiming `only_stale_review_blocks` to evidence recording a
/// second blocker.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_unsettled_newer_assessment_plans_no_clearance() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let assessment = clearable_stale_review_assessment(INITIAL_HEAD, BASE_REVISION)?;
    let generation = committed_generation(
        store
            .commit_with_convergence(
                &repository,
                RepoWatchCommitRequest::new(None, candidate(Some(INITIAL_HEAD))?, Vec::new()),
                std::slice::from_ref(&assessment),
            )
            .await?,
    );
    store
        .record_convergence_assessments(
            &repository,
            generation,
            &[unsettled_stale_review_assessment(
                INITIAL_HEAD,
                BASE_REVISION,
            )?],
        )
        .await?;

    let planned = store
        .plan_stale_review_clearances(
            &repository,
            generation,
            &[stale_review_clearance_candidate(&assessment)?],
        )
        .await;

    assert!(matches!(
        planned,
        Err(RepoWatchStoreError::StaleReviewClearanceMismatch)
    ));
    let intents: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_stale_review_clearance")
        .fetch_one(&pool)
        .await?;
    assert_eq!(intents, 0);
    Ok(())
}

/// The durable gate proves mergeability against the recorded row rather than
/// inferring it from the settlement recorded beside it, so evidence GitHub had
/// not decided plans nothing even when its writer called the head settled.
/// `unknown` is that pending state, and dismissing against it would claim the
/// review was the head's only blocker while the recorded evidence names
/// mergeability as another.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_undecided_mergeability_assessment_plans_no_clearance() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let generation = committed_generation(
        store
            .commit_with_convergence(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    undecided_mergeability_candidate(INITIAL_HEAD)?,
                    Vec::new(),
                ),
                &[undecided_mergeability_stale_review_assessment(
                    INITIAL_HEAD,
                    BASE_REVISION,
                )?],
            )
            .await?,
    );
    let candidate = stale_review_clearance_candidate(&clearable_stale_review_assessment(
        INITIAL_HEAD,
        BASE_REVISION,
    )?)?;

    let planned = store
        .plan_stale_review_clearances(&repository, generation, &[candidate])
        .await;

    assert!(matches!(
        planned,
        Err(RepoWatchStoreError::StaleReviewClearanceMismatch)
    ));
    let intents: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_stale_review_clearance")
        .fetch_one(&pool)
        .await?;
    assert_eq!(intents, 0);
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
        "UPDATE repo_watch_cursor SET cursor_payload = '{\"storage_version\":4}'::jsonb WHERE repository = $1",
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

const REAPPEARING_WORKFLOW_RUN_ID: u64 = 71;
const REAPPEARING_WORKFLOW_ID: u64 = 72;

const RENAMED_WORKFLOW_NAME: &str = "required checks (renamed)";

/// Whether the reappearing branch-workflow run is in the observation.
enum WorkflowRunPresence {
    /// The provider lists the completed run.
    Listed,
    /// The provider omits it, as it does while the branch is deleted.
    Omitted,
}

/// An observation carrying only the reappearing branch-workflow run, or none.
fn workflow_run_observation(
    presence: WorkflowRunPresence,
) -> Result<RepoWatchObservation, Box<dyn Error>> {
    let workflow_runs = match presence {
        WorkflowRunPresence::Listed => vec![RepoWatchWorkflowRunObservation::new(
            GitHubObjectId::new(REAPPEARING_WORKFLOW_RUN_ID.try_into()?),
            GitHubObjectId::new(REAPPEARING_WORKFLOW_ID.try_into()?),
            RepoWatchWorkflowRunAttempt::new(NonZeroU64::MIN),
            BranchName::try_new(BASE_BRANCH.to_owned())?,
            WorkflowName::try_new(WORKFLOW_NAME.to_owned())?,
            CheckConclusion::Success,
        )],
        WorkflowRunPresence::Omitted => Vec::new(),
    };
    Ok(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: Vec::new(),
            workflow_runs,
            branch_heads: Vec::new(),
        })?,
    ))
}

/// The same run, listed after its workflow was given a new display name.
fn renamed_workflow_run_observation() -> Result<RepoWatchObservation, Box<dyn Error>> {
    Ok(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: Vec::new(),
            workflow_runs: vec![RepoWatchWorkflowRunObservation::new(
                GitHubObjectId::new(REAPPEARING_WORKFLOW_RUN_ID.try_into()?),
                GitHubObjectId::new(REAPPEARING_WORKFLOW_ID.try_into()?),
                RepoWatchWorkflowRunAttempt::new(NonZeroU64::MIN),
                BranchName::try_new(BASE_BRANCH.to_owned())?,
                WorkflowName::try_new(RENAMED_WORKFLOW_NAME.to_owned())?,
                CheckConclusion::Success,
            )],
            branch_heads: Vec::new(),
        })?,
    ))
}

/// A branch-workflow run that leaves the observation and returns re-derives a
/// byte-identical content identity. Before commit coalescing that duplicate
/// aborted the whole cursor-and-event transaction, so the cursor never left the
/// run-absent generation and every later poll repeated the same failure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_reappearing_workflow_run_advances_the_cursor_without_a_duplicate_event()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let mut ids = FixedEventIds::default();

    let empty =
        RepoWatchCursorCandidate::new(workflow_run_observation(WorkflowRunPresence::Omitted)?);
    let first = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, empty.clone(), Vec::new()),
            )
            .await?,
    );

    // The run completes and is recorded.
    let mut frontier = empty.event_identity_frontier().clone();
    let present = workflow_run_observation(WorkflowRunPresence::Listed)?;
    let recorded = derive_repo_watch_events(
        &repository,
        Some(empty.observation()),
        &present,
        &mut frontier,
        &mut ids,
    )?;
    assert_eq!(recorded.len(), 1);
    let identity = recorded[0].content_identity();
    let recorded_event_id = recorded[0].event().id();
    let present_candidate =
        RepoWatchCursorCandidate::with_event_identity_frontier(present, frontier);
    let second = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(Some(first), present_candidate.clone(), recorded),
            )
            .await?,
    );

    // The branch is deleted, so the run leaves the observation.
    let mut frontier = present_candidate.event_identity_frontier().clone();
    let absent = workflow_run_observation(WorkflowRunPresence::Omitted)?;
    let none_derived = derive_repo_watch_events(
        &repository,
        Some(present_candidate.observation()),
        &absent,
        &mut frontier,
        &mut ids,
    )?;
    assert!(none_derived.is_empty());
    let absent_candidate = RepoWatchCursorCandidate::with_event_identity_frontier(absent, frontier);
    let third = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(Some(second), absent_candidate.clone(), none_derived),
            )
            .await?,
    );

    // The branch is recreated and the historical run reappears. The differ
    // re-derives the same occurrence, identity and all.
    let mut frontier = absent_candidate.event_identity_frontier().clone();
    let returned = workflow_run_observation(WorkflowRunPresence::Listed)?;
    let reemitted = derive_repo_watch_events(
        &repository,
        Some(absent_candidate.observation()),
        &returned,
        &mut frontier,
        &mut ids,
    )?;
    assert_eq!(reemitted.len(), 1);
    assert_eq!(reemitted[0].content_identity(), identity);
    // A fresh candidate id on an equal occurrence is exactly the case commit
    // coalescing has to recognize.
    assert_ne!(reemitted[0].event().id(), recorded_event_id);
    let returned_candidate =
        RepoWatchCursorCandidate::with_event_identity_frontier(returned, frontier);

    let outcome = store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(Some(third), returned_candidate, reemitted),
        )
        .await?;

    // The commit succeeds and the cursor advances rather than stalling.
    let fourth = committed_generation(outcome);
    assert_eq!(fourth.get(), third.get() + 1);
    assert_eq!(
        store
            .load_cursor(&repository)
            .await?
            .expect("cursor remains readable")
            .generation(),
        fourth
    );
    // The occurrence is recorded exactly once, so the identity still names one row.
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_watch_event
          WHERE repository = $1 AND content_identity = $2",
    )
    .bind(REPOSITORY)
    .bind(identity.as_bytes().as_slice())
    .fetch_one(&pool)
    .await?;
    assert_eq!(rows, 1);
    Ok(())
}

/// The same run returning after its workflow was renamed restates its content
/// identity, because the digest excludes the mutable display name. Storage has
/// to recognize it through the same equivalence: comparing whole events would
/// call the renamed payload a different fact, attempt an insert under an
/// already-durable identity, and abort the commit on the unique constraint,
/// leaving the cursor stuck at the run-absent generation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_renamed_workflow_run_reappearance_commits_without_aborting() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let mut ids = FixedEventIds::default();

    let empty =
        RepoWatchCursorCandidate::new(workflow_run_observation(WorkflowRunPresence::Omitted)?);
    let first = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, empty.clone(), Vec::new()),
            )
            .await?,
    );

    let mut frontier = empty.event_identity_frontier().clone();
    let present = workflow_run_observation(WorkflowRunPresence::Listed)?;
    let recorded = derive_repo_watch_events(
        &repository,
        Some(empty.observation()),
        &present,
        &mut frontier,
        &mut ids,
    )?;
    assert_eq!(recorded.len(), 1);
    let identity = recorded[0].content_identity();
    let present_candidate =
        RepoWatchCursorCandidate::with_event_identity_frontier(present, frontier);
    let second = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(Some(first), present_candidate.clone(), recorded),
            )
            .await?,
    );

    let mut frontier = present_candidate.event_identity_frontier().clone();
    let absent = workflow_run_observation(WorkflowRunPresence::Omitted)?;
    let none_derived = derive_repo_watch_events(
        &repository,
        Some(present_candidate.observation()),
        &absent,
        &mut frontier,
        &mut ids,
    )?;
    let absent_candidate = RepoWatchCursorCandidate::with_event_identity_frontier(absent, frontier);
    let third = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(Some(second), absent_candidate.clone(), none_derived),
            )
            .await?,
    );

    // The workflow is renamed while the run is out of the observation.
    let mut frontier = absent_candidate.event_identity_frontier().clone();
    let renamed = renamed_workflow_run_observation()?;
    let reemitted = derive_repo_watch_events(
        &repository,
        Some(absent_candidate.observation()),
        &renamed,
        &mut frontier,
        &mut ids,
    )?;
    assert_eq!(reemitted.len(), 1);
    assert_eq!(reemitted[0].content_identity(), identity);
    let renamed_candidate =
        RepoWatchCursorCandidate::with_event_identity_frontier(renamed, frontier);

    let fourth = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(Some(third), renamed_candidate, reemitted),
            )
            .await?,
    );

    assert_eq!(fourth.get(), third.get() + 1);
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_watch_event
          WHERE repository = $1 AND content_identity = $2",
    )
    .bind(REPOSITORY)
    .bind(identity.as_bytes().as_slice())
    .fetch_one(&pool)
    .await?;
    assert_eq!(rows, 1);
    Ok(())
}

/// A commit whose occurrences are all already durable stores no event, so a
/// stale retry of it has no durable UUID to be checked against: the fact it
/// restates is durable under the UUID of the occurrence that first recorded it.
/// Such a retry therefore replays on cursor candidate and content identity
/// alone, which is the narrowed meaning of exact replay for coalesced
/// occurrences.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_fully_coalesced_retry_replays_on_candidate_and_content_identity()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let mut ids = FixedEventIds::default();

    let empty =
        RepoWatchCursorCandidate::new(workflow_run_observation(WorkflowRunPresence::Omitted)?);
    let first = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, empty.clone(), Vec::new()),
            )
            .await?,
    );

    let mut frontier = empty.event_identity_frontier().clone();
    let present = workflow_run_observation(WorkflowRunPresence::Listed)?;
    let recorded = derive_repo_watch_events(
        &repository,
        Some(empty.observation()),
        &present,
        &mut frontier,
        &mut ids,
    )?;
    let present_candidate =
        RepoWatchCursorCandidate::with_event_identity_frontier(present, frontier);
    let second = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(Some(first), present_candidate.clone(), recorded),
            )
            .await?,
    );

    let mut frontier = present_candidate.event_identity_frontier().clone();
    let absent = workflow_run_observation(WorkflowRunPresence::Omitted)?;
    let none_derived = derive_repo_watch_events(
        &repository,
        Some(present_candidate.observation()),
        &absent,
        &mut frontier,
        &mut ids,
    )?;
    let absent_candidate = RepoWatchCursorCandidate::with_event_identity_frontier(absent, frontier);
    let third = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(Some(second), absent_candidate.clone(), none_derived),
            )
            .await?,
    );

    // The run reappears: its occurrence is already durable, so this generation
    // stores no event at all.
    let build_return = |seed: u128| -> Result<
        (RepoWatchCursorCandidate, Vec<RepoWatchEventOccurrenceV1>),
        Box<dyn Error>,
    > {
        let mut frontier = absent_candidate.event_identity_frontier().clone();
        let returned = workflow_run_observation(WorkflowRunPresence::Listed)?;
        let events = derive_repo_watch_events(
            &repository,
            Some(absent_candidate.observation()),
            &returned,
            &mut frontier,
            &mut FixedEventIds(seed),
        )?;
        Ok((
            RepoWatchCursorCandidate::with_event_identity_frontier(returned, frontier),
            events,
        ))
    };

    let (returned_candidate, first_attempt) = build_return(500)?;
    let fourth = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    Some(third),
                    returned_candidate.clone(),
                    first_attempt.clone(),
                ),
            )
            .await?,
    );
    let stored_at_fourth: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_watch_event WHERE repository = $1 AND cursor_generation = $2",
    )
    .bind(REPOSITORY)
    .bind(i64::try_from(fourth.get())?)
    .fetch_one(&pool)
    .await?;

    // A stale retry carrying the same candidate and content identity but a
    // freshly minted event UUID.
    let (retry_candidate, retry_events) = build_return(900)?;
    let outcome = store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(Some(third), retry_candidate, retry_events.clone()),
        )
        .await?;

    // The coalescing generation wrote no event at all.
    assert_eq!(stored_at_fourth, 0);
    // The retry restates the same occurrence under a freshly minted candidate.
    assert_eq!(
        first_attempt[0].content_identity(),
        retry_events[0].content_identity()
    );
    assert_ne!(first_attempt[0].event().id(), retry_events[0].event().id());
    assert_eq!(replayed_generation(outcome), fourth);
    // The fact still names exactly one row, recorded under the identity of the
    // occurrence that first carried it.
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_watch_event
          WHERE repository = $1 AND content_identity = $2",
    )
    .bind(REPOSITORY)
    .bind(first_attempt[0].content_identity().as_bytes().as_slice())
    .fetch_one(&pool)
    .await?;
    assert_eq!(rows, 1);
    Ok(())
}
