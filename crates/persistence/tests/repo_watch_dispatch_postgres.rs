#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{error::Error, time::Duration};

use signalbox_application::{
    RepoWatchDispatchService, RepoWatchObservation, RepoWatchPullRequestLifecycle,
    RepoWatchPullRequestState, RepoWatchPullRequestStateInput, RepoWatchRepositoryState,
    RepoWatchRepositoryStateInput, RepoWatchResolvedTemplate, RepoWatchRuleEvaluationOutcome,
    RepoWatchTemplateResolver, UuidV7RepoWatchDispatchIdGenerator,
};
use signalbox_domain::{
    BranchName, CommitSha, DangerousToolAutoApproval, DescendantTerminationScope,
    DirectModelSelection, DurableCommandId, GoalCommandResult, GoalNeed, GoalSchedulerProvenance,
    GoalState, GoalStatement, GoalUserAction, GoalUserCommand, MergeableState,
    ModelSelectionRequest, PullRequestBody, PullRequestEventContext, PullRequestEventContextInput,
    PullRequestNumber, PullRequestTitle, RepoWatchActionV1, RepoWatchAuthorLogin, RepoWatchEvent,
    RepoWatchEventId, RepoWatchEventKindNameV1, RepoWatchEventKindV1, RepoWatchMatcherV1,
    RepoWatchMatcherV1Input, RepoWatchRule, RepoWatchRuleActionV1, RepoWatchRuleId,
    RepoWatchSingletonScope, RepositorySlug, SessionConfigurationDefaults, SessionId,
    SessionSystemPrompt, SessionTemplateContentDigest, SessionTemplateName,
    SessionTemplateProvenance, TurnId, UserContent,
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
const TEMPLATE: &str = "merge-forward";
const RULE: &str = "merge-forward-on-conflict";
const DISPATCH_CONTEXT: &str = r#"{"fixture":"repository-watch"}"#;
const FIRST_TERMINAL_IDENTITY_SEED: u128 = 0x10_000;

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
        number: PullRequestNumber::new(41_u64.try_into()?),
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

fn merged_event(value: u128, head: &str) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context(head)?,
        RepoWatchEventKindV1::PullRequestMerged,
    )?)
}

fn opened_event(value: u128, head: &str) -> Result<RepoWatchEvent, Box<dyn Error>> {
    Ok(RepoWatchEvent::try_pull_request(
        RepoWatchEventId::from_uuid(Uuid::from_u128(value)),
        repository()?,
        context(head)?,
        RepoWatchEventKindV1::PullRequestOpened,
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
/// This stops the goal rather than failing its turn, and the difference is the
/// point: the dispatched work turn is that goal's own turn, so failing it and
/// then blocking would leave every delivery turn terminal and the goal
/// non-pursuing at once, and the terminal-goal trigger would insert the release
/// here instead of at the moment the caller is measuring. A user stop ends
/// pursuit while the work turn is still queued, which is what makes that
/// trigger's release check decline.
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
    let initial = observation(context(INITIAL_HEAD)?)?;
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(
                    None,
                    RepoWatchCursorCandidate::new(initial),
                    vec![opened_event(100, INITIAL_HEAD)?],
                ),
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
    let (loaded, observation) = load_second_conflict(fixture).await?;
    Ok(
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(
                loaded,
                &fixture.rule,
                &observation,
                &TemplateResolver,
                dispatch_context(),
            )
            .await?,
    )
}

async fn load_second_conflict(
    fixture: &DispatchFixture,
) -> Result<(RepoWatchEvent, RepoWatchObservation), Box<dyn Error>> {
    let event_store = PostgresRepoWatchStore::new(fixture.pool.clone());
    let cursor = event_store
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let event = conflict_event(102, SECOND_HEAD)?;
    let observation = observation(context(SECOND_HEAD)?)?;
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

async fn commit_merge(fixture: &DispatchFixture, value: u128) -> Result<(), Box<dyn Error>> {
    let event_store = PostgresRepoWatchStore::new(fixture.pool.clone());
    let cursor = event_store
        .load_cursor(&fixture.repository)
        .await?
        .expect("fixture cursor exists");
    let merged =
        lifecycle_observation(context(SECOND_HEAD)?, RepoWatchPullRequestLifecycle::Merged)?;
    event_store
        .commit(
            &fixture.repository,
            RepoWatchCommitRequest::new(
                Some(cursor.generation()),
                RepoWatchCursorCandidate::new(merged),
                vec![merged_event(value, SECOND_HEAD)?],
            ),
        )
        .await?;
    Ok(())
}

/// A merged pull request withdraws the still-pursuing goal that repository
/// watch commissioned and releases its queued singleton.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn merged_pull_request_ends_the_commissioned_goal() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let session = fixture.session(0);
    commit_merge(&fixture, 0x51_000).await?;

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

/// Dispatch admission rechecks durable lifecycle after an event was loaded, so
/// a merge committed in between prevents a stale matching event from firing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn matching_event_loaded_before_merge_records_target_closed() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture_for(one_action_rule(Duration::ZERO)?).await?;
    let (loaded, stale_open_observation) = load_second_conflict(&fixture).await?;
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
    let dispatch_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_dispatch_batch")
        .fetch_one(&fixture.pool)
        .await?;
    assert_eq!(dispatch_count, 1);
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
    withdraw_dispatched_goal(&fixture.pool, fixture.sessions[0], 0x20_000).await?;
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
    withdraw_dispatched_goal(&fixture.pool, fixture.sessions[0], 0x30_000).await?;
    withdraw_dispatched_goal(&fixture.pool, fixture.sessions[1], 0x40_000).await?;
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
