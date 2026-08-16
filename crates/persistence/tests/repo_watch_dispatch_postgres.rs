#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{error::Error, time::Duration};

use signalbox_application::{
    RepoWatchBranchHead, RepoWatchDispatchService, RepoWatchDispatchTransaction,
    RepoWatchObservation, RepoWatchPullRequestLifecycle, RepoWatchPullRequestState,
    RepoWatchPullRequestStateInput, RepoWatchRepositoryState, RepoWatchRepositoryStateInput,
    RepoWatchResolvedTemplate, RepoWatchRuleEvaluation, RepoWatchRuleEvaluationOutcome,
    RepoWatchTemplateResolver, UuidV7RepoWatchDispatchIdGenerator,
};
use signalbox_domain::{
    BranchName, CommitSha, DangerousToolAutoApproval, DescendantTerminationScope,
    DirectModelSelection, DurableCommandId, GoalCommandResult, GoalNeed, GoalSchedulerProvenance,
    GoalState, GoalStatement, GoalUserAction, GoalUserCommand, MergeableState,
    ModelSelectionRequest, PullRequestBody, PullRequestEventContext, PullRequestEventContextInput,
    PullRequestNumber, PullRequestTitle, RepoWatchActionV1, RepoWatchAuthorLogin, RepoWatchEvent,
    RepoWatchEventId, RepoWatchEventKindNameV1, RepoWatchEventKindV1, RepoWatchEventTarget,
    RepoWatchMatcherV1, RepoWatchMatcherV1Input, RepoWatchPattern, RepoWatchRule,
    RepoWatchRuleActionV1, RepoWatchRuleId, RepoWatchSingletonScope, RepositorySlug,
    SessionConfigurationDefaults, SessionId, SessionSystemPrompt, SessionTemplateContentDigest,
    SessionTemplateName, SessionTemplateProvenance, TurnId, UserContent,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential, disposable_test_container_labels,
    goal::{GoalCommandHandlingOutcome, GoalRepository, GoalTransitionOutcome},
    local_test_connection_options, migrate,
    repo_watch::{
        PostgresRepoWatchStore, RepoWatchCommitOutcome, RepoWatchCommitRequest,
        RepoWatchCursorCandidate, RepoWatchCursorGeneration,
    },
    repo_watch_dispatch::{PostgresRepoWatchDispatchStore, RepoWatchDispatchRepositoryError},
    repo_watch_dispatch_obligation::RepoWatchDispatchObligation,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_repo_watch_dispatch";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const REPOSITORY: &str = "signalbox/repository";
const HEAD_REPOSITORY: &str = "contributor/repository";
const BASE_BRANCH: &str = "main";
const HEAD_BRANCH: &str = "feature/repo-watch";
const INITIAL_HEAD: &str = "0000000000000000000000000000000000000000";
const FIRST_HEAD: &str = "1111111111111111111111111111111111111111";
const SECOND_HEAD: &str = "2222222222222222222222222222222222222222";
const THIRD_HEAD: &str = "3333333333333333333333333333333333333333";
const TEMPLATE: &str = "merge-forward";
const RULE: &str = "merge-forward-on-conflict";
const EAGER_RULE: &str = "merge-forward-on-base-advance";
const AGENT_HEAD_PATTERN: &str = "^agent/.+$";
const BOTTOM_AGENT_BRANCH: &str = "agent/bottom";
const TOP_AGENT_BRANCH: &str = "agent/top";
const BOTTOM_PULL_REQUEST_NUMBER: u64 = 41;
const TOP_PULL_REQUEST_NUMBER: u64 = 42;
const MAIN_OPENED_EVENT_ID: u128 = 0x60_000;
const MAIN_BASE_ADVANCED_EVENT_ID: u128 = 0x60_001;
const STACK_BOTTOM_OPENED_EVENT_ID: u128 = 0x60_100;
const STACK_TOP_OPENED_EVENT_ID: u128 = 0x60_101;
const STACK_PARENT_HEAD_CHANGED_EVENT_ID: u128 = 0x60_102;
const STACK_BASE_ADVANCED_EVENT_ID: u128 = 0x60_103;
const DISPATCH_CONTEXT: &str = r#"{"fixture":"repository-watch"}"#;
const FIRST_TERMINAL_IDENTITY_SEED: u128 = 0x10_000;
const CORRUPT_GOAL_GENERATION: i64 = 2;
const MERGED_GOAL_CUTOFF_EVENT_ID: u128 = 0x51_000;
const CORRUPT_GOAL_FIRST_CUTOFF_EVENT_ID: u128 = 0x51_200;
const CORRUPT_GOAL_SECOND_CUTOFF_EVENT_ID: u128 = 0x51_300;
const TERMINAL_RULE_OPENED_EVENT_ID: u128 = 0x54_000;
const TERMINAL_RULE_MERGED_EVENT_ID: u128 = 0x54_100;
const STARTUP_DRAIN_CUTOFF_EVENT_ID: u128 = 0x57_100;
const STARTUP_DRAIN_STOP_COMMAND_ID: u128 = 0x57_110;

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

fn repository() -> Result<RepositorySlug, Box<dyn Error>> {
    Ok(RepositorySlug::try_new(REPOSITORY.to_owned())?)
}

fn context(head: &str) -> Result<PullRequestEventContext, Box<dyn Error>> {
    Ok(PullRequestEventContext::new(PullRequestEventContextInput {
        number: PullRequestNumber::new(BOTTOM_PULL_REQUEST_NUMBER.try_into()?),
        head_sha: CommitSha::try_new(head.to_owned())?,
        head_repository: RepositorySlug::try_new(HEAD_REPOSITORY.to_owned())?,
        base_branch: BranchName::try_new(BASE_BRANCH.to_owned())?,
        head_branch: BranchName::try_new(HEAD_BRANCH.to_owned())?,
        title: PullRequestTitle::try_new("Merge forward".to_owned())?,
        body: PullRequestBody::try_new("Resolve the conflict.".to_owned())?,
        labels: Vec::new(),
        draft: false,
        author: Some(RepoWatchAuthorLogin::try_new("fixture-author".to_owned())?),
    }))
}

/// Named facts for one same-repository pull request in the eager-rule tests.
struct SameRepositoryContextFacts<'a> {
    number: u64,
    head: &'a str,
    base_branch: &'a str,
    head_branch: &'a str,
}

fn same_repository_context(
    facts: SameRepositoryContextFacts<'_>,
) -> Result<PullRequestEventContext, Box<dyn Error>> {
    Ok(PullRequestEventContext::new(PullRequestEventContextInput {
        number: PullRequestNumber::new(facts.number.try_into()?),
        head_sha: CommitSha::try_new(facts.head.to_owned())?,
        head_repository: repository()?,
        base_branch: BranchName::try_new(facts.base_branch.to_owned())?,
        head_branch: BranchName::try_new(facts.head_branch.to_owned())?,
        title: PullRequestTitle::try_new("Merge forward".to_owned())?,
        body: PullRequestBody::try_new("Advance the dependent.".to_owned())?,
        labels: Vec::new(),
        draft: false,
        author: Some(RepoWatchAuthorLogin::try_new("fixture-author".to_owned())?),
    }))
}

fn observation(context: PullRequestEventContext) -> Result<RepoWatchObservation, Box<dyn Error>> {
    lifecycle_observation(context, RepoWatchPullRequestLifecycle::Open)
}

fn lifecycle_observation(
    context: PullRequestEventContext,
    lifecycle: RepoWatchPullRequestLifecycle,
) -> Result<RepoWatchObservation, Box<dyn Error>> {
    Ok(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![RepoWatchPullRequestState::try_new(
                RepoWatchPullRequestStateInput {
                    context,
                    lifecycle,
                    mergeable_state: MergeableState::Conflicting,
                    completed_check_suites: Vec::new(),
                    completed_check_runs: Vec::new(),
                    reviews: Vec::new(),
                    threads: Vec::new(),
                    reactions: Vec::new(),
                },
            )?],
            workflow_runs: Vec::new(),
            branch_heads: Vec::new(),
        })?,
    ))
}

fn mergeable_observation(
    contexts: Vec<PullRequestEventContext>,
    branch_heads: Vec<RepoWatchBranchHead>,
) -> Result<RepoWatchObservation, Box<dyn Error>> {
    let pull_requests = contexts
        .into_iter()
        .map(|context| {
            RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
                context,
                lifecycle: RepoWatchPullRequestLifecycle::Open,
                mergeable_state: MergeableState::Mergeable,
                completed_check_suites: Vec::new(),
                completed_check_runs: Vec::new(),
                reviews: Vec::new(),
                threads: Vec::new(),
                reactions: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests,
            workflow_runs: Vec::new(),
            branch_heads,
        })?,
    ))
}

fn opened_event(value: u128, head: &str) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context(head)?,
        RepoWatchEventKindV1::PullRequestOpened,
    )?)
}

fn merged_event(value: u128, head: &str) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context(head)?,
        RepoWatchEventKindV1::PullRequestMerged,
    )?)
}

fn conflict_event(value: u128, head: &str) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context(head)?,
        RepoWatchEventKindV1::MergeableStateChanged {
            current: MergeableState::Conflicting,
        },
    )?)
}

fn opened_event_for(
    value: u128,
    context: PullRequestEventContext,
) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context,
        RepoWatchEventKindV1::PullRequestOpened,
    )?)
}

fn head_changed_event(
    value: u128,
    context: PullRequestEventContext,
    previous: &str,
) -> Result<RepoWatchEvent, Box<dyn Error>> {
    let current = context.head_sha().clone();
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context,
        RepoWatchEventKindV1::HeadChanged {
            previous: CommitSha::try_new(previous.to_owned())?,
            current,
        },
    )?)
}

fn base_advanced_event(
    value: u128,
    context: PullRequestEventContext,
) -> Result<RepoWatchEvent, Box<dyn Error>> {
    let branch = context.base_branch().clone();
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context,
        RepoWatchEventKindV1::BaseAdvanced { branch },
    )?)
}

fn rule_with_actions_and_cooldown(
    actions: Vec<RepoWatchRuleActionV1>,
    cooldown: Duration,
) -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(RULE.to_owned())?,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::MergeableStateChanged],
            mergeable_state: vec![MergeableState::Conflicting],
            ..RepoWatchMatcherV1Input::default()
        }),
        actions,
        RepoWatchSingletonScope::PullRequest,
        cooldown,
    )?)
}

fn rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    let template = SessionTemplateName::try_new(TEMPLATE.to_owned())?;
    rule_with_actions_and_cooldown(
        vec![
            RepoWatchRuleActionV1::DispatchSession {
                template: template.clone(),
            },
            RepoWatchRuleActionV1::DispatchSession { template },
        ],
        Duration::ZERO,
    )
}

fn cooldown_rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    rule_with_actions_and_cooldown(
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(TEMPLATE.to_owned())?,
        }],
        Duration::from_secs(60 * 60),
    )
}

fn one_action_rule(cooldown: Duration) -> Result<RepoWatchRule, Box<dyn Error>> {
    rule_with_actions_and_cooldown(
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(TEMPLATE.to_owned())?,
        }],
        cooldown,
    )
}

fn eager_merge_forward_rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(EAGER_RULE.to_owned())?,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::BaseAdvanced],
            repository: Some(repository()?),
            head_branch: Some(RepoWatchPattern::try_new(AGENT_HEAD_PATTERN.to_owned())?),
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(TEMPLATE.to_owned())?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?)
}

fn merged_event_rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(RULE.to_owned())?,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::PullRequestMerged],
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(TEMPLATE.to_owned())?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?)
}

fn conflict_and_merged_event_rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(RULE.to_owned())?,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![
                RepoWatchEventKindNameV1::MergeableStateChanged,
                RepoWatchEventKindNameV1::PullRequestMerged,
            ],
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(TEMPLATE.to_owned())?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?)
}

struct TemplateResolver;

impl RepoWatchTemplateResolver for TemplateResolver {
    fn resolve_repo_watch_template(
        &self,
        name: &SessionTemplateName,
    ) -> Option<RepoWatchResolvedTemplate> {
        Some(RepoWatchResolvedTemplate::new(
            SessionTemplateProvenance::new(
                name.clone(),
                SessionTemplateContentDigest::from_bytes([7; 32]),
            ),
            SessionConfigurationDefaults::complete(
                ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(Uuid::from_u128(
                    901,
                ))),
                DangerousToolAutoApproval::Disabled,
                Some(
                    SessionSystemPrompt::try_new("Merge the base branch forward.".to_owned())
                        .expect("fixture prompt is valid"),
                ),
            ),
        ))
    }
}

struct ObligationTransaction {
    store: PostgresRepoWatchDispatchStore,
    obligation: Option<RepoWatchDispatchObligation>,
}

struct EvaluatedConflict {
    outcome: RepoWatchRuleEvaluationOutcome,
    event_id: RepoWatchEventId,
}

#[derive(Debug, sqlx::FromRow)]
struct OutstandingCooldownVisibility {
    matched_event_count: i64,
    eligible_at_is_future: bool,
    ready: bool,
}

#[derive(Debug, sqlx::FromRow)]
struct HeldSlotVisibility {
    every_action_delivered: bool,
    every_delivery_turn_releasable: bool,
    no_live_runtime_turn: bool,
    every_goal_nonpursuing: bool,
    blockers: Vec<String>,
}

impl RepoWatchDispatchTransaction for ObligationTransaction {
    type Error = RepoWatchDispatchRepositoryError;

    async fn handle_repo_watch_evaluation(
        &mut self,
        evaluation: RepoWatchRuleEvaluation,
    ) -> Result<RepoWatchRuleEvaluationOutcome, Self::Error> {
        let obligation =
            self.obligation
                .take()
                .ok_or(RepoWatchDispatchRepositoryError::Corruption(
                    "test obligation transaction was reused",
                ))?;
        self.store
            .handle_repo_watch_obligation_with_alias_resolver(obligation, evaluation, |_| None)
            .await
    }
}

fn credential_pin() -> SessionCredentialPin {
    SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        "fixture-family",
        "fixture-credential",
    )])
    .expect("fixture credential pin is valid")
}

fn dispatch_context() -> UserContent {
    UserContent::try_text(DISPATCH_CONTEXT.to_owned()).expect("fixture dispatch context is valid")
}

fn generation(outcome: RepoWatchCommitOutcome) -> RepoWatchCursorGeneration {
    match outcome {
        RepoWatchCommitOutcome::Committed(cursor) => cursor.generation(),
        _ => panic!("fixture cursor commit must be new"),
    }
}

fn dispatched(
    outcome: RepoWatchRuleEvaluationOutcome,
) -> (signalbox_domain::RepoWatchDispatchId, Box<[SessionId]>) {
    match outcome {
        RepoWatchRuleEvaluationOutcome::Dispatched {
            dispatch_id,
            sessions,
        } => (dispatch_id, sessions),
        _ => panic!("fixture rule evaluation must dispatch"),
    }
}

fn replayed(
    outcome: RepoWatchRuleEvaluationOutcome,
) -> (signalbox_domain::RepoWatchDispatchId, Box<[SessionId]>) {
    match outcome {
        RepoWatchRuleEvaluationOutcome::Replayed {
            dispatch_id,
            sessions,
        } => (dispatch_id, sessions),
        _ => panic!("fixture obligation must replay its dispatch"),
    }
}

fn pull_request_number(event: &RepoWatchEvent) -> PullRequestNumber {
    let RepoWatchEventTarget::PullRequest(context) = event.target() else {
        panic!("fixture event must target a pull request");
    };
    context.number()
}

fn session_uuids(fixture: &DispatchFixture) -> Vec<Uuid> {
    fixture
        .sessions
        .iter()
        .map(|session| *session.as_uuid())
        .collect()
}

fn reused_rule_identity(error: &RepoWatchDispatchRepositoryError) -> bool {
    matches!(
        error,
        RepoWatchDispatchRepositoryError::ReusedRuleIdentity { .. }
    )
}

fn changed_rule_identity(error: &RepoWatchDispatchRepositoryError) -> bool {
    matches!(
        error,
        RepoWatchDispatchRepositoryError::ChangedRuleIdentity { .. }
    )
}

fn outcome_is_dispatched(outcome: &RepoWatchRuleEvaluationOutcome) -> bool {
    matches!(outcome, RepoWatchRuleEvaluationOutcome::Dispatched { .. })
}

#[track_caller]
fn assert_applied_goal_transition(outcome: GoalTransitionOutcome) {
    let GoalTransitionOutcome::Applied(_) = outcome else {
        panic!("fixture goal transition must apply");
    };
}

#[track_caller]
fn assert_applied_goal_command(outcome: GoalCommandHandlingOutcome) {
    let GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(_)) = outcome else {
        panic!("fixture goal command must apply");
    };
}

/// Withdraws the goal commissioned for one dispatched session.
///
/// A dispatched session's pursuing goal holds the batch's singleton on its own,
/// so a test whose subject is the release mechanism has to end that pursuit
/// before the turn it is exercising can release anything.
///
/// This stops the goal rather than failing its turn. A stopped queued or active
/// turn is runtime-irrelevant and therefore no longer owns the singleton.
async fn withdraw_dispatched_goal(
    pool: &PgPool,
    session: SessionId,
    identity_seed: u128,
) -> Result<(), Box<dyn Error>> {
    assert_applied_goal_command(
        GoalRepository::new(pool.clone())
            .handle_user_command(
                GoalUserCommand::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(identity_seed)),
                    session,
                    GoalUserAction::Stop {
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                    },
                ),
                None,
                |_| None,
            )
            .await?,
    );
    Ok(())
}

/// Terminalizes a dispatched session's only queued turn as a failure.
///
/// A dispatched session holds exactly one queued turn, so the turn this fails
/// is always first in its session and has no predecessor to name.
async fn mark_queued_turn_failed(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    identity_seed: u128,
) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal', start_lineage_kind = 'first_in_session',
                immediate_predecessor_turn_id = NULL, starting_frontier_id = $3,
                terminal_frontier_id = $4, terminal_disposition_kind = 'failed'
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.as_uuid())
    .bind(turn.as_uuid())
    .bind(Uuid::from_u128(identity_seed))
    .bind(Uuid::from_u128(identity_seed + 1))
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

async fn check_completed_turn_for_release(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
) -> Result<(), Box<dyn Error>> {
    sqlx::query("SELECT repo_watch_release_completed_dispatch_batches_for_turn($1, $2)")
        .bind(turn.as_uuid())
        .bind(session.as_uuid())
        .execute(pool)
        .await?;
    Ok(())
}

async fn release_count(fixture: &DispatchFixture) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_release WHERE dispatch_id = $1")
        .bind(fixture.dispatch_id.as_uuid())
        .fetch_one(&fixture.pool)
        .await
}

async fn wait_for_backend_lock(pool: &PgPool, backend: i32) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                    SELECT 1
                      FROM pg_stat_activity
                     WHERE pid = $1
                       AND wait_event_type = 'Lock'
                )",
            )
            .bind(backend)
            .fetch_one(pool)
            .await?;
            if waiting {
                return Ok::<(), sqlx::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}

async fn wait_for_advisory_lock(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                    SELECT 1
                      FROM pg_locks
                     WHERE locktype = 'advisory'
                       AND NOT granted
                )",
            )
            .fetch_one(pool)
            .await?;
            if waiting {
                return Ok::<(), sqlx::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}

struct DispatchFixture {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
    repository: RepositorySlug,
    event: RepoWatchEvent,
    observation: RepoWatchObservation,
    rule: RepoWatchRule,
    store: PostgresRepoWatchDispatchStore,
    dispatch_id: signalbox_domain::RepoWatchDispatchId,
    sessions: Box<[SessionId]>,
}

impl DispatchFixture {
    /// The session this dispatch created for the action at the given ordinal.
    #[track_caller]
    fn session(&self, action_ordinal: usize) -> SessionId {
        self.sessions[action_ordinal]
    }
}

async fn dispatch_fixture() -> Result<DispatchFixture, Box<dyn Error>> {
    dispatch_fixture_for(rule()?).await
}

async fn dispatch_fixture_for(rule: RepoWatchRule) -> Result<DispatchFixture, Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let initial_observation = observation(context(INITIAL_HEAD)?)?;
    let initial = RepoWatchCursorCandidate::new(initial_observation);
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, initial, vec![opened_event(100, INITIAL_HEAD)?]),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let event = conflict_event(101, FIRST_HEAD)?;
    let observation = observation(context(FIRST_HEAD)?)?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(observation.clone()),
                vec![event.clone()],
            ),
        )
        .await?;
    let loaded = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("activated fixture rule sees its first event");
    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded,
                &rule,
                &observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    let (dispatch_id, sessions) = dispatched(outcome);
    Ok(DispatchFixture {
        _container: container,
        pool,
        repository,
        event,
        observation,
        rule,
        store: dispatch_store,
        dispatch_id,
        sessions,
    })
}

async fn evaluate_second_conflict(
    fixture: &DispatchFixture,
) -> Result<RepoWatchRuleEvaluationOutcome, Box<dyn Error>> {
    Ok(evaluate_conflict(fixture, 102, SECOND_HEAD).await?.outcome)
}

async fn evaluate_conflict(
    fixture: &DispatchFixture,
    event_id: u128,
    head: &str,
) -> Result<EvaluatedConflict, Box<dyn Error>> {
    let (loaded, observation) = load_conflict(fixture, event_id, head).await?;
    let event_id = loaded.id();
    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                loaded,
                &fixture.rule,
                &observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    Ok(EvaluatedConflict { outcome, event_id })
}

async fn load_second_conflict(
    fixture: &DispatchFixture,
) -> Result<(RepoWatchEvent, RepoWatchObservation), Box<dyn Error>> {
    load_conflict(fixture, 102, SECOND_HEAD).await
}

async fn load_conflict(
    fixture: &DispatchFixture,
    event_id: u128,
    head: &str,
) -> Result<(RepoWatchEvent, RepoWatchObservation), Box<dyn Error>> {
    let event_store = PostgresRepoWatchStore::new(fixture.pool.clone());
    let cursor = event_store
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let event = conflict_event(event_id, head)?;
    let observation = observation(context(head)?)?;
    event_store
        .commit(
            &fixture.repository,
            RepoWatchCommitRequest::new(
                Some(cursor.generation()),
                RepoWatchCursorCandidate::new(observation.clone()),
                vec![event],
            ),
        )
        .await?;
    let loaded = fixture
        .store
        .load_next_event(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?
        .expect("second conflict remains unevaluated");
    Ok((loaded, observation))
}

async fn commit_lifecycle(
    fixture: &DispatchFixture,
    observation: RepoWatchObservation,
    event: RepoWatchEvent,
) -> Result<(), Box<dyn Error>> {
    let event_store = PostgresRepoWatchStore::new(fixture.pool.clone());
    let cursor = event_store
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    event_store
        .commit(
            &fixture.repository,
            RepoWatchCommitRequest::new(
                Some(cursor.generation()),
                RepoWatchCursorCandidate::new(observation),
                vec![event],
            ),
        )
        .await?;
    Ok(())
}

async fn commit_merge(fixture: &DispatchFixture, event_id: u128) -> Result<(), Box<dyn Error>> {
    commit_lifecycle(
        fixture,
        lifecycle_observation(context(SECOND_HEAD)?, RepoWatchPullRequestLifecycle::Merged)?,
        merged_event(event_id, SECOND_HEAD)?,
    )
    .await
}

async fn commit_reopen(fixture: &DispatchFixture, event_id: u128) -> Result<(), Box<dyn Error>> {
    commit_lifecycle(
        fixture,
        lifecycle_observation(context(THIRD_HEAD)?, RepoWatchPullRequestLifecycle::Open)?,
        opened_event(event_id, THIRD_HEAD)?,
    )
    .await
}

async fn commit_second_merge(
    fixture: &DispatchFixture,
    event_id: u128,
) -> Result<(), Box<dyn Error>> {
    commit_lifecycle(
        fixture,
        lifecycle_observation(context(THIRD_HEAD)?, RepoWatchPullRequestLifecycle::Merged)?,
        merged_event(event_id, THIRD_HEAD)?,
    )
    .await
}

async fn corrupt_goal_generation(pool: &PgPool, session: SessionId) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE goal_event DISABLE TRIGGER goal_event_is_append_only")
        .execute(pool)
        .await?;
    sqlx::query("UPDATE goal_event SET generation = $2 WHERE session_id = $1")
        .bind(session.as_uuid())
        .bind(CORRUPT_GOAL_GENERATION)
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE goal_event ENABLE TRIGGER goal_event_is_append_only")
        .execute(pool)
        .await?;
    Ok(())
}

async fn evaluate_obligation(
    fixture: &DispatchFixture,
    obligation: RepoWatchDispatchObligation,
    observation: &RepoWatchObservation,
) -> Result<RepoWatchRuleEvaluationOutcome, Box<dyn Error>> {
    let event = obligation.latest_event().clone();
    Ok(RepoWatchDispatchService::new(
        UuidV7RepoWatchDispatchIdGenerator,
        ObligationTransaction {
            store: fixture.store.clone(),
            obligation: Some(obligation),
        },
    )
    .evaluate(
        event,
        &fixture.rule,
        observation,
        &TemplateResolver,
        dispatch_context(),
    )
    .await?)
}

/// A `main` update dispatches its mergeable dependent directly from the
/// `BaseAdvanced` fact, without any check-completion or conflict event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn eager_main_advance_dispatches_its_mergeable_dependent_immediately()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = eager_merge_forward_rule()?;
    let dependent = same_repository_context(SameRepositoryContextFacts {
        number: BOTTOM_PULL_REQUEST_NUMBER,
        head: INITIAL_HEAD,
        base_branch: BASE_BRANCH,
        head_branch: BOTTOM_AGENT_BRANCH,
    })?;
    let initial_observation = mergeable_observation(
        vec![dependent.clone()],
        vec![RepoWatchBranchHead::new(
            dependent.base_branch().clone(),
            CommitSha::try_new(INITIAL_HEAD.to_owned())?,
        )],
    )?;
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(initial_observation),
                    vec![opened_event_for(MAIN_OPENED_EVENT_ID, dependent.clone())?],
                ),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let current_observation = mergeable_observation(
        vec![dependent.clone()],
        vec![RepoWatchBranchHead::new(
            dependent.base_branch().clone(),
            CommitSha::try_new(FIRST_HEAD.to_owned())?,
        )],
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(current_observation.clone()),
                vec![base_advanced_event(
                    MAIN_BASE_ADVANCED_EVENT_ID,
                    dependent.clone(),
                )?],
            ),
        )
        .await?;

    let loaded_dependent = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the main-based pull request remains unevaluated");
    assert_eq!(pull_request_number(&loaded_dependent), dependent.number());
    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded_dependent,
                &rule,
                &current_observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    let batch_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&pool)
        .await?;

    assert!(outcome_is_dispatched(&outcome));
    assert_eq!(batch_count, 1);
    Ok(())
}

/// A stacked parent update dispatches its mergeable child directly. The
/// parent's own `HeadChanged` fact remains nonmatching, so only the dependent
/// receives the merge-forward session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn eager_parent_advance_dispatches_only_its_mergeable_child_immediately()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let rule = eager_merge_forward_rule()?;
    let initial_parent = same_repository_context(SameRepositoryContextFacts {
        number: BOTTOM_PULL_REQUEST_NUMBER,
        head: INITIAL_HEAD,
        base_branch: BASE_BRANCH,
        head_branch: BOTTOM_AGENT_BRANCH,
    })?;
    let child = same_repository_context(SameRepositoryContextFacts {
        number: TOP_PULL_REQUEST_NUMBER,
        head: SECOND_HEAD,
        base_branch: BOTTOM_AGENT_BRANCH,
        head_branch: TOP_AGENT_BRANCH,
    })?;
    let initial_observation = mergeable_observation(
        vec![initial_parent.clone(), child.clone()],
        vec![RepoWatchBranchHead::new(
            initial_parent.head_branch().clone(),
            initial_parent.head_sha().clone(),
        )],
    )?;
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(initial_observation),
                    vec![
                        opened_event_for(STACK_BOTTOM_OPENED_EVENT_ID, initial_parent.clone())?,
                        opened_event_for(STACK_TOP_OPENED_EVENT_ID, child.clone())?,
                    ],
                ),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let advanced_parent = same_repository_context(SameRepositoryContextFacts {
        number: BOTTOM_PULL_REQUEST_NUMBER,
        head: FIRST_HEAD,
        base_branch: BASE_BRANCH,
        head_branch: BOTTOM_AGENT_BRANCH,
    })?;
    let current_observation = mergeable_observation(
        vec![advanced_parent.clone(), child.clone()],
        vec![RepoWatchBranchHead::new(
            advanced_parent.head_branch().clone(),
            advanced_parent.head_sha().clone(),
        )],
    )?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(current_observation.clone()),
                vec![
                    head_changed_event(
                        STACK_PARENT_HEAD_CHANGED_EVENT_ID,
                        advanced_parent.clone(),
                        INITIAL_HEAD,
                    )?,
                    base_advanced_event(STACK_BASE_ADVANCED_EVENT_ID, child.clone())?,
                ],
            ),
        )
        .await?;

    let loaded_parent = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the activated eager rule sees the parent head change");
    assert_eq!(
        pull_request_number(&loaded_parent),
        advanced_parent.number()
    );
    let parent_outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded_parent,
                &rule,
                &current_observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    let loaded_child = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the stacked child remains unevaluated");
    assert_eq!(pull_request_number(&loaded_child), child.number());
    let child_outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded_child,
                &rule,
                &current_observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    let batch_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&pool)
        .await?;

    assert_eq!(parent_outcome, RepoWatchRuleEvaluationOutcome::NotMatched);
    assert!(outcome_is_dispatched(&child_outcome));
    assert_eq!(batch_count, 1);
    Ok(())
}

/// A user stop retires a queued dispatch turn without changing its physical
/// lifecycle state, so release follows runtime relevance rather than waiting
/// for a terminal state transition that will never happen.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stopped_runtime_irrelevant_turn_releases_its_singleton() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;

    withdraw_dispatched_goal(&fixture.pool, fixture.session(0), 0x50_100).await?;

    assert_eq!(release_count(&fixture).await?, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn held_slot_projection_names_each_failed_release_clause() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;

    let held: HeldSlotVisibility = sqlx::query_as(
        "SELECT every_action_delivered, every_delivery_turn_releasable,
                no_live_runtime_turn, every_goal_nonpursuing, blockers
           FROM repo_watch_held_dispatch_slot
          WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert!(held.every_action_delivered);
    assert!(!held.every_delivery_turn_releasable);
    assert!(!held.no_live_runtime_turn);
    assert!(!held.every_goal_nonpursuing);
    assert_eq!(
        held.blockers,
        vec![
            String::from("delivery_turn_runtime_relevant"),
            String::from("live_runtime_turn"),
            String::from("pursuing_goal"),
        ]
    );
    Ok(())
}

/// A terminal pull-request lifecycle withdraws only the generation-one goals
/// repository watch commissioned for that pull request.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn merged_pull_request_ends_the_commissioned_goal() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let session = fixture.session(0);
    commit_merge(&fixture, MERGED_GOAL_CUTOFF_EVENT_ID).await?;

    let processed = fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x51_100))
        })
        .await?;
    let replayed = fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x51_100))
        })
        .await?;

    let goal = GoalRepository::new(fixture.pool.clone())
        .load_goal(session)
        .await?
        .expect("the dispatched goal remains readable");
    let cutoff_goal_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_lifecycle_cutoff_goal
          WHERE session_id = $1",
    )
    .bind(session.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert!(processed);
    assert!(!replayed);
    assert_eq!(goal.current().state(), &GoalState::UserStopped);
    assert_eq!(cutoff_goal_count, 1);
    assert_eq!(release_count(&fixture).await?, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_cutoff_cleans_dispatches_from_later_same_observation_facts()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(cooldown_rule()?).await?;
    let session = fixture.session(0);
    let occupied = evaluate_conflict(&fixture, 102, SECOND_HEAD).await?;
    assert_eq!(occupied.outcome, RepoWatchRuleEvaluationOutcome::Occupied);
    commit_merge(&fixture, MERGED_GOAL_CUTOFF_EVENT_ID).await?;
    sqlx::query("ALTER TABLE repo_watch_event DISABLE TRIGGER ALL")
        .execute(&fixture.pool)
        .await?;
    sqlx::query(
        "WITH cutoff AS (
             SELECT cursor_generation
               FROM repo_watch_event
              WHERE event_id = $1
         ), ordered AS (
             SELECT event_id,
                    row_number() OVER (ORDER BY event_id) + 1 AS event_ordinal
               FROM repo_watch_event
              WHERE event_id IN (
                    SELECT event_id
                      FROM repo_watch_dispatch_action
                     WHERE session_id = $2
                    UNION ALL
                    SELECT $3
              )
         )
         UPDATE repo_watch_event AS event
            SET cursor_generation = cutoff.cursor_generation,
                event_ordinal = ordered.event_ordinal
           FROM cutoff, ordered
          WHERE event.event_id = ordered.event_id",
    )
    .bind(Uuid::from_u128(MERGED_GOAL_CUTOFF_EVENT_ID))
    .bind(session.as_uuid())
    .bind(occupied.event_id.as_uuid())
    .execute(&fixture.pool)
    .await?;
    sqlx::query("ALTER TABLE repo_watch_event ENABLE TRIGGER ALL")
        .execute(&fixture.pool)
        .await?;

    fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x51_105))
        })
        .await?;

    let goal = GoalRepository::new(fixture.pool.clone())
        .load_goal(session)
        .await?
        .expect("the stale dispatch goal remains readable");
    let settlement: String = sqlx::query_scalar(
        "SELECT settled_kind
           FROM repo_watch_dispatch_obligation
          WHERE latest_event_id = $1",
    )
    .bind(occupied.event_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(goal.current().state(), &GoalState::UserStopped);
    assert_eq!(settlement, "target_closed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn close_reopen_close_classifies_each_cutoff_against_its_following_lifecycle()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let first_merge_id = 0x51_110;
    let reopened_id = 0x51_120;
    let second_merge_id = 0x51_130;
    commit_merge(&fixture, first_merge_id).await?;
    commit_reopen(&fixture, reopened_id).await?;
    commit_second_merge(&fixture, second_merge_id).await?;

    let first_processed = fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x51_140))
        })
        .await?;
    let second_processed = fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x51_150))
        })
        .await?;
    let cutoffs: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT event_id, disposition_kind
           FROM repo_watch_lifecycle_cutoff
          ORDER BY event_id",
    )
    .fetch_all(&fixture.pool)
    .await?;

    assert!(first_processed);
    assert!(second_processed);
    assert_eq!(
        cutoffs,
        vec![
            (Uuid::from_u128(first_merge_id), String::from("reopened")),
            (Uuid::from_u128(second_merge_id), String::from("terminal")),
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn startup_drain_continues_after_corrupt_goal_cutoff() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    corrupt_goal_generation(&fixture.pool, fixture.session(0)).await?;
    commit_merge(&fixture, CORRUPT_GOAL_FIRST_CUTOFF_EVENT_ID).await?;
    commit_second_merge(&fixture, CORRUPT_GOAL_SECOND_CUTOFF_EVENT_ID).await?;

    fixture
        .store
        .process_pending_lifecycle_cutoffs(|| DurableCommandId::from_uuid(Uuid::now_v7()))
        .await?;
    let cutoff_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_lifecycle_cutoff",
    )
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(cutoff_count, 2);
    Ok(())
}

/// Dispatch admission rechecks durable lifecycle after an event was loaded, so
/// a merge committed in between prevents the stale match from firing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn matching_event_loaded_before_merge_records_target_closed() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let (loaded, stale_open_observation) = load_second_conflict(&fixture).await?;
    let batches_before: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&fixture.pool)
        .await?;
    commit_merge(&fixture, 0x52_000).await?;

    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                loaded,
                &fixture.rule,
                &stale_open_observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::TargetClosed);
    let batches_after: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&fixture.pool)
        .await?;
    assert_eq!(batches_after, batches_before);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn matching_merged_event_dispatch_survives_its_lifecycle_cutoff() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let rule = merged_event_rule()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let initial = observation(context(INITIAL_HEAD)?)?;
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(initial),
                    vec![opened_event(TERMINAL_RULE_OPENED_EVENT_ID, INITIAL_HEAD)?],
                ),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let merged =
        lifecycle_observation(context(SECOND_HEAD)?, RepoWatchPullRequestLifecycle::Merged)?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(first_generation),
                RepoWatchCursorCandidate::new(merged.clone()),
                vec![merged_event(TERMINAL_RULE_MERGED_EVENT_ID, SECOND_HEAD)?],
            ),
        )
        .await?;
    let loaded = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the terminal-event rule sees the merge event");
    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded,
                &rule,
                &merged,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    let (_, sessions) = dispatched(outcome);
    let session = sessions[0];
    let cutoff_processed = dispatch_store
        .process_next_lifecycle_cutoff(&repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_200))
        })
        .await?;
    let goal = GoalRepository::new(pool.clone())
        .load_goal(session)
        .await?
        .expect("the terminal-event dispatch goal remains readable");
    let cutoff_goal_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_lifecycle_cutoff_goal
          WHERE session_id = $1",
    )
    .bind(session.as_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(sessions.len(), rule.actions().len());
    assert!(cutoff_processed);
    assert_eq!(goal.current().state(), &GoalState::Pursuing);
    assert_eq!(cutoff_goal_count, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn merged_event_before_a_later_terminal_cutoff_records_target_closed()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let rule = merged_event_rule()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool, credential_pin());
    let initial = observation(context(INITIAL_HEAD)?)?;
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(initial),
                    vec![opened_event(0x54_300, INITIAL_HEAD)?],
                ),
            )
            .await?,
    );
    dispatch_store
        .reconcile_rules(&repository, std::slice::from_ref(&rule))
        .await?;
    let first_merged =
        lifecycle_observation(context(SECOND_HEAD)?, RepoWatchPullRequestLifecycle::Merged)?;
    let second_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    Some(first_generation),
                    RepoWatchCursorCandidate::new(first_merged),
                    vec![merged_event(0x54_310, SECOND_HEAD)?],
                ),
            )
            .await?,
    );
    let reopened =
        lifecycle_observation(context(THIRD_HEAD)?, RepoWatchPullRequestLifecycle::Open)?;
    let third_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    Some(second_generation),
                    RepoWatchCursorCandidate::new(reopened),
                    vec![opened_event(0x54_320, THIRD_HEAD)?],
                ),
            )
            .await?,
    );
    let second_merged =
        lifecycle_observation(context(THIRD_HEAD)?, RepoWatchPullRequestLifecycle::Merged)?;
    event_store
        .commit(
            &repository,
            RepoWatchCommitRequest::new(
                Some(third_generation),
                RepoWatchCursorCandidate::new(second_merged.clone()),
                vec![merged_event(0x54_330, THIRD_HEAD)?],
            ),
        )
        .await?;
    let first_cutoff = dispatch_store
        .process_next_lifecycle_cutoff(&repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_340))
        })
        .await?;
    let second_cutoff = dispatch_store
        .process_next_lifecycle_cutoff(&repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x54_350))
        })
        .await?;
    let loaded = dispatch_store
        .load_next_event(&repository, rule.id(), rule.version())
        .await?
        .expect("the older merge remains unevaluated");
    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, dispatch_store.clone())
            .evaluate(
                loaded,
                &rule,
                &second_merged,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    assert!(first_cutoff);
    assert!(second_cutoff);
    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::TargetClosed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_target_settles_owed_work_without_dispatch() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(cooldown_rule()?).await?;
    let _occupied = evaluate_second_conflict(&fixture).await?;
    commit_merge(&fixture, 0x53_000).await?;
    fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x53_100))
        })
        .await?;
    let pending_obligation = fixture
        .store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?;
    let settlement: String = sqlx::query_scalar(
        "SELECT settled_kind
           FROM repo_watch_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;

    assert!(pending_obligation.is_none());
    assert_eq!(settlement, "target_closed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_cutoff_preserves_an_obligation_for_its_own_event() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(conflict_and_merged_event_rule()?).await?;
    commit_merge(&fixture, 0x53_200).await?;
    let merged =
        lifecycle_observation(context(SECOND_HEAD)?, RepoWatchPullRequestLifecycle::Merged)?;
    let loaded = fixture
        .store
        .load_next_event(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?
        .expect("the terminal event remains unevaluated");
    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                loaded,
                &fixture.rule,
                &merged,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    let cutoff_processed = fixture
        .store
        .process_next_lifecycle_cutoff(&fixture.repository, || {
            DurableCommandId::from_uuid(Uuid::from_u128(0x53_210))
        })
        .await?;
    let obligation_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;
    let settled_kind: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT settled_kind
           FROM repo_watch_dispatch_obligation",
    )
    .fetch_optional(&fixture.pool)
    .await?
    .flatten();

    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::Occupied);
    assert!(cutoff_processed);
    assert_eq!(obligation_count, 1);
    assert_eq!(settled_kind, None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn dispatch_batch_creates_every_session_and_audit_row_atomically()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let action_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_watch_dispatch_action WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let expected_action_count = fixture.rule.actions().len();

    assert_eq!(fixture.sessions.len(), expected_action_count);
    assert_eq!(usize::try_from(action_count)?, expected_action_count);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn equal_event_rule_recovery_replays_the_original_sessions() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let replay =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                fixture.event,
                &fixture.rule,
                &fixture.observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;

    assert_eq!(
        replay,
        RepoWatchRuleEvaluationOutcome::Replayed {
            dispatch_id: fixture.dispatch_id,
            sessions: fixture.sessions,
        }
    );
    Ok(())
}

/// The goal a dispatch synthesizes is committed with the session itself.
///
/// A dispatched session declares nothing about itself, so without this it
/// reaches its first turn with no statement of the authority it was created
/// under, and every consumer that reads session authority — the approval judge
/// above all — has nothing to read.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn dispatched_sessions_are_commissioned_with_their_synthesized_goal()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let expected = synthesized_dispatch_goal(&fixture)?;

    assert_eq!(fixture.sessions.len(), fixture.rule.actions().len());
    assert_commissioned_with(&fixture, fixture.session(0), &expected).await?;
    assert_commissioned_with(&fixture, fixture.session(1), &expected).await?;
    Ok(())
}

/// The dispatched work turn carries the tagged context through submit-input,
/// and the commission that follows records that same turn as its generation's
/// goal turn. This is what lets a consumer read the authority the dispatched
/// work ran under from the turn itself.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_dispatched_work_turn_is_its_generations_goal_turn() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;

    let work_turns_bound_to_a_goal: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_dispatch_delivery AS delivery
           JOIN goal_turn
             ON goal_turn.turn_id = delivery.turn_id
            AND goal_turn.accepted_input_id = delivery.accepted_input_id
          WHERE delivery.dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let expected_action_count = fixture.rule.actions().len();

    assert_eq!(
        usize::try_from(work_turns_bound_to_a_goal)?,
        expected_action_count
    );
    Ok(())
}

/// Commissioning adopts the tagged-context turn instead of scheduling one of
/// its own, so one dispatched event queues exactly one turn and runs its
/// template once. A second queued turn would run that template again against
/// the statement alone once the first terminalized.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_dispatched_session_commits_exactly_one_queued_turn() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;

    assert_eq!(queued_turn_count(&fixture, fixture.session(0)).await?, 1);
    assert_eq!(queued_turn_count(&fixture, fixture.session(1)).await?, 1);
    Ok(())
}

async fn queued_turn_count(
    fixture: &DispatchFixture,
    session: SessionId,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM turn_lifecycle
          WHERE session_id = $1 AND state_kind = 'queued'",
    )
    .bind(session.as_uuid())
    .fetch_one(&fixture.pool)
    .await
}

fn synthesized_dispatch_goal(fixture: &DispatchFixture) -> Result<GoalStatement, Box<dyn Error>> {
    let actions = fixture.rule.actions_for_event(&fixture.event)?;
    // One variant, so this destructuring is irrefutable rather than a branch.
    let RepoWatchActionV1::DispatchSession(action) = &actions[0];
    Ok(action.synthesized_goal_statement(fixture.rule.id())?)
}

async fn assert_commissioned_with(
    fixture: &DispatchFixture,
    session: SessionId,
    expected: &GoalStatement,
) -> Result<(), Box<dyn Error>> {
    let goal = GoalRepository::new(fixture.pool.clone())
        .load_goal(session)
        .await?
        .ok_or("a dispatched session is commissioned when it is created")?;

    assert_eq!(goal.current().statement(), expected);
    assert_eq!(goal.generations().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn dispatched_sessions_commit_their_initial_context_and_queued_turn_atomically()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let delivery_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_watch_dispatch_delivery WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let queued_context_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_dispatch_delivery AS delivery
           JOIN turn_lifecycle AS turn ON turn.turn_id = delivery.turn_id
           JOIN submit_input_command AS command
             ON command.command_id = delivery.submit_command_id
          WHERE delivery.dispatch_id = $1
            AND turn.state_kind = 'queued'
            AND command.content_text = $2",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .bind(DISPATCH_CONTEXT)
    .fetch_one(&fixture.pool)
    .await?;
    let expected_action_count = fixture.rule.actions().len();

    assert_eq!(usize::try_from(delivery_count)?, expected_action_count);
    assert_eq!(
        usize::try_from(queued_context_count)?,
        expected_action_count
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retired_rule_identity_cannot_resume_from_its_old_activation() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture().await?;
    fixture
        .store
        .reconcile_rules(&fixture.repository, &[])
        .await?;
    let error = fixture
        .store
        .reconcile_rules(&fixture.repository, std::slice::from_ref(&fixture.rule))
        .await
        .expect_err("retired rule identity must not reactivate");

    assert!(reused_rule_identity(&error));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn removed_repository_deactivates_its_rule_identities() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let no_configured_repositories = [];
    fixture
        .store
        .deactivate_unconfigured_repositories(&no_configured_repositories)
        .await?;
    let error = fixture
        .store
        .reconcile_rules(&fixture.repository, std::slice::from_ref(&fixture.rule))
        .await
        .expect_err("a rule from a removed repository must be retired");

    assert!(reused_rule_identity(&error));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn startup_drain_processes_cutoff_after_repository_removal() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let session = fixture.session(0);
    commit_merge(&fixture, STARTUP_DRAIN_CUTOFF_EVENT_ID).await?;
    fixture
        .store
        .deactivate_unconfigured_repositories(&[])
        .await?;

    fixture
        .store
        .process_pending_lifecycle_cutoffs(|| {
            DurableCommandId::from_uuid(Uuid::from_u128(STARTUP_DRAIN_STOP_COMMAND_ID))
        })
        .await?;

    let goal = GoalRepository::new(fixture.pool.clone())
        .load_goal(session)
        .await?
        .expect("the removed repository goal remains readable");
    assert_eq!(goal.current().state(), &GoalState::UserStopped);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn active_rule_identity_rejects_in_place_content_changes() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let changed_rule = cooldown_rule()?;
    let error = fixture
        .store
        .reconcile_rules(&fixture.repository, std::slice::from_ref(&changed_rule))
        .await
        .expect_err("active rule semantics require a new identity");

    assert!(changed_rule_identity(&error));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn deactivated_rule_cannot_dispatch_an_already_loaded_event() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let (loaded, observation) = load_second_conflict(&fixture).await?;
    let batches_before: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&fixture.pool)
        .await?;
    fixture
        .store
        .reconcile_rules(&fixture.repository, &[])
        .await?;
    let outcome =
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                loaded,
                &fixture.rule,
                &observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?;
    let batches_after: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&fixture.pool)
        .await?;

    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::Inactive);
    assert_eq!(batches_after, batches_before);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cooldown_uses_the_terminal_transition_time_not_the_next_evaluation_time()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(cooldown_rule()?).await?;
    sqlx::query(
        "INSERT INTO repo_watch_dispatch_release (dispatch_id, released_at)
         VALUES ($1, transaction_timestamp() - interval '2 hours')",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .execute(&fixture.pool)
    .await?;
    let release_age_seconds: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM (transaction_timestamp() - released_at))::float8
           FROM repo_watch_dispatch_release
          WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let outcome = evaluate_second_conflict(&fixture).await?;

    assert!(release_age_seconds >= 7_199.0);
    assert!(outcome_is_dispatched(&outcome));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pursuing_goal_holds_singleton_until_its_terminal_transition() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let session = fixture.sessions[0];
    let dispatched_turn = TurnId::from_uuid(
        sqlx::query_scalar::<_, Uuid>(
            "SELECT turn_id FROM repo_watch_dispatch_delivery WHERE dispatch_id = $1",
        )
        .bind(fixture.dispatch_id.as_uuid())
        .fetch_one(&fixture.pool)
        .await?,
    );
    mark_queued_turn_failed(&fixture.pool, session, dispatched_turn, 0x4_000).await?;
    check_completed_turn_for_release(&fixture.pool, session, dispatched_turn).await?;
    assert_eq!(release_count(&fixture).await?, 0);
    assert_applied_goal_transition(
        GoalRepository::new(fixture.pool.clone())
            .block_execution_failure(
                session,
                GoalNeed::try_new(String::from("repair the failed goal turn"))
                    .expect("fixture goal need is valid"),
                GoalSchedulerProvenance::new(dispatched_turn),
            )
            .await?,
    );
    assert_eq!(release_count(&fixture).await?, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn release_timestamp_is_sampled_after_dispatch_lock_wait() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let turn: Uuid = sqlx::query_scalar(
        "SELECT turn_id FROM repo_watch_dispatch_delivery WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    sqlx::query(
        "ALTER TABLE goal_event
         DISABLE TRIGGER repo_watch_dispatch_release_on_terminal_goal",
    )
    .execute(&fixture.pool)
    .await?;
    withdraw_dispatched_goal(&fixture.pool, fixture.sessions[0], 0x20_000).await?;
    sqlx::query(
        "ALTER TABLE goal_event
         ENABLE TRIGGER repo_watch_dispatch_release_on_terminal_goal",
    )
    .execute(&fixture.pool)
    .await?;
    let mut dispatch_lock = fixture.pool.begin().await?;
    sqlx::query("SELECT 1 FROM repo_watch_dispatch_batch WHERE dispatch_id = $1 FOR UPDATE")
        .bind(fixture.dispatch_id.as_uuid())
        .execute(&mut *dispatch_lock)
        .await?;
    let mut release_connection = fixture.pool.acquire().await?;
    let release_backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *release_connection)
        .await?;
    let release = tokio::spawn(async move {
        sqlx::query("SELECT repo_watch_release_completed_dispatch_batches_for_turn($1, $2)")
            .bind(turn)
            .bind(fixture.sessions[0].as_uuid())
            .execute(&mut *release_connection)
            .await
    });

    wait_for_backend_lock(&fixture.pool, release_backend).await?;
    let serialized_at: f64 =
        sqlx::query_scalar("SELECT extract(epoch FROM clock_timestamp())::float8")
            .fetch_one(&fixture.pool)
            .await?;
    dispatch_lock.commit().await?;
    release.await??;
    let released_at: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM released_at)::float8
           FROM repo_watch_dispatch_release
          WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert!(released_at >= serialized_at);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn concurrent_terminal_batch_checks_serialize_on_the_dispatch() -> Result<(), Box<dyn Error>>
{
    let fixture = dispatch_fixture().await?;
    let turns: Vec<Uuid> = sqlx::query_scalar(
        "SELECT turn_id
           FROM repo_watch_dispatch_delivery
          WHERE dispatch_id = $1
          ORDER BY action_ordinal",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_all(&fixture.pool)
    .await?;
    sqlx::query(
        "ALTER TABLE goal_event
         DISABLE TRIGGER repo_watch_dispatch_release_on_terminal_goal",
    )
    .execute(&fixture.pool)
    .await?;
    withdraw_dispatched_goal(&fixture.pool, fixture.sessions[0], 0x30_000).await?;
    withdraw_dispatched_goal(&fixture.pool, fixture.sessions[1], 0x40_000).await?;
    sqlx::query(
        "ALTER TABLE goal_event
         ENABLE TRIGGER repo_watch_dispatch_release_on_terminal_goal",
    )
    .execute(&fixture.pool)
    .await?;
    mark_queued_turn_failed(
        &fixture.pool,
        fixture.sessions[0],
        TurnId::from_uuid(turns[0]),
        FIRST_TERMINAL_IDENTITY_SEED,
    )
    .await?;
    let mut first = fixture.pool.begin().await?;
    sqlx::query("SELECT repo_watch_release_completed_dispatch_batches_for_turn($1, $2)")
        .bind(turns[0])
        .bind(fixture.sessions[0].as_uuid())
        .execute(&mut *first)
        .await?;
    let mut second = fixture.pool.acquire().await?;
    let second_backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *second)
        .await?;
    let second_check = tokio::spawn(async move {
        sqlx::query("SELECT repo_watch_release_completed_dispatch_batches_for_turn($1, $2)")
            .bind(turns[1])
            .bind(fixture.sessions[1].as_uuid())
            .execute(&mut *second)
            .await
    });

    wait_for_backend_lock(&fixture.pool, second_backend).await?;
    first.commit().await?;
    second_check.await??;
    let release_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_dispatch_release
          WHERE dispatch_id = $1",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(release_count, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cooldown_clock_is_sampled_after_singleton_lock_wait() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    sqlx::query(
        "INSERT INTO repo_watch_dispatch_release (dispatch_id, released_at)
         VALUES ($1, clock_timestamp() + interval '2 seconds')",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .execute(&fixture.pool)
    .await?;
    let (loaded, observation) = load_second_conflict(&fixture).await?;
    let mut repository_lock = fixture.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(fixture.repository.as_str())
        .execute(&mut *repository_lock)
        .await?;
    let store = fixture.store.clone();
    let rule = fixture.rule.clone();
    let evaluation = tokio::spawn(async move {
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, store)
            .evaluate(
                loaded,
                &rule,
                &observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await
    });

    wait_for_advisory_lock(&fixture.pool).await?;
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    repository_lock.commit().await?;
    let outcome = evaluation.await??;

    assert!(outcome_is_dispatched(&outcome));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_pull_request_singleton_suppresses_a_later_match() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let outcome = evaluate_second_conflict(&fixture).await?;

    assert_eq!(outcome, RepoWatchRuleEvaluationOutcome::Occupied);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn occupied_matches_collapse_into_one_visible_dispatch_obligation()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let second = evaluate_second_conflict(&fixture).await?;
    let third = evaluate_conflict(&fixture, 103, THIRD_HEAD).await?;
    let visible: (i64, Uuid, String, Uuid, Vec<Uuid>, bool) = sqlx::query_as(
        "SELECT matched_event_count, latest_event_id,
                singleton_pull_request_number::text, occupying_dispatch_id,
                occupying_session_ids, ready
           FROM repo_watch_outstanding_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(second, RepoWatchRuleEvaluationOutcome::Occupied);
    assert_eq!(third.outcome, RepoWatchRuleEvaluationOutcome::Occupied);
    assert_eq!(visible.0, 2);
    assert_eq!(visible.1, *third.event_id.as_uuid());
    assert_eq!(
        visible.2,
        pull_request_number(&fixture.event).get().to_string()
    );
    assert_eq!(visible.3, *fixture.dispatch_id.as_uuid());
    assert_eq!(visible.4, session_uuids(&fixture));
    assert!(!visible.5);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn released_obligation_dispatches_latest_state_once_and_replays_that_delivery()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let _second = evaluate_second_conflict(&fixture).await?;
    sqlx::query(
        "INSERT INTO repo_watch_dispatch_release (dispatch_id)
         VALUES ($1)",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .execute(&fixture.pool)
    .await?;
    let third = evaluate_conflict(&fixture, 103, THIRD_HEAD).await?;
    let cursor = PostgresRepoWatchStore::new(fixture.pool.clone())
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let obligation = fixture
        .store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?
        .expect("released obligation is ready");
    let replay_candidate = obligation.clone();

    assert_eq!(third.outcome, RepoWatchRuleEvaluationOutcome::Occupied);
    assert_eq!(obligation.matched_event_count(), 2);
    assert_eq!(obligation.latest_event().id(), third.event_id);
    let (dispatch_id, sessions) = dispatched(
        evaluate_obligation(&fixture, obligation, cursor.candidate().observation()).await?,
    );
    let outstanding: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_outstanding_dispatch_obligation")
            .fetch_one(&fixture.pool)
            .await?;
    let batch_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&fixture.pool)
        .await?;
    let (replayed_dispatch, replayed_sessions) = replayed(
        evaluate_obligation(&fixture, replay_candidate, cursor.candidate().observation()).await?,
    );
    let replayed_batch_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
            .fetch_one(&fixture.pool)
            .await?;

    assert_eq!(outstanding, 0);
    assert_eq!(batch_count, 2);
    assert_eq!(replayed_dispatch, dispatch_id);
    assert_eq!(replayed_sessions, sessions);
    assert_eq!(replayed_batch_count, batch_count);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn dispatch_obligation_waits_visibly_through_configured_cooldown()
-> Result<(), Box<dyn Error>> {
    let fixture =
        dispatch_fixture_for(one_action_rule(Duration::from_secs(i64::MAX as u64))?).await?;
    let _outcome = evaluate_second_conflict(&fixture).await?;
    sqlx::query(
        "INSERT INTO repo_watch_dispatch_release (dispatch_id)
         VALUES ($1)",
    )
    .bind(fixture.dispatch_id.as_uuid())
    .execute(&fixture.pool)
    .await?;
    let obligation = fixture
        .store
        .load_next_dispatch_obligation(
            &fixture.repository,
            fixture.rule.id(),
            fixture.rule.version(),
        )
        .await?;
    let visible: OutstandingCooldownVisibility = sqlx::query_as(
        "SELECT matched_event_count,
                eligible_at > clock_timestamp() AS eligible_at_is_future, ready
           FROM repo_watch_outstanding_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;

    assert!(obligation.is_none());
    assert_eq!(visible.matched_event_count, 1);
    assert!(visible.eligible_at_is_future);
    assert!(!visible.ready);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn rule_deactivation_settles_its_outstanding_dispatch_obligation()
-> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let _outcome = evaluate_second_conflict(&fixture).await?;
    fixture
        .store
        .reconcile_rules(&fixture.repository, &[])
        .await?;
    let outstanding: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_outstanding_dispatch_obligation")
            .fetch_one(&fixture.pool)
            .await?;
    let settlement: String = sqlx::query_scalar(
        "SELECT settled_kind
           FROM repo_watch_dispatch_obligation",
    )
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(outstanding, 0);
    assert_eq!(settlement, "deactivated");
    Ok(())
}
