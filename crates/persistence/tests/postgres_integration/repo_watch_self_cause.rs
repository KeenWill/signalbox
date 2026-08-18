//! Exact-object suppression for repository-watch writes caused by session tools.

use std::{error::Error, num::NonZeroU64, time::Duration};

use signalbox_application::{
    RepoWatchDispatchService, RepoWatchObservation, RepoWatchPullRequestLifecycle,
    RepoWatchPullRequestState, RepoWatchPullRequestStateInput, RepoWatchRepositoryState,
    RepoWatchRepositoryStateInput, RepoWatchResolvedTemplate, RepoWatchReviewObservation,
    RepoWatchRuleEvaluationOutcome, RepoWatchTemplateResolver, RepoWatchThreadObservation,
    RepoWatchThreadState, UuidV7RepoWatchDispatchIdGenerator, UuidV7RepoWatchEventIdGenerator,
    derive_repo_watch_events,
};
use signalbox_domain::{
    BranchName, CommitSha, DangerousToolAutoApproval, DirectModelSelection, GitHubObjectId,
    MergeableState, ModelSelectionRequest, PullRequestBody, PullRequestEventContext,
    PullRequestEventContextInput, PullRequestNumber, PullRequestTitle, RepoWatchAuthorLogin,
    RepoWatchEventKindNameV1, RepoWatchMatcherV1, RepoWatchMatcherV1Input, RepoWatchRule,
    RepoWatchRuleActionV1, RepoWatchRuleId, RepoWatchSingletonScope, RepositorySlug, ReviewState,
    ReviewThreadId, SessionConfigurationDefaults, SessionSystemPrompt,
    SessionTemplateContentDigest, SessionTemplateName, SessionTemplateProvenance, ToolAttemptId,
    ToolAttemptObservation, ToolEffectClass, ToolResultContent, ToolResultText, UserContent,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential,
    repo_watch::{
        PostgresRepoWatchStore, RepoWatchCommitOutcome, RepoWatchCommitRequest,
        RepoWatchCursorCandidate, RepoWatchCursorGeneration, RepoWatchObservationBoundary,
        RepoWatchThreadObservationBoundary,
    },
    repo_watch_dispatch::PostgresRepoWatchDispatchStore,
    tool_loop::PostgresToolLoopRepository,
};

use crate::*;

const REPOSITORY: &str = "signalbox/repository";
const HEAD_REPOSITORY: &str = "contributor/repository";
const HEAD: &str = "1111111111111111111111111111111111111111";
const THREAD: &str = "PRRT_fixture_thread";
const USER: &str = "fixture-user";
const SELF_REVIEW_ID: u64 = 80_021;
const USER_REVIEW_ID: u64 = 80_022;

struct TemplateResolver;

impl RepoWatchTemplateResolver for TemplateResolver {
    fn resolve_repo_watch_template(
        &self,
        name: &SessionTemplateName,
    ) -> Option<RepoWatchResolvedTemplate> {
        Some(RepoWatchResolvedTemplate::new(
            SessionTemplateProvenance::new(
                name.clone(),
                SessionTemplateContentDigest::from_bytes([9; 32]),
            ),
            SessionConfigurationDefaults::complete(
                ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(Uuid::from_u128(
                    90_001,
                ))),
                DangerousToolAutoApproval::Disabled,
                Some(
                    SessionSystemPrompt::try_new(String::from("Respond to the review."))
                        .expect("fixture prompt is valid"),
                ),
            ),
        ))
    }
}

fn repository() -> Result<RepositorySlug, Box<dyn Error>> {
    Ok(RepositorySlug::try_new(String::from(REPOSITORY))?)
}

fn context() -> Result<PullRequestEventContext, Box<dyn Error>> {
    Ok(PullRequestEventContext::new(PullRequestEventContextInput {
        number: PullRequestNumber::new(NonZeroU64::new(41).expect("fixture number is positive")),
        head_sha: CommitSha::try_new(String::from(HEAD))?,
        head_repository: RepositorySlug::try_new(String::from(HEAD_REPOSITORY))?,
        base_branch: BranchName::try_new(String::from("main"))?,
        head_branch: BranchName::try_new(String::from("feature/review-response"))?,
        title: PullRequestTitle::try_new(String::from("Review response"))?,
        body: PullRequestBody::try_new(String::from("A synthetic pull request."))?,
        labels: Vec::new(),
        draft: false,
        author: Some(RepoWatchAuthorLogin::try_new(String::from(USER))?),
    }))
}

fn observation_with_threads(
    review_ids: &[u64],
    threads: Vec<RepoWatchThreadObservation>,
) -> Result<RepoWatchObservation, Box<dyn Error>> {
    let reviews = review_ids
        .iter()
        .map(|id| {
            Ok(RepoWatchReviewObservation::new(
                GitHubObjectId::new(NonZeroU64::new(*id).expect("fixture id is positive")),
                RepoWatchAuthorLogin::try_new(String::from(USER))?,
                Some(ReviewState::Commented),
                CommitSha::try_new(String::from(HEAD))?,
            ))
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    Ok(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![RepoWatchPullRequestState::try_new(
                RepoWatchPullRequestStateInput {
                    context: context()?,
                    lifecycle: RepoWatchPullRequestLifecycle::Open,
                    mergeable_state: MergeableState::Mergeable,
                    completed_check_suites: Vec::new(),
                    completed_check_runs: Vec::new(),
                    reviews,
                    threads,
                    reactions: Vec::new(),
                },
            )?],
            workflow_runs: Vec::new(),
            branch_heads: Vec::new(),
        })?,
    ))
}

fn observation(review_ids: &[u64]) -> Result<RepoWatchObservation, Box<dyn Error>> {
    observation_with_threads(
        review_ids,
        vec![RepoWatchThreadObservation::new(
            ReviewThreadId::try_new(String::from(THREAD))?,
            RepoWatchThreadState::Open,
            Some(GitHubObjectId::new(
                NonZeroU64::new(SELF_REVIEW_ID).expect("fixture id is positive"),
            )),
        )],
    )
}

fn observation_without_threads() -> Result<RepoWatchObservation, Box<dyn Error>> {
    observation_with_threads(&[], Vec::new())
}

fn rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(String::from("review-response"))?,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![
                RepoWatchEventKindNameV1::ReviewSubmitted,
                RepoWatchEventKindNameV1::ThreadOpened,
            ],
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(String::from("review-response"))?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?)
}

fn credential_pin() -> SessionCredentialPin {
    SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        "fixture-family",
        "fixture-credential",
    )])
    .expect("fixture credential pin is valid")
}

fn dispatch_context() -> UserContent {
    UserContent::try_text(String::from("synthetic dispatch context"))
        .expect("fixture dispatch context is valid")
}

async fn complete_thread_reply(pool: &PgPool, seed: u128) -> Result<(), Box<dyn Error>> {
    let arguments = format!(r#"{{"body":"synthetic reply","thread_id":"{THREAD}"}}"#);
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(pool, seed, "change_request_thread_reply", &arguments)
            .await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x21)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x22)),
        )
        .await?;
    let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x23));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?;
    let authorized = repository
        .authorize_attempt(fixture.session, fixture.turn, attempt)
        .await?;
    repository
        .commit_observation(
            authorized
                .executor_fence()
                .bind(ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(format!(
                            r#"{{"comment_id":70011,"comment_node_id":"PRRC_fixture_reply","review_id":{SELF_REVIEW_ID},"url":"https://github.example/comment/70011"}}"#
                        ))
                        .expect("fixture tool result is bounded"),
                    ),
                }),
        )
        .await?;
    Ok(())
}

async fn complete_published_review_for_repository(
    pool: &PgPool,
    seed: u128,
    repository_argument: &str,
    comments: &str,
) -> Result<(), Box<dyn Error>> {
    let arguments = format!(
        r#"{{"commit_id":"{HEAD}","comments":{comments},"event":"comment","number":41,"repository":"{repository_argument}"}}"#
    );
    let (fixture, _, _, request) = checkpoint_confirmed_tool_round(
        pool,
        seed,
        "github_pull_request_publish_review",
        &arguments,
    )
    .await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x21)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x22)),
        )
        .await?;
    let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x23));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?;
    let authorized = repository
        .authorize_attempt(fixture.session, fixture.turn, attempt)
        .await?;
    repository
        .commit_observation(
            authorized
                .executor_fence()
                .bind(ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(format!(
                            r#"{{"commit_id":"{HEAD}","id":{SELF_REVIEW_ID},"state":"COMMENTED","url":"https://github.example/review/{SELF_REVIEW_ID}"}}"#
                        ))
                        .expect("fixture tool result is bounded"),
                    ),
                }),
        )
        .await?;
    Ok(())
}

async fn complete_published_review(pool: &PgPool, seed: u128) -> Result<(), Box<dyn Error>> {
    complete_published_review_for_repository(
        pool,
        seed,
        REPOSITORY,
        r#"[{"body":"synthetic inline comment","line":1,"path":"src/lib.rs","side":"RIGHT"}]"#,
    )
    .await
}

async fn ambiguous_published_review(
    pool: &PgPool,
    seed: u128,
    attempt: ToolAttemptId,
    comments: &str,
) -> Result<(), Box<dyn Error>> {
    let arguments = format!(
        r#"{{"commit_id":"{HEAD}","comments":{comments},"event":"comment","number":41,"repository":"{REPOSITORY}"}}"#
    );
    let (fixture, _, _, request) = checkpoint_confirmed_tool_round(
        pool,
        seed,
        "github_pull_request_publish_review",
        &arguments,
    )
    .await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x21)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x22)),
        )
        .await?;
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?;
    let authorized = repository
        .authorize_attempt(fixture.session, fixture.turn, attempt)
        .await?;
    repository
        .commit_observation(
            authorized
                .executor_fence()
                .bind(ToolAttemptObservation::Ambiguous),
        )
        .await?;
    Ok(())
}

async fn complete_thread_resolve(pool: &PgPool, seed: u128) -> Result<(), Box<dyn Error>> {
    let arguments = format!(r#"{{"thread_id":"{THREAD}"}}"#);
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(pool, seed, "change_request_thread_resolve", &arguments)
            .await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x21)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x22)),
        )
        .await?;
    let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x23));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?;
    let authorized = repository
        .authorize_attempt(fixture.session, fixture.turn, attempt)
        .await?;
    repository
        .commit_observation(
            authorized
                .executor_fence()
                .bind(ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(format!(
                            r#"{{"resolved":true,"thread_id":"{THREAD}"}}"#
                        ))
                        .expect("fixture tool result is bounded"),
                    ),
                }),
        )
        .await?;
    Ok(())
}

fn thread_observation(state: RepoWatchThreadState) -> Result<RepoWatchObservation, Box<dyn Error>> {
    observation_with_threads(
        &[],
        vec![RepoWatchThreadObservation::new(
            ReviewThreadId::try_new(String::from(THREAD))?,
            state,
            None,
        )],
    )
}

fn thread_resolved_rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(String::from("thread-resolution"))?,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::ThreadResolved],
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(String::from("review-response"))?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?)
}

fn historical_cursor(reviews: serde_json::Value, threads: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "storage_version": 1,
        "signal_reviewers": [],
        "state": {
            "pull_requests": [{
                "number": 41,
                "head_sha": HEAD,
                "head_repository": HEAD_REPOSITORY,
                "base_branch": "main",
                "head_branch": "feature/review-response",
                "title": "Review response",
                "body": "A synthetic pull request.",
                "labels": [],
                "draft": false,
                "author": USER,
                "lifecycle": "open",
                "mergeable_state": "mergeable",
                "completed_check_suites": [],
                "completed_check_runs": [],
                "reviews": reviews,
                "threads": threads,
                "reactions": []
            }],
            "workflow_runs": [],
            "branch_heads": []
        }
    })
}

async fn postgres_before_self_cause_migration()
-> Result<(ContainerAsync<Postgres>, PgPool, String), Box<dyn Error>> {
    let (container, pool, database_url) = unmigrated_postgres().await?;
    let mut connection = pool.acquire().await?;
    connection
        .ensure_migrations_table("_sqlx_migrations")
        .await?;
    for migration in MIGRATOR
        .iter()
        .take_while(|migration| migration.version < 202608110016)
    {
        connection.apply("_sqlx_migrations", migration).await?;
    }
    drop(connection);
    Ok((container, pool, database_url))
}

fn committed_generation(outcome: RepoWatchCommitOutcome) -> RepoWatchCursorGeneration {
    let RepoWatchCommitOutcome::Committed(cursor) = outcome else {
        panic!("fixture cursor commit is new")
    };
    cursor.generation()
}

async fn commit_initial_observation(
    event_store: &PostgresRepoWatchStore,
    repository: &RepositorySlug,
    observation: &RepoWatchObservation,
) -> Result<RepoWatchCursorGeneration, Box<dyn Error>> {
    let boundary = event_store.capture_observation_boundary().await?;
    let events = derive_repo_watch_events(
        repository,
        None,
        observation,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    Ok(committed_generation(
        event_store
            .commit(
                repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(observation.clone()),
                    events,
                )
                .with_thread_observation_boundaries(thread_observation_boundaries(boundary)?),
            )
            .await?,
    ))
}

fn thread_observation_boundaries(
    boundary: RepoWatchObservationBoundary,
) -> Result<Vec<RepoWatchThreadObservationBoundary>, Box<dyn Error>> {
    Ok(vec![RepoWatchThreadObservationBoundary::new(
        ReviewThreadId::try_new(String::from(THREAD))?,
        boundary,
    )])
}

fn unchanged_generation(outcome: RepoWatchCommitOutcome) -> RepoWatchCursorGeneration {
    let RepoWatchCommitOutcome::Unchanged(cursor) = outcome else {
        panic!("fixture cursor commit is unchanged")
    };
    cursor.generation()
}

#[track_caller]
fn assert_dispatched(outcome: RepoWatchRuleEvaluationOutcome) {
    let RepoWatchRuleEvaluationOutcome::Dispatched { .. } = outcome else {
        panic!("fixture user event dispatches")
    };
}

/// Existing review events acquire their exact cursor identity, while cursor
/// threads acquire the explicit unknown-origin field required by the new shape.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn self_cause_migrations_upgrade_existing_watch_rows() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = postgres_before_self_cause_migration().await?;
    let before = historical_cursor(serde_json::json!([]), serde_json::json!([]));
    let after = historical_cursor(
        serde_json::json!([{
            "id": SELF_REVIEW_ID,
            "reviewer": USER,
            "state": "commented",
            "commit": HEAD
        }]),
        serde_json::json!([{
            "thread": THREAD,
            "state": "open"
        }]),
    );
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO repo_watch_cursor (
             repository, generation, storage_version, cursor_payload,
             recording_transaction_id
         ) VALUES ($1, 1, 1, $2, pg_current_xact_id()),
                  ($1, 2, 1, $3, pg_current_xact_id())",
    )
    .bind(REPOSITORY)
    .bind(before)
    .bind(after)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO repo_watch_event (
             event_id, repository, cursor_generation, event_ordinal,
             event_version, target_kind, event_kind, pull_request_number,
             head_sha, head_repository, base_branch, head_branch, title, body,
             labels, draft, author, review_reviewer, review_state, review_commit
         ) VALUES (
             $1, $2, 2, 1, 1, 'pull_request', 'review_submitted', 41,
             $3, $4, 'main', 'feature/review-response', 'Review response',
             'A synthetic pull request.', ARRAY[]::text[], false, $5, $5,
             'commented', $3
         )",
    )
    .bind(Uuid::from_u128(0x90_200))
    .bind(REPOSITORY)
    .bind(HEAD)
    .bind(HEAD_REPOSITORY)
    .bind(USER)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    migrate(&pool).await?;

    let stored_review_id: String = sqlx::query_scalar(
        "SELECT review_id::text
           FROM repo_watch_event
          WHERE event_id = $1",
    )
    .bind(Uuid::from_u128(0x90_200))
    .fetch_one(&pool)
    .await?;
    let migrated_origin_is_null: bool = sqlx::query_scalar(
        "SELECT cursor_payload #> '{state,pull_requests,0,threads,0,originating_review_id}'
                = 'null'::jsonb
           FROM repo_watch_cursor
          WHERE repository = $1 AND generation = 2",
    )
    .bind(REPOSITORY)
    .fetch_one(&pool)
    .await?;

    assert_eq!(stored_review_id, SELF_REVIEW_ID.to_string());
    assert!(migrated_origin_is_null);
    Ok(())
}

/// A published review's inline thread is suppressed by its exact immutable
/// parent-review identity even when GraphQL reports the thread one poll later.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn published_review_inline_thread_is_self_caused() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    complete_published_review(&pool, 0x90_000).await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = rule()?;
    let before = observation_without_threads()?;
    let first_generation = commit_initial_observation(&event_store, &repository, &before).await?;
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let review_only = observation_with_threads(&[SELF_REVIEW_ID], Vec::new())?;
    let review_events = derive_repo_watch_events(
        &repository,
        Some(&before),
        &review_only,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    let review_generation = committed_generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    Some(first_generation),
                    RepoWatchCursorCandidate::new(review_only.clone()),
                    review_events,
                ),
            )
            .await?,
    );
    let review_event = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the published review event is pending");
    let review_outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                review_event,
                &rule,
                &review_only,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    assert_eq!(review_outcome, RepoWatchRuleEvaluationOutcome::SelfCaused);
    let published = observation(&[SELF_REVIEW_ID])?;
    let thread_events = derive_repo_watch_events(
        &repository,
        Some(&review_only),
        &published,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(review_generation),
                RepoWatchCursorCandidate::new(published.clone()),
                thread_events,
            ),
        )
        .await?;
    let thread_event = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the published inline-thread event is pending");
    let thread_outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store)
            .evaluate(
                thread_event,
                &rule,
                &published,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    assert_eq!(thread_outcome, RepoWatchRuleEvaluationOutcome::SelfCaused);
    let observation_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_github_write_observation",
    )
    .fetch_one(&pool)
    .await?;
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_action")
        .fetch_one(&pool)
        .await?;
    assert_eq!(observation_count, 1);
    assert_eq!(sessions, 0);
    Ok(())
}

/// Repository arguments accepted with mixed case are projected into the
/// canonical watch key instead of aborting completed tool persistence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn completed_review_receipt_canonicalizes_repository() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    complete_published_review_for_repository(&pool, 0x90_050, "SignalBox/Repository", "[]").await?;

    let stored_repository: String = sqlx::query_scalar(
        "SELECT repository
           FROM repo_watch_github_write_receipt
          WHERE operation_kind = 'publish_review'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored_repository, REPOSITORY);
    Ok(())
}

/// A commentless review attempt cannot cause a thread, so even an ambiguous
/// attempt does not park a later user-created thread event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn commentless_ambiguous_review_does_not_block_user_thread() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    ambiguous_published_review(
        &pool,
        0x90_060,
        ToolAttemptId::from_uuid(Uuid::from_u128(0x90_083)),
        "[]",
    )
    .await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = rule()?;
    let before = observation_without_threads()?;
    let opening_events = derive_repo_watch_events(
        &repository,
        None,
        &before,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    let first_generation = committed_generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(before.clone()),
                    opening_events,
                ),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let user_thread = thread_observation(RepoWatchThreadState::Open)?;
    let events = derive_repo_watch_events(
        &repository,
        Some(&before),
        &user_thread,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(user_thread.clone()),
                events,
            ),
        )
        .await?;
    let event = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the user thread event is pending");
    let outcome = RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store)
        .evaluate(
            event,
            &rule,
            &user_thread,
            &TemplateResolver,
            dispatch_context(),
        )
        .await?;

    assert_dispatched(outcome);
    Ok(())
}

/// A mutation attempt created after an older provider event cannot be its
/// cause, even when the later attempt remains commit-ambiguous.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn later_ambiguous_review_does_not_block_older_user_thread() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = rule()?;
    let before = observation_without_threads()?;
    let opening_events = derive_repo_watch_events(
        &repository,
        None,
        &before,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    let first_generation = committed_generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(before.clone()),
                    opening_events,
                ),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let user_thread = thread_observation(RepoWatchThreadState::Open)?;
    let events = derive_repo_watch_events(
        &repository,
        Some(&before),
        &user_thread,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(user_thread.clone()),
                events,
            ),
        )
        .await?;
    ambiguous_published_review(
        &pool,
        0x90_070,
        ToolAttemptId::from_uuid(Uuid::from_u128(u128::MAX - 1)),
        r#"[{"body":"synthetic inline comment","line":1,"path":"src/lib.rs","side":"RIGHT"}]"#,
    )
    .await?;
    let event = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the older user thread event is pending");
    let outcome = RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store)
        .evaluate(
            event,
            &rule,
            &user_thread,
            &TemplateResolver,
            dispatch_context(),
        )
        .await?;

    assert_dispatched(outcome);
    Ok(())
}

/// A resolve receipt is consumed by the first exact-thread observation, so a
/// later user resolution remains dispatchable after an intervening reopen.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reopened_thread_does_not_reuse_stale_resolve_receipt() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = thread_resolved_rule()?;
    let reopened = thread_observation(RepoWatchThreadState::Open)?;
    let first_generation = commit_initial_observation(&event_store, &repository, &reopened).await?;
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    complete_thread_resolve(&pool, 0x90_100).await?;
    let post_mutation_poll = event_store.capture_observation_boundary().await?;
    let unchanged = event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(reopened.clone()),
                Vec::new(),
            )
            .with_thread_observation_boundaries(thread_observation_boundaries(post_mutation_poll)?),
        )
        .await?;
    assert_eq!(unchanged_generation(unchanged), first_generation);
    let user_resolved = thread_observation(RepoWatchThreadState::Resolved)?;
    let events = derive_repo_watch_events(
        &repository,
        Some(&reopened),
        &user_resolved,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(user_resolved.clone()),
                events,
            ),
        )
        .await?;
    let event = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the user resolution event is pending");
    let outcome = RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store)
        .evaluate(
            event,
            &rule,
            &user_resolved,
            &TemplateResolver,
            dispatch_context(),
        )
        .await?;

    assert_dispatched(outcome);
    Ok(())
}

/// An unchanged poll that began before a resolve receipt cannot consume it;
/// the following post-mutation observation still suppresses its resolution.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pre_mutation_unchanged_poll_does_not_consume_resolve_receipt() -> Result<(), Box<dyn Error>>
{
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = thread_resolved_rule()?;
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let open = thread_observation(RepoWatchThreadState::Open)?;
    let first_generation = commit_initial_observation(&event_store, &repository, &open).await?;

    let pre_mutation_poll = event_store.capture_observation_boundary().await?;
    complete_thread_resolve(&pool, 0x90_150).await?;
    let unchanged = event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(open.clone()),
                Vec::new(),
            )
            .with_thread_observation_boundaries(thread_observation_boundaries(pre_mutation_poll)?),
        )
        .await?;
    assert_eq!(unchanged_generation(unchanged), first_generation);

    let post_mutation_poll = event_store.capture_observation_boundary().await?;
    let resolved = thread_observation(RepoWatchThreadState::Resolved)?;
    let events = derive_repo_watch_events(
        &repository,
        Some(&open),
        &resolved,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(resolved.clone()),
                events,
            )
            .with_thread_observation_boundaries(thread_observation_boundaries(post_mutation_poll)?),
        )
        .await?;
    let event = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the session-created resolution event is pending");
    let outcome = RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store)
        .evaluate(
            event,
            &rule,
            &resolved,
            &TemplateResolver,
            dispatch_context(),
        )
        .await?;

    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::SelfCaused);
    Ok(())
}

/// A resolve receipt completed after the poll began but before its provider
/// snapshot was observed is linked to the resolution contained in that snapshot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn resolve_completed_during_poll_is_linked_to_observed_snapshot() -> Result<(), Box<dyn Error>>
{
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = thread_resolved_rule()?;
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let open = thread_observation(RepoWatchThreadState::Open)?;
    let first_generation = commit_initial_observation(&event_store, &repository, &open).await?;

    let _poll_started_at = event_store.capture_observation_boundary().await?;
    complete_thread_resolve(&pool, 0x90_170).await?;
    let resolved = thread_observation(RepoWatchThreadState::Resolved)?;
    let snapshot_observed_at = event_store.capture_observation_boundary().await?;
    let events = derive_repo_watch_events(
        &repository,
        Some(&open),
        &resolved,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(resolved.clone()),
                events,
            )
            .with_thread_observation_boundaries(thread_observation_boundaries(
                snapshot_observed_at,
            )?),
        )
        .await?;
    let event = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the session-created resolution event is pending");
    let outcome = RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store)
        .evaluate(
            event,
            &rule,
            &resolved,
            &TemplateResolver,
            dispatch_context(),
        )
        .await?;

    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::SelfCaused);
    Ok(())
}

/// A resolved state fetched before a session resolve completed cannot consume
/// that receipt or suppress the user's earlier transition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pre_mutation_user_resolution_does_not_consume_later_resolve_receipt()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = thread_resolved_rule()?;
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let open = thread_observation(RepoWatchThreadState::Open)?;
    let first_generation = commit_initial_observation(&event_store, &repository, &open).await?;

    let user_poll = event_store.capture_observation_boundary().await?;
    let resolved = thread_observation(RepoWatchThreadState::Resolved)?;
    complete_thread_resolve(&pool, 0x90_180).await?;
    let events = derive_repo_watch_events(
        &repository,
        Some(&open),
        &resolved,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(resolved.clone()),
                events,
            )
            .with_thread_observation_boundaries(thread_observation_boundaries(user_poll)?),
        )
        .await?;
    let event = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the user resolution event is pending");
    let outcome = RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store)
        .evaluate(
            event,
            &rule,
            &resolved,
            &TemplateResolver,
            dispatch_context(),
        )
        .await?;

    assert_dispatched(outcome);
    Ok(())
}

/// A user resolution committed from a provider snapshot taken while a
/// redundant session resolve was in flight remains dispatchable when that
/// resolve receipt arrives later.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn user_resolution_snapshot_before_late_resolve_receipt_dispatches()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = thread_resolved_rule()?;
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let open = thread_observation(RepoWatchThreadState::Open)?;
    let first_generation = commit_initial_observation(&event_store, &repository, &open).await?;

    let arguments = format!(r#"{{"thread_id":"{THREAD}"}}"#);
    let (fixture, _, _, request) = checkpoint_confirmed_tool_round(
        &pool,
        0x90_190,
        "change_request_thread_resolve",
        &arguments,
    )
    .await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(0x90_1b1)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(0x90_1b2)),
        )
        .await?;
    let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(0x90_1b3));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?;
    let authorized = tool_repository
        .authorize_attempt(fixture.session, fixture.turn, attempt)
        .await?;

    let resolved = thread_observation(RepoWatchThreadState::Resolved)?;
    let snapshot_observed_at = event_store.capture_observation_boundary().await?;
    let events = derive_repo_watch_events(
        &repository,
        Some(&open),
        &resolved,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(resolved.clone()),
                events,
            )
            .with_thread_observation_boundaries(thread_observation_boundaries(
                snapshot_observed_at,
            )?),
        )
        .await?;

    tool_repository
        .commit_observation(
            authorized
                .executor_fence()
                .bind(ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(format!(
                            r#"{{"resolved":true,"thread_id":"{THREAD}"}}"#
                        ))
                        .expect("fixture tool result is bounded"),
                    ),
                }),
        )
        .await?;

    let event = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the user resolution event is pending");
    let durable_cause: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM repo_watch_event_self_cause
              WHERE event_id = $1
         )",
    )
    .bind(event.id().as_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(!durable_cause);
    let outcome = RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store)
        .evaluate(
            event,
            &rule,
            &resolved,
            &TemplateResolver,
            dispatch_context(),
        )
        .await?;

    assert_dispatched(outcome);
    Ok(())
}

/// Dispatch waits while an exact mutation is in flight, then reconciles the
/// completed receipt to the already-stored provider events.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn provider_event_before_tool_completion_is_reconciled_before_dispatch()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = rule()?;
    let before = observation_without_threads()?;
    let first_generation = commit_initial_observation(&event_store, &repository, &before).await?;
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;

    let arguments = format!(r#"{{"body":"synthetic reply","thread_id":"{THREAD}"}}"#);
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, 0x90_500, "change_request_thread_reply", &arguments)
            .await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(0x90_521)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(0x90_522)),
        )
        .await?;
    let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(0x90_523));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?;
    let authorized = tool_repository
        .authorize_attempt(fixture.session, fixture.turn, attempt)
        .await?;

    let self_caused = observation(&[SELF_REVIEW_ID])?;
    let events = derive_repo_watch_events(
        &repository,
        Some(&before),
        &self_caused,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(self_caused.clone()),
                events,
            ),
        )
        .await?;
    let event = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the matching review event is pending");
    let pending =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                event.clone(),
                &rule,
                &self_caused,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    assert_eq!(pending, RepoWatchRuleEvaluationOutcome::PendingSelfCause);

    tool_repository
        .commit_observation(
            authorized
                .executor_fence()
                .bind(ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(format!(
                            r#"{{"comment_id":70011,"comment_node_id":"PRRC_fixture_reply","review_id":{SELF_REVIEW_ID},"url":"https://github.example/comment/70011"}}"#
                        ))
                        .expect("fixture tool result is bounded"),
                    ),
                }),
        )
        .await?;
    let durable_cause: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM repo_watch_event_self_cause
              WHERE event_id = $1
         )",
    )
    .bind(event.id().as_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(durable_cause);
    let reconciled =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                event,
                &rule,
                &self_caused,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    assert_eq!(reconciled, RepoWatchRuleEvaluationOutcome::SelfCaused);
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_action")
        .fetch_one(&pool)
        .await?;
    assert_eq!(sessions, 0);
    Ok(())
}

/// A session-created review is suppressed by exact provider identity, while a
/// distinct user-created review with the same login still dispatches.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn exact_session_write_is_self_caused_but_same_author_user_write_dispatches()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    complete_thread_reply(&pool, 0x91_000).await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = rule()?;
    let before = observation_without_threads()?;
    let first_generation = commit_initial_observation(&event_store, &repository, &before).await?;
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let self_caused = observation(&[SELF_REVIEW_ID])?;
    let self_events = derive_repo_watch_events(
        &repository,
        Some(&before),
        &self_caused,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(self_caused.clone()),
                self_events,
            ),
        )
        .await?;
    let self_event = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the matching self-caused review is pending");
    let self_outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                self_event,
                &rule,
                &self_caused,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    assert_eq!(self_outcome, RepoWatchRuleEvaluationOutcome::SelfCaused);
    let reply_thread_event = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the matching reply-opened thread is pending");
    let reply_thread_outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                reply_thread_event,
                &rule,
                &self_caused,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    assert_eq!(
        reply_thread_outcome,
        RepoWatchRuleEvaluationOutcome::SelfCaused
    );
    let self_sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_action")
        .fetch_one(&pool)
        .await?;
    assert_eq!(self_sessions, 0);

    let cursor = event_store
        .load_cursor(&repository)
        .await?
        .expect("the self-caused cursor exists");
    let user_authored = observation(&[SELF_REVIEW_ID, USER_REVIEW_ID])?;
    let user_events = derive_repo_watch_events(
        &repository,
        Some(&self_caused),
        &user_authored,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(cursor.generation()),
                RepoWatchCursorCandidate::new(user_authored.clone()),
                user_events,
            ),
        )
        .await?;
    let user_event = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the user-authored review is pending");
    let user_outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store)
            .evaluate(
                user_event,
                &rule,
                &user_authored,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    assert_dispatched(user_outcome);
    let user_sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_action")
        .fetch_one(&pool)
        .await?;
    assert_eq!(user_sessions, 1);
    Ok(())
}
