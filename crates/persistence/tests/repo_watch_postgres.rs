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
        RepoWatchCursorCandidate, RepoWatchCursorGeneration, RepoWatchEntityTag,
        RepoWatchEventPageSize, RepoWatchPersistenceCorruption, RepoWatchResourceKey,
        RepoWatchResourceValidator, RepoWatchStoreError,
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
const RESOURCE: &str = "pulls/page/1";
const INITIAL_ETAG: &str = "\"etag-one\"";
const CHANGED_ETAG: &str = "\"etag-two\"";
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

fn candidate(etag: &str, head: Option<&str>) -> Result<RepoWatchCursorCandidate, Box<dyn Error>> {
    Ok(RepoWatchCursorCandidate::try_new(
        vec![RepoWatchResourceValidator::new(
            RepoWatchResourceKey::try_new(RESOURCE.to_owned())?,
            RepoWatchEntityTag::try_new(etag.to_owned())?,
        )],
        observation(head)?,
    )?)
}

fn committed_generation(outcome: RepoWatchCommitOutcome) -> RepoWatchCursorGeneration {
    match outcome {
        RepoWatchCommitOutcome::Committed(cursor) => cursor.generation(),
        _ => panic!("fixture commit must be newly committed"),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cursor_event_commit_replay_conflict_and_keyset_page_are_durable()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool);
    let first_candidate = candidate(INITIAL_ETAG, None)?;
    let first_generation = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, first_candidate.clone(), Vec::new()),
            )
            .await?,
    );
    let second_candidate = candidate(CHANGED_ETAG, Some(INITIAL_HEAD))?;
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
    let first_page = store
        .load_event_page(
            &repository,
            None,
            RepoWatchEventPageSize::try_new(NonZeroU16::MIN)?,
        )
        .await?;
    let second_page = store
        .load_event_page(
            &repository,
            first_page.next_after(),
            RepoWatchEventPageSize::try_new(NonZeroU16::MIN)?,
        )
        .await?;
    let replay = store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(Some(first_generation), second_candidate.clone(), events),
        )
        .await?;
    let conflict = store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(None, second_candidate.clone(), Vec::new()),
        )
        .await?;
    let unchanged = store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(Some(second_generation), second_candidate, Vec::new()),
        )
        .await?;

    assert_eq!(first_generation, RepoWatchCursorGeneration::INITIAL);
    assert_eq!(second_generation.get(), 2);
    assert_eq!(first_page.events().len(), 1);
    assert!(first_page.next_after().is_some());
    assert_eq!(second_page.events().len(), 1);
    assert!(second_page.next_after().is_none());
    assert!(matches!(replay, RepoWatchCommitOutcome::Replayed(_)));
    assert!(matches!(conflict, RepoWatchCommitOutcome::Conflict { .. }));
    assert!(matches!(unchanged, RepoWatchCommitOutcome::Unchanged(_)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn event_identity_failure_rolls_back_cursor_and_events() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    let baseline = candidate(INITIAL_ETAG, None)?;
    let first_generation = committed_generation(
        store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, baseline.clone(), Vec::new()),
            )
            .await?,
    );
    let current = candidate(CHANGED_ETAG, Some(INITIAL_HEAD))?;
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
    let changed = candidate(CHANGED_ETAG, Some(CHANGED_HEAD))?;
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

    assert!(matches!(failure, Err(RepoWatchStoreError::Database(_))));
    assert_eq!(cursor.generation(), second_generation);
    assert_eq!(cursor_rows, 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn append_only_guards_and_malformed_cursor_reads_fail_closed() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let store = PostgresRepoWatchStore::new(pool.clone());
    store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(None, candidate(INITIAL_ETAG, None)?, Vec::new()),
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
    let corrupt = store.load_cursor(&repository).await;

    assert!(update.is_err());
    assert!(delete.is_err());
    assert!(truncate.is_err());
    assert!(matches!(
        corrupt,
        Err(RepoWatchStoreError::Corruption(
            RepoWatchPersistenceCorruption::MalformedCursorDocument
        ))
    ));
    Ok(())
}
