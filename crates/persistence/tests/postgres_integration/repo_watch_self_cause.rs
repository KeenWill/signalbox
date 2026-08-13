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
        RepoWatchCursorCandidate, RepoWatchCursorGeneration,
    },
    repo_watch_dispatch::PostgresRepoWatchDispatchStore,
    tool_loop::PostgresToolLoopRepository,
};

use crate::*;

const REPOSITORY: &str = "signalbox/repository";
const HEAD_REPOSITORY: &str = "contributor/repository";
const HEAD: &str = "1111111111111111111111111111111111111111";
const THREAD: &str = "PRRT_fixture_thread";
const OWNER: &str = "fixture-owner";
const SELF_REVIEW_ID: u64 = 80_021;
const OWNER_REVIEW_ID: u64 = 80_022;

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
        author: Some(RepoWatchAuthorLogin::try_new(String::from(OWNER))?),
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
                RepoWatchAuthorLogin::try_new(String::from(OWNER))?,
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

fn committed_generation(outcome: RepoWatchCommitOutcome) -> RepoWatchCursorGeneration {
    let RepoWatchCommitOutcome::Committed(cursor) = outcome else {
        panic!("fixture cursor commit is new")
    };
    cursor.generation()
}

#[track_caller]
fn assert_dispatched(outcome: RepoWatchRuleEvaluationOutcome) {
    let RepoWatchRuleEvaluationOutcome::Dispatched { .. } = outcome else {
        panic!("fixture owner event dispatches")
    };
}

/// A session-created review is suppressed by exact provider identity, while a
/// distinct owner-created review with the same login still dispatches.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn exact_session_write_is_self_caused_but_same_author_owner_write_dispatches()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _database_url) = migrated_postgres().await?;
    complete_thread_reply(&pool, 0x91_000).await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = rule()?;
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;

    let before = observation_without_threads()?;
    let first_generation = committed_generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(before.clone()),
                    Vec::new(),
                ),
            )
            .await?,
    );
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
    let owner_authored = observation(&[SELF_REVIEW_ID, OWNER_REVIEW_ID])?;
    let owner_events = derive_repo_watch_events(
        &repository,
        Some(&self_caused),
        &owner_authored,
        &mut UuidV7RepoWatchEventIdGenerator,
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(cursor.generation()),
                RepoWatchCursorCandidate::new(owner_authored.clone()),
                owner_events,
            ),
        )
        .await?;
    let owner_event = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the owner-authored review is pending");
    let owner_outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store)
            .evaluate(
                owner_event,
                &rule,
                &owner_authored,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    assert_dispatched(owner_outcome);
    let owner_sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_action")
        .fetch_one(&pool)
        .await?;
    assert_eq!(owner_sessions, 1);
    Ok(())
}
