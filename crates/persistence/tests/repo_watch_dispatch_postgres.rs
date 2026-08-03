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
    BranchName, CommitSha, DangerousToolAutoApproval, DirectModelSelection, MergeableState,
    ModelSelectionRequest, PullRequestBody, PullRequestEventContext, PullRequestEventContextInput,
    PullRequestNumber, PullRequestTitle, RepoWatchAuthorLogin, RepoWatchEvent, RepoWatchEventId,
    RepoWatchEventKindNameV1, RepoWatchEventKindV1, RepoWatchMatcherV1, RepoWatchMatcherV1Input,
    RepoWatchRule, RepoWatchRuleActionV1, RepoWatchRuleId, RepoWatchSingletonScope, RepositorySlug,
    SessionConfigurationDefaults, SessionId, SessionSystemPrompt, SessionTemplateContentDigest,
    SessionTemplateName, SessionTemplateProvenance,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential, local_test_connection_options, migrate,
    repo_watch::{
        PostgresRepoWatchStore, RepoWatchCommitOutcome, RepoWatchCommitRequest,
        RepoWatchCursorCandidate, RepoWatchCursorGeneration,
    },
    repo_watch_dispatch::{
        PostgresRepoWatchDispatchStore, RepoWatchDeliveryCandidates, RepoWatchPendingDelivery,
    },
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
const FIRST_HEAD: &str = "1111111111111111111111111111111111111111";
const SECOND_HEAD: &str = "2222222222222222222222222222222222222222";
const TEMPLATE: &str = "merge-forward";
const RULE: &str = "merge-forward-on-conflict";

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
    Ok(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![RepoWatchPullRequestState::try_new(
                RepoWatchPullRequestStateInput {
                    context,
                    lifecycle: RepoWatchPullRequestLifecycle::Open,
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

fn rule() -> Result<RepoWatchRule, Box<dyn Error>> {
    let template = SessionTemplateName::try_new(TEMPLATE.to_owned())?;
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(RULE.to_owned())?,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::MergeableStateChanged],
            mergeable_state: vec![MergeableState::Conflicting],
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![
            RepoWatchRuleActionV1::DispatchSession {
                template: template.clone(),
            },
            RepoWatchRuleActionV1::DispatchSession { template },
        ],
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

fn credential_pin() -> SessionCredentialPin {
    SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        "fixture-family",
        "fixture-credential",
    )])
    .expect("fixture credential pin is valid")
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

async fn dispatch_fixture() -> Result<DispatchFixture, Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let repository = repository()?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let dispatch_store = PostgresRepoWatchDispatchStore::new(pool.clone(), credential_pin());
    let initial = RepoWatchCursorCandidate::new(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::default(),
    ));
    let first_generation = generation(
        event_store
            .commit(
                &repository,
                RepoWatchCommitRequest::new(None, initial, Vec::new()),
            )
            .await?,
    );
    let rule = rule()?;
    dispatch_store
        .activate_rule(&repository, rule.id(), rule.version())
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
            .evaluate(loaded, &rule, &observation, &TemplateResolver)
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

fn delivery_candidates(value: u128) -> RepoWatchDeliveryCandidates {
    RepoWatchDeliveryCandidates {
        submit_command_id: signalbox_domain::DurableCommandId::from_uuid(Uuid::from_u128(value)),
        accepted_input_id: signalbox_domain::AcceptedInputId::from_uuid(Uuid::from_u128(value + 1)),
        turn_id: signalbox_domain::TurnId::from_uuid(Uuid::from_u128(value + 2)),
        cancellation_entry_id: signalbox_domain::SemanticTranscriptEntryId::from_uuid(
            Uuid::from_u128(value + 3),
        ),
        cancellation_frontier_id: signalbox_domain::ContextFrontierId::from_uuid(Uuid::from_u128(
            value + 4,
        )),
    }
}

fn pending(delivery: Option<RepoWatchPendingDelivery>) -> RepoWatchPendingDelivery {
    delivery.expect("fixture has an undelivered action")
}

async fn evaluate_second_conflict(
    fixture: &DispatchFixture,
) -> Result<RepoWatchRuleEvaluationOutcome, Box<dyn Error>> {
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
    Ok(
        RepoWatchDispatchService::new(UuidV7RepoWatchDispatchIdGenerator, fixture.store.clone())
            .evaluate(loaded, &fixture.rule, &observation, &TemplateResolver)
            .await?,
    )
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

    assert_eq!(fixture.sessions.len(), 2);
    assert_eq!(action_count, 2);
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pending_context_delivery_reuses_its_reserved_identities() -> Result<(), Box<dyn Error>> {
    let fixture = dispatch_fixture().await?;
    let first = pending(
        fixture
            .store
            .prepare_next_delivery(&fixture.repository, delivery_candidates(1_001))
            .await?,
    );
    let replay = pending(
        fixture
            .store
            .prepare_next_delivery(&fixture.repository, delivery_candidates(2_001))
            .await?,
    );

    assert_eq!(first.dispatch_id(), replay.dispatch_id());
    assert_eq!(first.action_ordinal(), replay.action_ordinal());
    assert_eq!(first.identities(), replay.identities());
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
