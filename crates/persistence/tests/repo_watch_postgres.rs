#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{error::Error, num::NonZeroU16};

use signalbox_application::{
    RepoWatchEventIdGenerator, RepoWatchObservation, RepoWatchPullRequestLifecycle,
    RepoWatchPullRequestState, RepoWatchPullRequestStateInput, RepoWatchRepositoryState,
    RepoWatchRepositoryStateInput, derive_repo_watch_events,
};
use signalbox_domain::{
    BranchName, CommitSha, MergeableState, PullRequestBody, PullRequestEventContext,
    PullRequestEventContextInput, PullRequestNumber, PullRequestTitle, RepoWatchAuthorLogin,
    RepoWatchEventId, RepositorySlug,
};
use signalbox_persistence::{
    local_test_connection_options, migrate,
    repo_watch::{
        PostgresRepoWatchStore, RepoWatchCommitOutcome, RepoWatchCommitRequest,
        RepoWatchCursorCandidate, RepoWatchCursorGeneration, RepoWatchEventPageSize,
        RepoWatchPersistenceCorruption, RepoWatchStoreError,
    },
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
const TITLE: &str = "Persist repository-watch facts";
const BODY: &str = "A complete fixture pull request body.";
const AUTHOR: &str = "fixture-author";
const LABEL: &str = "watch-me";
const PULL_REQUEST: u64 = 41;

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
            branch_heads: Vec::new(),
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
            completed_check_suites: Vec::new(),
            completed_check_runs: Vec::new(),
            reviews: Vec::new(),
            threads: Vec::new(),
            reactions: Vec::new(),
        },
    )?)
}

fn candidate(head: Option<&str>) -> Result<RepoWatchCursorCandidate, Box<dyn Error>> {
    Ok(RepoWatchCursorCandidate::new(observation(head)?))
}

fn committed_generation(outcome: RepoWatchCommitOutcome) -> RepoWatchCursorGeneration {
    match outcome {
        RepoWatchCommitOutcome::Committed(cursor) => cursor.generation(),
        _ => panic!("fixture commit must be newly committed"),
    }
}

struct CommittedFixture {
    _container: ContainerAsync<Postgres>,
    repository: RepositorySlug,
    store: PostgresRepoWatchStore,
    second_candidate: RepoWatchCursorCandidate,
    events: Vec<signalbox_domain::RepoWatchEvent>,
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
    let second_candidate = candidate(Some(INITIAL_HEAD))?;
    let events = derive_repo_watch_events(
        &repository,
        Some(first_candidate.observation()),
        second_candidate.observation(),
        &mut FixedEventIds::default(),
    )?;
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
async fn exact_retry_finds_its_replay_after_a_later_generation_commits()
-> Result<(), Box<dyn Error>> {
    let fixture = committed_fixture().await?;
    let changed = candidate(Some(CHANGED_HEAD))?;
    let changed_events = derive_repo_watch_events(
        &fixture.repository,
        Some(fixture.second_candidate.observation()),
        changed.observation(),
        &mut FixedEventIds(100),
    )?;
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
    let current = candidate(Some(INITIAL_HEAD))?;
    let mut ids = FixedEventIds::default();
    let events = derive_repo_watch_events(
        &repository,
        Some(baseline.observation()),
        current.observation(),
        &mut ids,
    )?;
    let second_generation = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(Some(first_generation), current.clone(), events),
            )
            .await?,
    );
    let changed = candidate(Some(CHANGED_HEAD))?;
    let collision = derive_repo_watch_events(
        &repository,
        Some(current.observation()),
        changed.observation(),
        &mut FixedEventIds::default(),
    )?;
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
        "UPDATE repo_watch_cursor SET cursor_payload = '{\"storage_version\":1}'::jsonb WHERE repository = $1",
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
async fn pull_request_target_constraint_rejects_a_null_number() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal, event_version,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft
         ) VALUES (
            $1, $2, $3, 1, 1, 'pull_request', 'pull_request_opened', NULL,
            $4, $5, $6, $7, $8, $9, ARRAY[]::text[], false
         )",
    )
    .bind(Uuid::now_v7())
    .bind(repository.as_str())
    .bind(RepoWatchCursorGeneration::INITIAL.get() as i64)
    .bind(INITIAL_HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(BASE_BRANCH)
    .bind(HEAD_BRANCH)
    .bind(TITLE)
    .bind(BODY)
    .execute(&pool)
    .await;

    assert!(insert.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn comment_reaction_constraint_rejects_a_null_subject_id() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal, event_version,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft,
            reaction_subject_kind, reaction_subject_id, reaction_reactor,
            reaction_content, reaction_change
         ) VALUES (
            $1, $2, $3, 1, 1, 'pull_request', 'reaction_changed', $4,
            $5, $6, $7, $8, $9, $10, ARRAY[]::text[], false,
            'issue_comment', NULL, $11, '+1', 'added'
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
    .bind(AUTHOR)
    .execute(&pool)
    .await;

    assert!(insert.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn label_array_constraint_rejects_a_null_member() -> Result<(), Box<dyn Error>> {
    let (_container, pool, repository) = migrated_cursor_fixture().await?;
    let insert = sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal, event_version,
            target_kind, event_kind, pull_request_number, head_sha, head_repository,
            base_branch, head_branch, title, body, labels, draft, label_name
         ) VALUES (
            $1, $2, $3, 1, 1, 'pull_request', 'labeled', $4,
            $5, $6, $7, $8, $9, $10, ARRAY[$11, NULL]::text[], false, $11
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
    .bind(LABEL)
    .execute(&pool)
    .await;

    assert!(insert.is_err());
    Ok(())
}
